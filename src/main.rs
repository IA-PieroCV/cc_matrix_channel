mod access;
mod config;
mod matrix;
mod mcp;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use anyhow::Result;
use clap::Parser;
use matrix_sdk::ruma::OwnedRoomId;
use rmcp::{ServiceExt, transport::stdio};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::access::AccessControl;
use crate::config::Config;
use crate::matrix::{ChannelNotification, MatrixBridge, MatrixBridgeConfig, PermissionVerdict};
use crate::mcp::{MatrixChannelServer, McpServerConfig, SetupModeServer};

#[tokio::main]
async fn main() -> Result<()> {
    // All logging goes to stderr — stdout is exclusively for MCP JSON-RPC
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(false)
        .init();

    tracing::info!("cc_matrix_channel v{} starting", env!("CARGO_PKG_VERSION"));

    // Load .env file if present (process env vars from .mcp.json take precedence)
    let env_path = dirs_next::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude/channels/matrix/.env");
    if env_path.exists() {
        dotenvy::from_path(&env_path).ok();
        tracing::info!("Loaded .env from {}", env_path.display());
    }

    let config = Config::parse();

    // Graceful shutdown coordination
    let cancel = CancellationToken::new();

    // Linux: ask kernel to send SIGTERM when parent process dies.
    // Our signal handler below catches SIGTERM → cancels the token.
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) };
        if unsafe { libc::getppid() } == 1 {
            anyhow::bail!("Parent process already exited");
        }
    }

    // Signal handler — cancel on SIGTERM/SIGINT
    let cancel_for_signal = cancel.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
            let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
            tokio::select! {
                _ = sigterm.recv() => tracing::info!("Received SIGTERM"),
                _ = sigint.recv() => tracing::info!("Received SIGINT"),
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("Received Ctrl-C");
        }
        cancel_for_signal.cancel();
    });

    // Unix: poll parent PID to detect orphaning (primary mechanism on macOS,
    // belt-and-suspenders with prctl on Linux)
    #[cfg(unix)]
    {
        let cancel_for_orphan = cancel.clone();
        let original_ppid = unsafe { libc::getppid() };
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                let current_ppid = unsafe { libc::getppid() };
                if current_ppid != original_ppid {
                    tracing::warn!(
                        "Parent process died (was {original_ppid}, now {current_ppid}); shutting down"
                    );
                    cancel_for_orphan.cancel();
                    break;
                }
            }
        });
    }

    // Setup mode: if credentials aren't configured, start a bare MCP server
    // so the plugin loads and /matrix:configure skill registers as a slash command.
    // A background watcher polls for the .env file — when credentials appear,
    // the process exits cleanly so Claude Code restarts it with the full server.
    if !config.has_credentials() {
        tracing::warn!("Matrix credentials not configured. Run /matrix:configure to set up.");

        let env_path_watch = env_path.clone();
        let cancel_for_watch = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                if credentials_present(&env_path_watch) {
                    tracing::info!(
                        "Credentials detected in .env — restarting to initialize Matrix bridge"
                    );
                    cancel_for_watch.cancel();
                    break;
                }
            }
        });

        tokio::select! {
            result = async {
                let service = SetupModeServer
                    .serve(stdio())
                    .await
                    .map_err(|e| anyhow::anyhow!("MCP setup server failed: {e}"))?;
                service
                    .waiting()
                    .await
                    .map_err(|e| anyhow::anyhow!("MCP setup server failed: {e}"))?;
                Ok::<(), anyhow::Error>(())
            } => {
                result?;
            }
            _ = cancel.cancelled() => {
                // Credentials appeared or signal received — clean exit triggers auto-restart
                tracing::info!("Setup mode exiting for restart");
            }
        }
        return Ok(());
    }

    // Shared state
    let config_path = dirs_next::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("channels")
        .join("matrix")
        .join("access.json");
    let access_control = Arc::new(AccessControl::new(config_path));
    let (notification_tx, notification_rx) = mpsc::channel::<ChannelNotification>(256);
    let (permission_verdict_tx, permission_verdict_rx) = mpsc::channel::<PermissionVerdict>(16);
    let known_rooms: Arc<parking_lot::Mutex<HashSet<OwnedRoomId>>> =
        Arc::new(parking_lot::Mutex::new(HashSet::new()));
    let pending_permissions: Arc<parking_lot::Mutex<HashSet<String>>> =
        Arc::new(parking_lot::Mutex::new(HashSet::new()));
    // Starts false — set to true in initialize() when the MCP client declares
    // the claude/channel capability (i.e. the session was started with --dangerously-load-development-channels).
    let channel_mode = Arc::new(AtomicBool::new(false));
    let _ = AtomicOrdering::Relaxed; // suppress unused import warning

    // Matrix client
    let matrix_bridge = MatrixBridge::new(
        &config,
        MatrixBridgeConfig {
            notification_tx,
            permission_verdict_tx,
            access_control: access_control.clone(),
            known_rooms: known_rooms.clone(),
            pending_permissions: pending_permissions.clone(),
            channel_mode: channel_mode.clone(),
            cancel: cancel.clone(),
        },
    )
    .await?;
    let matrix_client = Arc::new(matrix_bridge.client().clone());

    // MCP server
    let mcp_server = MatrixChannelServer::new(McpServerConfig {
        matrix_client,
        access_control,
        known_rooms,
        pending_permissions,
        notification_rx,
        permission_verdict_rx,
        store_path: std::path::PathBuf::from(&config.store_path),
        channel_mode,
        cancel: cancel.clone(),
    });

    tracing::info!("Starting Matrix sync + MCP server");

    tokio::select! {
        result = matrix_bridge.run() => {
            tracing::error!("Matrix sync loop exited: {result:?}");
            cancel.cancel();
            result?;
        }
        result = async {
            let service = mcp_server
                .serve(stdio())
                .await
                .map_err(|e| anyhow::anyhow!("MCP serve failed: {e}"))?;
            service.waiting().await.map_err(|e| anyhow::anyhow!("MCP wait failed: {e}"))?;
            Ok::<(), anyhow::Error>(())
        } => {
            tracing::info!("MCP server exited: {result:?}");
            cancel.cancel();
            result?;
        }
        _ = cancel.cancelled() => {
            tracing::info!("Shutdown signal received");
        }
    }

    // Brief grace period for in-flight work to complete
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    tracing::info!("cc_matrix_channel shutting down");

    Ok(())
}

/// Check whether the .env file contains the minimum required credentials.
fn credentials_present(env_path: &std::path::Path) -> bool {
    let Ok(content) = std::fs::read_to_string(env_path) else {
        return false;
    };
    let has_homeserver = content.lines().any(|l| {
        l.starts_with("MATRIX_HOMESERVER_URL=") && l.len() > "MATRIX_HOMESERVER_URL=".len()
    });
    let has_user_id = content
        .lines()
        .any(|l| l.starts_with("MATRIX_USER_ID=") && l.len() > "MATRIX_USER_ID=".len());
    let has_auth = content.lines().any(|l| {
        (l.starts_with("MATRIX_PASSWORD=") && l.len() > "MATRIX_PASSWORD=".len())
            || (l.starts_with("MATRIX_ACCESS_TOKEN=") && l.len() > "MATRIX_ACCESS_TOKEN=".len())
    });
    has_homeserver && has_user_id && has_auth
}
