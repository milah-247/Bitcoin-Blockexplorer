use axum::{
    extract::State,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::error::AppError;
use crate::extract::Q;
use crate::state::{ApiResult, AppState};
use crate::util::{is_digits, is_hash, parse_address, MAX_INPUT_LEN};

#[derive(Deserialize)]
pub struct SearchQuery {
    q: Option<String>,
}

pub async fn search(State(st): State<AppState>, Q(sq): Q<SearchQuery>) -> ApiResult {
    let q = sq.q.unwrap_or_default().trim().to_string();
    if q.is_empty() {
        return Err(AppError::InvalidInput("missing query parameter `q`".into()));
    }
    if q.len() > MAX_INPUT_LEN {
        return Err(AppError::InvalidInput("query is too long".into()));
    }

    // 1. block height
    if is_digits(&q) && !is_hash(&q) {
        let h: u64 = q
            .parse()
            .map_err(|_| AppError::InvalidInput("height out of range".into()))?;
        let tip = st.tip().await?.height;
        if h > tip {
            return Err(AppError::NotFound(format!("no block at height {h} (tip is {tip})")));
        }
        return Ok(Json(json!({ "type": "block", "value": h, "path": format!("/api/block/{h}") })));
    }

    // 2. 64 hex chars: block hash first, then txid. Both lookups are cached, so the
    //    page the client opens next is served without another RPC call.
    if is_hash(&q) {
        let hash = q.to_ascii_lowercase();
        match st.block(&hash).await {
            Ok(_) => {
                return Ok(Json(json!({ "type": "block", "value": hash, "path": format!("/api/block/{hash}") })))
            }
            Err(AppError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        return match st.tx(&hash, None).await {
            Ok(_) => Ok(Json(json!({ "type": "tx", "value": hash, "path": format!("/api/tx/{hash}") }))),
            Err(AppError::NotFound(_)) => Err(AppError::NotFound(
                "no block or transaction with that hash. Without a transaction index, \
                 only blocks and mempool transactions can be found by hash"
                    .into(),
            )),
            Err(e) => Err(e),
        };
    }

    // 3. address
    match parse_address(&q, st.network) {
        Ok(a) => Ok(Json(json!({ "type": "address", "value": a.to_string(), "path": format!("/api/address/{a}") }))),
        Err(_) => Err(AppError::InvalidInput(
            "input is not a height, block hash, txid or address for this network".into(),
        )),
    }
}
