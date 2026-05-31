mod app;
mod auth;
mod config;
mod error;
mod metrics;
mod observe;
mod openai;
mod operator;
mod proxy;
mod routing;

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::app::AppState;
use crate::config::{Config, ConfigWatcher};
use crate::error::Result;
use crate::metrics::{Metrics, TelemetryGuard};

const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

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
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);
    let bind = config.server.bind;
    let state = Arc::new(AppState::new(config, metrics, shutdown_tx.clone())?);
    let _config_watcher = ConfigWatcher::start(&args.config, state.config.clone())?;
    let app = build_router(state);

    info!(%bind, "starting onair router");
    serve(bind, app, shutdown_tx).await?;
    telemetry_guard.shutdown();
    Ok(())
}

fn build_router(state: Arc<AppState>) -> Router {
    app::router(state)
}

async fn serve(bind: SocketAddr, app: Router, shutdown: watch::Sender<bool>) -> Result<()> {
    let listener = TcpListener::bind(bind).await?;
    let mut shutdown_rx = shutdown.subscribe();
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(shutdown))
    .into_future();
    tokio::pin!(server);

    tokio::select! {
        result = &mut server => result?,
        _ = wait_for_shutdown(&mut shutdown_rx) => {
            match tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, &mut server).await {
                Ok(result) => result?,
                Err(_) => {
                    warn!(
                        timeout_ms = GRACEFUL_SHUTDOWN_TIMEOUT.as_millis(),
                        "graceful shutdown timed out; forcing server task drop"
                    );
                }
            }
        }
    }
    Ok(())
}

async fn shutdown_signal(shutdown: watch::Sender<bool>) {
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
    let _ = shutdown.send(true);
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow_and_update() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }
}
