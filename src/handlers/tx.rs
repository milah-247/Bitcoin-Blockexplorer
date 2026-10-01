use axum::{
    extract::{Path, State},
    Json,
};
use bitcoincore_rpc::{Client, RpcApi};
use serde_json::{json, Value};

use crate::error::AppError;
use crate::state::{rpc_call, ApiResult, AppState, RpcResult};
use crate::util::{is_hash, sats};

/// Fetch a tx as JSON with `vin[].prevout` filled in.
/// Confirmed: pull it out of `getblock` verbosity 3 (Core 25+), which includes prevouts.
/// Unconfirmed: fetch each parent tx to get the spent outputs.
fn fetch_tx(c: &Client, txid: &str) -> RpcResult<Value> {
    let raw: Value = c.call("getrawtransaction", &[json!(txid), json!(true)])?;

    if let Some(bh) = raw["blockhash"].as_str() {
        let block: Value = c.call("getblock", &[json!(bh), json!(3)])?;
        let found = block["tx"]
            .as_array()
            .and_then(|a| a.iter().find(|t| t["txid"].as_str() == Some(txid)));
        if let Some(t) = found {
            let mut tx = t.clone();
            tx["blockhash"] = json!(bh);
            tx["height"] = block["height"].clone();
            tx["confirmations"] = raw["confirmations"].clone();
            tx["time"] = raw["blocktime"].clone();
            return Ok(tx);
        }
    }

    let mut tx = raw;
    if let Some(vin) = tx["vin"].as_array_mut() {
        for i in vin {
            let (Some(ptxid), Some(n)) = (i["txid"].as_str().map(String::from), i["vout"].as_u64())
            else {
                continue; // coinbase
            };
            let parent: Value = c.call("getrawtransaction", &[json!(ptxid), json!(true)])?;
            let o = &parent["vout"][n as usize];
            i["prevout"] = json!({ "value": o["value"], "scriptPubKey": o["scriptPubKey"] });
        }
    }
    Ok(tx)
}

/// Turn a raw node transaction into the API response shape.
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

    let fee = if coinbase || !complete { None } else { in_sum.checked_sub(out_sum) };
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
        "is_coinbase": coinbase,
        "fee_sat": fee,
        "inputs": inputs,
        "outputs": outputs,
    })
}

pub async fn tx_detail(State(st): State<AppState>, Path(txid): Path<String>) -> ApiResult {
    if !is_hash(&txid) {
        return Err(AppError::InvalidInput("txid must be 64 hex characters".into()));
    }
    let txid = txid.to_ascii_lowercase();
    let tx = rpc_call(&st, move |c| fetch_tx(c, &txid)).await?;
    Ok(Json(tx_json(&tx)))
}
