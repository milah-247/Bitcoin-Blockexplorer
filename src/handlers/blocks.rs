use axum::{
    extract::{Path, Query, State},
    Json,
};
use bitcoincore_rpc::RpcApi;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::state::{rpc_call, ApiResult, AppState};
use crate::util::{parse_block_id, resolve_block};

const MAX_BLOCKS_PAGE: u64 = 50;
const MAX_TXS_PAGE: u64 = 100;

#[derive(Deserialize)]
pub struct Page {
    start: Option<u64>,
    limit: Option<u64>,
}

pub async fn tip(State(st): State<AppState>) -> ApiResult {
    let v = rpc_call(&st, |c| {
        let info: Value = c.call("getblockchaininfo", &[])?;
        Ok(json!({
            "height": info["blocks"],
            "hash": info["bestblockhash"],
            "chain": info["chain"],
        }))
    })
    .await?;
    Ok(Json(v))
}

/// Latest blocks, newest first. `start` = height to begin at (default: tip).
pub async fn blocks(State(st): State<AppState>, Query(p): Query<Page>) -> ApiResult {
    let limit = p.limit.unwrap_or(20).clamp(1, MAX_BLOCKS_PAGE);
    let start = p.start;
    let out = rpc_call(&st, move |c| {
        let info: Value = c.call("getblockchaininfo", &[])?;
        let tip = info["blocks"].as_u64().unwrap_or(0);
        let start = start.unwrap_or(tip);
        if start > tip {
            return Ok(None);
        }
        let lo = (start + 1).saturating_sub(limit);
        let mut items = Vec::new();
        for h in (lo..=start).rev() {
            let hash: String = c.call("getblockhash", &[json!(h)])?;
            let hd: Value = c.call("getblockheader", &[json!(hash)])?;
            items.push(json!({
                "height": h,
                "hash": hash,
                "time": hd["time"],
                "tx_count": hd["nTx"],
                "previous_hash": hd["previousblockhash"],
            }));
        }
        let next_start = if lo > 0 { Some(lo - 1) } else { None };
        Ok(Some(json!({ "tip": tip, "blocks": items, "next_start": next_start })))
    })
    .await?;
    out.map(Json)
        .ok_or_else(|| AppError::InvalidInput("start is above the chain tip".into()))
}

pub async fn block_detail(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let id = parse_block_id(&id)?;
    let out = rpc_call(&st, move |c| {
        let Some(hash) = resolve_block(c, &id)? else {
            return Ok(None);
        };
        let b: Value = c.call("getblock", &[json!(hash), json!(1)])?;
        let stats: Value = c.call("getblockstats", &[json!(hash), json!(["totalfee"])])?;
        Ok(Some(json!({
            "hash": b["hash"],
            "height": b["height"],
            "time": b["time"],
            "size": b["size"],
            "weight": b["weight"],
            "tx_count": b["nTx"],
            "total_fees_sat": stats["totalfee"],
            "confirmations": b["confirmations"],
            "merkle_root": b["merkleroot"],
            "difficulty": b["difficulty"],
            "previous_hash": b["previousblockhash"],
            "next_hash": b["nextblockhash"],
        })))
    })
    .await?;
    out.map(Json)
        .ok_or_else(|| AppError::NotFound("block not found".into()))
}

/// Paginated txids of a block.
pub async fn block_txs(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(p): Query<Page>,
) -> ApiResult {
    let id = parse_block_id(&id)?;
    let start = p.start.unwrap_or(0) as usize;
    let limit = p.limit.unwrap_or(25).clamp(1, MAX_TXS_PAGE) as usize;
    let out = rpc_call(&st, move |c| {
        let Some(hash) = resolve_block(c, &id)? else {
            return Ok(None);
        };
        let b: Value = c.call("getblock", &[json!(hash), json!(1)])?;
        let all = b["tx"].as_array().cloned().unwrap_or_default();
        let total = all.len();
        let page: Vec<Value> = all.into_iter().skip(start).take(limit).collect();
        let next_start = if start + limit < total { Some(start + limit) } else { None };
        Ok(Some(json!({
            "block_hash": b["hash"],
            "total": total,
            "start": start,
            "limit": limit,
            "next_start": next_start,
            "txids": page,
        })))
    })
    .await?;
    out.map(Json)
        .ok_or_else(|| AppError::NotFound("block not found".into()))
}
