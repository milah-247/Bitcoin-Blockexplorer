mod check;
mod config;
mod error;
mod handlers;
mod router;
mod rpc;
mod state;
mod util;

use std::sync::Arc;

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

    let state = AppState {
        rpc: Arc::new(cfg.legacy_client()?),
        network: cfg.network,
    };
    let app = router::build(state);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    println!("block-explorer ({}) listening on http://{}", cfg.network, cfg.bind);
    axum::serve(listener, app).await?;
    Ok(())
}
