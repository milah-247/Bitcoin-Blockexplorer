mod config;
mod error;
mod handlers;
mod router;
mod state;
mod util;

use std::sync::Arc;

use state::AppState;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = config::Config::from_env()?;
    let state = AppState {
        rpc: Arc::new(cfg.client),
        network: cfg.network,
    };
    let app = router::build(state);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    println!("block-explorer ({}) listening on http://{}", cfg.network, cfg.bind);
    axum::serve(listener, app).await?;
    Ok(())
}