use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use tokio::net::TcpListener;

mod config;
mod gateway;
mod metrics;
mod model;
mod stream_translate;
mod translate;

use config::Config;
use gateway::AppState;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("axonhub-rs {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let config_path =
        if let Some(position) = args.iter().position(|arg| arg == "--config" || arg == "-c") {
            PathBuf::from(args.get(position + 1).context("--config needs a path")?)
        } else {
            PathBuf::from(
                std::env::var("AXONHUB_RS_CONFIG").unwrap_or_else(|_| "config.yml".to_string()),
            )
        };
    let config = Config::load(&config_path)?;
    if args
        .iter()
        .any(|arg| arg == "--check" || arg == "--validate")
    {
        println!(
            "config ok: {} channels, {} public models, listen {}",
            config.channels.len(),
            config.public_models().len(),
            config.listen
        );
        return Ok(());
    }
    let listen = config.listen.clone();
    tracing::info!(%listen, config = %config_path.display(), channels = config.channels.len(), "gateway starting");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(config, config_path, listen))
}

async fn run(config: Arc<Config>, config_path: PathBuf, listen: String) -> Result<()> {
    let state = AppState::new(config, config_path);
    spawn_sighup(state.clone());

    let app: Router = gateway::router(state.clone());
    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    tracing::info!(%listen, "gateway listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve")?;
    Ok(())
}

fn spawn_sighup(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut signal = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        {
            Ok(signal) => signal,
            Err(error) => {
                tracing::warn!("cannot install SIGHUP handler: {error}");
                return;
            }
        };
        while signal.recv().await.is_some() {
            match state.reload() {
                Ok(()) => tracing::info!("configuration reloaded on SIGHUP"),
                Err(error) => tracing::error!("configuration reload failed: {error:#}"),
            }
        }
    });
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown requested");
}
