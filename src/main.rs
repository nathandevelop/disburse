//! solmux entry point: parses CLI/config, spawns health monitors, starts the
//! axum servers for RPC and metrics, and handles graceful shutdown.

mod config;
mod error;
mod proxy;
mod routing;
mod solana;
mod upstream;

use crate::config::Config;
use crate::proxy::{app, metrics_app, AppState, Metrics};
use crate::routing::UpstreamPool;
use crate::solana::BlockhashCache;
use crate::upstream::spawn_health_monitors;
use clap::Parser;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "solmux", version, about = "Solana-native RPC router")]
struct Args {
    #[arg(long, default_value = "./config.yaml")]
    config: String,
    /// Validate config and exit without starting the server. For CI pipelines.
    #[arg(long)]
    check: bool,
    /// Issue a GET to --healthcheck-url and exit 0 on 2xx, 1 otherwise.
    /// Intended for Docker HEALTHCHECK directives on distroless images.
    #[arg(long)]
    healthcheck: bool,
    #[arg(long, default_value = "http://127.0.0.1:8899/livez")]
    healthcheck_url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    if args.healthcheck {
        return run_healthcheck(&args.healthcheck_url).await;
    }

    let cfg = Config::load(&args.config)?;

    if args.check {
        info!(config = %args.config, "config valid");
        return Ok(());
    }
    info!(config = %args.config, upstreams = cfg.upstreams.len(), "loaded config");

    let pool = Arc::new(UpstreamPool::from_config(&cfg));
    let blockhash_cache = Arc::new(BlockhashCache::new(
        cfg.health.blockhash_warmup_secs,
        cfg.health.blockhash_cache_size,
    ));
    let metrics = Arc::new(Metrics::new());

    spawn_health_monitors(pool.clone(), blockhash_cache.clone());

    let state = AppState {
        pool: pool.clone(),
        blockhash_cache: blockhash_cache.clone(),
        metrics: metrics.clone(),
    };

    let main_addr: SocketAddr = cfg.listen.parse()?;
    let metrics_addr: SocketAddr = cfg.metrics_listen.parse()?;

    let main_app = app(state.clone());
    let met_app = metrics_app(state);

    info!(listen = %main_addr, metrics = %metrics_addr, "solmux starting");

    let main_listener = tokio::net::TcpListener::bind(main_addr).await?;
    let metrics_listener = tokio::net::TcpListener::bind(metrics_addr).await?;

    let main = tokio::spawn(async move {
        axum::serve(main_listener, main_app)
            .with_graceful_shutdown(shutdown_signal())
            .await
    });
    let met = tokio::spawn(async move {
        axum::serve(metrics_listener, met_app)
            .with_graceful_shutdown(shutdown_signal())
            .await
    });

    let _ = tokio::try_join!(main, met);
    info!("solmux shut down");
    Ok(())
}

async fn run_healthcheck(url: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()?;
    match client.get(url).send().await {
        Ok(r) if r.status().is_success() => {
            println!("ok");
            Ok(())
        }
        Ok(r) => {
            eprintln!("healthcheck failed: status {}", r.status());
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("healthcheck failed: {e}");
            std::process::exit(1);
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut s) = signal(SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("SIGINT received, shutting down"),
        _ = terminate => info!("SIGTERM received, shutting down"),
    }
}
