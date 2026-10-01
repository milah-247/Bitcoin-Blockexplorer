use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::extract::Q;
use crate::state::{ApiResult, AppState};
use crate::util::{parse_block_id, parse_hash, sats, BlockId};

#[derive(Deserialize)]
pub struct TxQuery {
    /// Height or hash of the block containing the tx (needed without a txindex).
    block: Option<String>,
}

/// Turn a node transaction (verbosity 2, `vin[].prevout` filled where known)
/// into the API response shape.
pub fn tx_json(tx: &Value) -> Value {
    let (mut in_sum, mut out_sum) = (0u64, 0u64);
    let (mut coinbase, mut complete) = (false, true);

    let inputs: Vec<Value> = tx["vin"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .map(|i| {
            if i.get("coinbase").is_some() {
                coinbase = true;
                return json!({ "coinbase": true });
            }
            let pv = &i["prevout"];
            let val = pv.get("value").map(sats);
            match val {
                Some(v) => in_sum += v,
                None => complete = false,
            }
            json!({
                "txid": i["txid"],
                "vout": i["vout"],
                "address": pv["scriptPubKey"]["address"],
                "script_type": pv["scriptPubKey"]["type"],
                "value_sat": val,
            })
        })
        .collect();

    let outputs: Vec<Value> = tx["vout"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .map(|o| {
            let v = sats(&o["value"]);
            out_sum += v;
            json!({
                "n": o["n"],
                "address": o["scriptPubKey"]["address"],
                "script_type": o["scriptPubKey"]["type"],
                "value_sat": v,
            })
        })
        .collect();

    let fee = if coinbase {
        None
    } else if complete {
        in_sum.checked_sub(out_sum)
    } else {
        // Node-reported fee (verbosity 2, or the mempool entry) when some prevouts are unknown.
        tx.get("fee").filter(|f| f.is_number()).map(sats)
    };
    let vsize = tx["vsize"].as_u64();
    let fee_rate = match (fee, vsize) {
        (Some(f), Some(v)) if v > 0 => Some((f as f64 / v as f64 * 100.0).round() / 100.0),
        _ => None,
    };
    let confirmed = tx["blockhash"].is_string();

    json!({
        "txid": tx["txid"],
        "status": {
            "confirmed": confirmed,
            "confirmations": tx["confirmations"].as_u64().unwrap_or(0),
            "block_hash": tx["blockhash"],
            "block_height": tx["height"],
            "block_time": tx["time"],
        },
        "size": tx["size"],
        "vsize": tx["vsize"],
        "weight": tx["weight"],
        "version": tx["version"],
        "locktime": tx["locktime"],
        "is_coinbase": coinbase,
        "fee_sat": fee,
        "fee_rate_sat_vb": fee_rate,
        "inputs": inputs,
        "outputs": outputs,
    })
}

pub async fn tx_detail(State(st): State<AppState>, Path(txid): Path<String>, Q(q): Q<TxQuery>) -> ApiResult {
    let txid = parse_hash(&txid, "txid")?;
    let hint = match q.block.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(b) => Some(match parse_block_id(b)? {
            BlockId::Hash(h) => h,
            BlockId::Height(h) => st
                .hash_at(h)
                .await?
                .ok_or_else(|| AppError::NotFound("block not found".into()))?,
        }),
    };
    let cached = st.tx(&txid, hint).await?;
    let mut tx = (*cached).clone();

    // Confirmations change as blocks arrive; compute them fresh.
    if let (Some(h), Some(bh)) = (tx["height"].as_u64(), tx["blockhash"].as_str().map(String::from)) {
        let tip = st.tip().await?;
        tx["confirmations"] = json!(st.confirmations(&tip, h, &bh).await?.max(0));
    }
    Ok(Json(tx_json(&tx)))
}
