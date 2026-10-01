use std::{sync::Arc, time::Instant};

use axum::Json;
use bitcoin::Network;
use serde_json::Value;

use crate::{cache::Cache, error::AppError, rpc::Rpc};

/// Shared application state, cloned into every handler.
#[derive(Clone)]
pub struct AppState {
    pub rpc: Arc<Rpc>,
    pub network: Network,
    pub cache: Arc<Cache>,
    pub started: Instant,
}

pub type ApiResult = Result<Json<Value>, AppError>;
