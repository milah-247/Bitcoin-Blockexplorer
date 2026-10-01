use std::time::Duration;

use axum::{
    extract::{Request, State},
    http::{HeaderValue, Method},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    trace::TraceLayer,
};

use crate::error::AppError;
use crate::handlers::{address, blocks, health, search, tx};
use crate::state::AppState;

pub struct HttpSettings {
    pub request_timeout: Duration,
    /// None: no CORS headers (same-origin only). Some(["*"]): any origin.
    pub cors_origins: Option<Vec<String>>,
}

/// All HTTP routes in one place.
pub fn build(state: AppState, http: &HttpSettings) -> Router {
    let api = Router::new()
        .route("/api/health", get(health::health))
        .route("/api/tip", get(blocks::tip))
        .route("/api/blocks", get(blocks::blocks))
        .route("/api/block/:id", get(blocks::block_detail))
        .route("/api/block/:id/txs", get(blocks::block_txs))
        .route("/api/tx/:txid", get(tx::tx_detail))
        .route("/api/address/:addr", get(address::address_detail))
        .route("/api/search", get(search::search))
        .fallback(|| async { AppError::NotFound("no such API endpoint".into()) })
        .layer(middleware::from_fn_with_state(http.request_timeout, timeout))
        .with_state(state);

    let mut app = Router::new().merge(api).layer(
        TraceLayer::new_for_http()
            // Path only: query strings never carry secrets here, but keep logs lean.
            .make_span_with(|req: &Request| {
                tracing::info_span!("http", method = %req.method(), path = %req.uri().path())
            })
            .on_request(())
            .on_response(|res: &Response, latency: Duration, _span: &tracing::Span| {
                tracing::info!(status = res.status().as_u16(), latency_ms = latency.as_millis() as u64, "response");
            }),
    );
    if let Some(cors) = cors_layer(http.cors_origins.as_deref()) {
        app = app.layer(cors);
    }
    app
}

fn cors_layer(origins: Option<&[String]>) -> Option<CorsLayer> {
    let origins = origins?;
    let allow = if origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(origins.iter().filter_map(|o| HeaderValue::from_str(o).ok()))
    };
    Some(CorsLayer::new().allow_origin(allow).allow_methods([Method::GET, Method::HEAD, Method::OPTIONS]))
}

/// Bound the total time of one API request (all its RPC calls included).
async fn timeout(State(limit): State<Duration>, req: Request, next: Next) -> Response {
    match tokio::time::timeout(limit, next.run(req)).await {
        Ok(r) => r,
        Err(_) => AppError::Timeout(format!("request took longer than {}s", limit.as_secs())).into_response(),
    }
}
