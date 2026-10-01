use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::rpc::RpcError;

#[derive(Debug, Clone)]
pub enum AppError {
    NotFound(String),
    InvalidInput(String),
    /// Node is busy (scan in progress, warming up) or the provider rate-limited us
    Busy(String),
    RateLimited(String),
    /// The node/provider does not allow the RPC method this feature needs
    NotSupported(String),
    /// Node returned an error or is unreachable
    Upstream(String),
    /// Node (or the whole request) took too long
    Timeout(String),
}

impl From<RpcError> for AppError {
    fn from(e: RpcError) -> Self {
        match e {
            RpcError::Rpc { code, message } => match code {
                -5 => AppError::NotFound(message),
                -8 if message.contains("in progress") => AppError::Busy(message),
                -8 => AppError::InvalidInput(message),
                -28 => AppError::Busy(format!("node is starting up: {message}")),
                -32601 => AppError::NotSupported(message),
                _ => AppError::Upstream(message),
            },
            RpcError::Http { status: 429, .. } => {
                AppError::RateLimited("RPC provider rate limit reached, try again shortly".into())
            }
            RpcError::Http { status: 400, message } if message.contains("not permitted") => {
                AppError::NotSupported(format!("the RPC provider does not allow this: {message}"))
            }
            // Don't echo auth details back to API clients.
            RpcError::Http { status: 401 | 403, .. } => {
                AppError::Upstream("RPC provider rejected our credentials".into())
            }
            RpcError::Timeout => AppError::Timeout("node did not answer in time".into()),
            e => AppError::Upstream(e.to_string()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            AppError::NotFound(m) => (StatusCode::NOT_FOUND, "not_found", m),
            AppError::InvalidInput(m) => (StatusCode::BAD_REQUEST, "invalid_input", m),
            AppError::Busy(m) => (StatusCode::SERVICE_UNAVAILABLE, "node_busy", m),
            AppError::RateLimited(m) => (StatusCode::SERVICE_UNAVAILABLE, "rate_limited", m),
            AppError::NotSupported(m) => (StatusCode::NOT_IMPLEMENTED, "not_supported", m),
            AppError::Upstream(m) => (StatusCode::BAD_GATEWAY, "upstream_error", m),
            AppError::Timeout(m) => (StatusCode::GATEWAY_TIMEOUT, "upstream_timeout", m),
        };
        (status, Json(json!({ "error": { "code": code, "message": message } }))).into_response()
    }
}
