use axum::{
    extract::{Query, State},
    Json,
};
use bitcoincore_rpc::RpcApi;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{rpc_code, AppError};
use crate::state::{rpc_call, ApiResult, AppState};
use crate::util::{is_digits, is_hash, parse_address};

#[derive(Deserialize)]
pub struct SearchQuery {
    q: Option<String>,
}

pub async fn search(State(st): State<AppState>, Query(sq): Query<SearchQuery>) -> ApiResult {
    let q = sq.q.unwrap_or_default().trim().to_string();
    if q.is_empty() {
        return Err(AppError::InvalidInput("missing query parameter `q`".into()));
    }

    // 1. block height
    if is_digits(&q) && !is_hash(&q) {
        let h: u64 = q
            .parse()
            .map_err(|_| AppError::InvalidInput("height out of range".into()))?;
        let tip: u64 = rpc_call(&st, |c| c.call("getblockcount", &[])).await?;
        if h > tip {
            return Err(AppError::NotFound(format!("no block at height {h} (tip is {tip})")));
        }
        return Ok(Json(json!({ "type": "block", "value": h, "path": format!("/api/block/{h}") })));
    }

    // 2. 64 hex chars: block hash first (cheap), then txid
    if is_hash(&q) {
        let hash = q.to_ascii_lowercase();
        let h = hash.clone();
        let kind = rpc_call(&st, move |c| {
            match c.call::<Value>("getblockheader", &[json!(h)]) {
                Ok(_) => return Ok(Some("block")),
                Err(e) if rpc_code(&e) == Some(-5) => {}
                Err(e) => return Err(e),
            }
            match c.call::<Value>("getrawtransaction", &[json!(h), json!(false)]) {
                Ok(_) => Ok(Some("tx")),
                Err(e) if rpc_code(&e) == Some(-5) => Ok(None),
                Err(e) => Err(e),
            }
        })
        .await?;
        return match kind {
            Some(k) => Ok(Json(json!({ "type": k, "value": hash, "path": format!("/api/{k}/{hash}") }))),
            None => Err(AppError::NotFound("no block or transaction with that hash".into())),
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
