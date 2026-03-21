mod access;
mod config;
mod matrix;
mod mcp;

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use matrix_sdk::ruma::OwnedRoomId;
use rmcp::{ServiceExt, transport::stdio};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use crate::access::AccessControl;
use crate::config::Config;
use crate::matrix::{ChannelNotification, MatrixBridge};
use crate::mcp::MatrixChannelServer;

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

    let config = Config::parse();

    // Shared state
    let access_control = Arc::new(AccessControl::new(
        config.parse_allowed_users(),
        config.static_access,
        Some(&config.store_path),
    ));
    let (notification_tx, notification_rx) = mpsc::channel::<ChannelNotification>(256);
    let known_rooms: Arc<parking_lot::Mutex<HashSet<OwnedRoomId>>> =
        Arc::new(parking_lot::Mutex::new(HashSet::new()));

    // Matrix client
    let matrix_bridge = MatrixBridge::new(
        &config,
        notification_tx,
        access_control.clone(),
        known_rooms.clone(),
    )
    .await?;
    let matrix_client = Arc::new(matrix_bridge.client().clone());

    // MCP server
    let mcp_server = MatrixChannelServer::new(
        matrix_client,
        access_control,
        known_rooms,
        notification_rx,
        std::path::PathBuf::from(&config.store_path),
    );

    tracing::info!("Starting Matrix sync + MCP server");

    tokio::select! {
        result = matrix_bridge.run() => {
            tracing::error!("Matrix sync loop exited: {result:?}");
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
            result?;
        }
    }

    Ok(())
}
