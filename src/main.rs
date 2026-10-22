mod catalog;
mod client_api;
mod cluster;
mod config;
mod db;
mod emby_dto;
mod gateway;
mod hooks;
mod metadata;
mod model;
mod probe;
mod query;
mod resolver;
mod sessions;
mod streaming;
mod subtitles;
mod tv_api;
mod versions;
mod worker;

use std::sync::Arc;

use config::{Config, Role};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let config = Arc::new(Config::from_env()?);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    match config.role {
        Role::Gateway => {
            let state = gateway::GatewayState::new(Arc::clone(&config))?;
            info!(address = %config.bind, "gateway listening");
            axum::serve(listener, gateway::router(state))
                .with_graceful_shutdown(shutdown_signal())
                .await?;
        }
        Role::Worker => {
            let state = worker::WorkerState::new(Arc::clone(&config))?;
            tokio::spawn(worker::sync_loop(state.clone()));
            info!(node = %config.node_id, address = %config.bind, "worker listening");
            axum::serve(listener, worker::router(state))
                .with_graceful_shutdown(shutdown_signal())
                .await?;
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
