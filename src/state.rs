use std::sync::Arc;

use axum::Json;
use bitcoin::Network;
use bitcoincore_rpc::Client;
use serde_json::Value;

use crate::error::AppError;

/// Shared application state, cloned into every handler.
#[derive(Clone)]
pub struct AppState {
    pub rpc: Arc<Client>,
    pub network: Network,
}

pub type RpcResult<T> = Result<T, bitcoincore_rpc::Error>;
pub type ApiResult = Result<Json<Value>, AppError>;

/// Run blocking RPC code off the async runtime.
pub async fn rpc_call<T, F>(st: &AppState, f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&Client) -> RpcResult<T> + Send + 'static,
{
    let client = st.rpc.clone();
    tokio::task::spawn_blocking(move || f(&*client))
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?
        .map_err(AppError::from)
}