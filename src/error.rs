use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use bitcoincore_rpc::{jsonrpc, Error as RpcError};
use serde_json::json;

#[derive(Debug)]
pub enum AppError {
    NotFound(String),
    InvalidInput(String),
    /// Node is busy (e.g. another scantxoutset is running)
    Busy(String),
    /// Node returned an error or is unreachable
    Upstream(String),
    Internal(String),
}

/// Extract the JSON-RPC error code, if the error came from the node.
pub fn rpc_code(e: &RpcError) -> Option<i32> {
    match e {
        RpcError::JsonRpc(jsonrpc::Error::Rpc(r)) => Some(r.code),
        _ => None,
    }
}

impl From<RpcError> for AppError {
    fn from(e: RpcError) -> Self {
        if let RpcError::JsonRpc(jsonrpc::Error::Rpc(r)) = &e {
            let msg = r.message.clone();
            return match r.code {
                -5 => AppError::NotFound(msg),
                -8 if msg.contains("in progress") => AppError::Busy(msg),
                -8 => AppError::InvalidInput(msg),
                _ => AppError::Upstream(msg),
            };
        }
        AppError::Upstream(e.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            AppError::NotFound(m) => (StatusCode::NOT_FOUND, "not_found", m),
            AppError::InvalidInput(m) => (StatusCode::BAD_REQUEST, "invalid_input", m),
            AppError::Busy(m) => (StatusCode::SERVICE_UNAVAILABLE, "node_busy", m),
            AppError::Upstream(m) => (StatusCode::BAD_GATEWAY, "upstream_error", m),
            AppError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error", m),
        };
        (status, Json(json!({ "error": { "code": code, "message": message } }))).into_response()
    }
}