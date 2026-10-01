use axum::{routing::get, Router};

use crate::handlers::{address, blocks, search, tx};
use crate::state::AppState;

/// All HTTP routes in one place.
pub fn build(state: AppState) -> Router {
    Router::new()
        .route("/api/tip", get(blocks::tip))
        .route("/api/blocks", get(blocks::blocks))
        .route("/api/block/:id", get(blocks::block_detail))
        .route("/api/block/:id/txs", get(blocks::block_txs))
        .route("/api/tx/:txid", get(tx::tx_detail))
        .route("/api/address/:addr", get(address::address_detail))
        .route("/api/search", get(search::search))
        .with_state(state)
}