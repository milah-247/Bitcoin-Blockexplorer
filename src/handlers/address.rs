use std::{sync::Arc, time::Duration};

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cache::Ttl;
use crate::error::AppError;
use crate::extract::Q;
use crate::index::Index;
use crate::state::{ApiResult, AppState};
use crate::util::{parse_address, sats};

/// scantxoutset walks the whole UTXO set: minutes on mainnet.
const SCAN_TIMEOUT: Duration = Duration::from_secs(600);
/// UTXOs listed inline in /api/address/:addr (the rest are counted).
const MAX_UTXOS: usize = 100;
pub const MAX_ADDR_TXS_PAGE: u64 = 100;
const MAX_ADDR_OFFSET: u64 = 100_000;

#[derive(Deserialize)]
pub struct Page {
    start: Option<u64>,
    limit: Option<u64>,
}

/// The index must have written at least one block before it can answer.
fn ready_index(st: &AppState) -> Result<Option<(Arc<Index>, u64, u64)>, AppError> {
    let Some(idx) = &st.index else { return Ok(None) };
    let s = idx.status();
    match s.indexed_height {
        Some(h) => Ok(Some((idx.clone(), s.start_height, h))),
        None => Err(AppError::Busy(format!(
            "the address index is still being built (starting at block {}); try again shortly",
            s.start_height
        ))),
    }
}

fn coverage_note(start: u64, indexed: u64) -> String {
    if start == 0 {
        format!("Covers the whole chain up to block {indexed}. Unconfirmed (mempool) transactions are not included.")
    } else {
        format!(
            "Partial history: only blocks {start}–{indexed} are indexed. Coins this address received before block {start} \
             are not counted, so the real balance may be higher. Unconfirmed (mempool) transactions are not included."
        )
    }
}

/// Balance and UTXOs. From the SQLite index when enabled, otherwise scantxoutset.
pub async fn address_detail(State(st): State<AppState>, Path(addr): Path<String>) -> ApiResult {
    let address = parse_address(&addr, st.network)?;
    let addr = address.to_string();

    if let Some((idx, start, indexed)) = ready_index(&st)? {
        let s = idx.address_summary(address.script_pubkey(), MAX_UTXOS).await?;
        return Ok(Json(json!({
            "address": addr,
            "balance_sat": s.balance_sat,
            "utxo_count": s.utxo_count,
            "unspents": s.unspents,
            "unspents_truncated": s.utxo_count as usize > MAX_UTXOS,
            "scanned_at_height": indexed,
            "tx_count": s.tx_count,
            "received_sat": s.received_sat,
            "sent_sat": s.sent_sat,
            "source": "index",
            "index": idx.coverage(),
            "note": coverage_note(start, indexed),
        })));
    }

    let res = st
        .cache
        .get_or(&format!("scan:{addr}"), async {
            let v: Value = st
                .rpc
                .call_timeout("scantxoutset", &[json!("start"), json!([format!("addr({addr})")])], SCAN_TIMEOUT)
                .await?;
            Ok((v, Ttl::Short))
        })
        .await?;

    let unspents: Vec<Value> = res["unspents"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .map(|u| {
            json!({
                "txid": u["txid"],
                "vout": u["vout"],
                "height": u["height"],
                "amount_sat": sats(&u["amount"]),
            })
        })
        .collect();

    Ok(Json(json!({
        "address": addr,
        "balance_sat": sats(&res["total_amount"]),
        "utxo_count": unspents.len(),
        "unspents": unspents,
        "scanned_at_height": res["height"],
        "source": "scantxoutset",
        "index": null,
        "note": "Confirmed UTXOs only. Transaction history requires the address index (INDEX_ENABLED=true).",
    })))
}

/// Paginated transaction history (newest first) from the address index.
pub async fn address_txs(State(st): State<AppState>, Path(addr): Path<String>, Q(p): Q<Page>) -> ApiResult {
    let address = parse_address(&addr, st.network)?;
    let Some((idx, start_h, indexed)) = ready_index(&st)? else {
        return Err(AppError::NotSupported(
            "address history needs the address index, which is disabled (INDEX_ENABLED=false)".into(),
        ));
    };
    let start = p.start.unwrap_or(0).min(MAX_ADDR_OFFSET);
    let limit = p.limit.unwrap_or(25).clamp(1, MAX_ADDR_TXS_PAGE);
    // Fetch one extra row to know whether another page exists.
    let mut txs = idx.address_txs(address.script_pubkey(), start, limit + 1).await?;
    let more = txs.len() as u64 > limit;
    txs.truncate(limit as usize);
    Ok(Json(json!({
        "address": address.to_string(),
        "start": start,
        "limit": limit,
        "next_start": if more { Some(start + limit) } else { None },
        "txs": txs,
        "index": idx.coverage(),
        "note": coverage_note(start_h, indexed),
    })))
}
