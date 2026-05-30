mod app;
mod auth;
mod config;
mod debug_capture;
mod error;
mod metrics;
mod openai;
mod proxy;
mod routing;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use clap::Parser;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::app::AppState;
use crate::config::{Config, ConfigWatcher};
use crate::error::Result;
use crate::metrics::{Metrics, TelemetryGuard};

#[derive(Debug, Parser)]
#[command(version, about = "OpenAI-compatible reverse proxy router")]
struct Args {
    /// Path to the TOML configuration file.
    #[arg(short, long, env = "ONAIR_CONFIG", default_value = "onair.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let config = Config::load(&args.config)?;
    let telemetry_guard = TelemetryGuard::install(&config.telemetry)?;
    let metrics = Metrics::new();
    let bind = config.server.bind;
    let state = Arc::new(AppState::new(config, metrics)?);
    let _config_watcher = ConfigWatcher::start(&args.config, state.config.clone())?;
    let app = build_router(state);

    info!(%bind, "starting onair router");
    serve(bind, app).await?;
    telemetry_guard.shutdown();
    Ok(())
}

fn build_router(state: Arc<AppState>) -> Router {
    app::router(state)
}

async fn serve(bind: SocketAddr, app: Router) -> Result<()> {
    let listener = TcpListener::bind(bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
