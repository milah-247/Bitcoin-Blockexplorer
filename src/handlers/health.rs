use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde_json::json;

use crate::state::AppState;

/// Liveness + upstream status. Uses the cached tip (a few seconds old at most),
/// so frequent health probes do not eat into the provider's rate limit.
pub async fn health(State(st): State<AppState>) -> impl IntoResponse {
    let (entries, bytes) = st.cache.stats();
    let (rpc_ok, rpc) = match st.tip().await {
        Ok(t) => (true, json!({ "ok": true, "chain": t.chain, "tip": t.height })),
        Err(e) => (false, json!({ "ok": false, "error": format!("{e:?}") })),
    };
    let body = json!({
        "status": if rpc_ok { "ok" } else { "degraded" },
        "version": env!("CARGO_PKG_VERSION"),
        "network": st.network.to_string(),
        "uptime_secs": st.started.elapsed().as_secs(),
        "rpc": rpc,
        "rpc_rate_limit": st.rpc.rate_limit(),
        "cache": { "entries": entries, "approx_bytes": bytes },
        "index": st.index.as_ref().map(|i| i.status()),
    });
    let code = if rpc_ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (code, Json(body))
}
