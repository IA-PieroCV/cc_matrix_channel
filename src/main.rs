mod access;
mod config;
mod live_status;
mod matrix;
mod mcp;
mod rooms;
mod status;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use matrix_sdk::ruma::OwnedRoomId;
use rmcp::{ServiceExt, transport::stdio};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriterExt;

use crate::access::AccessControl;
use crate::config::Config;
use crate::matrix::{ChannelNotification, MatrixBridge, MatrixBridgeConfig, PermissionVerdict};
use crate::mcp::{MatrixChannelServer, McpServerConfig};

#[tokio::main]
async fn main() -> Result<()> {
    // Logging goes to stderr — stdout is exclusively for MCP JSON-RPC. Stderr is
    // captured and discarded by the host (Claude Code), so a copy also goes to a
    // rolling file; without it a silent stall leaves no evidence anywhere.
    let log_dir = dirs_next::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("channels")
        .join("matrix");
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let file_logging = std::fs::create_dir_all(&log_dir).ok().and_then(|()| {
        tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("bridge")
            .filename_suffix("log")
            .max_log_files(7)
            .build(&log_dir)
            .ok()
    });

    // Guard must outlive main's body — dropping it stops the writer thread.
    let _log_guard = match file_logging {
        Some(appender) => {
            let (non_blocking, guard) = tracing_appender::non_blocking(appender);
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr.and(non_blocking))
                .with_env_filter(filter())
                .with_ansi(false)
                .init();
            Some(guard)
        }
        None => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(filter())
                .with_ansi(false)
                .init();
            None
        }
    };

    tracing::info!("cc_matrix_channel v{} starting", env!("CARGO_PKG_VERSION"));
    if _log_guard.is_some() {
        tracing::info!("Logging to {}/bridge.<date>.log", log_dir.display());
    } else {
        tracing::warn!(
            "File logging unavailable ({}) — stderr only",
            log_dir.display()
        );
    }

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
    // Restored from disk so a restart does not forget where to post status — otherwise the
    // liveness feature goes silent until someone messages the bridge again.
    let (restored_rooms, restored_last_active) = rooms::load(&rooms::store_path());
    let known_rooms: Arc<parking_lot::Mutex<HashSet<OwnedRoomId>>> =
        Arc::new(parking_lot::Mutex::new(restored_rooms));
    let last_active_room: Arc<parking_lot::Mutex<Option<OwnedRoomId>>> =
        Arc::new(parking_lot::Mutex::new(restored_last_active));
    let pending_permissions: Arc<parking_lot::Mutex<HashSet<String>>> =
        Arc::new(parking_lot::Mutex::new(HashSet::new()));

    // Initialize Matrix bridge if credentials are available; otherwise setup mode
    let matrix_bridge = if config.has_credentials() {
        let bridge = MatrixBridge::new(
            &config,
            MatrixBridgeConfig {
                notification_tx: notification_tx.clone(),
                permission_verdict_tx: permission_verdict_tx.clone(),
                access_control: access_control.clone(),
                known_rooms: known_rooms.clone(),
                last_active_room: last_active_room.clone(),
                pending_permissions: pending_permissions.clone(),
                cancel: cancel.clone(),
            },
        )
        .await?;
        Some(bridge)
    } else {
        tracing::warn!("Matrix credentials not configured. Run /matrix:configure to set up.");
        None
    };

    let matrix_client = matrix_bridge.as_ref().map(|b| Arc::new(b.client().clone()));

    // Live agent-status message. Alongside the parent-PID poll above, this is the other
    // signal that does not depend on the agent being responsive enough to speak for itself.
    if let Some(client) = matrix_client.clone() {
        live_status::spawn(
            client,
            known_rooms.clone(),
            last_active_room.clone(),
            cancel.clone(),
        );
    }

    // MCP server — handles both setup and full mode
    let mcp_server = MatrixChannelServer::new(McpServerConfig {
        matrix_client,
        access_control,
        known_rooms,
        last_active_room,
        pending_permissions,
        notification_tx,
        notification_rx,
        permission_verdict_tx,
        permission_verdict_rx,
        store_path: PathBuf::from(&config.store_path),
        env_path,
        cancel: cancel.clone(),
    });

    if matrix_bridge.is_some() {
        tracing::info!("Starting Matrix sync + MCP server");
    } else {
        tracing::info!("Starting MCP server in setup mode (waiting for credentials)");
    }

    tokio::select! {
        result = async {
            if let Some(bridge) = matrix_bridge {
                bridge.run().await
            } else {
                // No bridge — wait for cancellation (hot-transition handles bridge startup)
                cancel.cancelled().await;
                Ok(())
            }
        } => {
            if let Err(ref e) = result {
                tracing::error!("Matrix sync loop exited: {e:?}");
            }
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
