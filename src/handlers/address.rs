use std::time::Duration;

use axum::{
    extract::{Path, State},
    Json,
};
use serde_json::{json, Value};

use crate::cache::Ttl;
use crate::state::{ApiResult, AppState};
use crate::util::{parse_address, sats};

/// scantxoutset walks the whole UTXO set: minutes on mainnet.
const SCAN_TIMEOUT: Duration = Duration::from_secs(600);

/// Balance + UTXOs via scantxoutset (confirmed UTXOs only; slow on mainnet).
/// Core runs one scan at a time; a concurrent request gets 503 `node_busy`.
pub async fn address_detail(State(st): State<AppState>, Path(addr): Path<String>) -> ApiResult {
    let addr = parse_address(&addr, st.network)?.to_string();
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
        "note": "Confirmed UTXOs only. Transaction history requires a custom address index.",
    })))
}
