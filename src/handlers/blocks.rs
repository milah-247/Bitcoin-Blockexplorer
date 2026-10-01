use axum::{extract::{Path, State}, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::extract::Q;
use crate::state::{ApiResult, AppState};
use crate::util::{parse_block_id, BlockId};

pub const MAX_BLOCKS_PAGE: u64 = 50;
pub const MAX_TXS_PAGE: u64 = 100;
/// No mainnet block has anywhere near this many transactions.
const MAX_TX_OFFSET: u64 = 1_000_000;

#[derive(Deserialize)]
pub struct Page {
    start: Option<u64>,
    limit: Option<u64>,
}

pub async fn tip(State(st): State<AppState>) -> ApiResult {
    let t = st.tip().await?;
    Ok(Json(json!({ "height": t.height, "hash": t.hash, "chain": t.chain })))
}

/// Latest blocks, newest first. `start` = height to begin at (default: tip).
pub async fn blocks(State(st): State<AppState>, Q(p): Q<Page>) -> ApiResult {
    let limit = p.limit.unwrap_or(20).clamp(1, MAX_BLOCKS_PAGE);
    let tip = st.tip().await?;
    let start = p.start.unwrap_or(tip.height);
    if start > tip.height {
        return Err(AppError::InvalidInput("start is above the chain tip".into()));
    }
    let lo = (start + 1).saturating_sub(limit);
    let heights: Vec<u64> = (lo..=start).rev().collect();
    let hashes = st.hashes_at(tip.height, &heights).await?;
    let blocks = st.blocks(&hashes).await?;
    let items: Vec<Value> = heights
        .iter()
        .zip(&blocks)
        .map(|(h, b)| {
            json!({
                "height": h,
                "hash": b["hash"],
                "time": b["time"],
                "tx_count": b["tx_count"],
                "previous_hash": b["previous_hash"],
                "size": b["size"],
                "weight": b["weight"],
            })
        })
        .collect();
    let next_start = if lo > 0 { Some(lo - 1) } else { None };
    Ok(Json(json!({ "tip": tip.height, "blocks": items, "next_start": next_start })))
}

async fn resolve(st: &AppState, id: &str) -> Result<String, AppError> {
    match parse_block_id(id)? {
        BlockId::Hash(h) => Ok(h),
        BlockId::Height(h) => st.hash_at(h).await?.ok_or_else(|| AppError::NotFound("block not found".into())),
    }
}

pub async fn block_detail(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let hash = resolve(&st, &id).await?;
    let b = st.block(&hash).await?;
    let tip = st.tip().await?;
    let height = b["height"].as_u64().unwrap_or(0);
    let confirmations = st.confirmations(&tip, height, &hash).await?;
    let next_hash = if confirmations > 0 && height < tip.height {
        st.hashes_at(tip.height, &[height + 1]).await?.pop()
    } else {
        None
    };
    let fees = st.block_fees(&b).await;
    Ok(Json(json!({
        "hash": b["hash"],
        "height": b["height"],
        "time": b["time"],
        "median_time": b["median_time"],
        "size": b["size"],
        "stripped_size": b["stripped_size"],
        "weight": b["weight"],
        "tx_count": b["tx_count"],
        "total_fees_sat": fees,
        "total_fees_source": "coinbase_minus_subsidy",
        "subsidy_sat": crate::util::block_subsidy(height, st.network),
        "confirmations": confirmations,
        "merkle_root": b["merkle_root"],
        "difficulty": b["difficulty"],
        "version": b["version"],
        "bits": b["bits"],
        "nonce": b["nonce"],
        "previous_hash": b["previous_hash"],
        "next_hash": next_hash,
    })))
}

/// Paginated txids of a block.
pub async fn block_txs(State(st): State<AppState>, Path(id): Path<String>, Q(p): Q<Page>) -> ApiResult {
    let hash = resolve(&st, &id).await?;
    let start = p.start.unwrap_or(0).min(MAX_TX_OFFSET) as usize;
    let limit = p.limit.unwrap_or(25).clamp(1, MAX_TXS_PAGE) as usize;
    let b = st.block(&hash).await?;
    let all = b["txids"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let total = all.len();
    let page: Vec<Value> = all.iter().skip(start).take(limit).cloned().collect();
    let next_start = if start + limit < total { Some(start + limit) } else { None };
    Ok(Json(json!({
        "block_hash": b["hash"],
        "block_height": b["height"],
        "total": total,
        "start": start,
        "limit": limit,
        "next_start": next_start,
        "txids": page,
    })))
}
