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

#[cfg(test)]
mod tests {
    use super::*;

    fn spk(addr: &str) -> Value {
        json!({ "address": addr, "type": "witness_v0_keyhash" })
    }

    #[test]
    fn confirmed_tx_fee_and_rate() {
        let tx = json!({
            "txid": "t", "blockhash": "b", "height": 10, "time": 1700000000, "confirmations": 3,
            "size": 222, "vsize": 141, "weight": 561, "version": 2, "locktime": 0,
            "vin": [{ "txid": "p", "vout": 1, "prevout": { "value": 0.024942, "scriptPubKey": spk("in1") } }],
            "vout": [
                { "n": 0, "value": 0.012, "scriptPubKey": spk("out0") },
                { "n": 1, "value": 0.012871, "scriptPubKey": spk("out1") }
            ]
        });
        let j = tx_json(&tx);
        assert_eq!(j["fee_sat"], 7100);
        assert_eq!(j["fee_rate_sat_vb"], 50.35);
        assert_eq!(j["status"]["confirmed"], true);
        assert_eq!(j["status"]["confirmations"], 3);
        assert_eq!(j["status"]["block_height"], 10);
        assert_eq!(j["inputs"][0]["address"], "in1");
        assert_eq!(j["inputs"][0]["value_sat"], 2_494_200);
        assert_eq!(j["outputs"][1]["value_sat"], 1_287_100);
        assert_eq!(j["is_coinbase"], false);
    }

    #[test]
    fn coinbase_has_no_fee() {
        let tx = json!({
            "txid": "c", "vsize": 100,
            "vin": [{ "coinbase": "03aabbcc" }],
            "vout": [{ "n": 0, "value": 3.125, "scriptPubKey": spk("miner") }]
        });
        let j = tx_json(&tx);
        assert_eq!(j["is_coinbase"], true);
        assert!(j["fee_sat"].is_null());
        assert!(j["fee_rate_sat_vb"].is_null());
        assert_eq!(j["inputs"][0]["coinbase"], true);
        assert_eq!(j["status"]["confirmed"], false);
    }

    #[test]
    fn unknown_prevout_falls_back_to_node_fee() {
        let tx = json!({
            "txid": "m", "vsize": 200, "fee": 0.00002,
            "vin": [{ "txid": "p", "vout": 0 }],
            "vout": [{ "n": 0, "value": 1.0, "scriptPubKey": spk("x") }]
        });
        let j = tx_json(&tx);
        assert!(j["inputs"][0]["value_sat"].is_null());
        assert_eq!(j["fee_sat"], 2000);
        assert_eq!(j["fee_rate_sat_vb"], 10.0);
    }

    #[test]
    fn unknown_prevout_without_node_fee_is_null() {
        let tx = json!({
            "txid": "m", "vsize": 200,
            "vin": [{ "txid": "p", "vout": 0 }],
            "vout": [{ "n": 0, "value": 1.0, "scriptPubKey": { "type": "nulldata" } }]
        });
        let j = tx_json(&tx);
        assert!(j["fee_sat"].is_null());
        assert!(j["outputs"][0]["address"].is_null());
        assert_eq!(j["outputs"][0]["script_type"], "nulldata");
    }
}
