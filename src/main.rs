mod cache;
mod chain;
mod check;
mod config;
mod error;
mod extract;
mod handlers;
mod index;
mod indexer;
mod router;
mod rpc;
mod state;
mod util;

use std::{sync::Arc, time::Instant};

use state::AppState;

const USAGE: &str = "usage: block-explorer [--check] [--check-scan]

  (no flags)    run the HTTP server
  --check       probe the configured RPC endpoint and print what it supports
  --check-scan  like --check, and also time a real scantxoutset (slow on mainnet)

Configuration comes from environment variables and ./.env (see .env.example).";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    config::load_dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cfg = config::Config::from_env()?;

    let scan = args.iter().any(|a| a == "--check-scan");
    if scan || args.iter().any(|a| a == "--check") {
        let rpc = rpc::Rpc::new(cfg.rpc.clone())?;
        let ok = check::run(&rpc, cfg.network, scan).await;
        std::process::exit(if ok { 0 } else { 1 });
    }
    if let Some(a) = args.first() {
        return Err(format!("unknown argument `{a}`\n\n{USAGE}").into());
    }

    let rpc = Arc::new(rpc::Rpc::new(cfg.rpc.clone())?);
    tracing::info!(network = %cfg.network, rpc = %rpc.safe_url(), auth = ?cfg.rpc.auth, "starting");
    let index = match &cfg.index {
        Some(s) => Some(indexer::spawn(rpc.clone(), cfg.network, s.clone())?),
        None => {
            tracing::info!("address index disabled; /api/address falls back to scantxoutset");
            None
        }
    };
    let state = AppState {
        rpc,
        network: cfg.network,
        cache: Arc::new(cache::Cache::new(cfg.cache.clone())),
        started: Instant::now(),
        index,
    };
    let app = router::build(state, &cfg.http);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!("block-explorer ({}) listening on http://{}", cfg.network, cfg.bind);
    axum::serve(listener, app).await?;
    Ok(())
}
