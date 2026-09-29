mod error;

use std::{env, str::FromStr, sync::Arc, time::Duration};

use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use bitcoin::{Address, Network};
use bitcoincore_rpc::{
    jsonrpc::{self, simple_http::SimpleHttpTransport},
    Client, RpcApi,
};
use error::{rpc_code, AppError};
use serde::Deserialize;
use serde_json::{json, Value};

const MAX_BLOCKS_PAGE: u64 = 50;
const MAX_TXS_PAGE: u64 = 100;

#[derive(Clone)]
struct AppState {
    rpc: Arc<Client>,
    network: Network,
}

type RpcResult<T> = Result<T, bitcoincore_rpc::Error>;
type ApiResult = Result<Json<Value>, AppError>;

// ---------------------------------------------------------------- helpers

/// Run blocking RPC code off the async runtime.
async fn rpc_call<T, F>(st: &AppState, f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&Client) -> RpcResult<T> + Send + 'static,
{
    let client = st.rpc.clone();
    tokio::task::spawn_blocking(move || f(&*client))
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?
        .map_err(AppError::from)
}

fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// BTC (float from JSON) -> satoshis
fn sats(v: &Value) -> u64 {
    (v.as_f64().unwrap_or(0.0) * 1e8).round() as u64
}

enum BlockId {
    Height(u64),
    Hash(String),
}

fn parse_block_id(s: &str) -> Result<BlockId, AppError> {
    if is_digits(s) && !is_hash(s) {
        s.parse::<u64>()
            .map(BlockId::Height)
            .map_err(|_| AppError::InvalidInput("height out of range".into()))
    } else if is_hash(s) {
        Ok(BlockId::Hash(s.to_ascii_lowercase()))
    } else {
        Err(AppError::InvalidInput(
            "expected a block height or a 64-character hex block hash".into(),
        ))
    }
}

/// Height -> hash (None if above tip); hash passes through.
fn resolve_block(c: &Client, id: &BlockId) -> RpcResult<Option<String>> {
    match id {
        BlockId::Hash(h) => Ok(Some(h.clone())),
        BlockId::Height(h) => {
            let tip: u64 = c.call("getblockcount", &[])?;
            if *h > tip {
                return Ok(None);
            }
            Ok(Some(c.call("getblockhash", &[json!(h)])?))
        }
    }
}

// ---------------------------------------------------------------- tip / blocks

async fn tip(State(st): State<AppState>) -> ApiResult {
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

#[derive(Deserialize)]
struct Page {
    start: Option<u64>,
    limit: Option<u64>,
}

/// Latest blocks, newest first. `start` = height to begin at (default: tip).
async fn blocks(State(st): State<AppState>, Query(p): Query<Page>) -> ApiResult {
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

async fn block_detail(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
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
async fn block_txs(
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

// ---------------------------------------------------------------- transactions

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

fn tx_json(tx: &Value) -> Value {
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

async fn tx_detail(State(st): State<AppState>, Path(txid): Path<String>) -> ApiResult {
    if !is_hash(&txid) {
        return Err(AppError::InvalidInput("txid must be 64 hex characters".into()));
    }
    let txid = txid.to_ascii_lowercase();
    let tx = rpc_call(&st, move |c| fetch_tx(c, &txid)).await?;
    Ok(Json(tx_json(&tx)))
}

// ---------------------------------------------------------------- address

fn parse_address(s: &str, net: Network) -> Result<Address, AppError> {
    Address::from_str(s)
        .map_err(|_| AppError::InvalidInput("not a valid Bitcoin address".into()))?
        .require_network(net)
        .map_err(|_| AppError::InvalidInput(format!("address is not valid for network {net}")))
}

/// Balance + UTXOs via scantxoutset (confirmed UTXOs only; slow on mainnet).
async fn address_detail(State(st): State<AppState>, Path(addr): Path<String>) -> ApiResult {
    let addr = parse_address(&addr, st.network)?.to_string();
    let a = addr.clone();
    let res = rpc_call(&st, move |c| {
        c.call::<Value>(
            "scantxoutset",
            &[json!("start"), json!([format!("addr({a})")])],
        )
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

// ---------------------------------------------------------------- search

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

async fn search(State(st): State<AppState>, Query(sq): Query<SearchQuery>) -> ApiResult {
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

// ---------------------------------------------------------------- main

fn build_client() -> Result<(Client, Network), Box<dyn std::error::Error>> {
    let (network, default_port) = match env::var("NETWORK").unwrap_or_else(|_| "regtest".into()).as_str() {
        "mainnet" | "bitcoin" => (Network::Bitcoin, 8332),
        "testnet" => (Network::Testnet, 18332),
        "signet" => (Network::Signet, 38332),
        "regtest" => (Network::Regtest, 18443),
        other => return Err(format!("unknown NETWORK `{other}`").into()),
    };
    let url = env::var("RPC_URL").unwrap_or_else(|_| format!("http://127.0.0.1:{default_port}"));

    let (user, pass) = if let Ok(path) = env::var("RPC_COOKIE") {
        let s = std::fs::read_to_string(path)?;
        let (u, p) = s.trim().split_once(':').ok_or("bad cookie file")?;
        (u.to_string(), p.to_string())
    } else {
        (
            env::var("RPC_USER").map_err(|_| "set RPC_USER/RPC_PASS or RPC_COOKIE")?,
            env::var("RPC_PASS").map_err(|_| "set RPC_USER/RPC_PASS or RPC_COOKIE")?,
        )
    };

    // Long timeout: scantxoutset can take minutes on mainnet.
    let transport = SimpleHttpTransport::builder()
        .url(&url)?
        .auth(user, Some(pass))
        .timeout(Duration::from_secs(600))
        .build();
    let client = Client::from_jsonrpc(jsonrpc::Client::with_transport(transport));
    Ok((client, network))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (client, network) = build_client()?;
    let state = AppState { rpc: Arc::new(client), network };

    let app = Router::new()
        .route("/api/tip", get(tip))
        .route("/api/blocks", get(blocks))
        .route("/api/block/:id", get(block_detail))
        .route("/api/block/:id/txs", get(block_txs))
        .route("/api/tx/:txid", get(tx_detail))
        .route("/api/address/:addr", get(address_detail))
        .route("/api/search", get(search))
        .with_state(state);

    let bind = env::var("BIND").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("block-explorer ({network}) listening on http://{bind}");
    axum::serve(listener, app).await?;
    Ok(())
}