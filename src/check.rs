//! `block-explorer --check`: probe what the configured RPC endpoint supports.
//!
//! Uses the same client (auth, timeouts, rate limiting) as the server, so a green
//! table means the server will work too. `--check-scan` additionally runs a real
//! `scantxoutset` (minutes on mainnet, heavy for a shared node, hence opt-in).

use std::time::{Duration, Instant};

use bitcoin::Network;
use serde_json::{json, Value};

use crate::rpc::{Rpc, RpcError};

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Yes,
    No,
    Warn,
    Info,
}

struct Row {
    name: &'static str,
    status: Status,
    detail: String,
}

struct Report {
    rows: Vec<Row>,
}

impl Report {
    fn add(&mut self, name: &'static str, status: Status, detail: impl Into<String>) {
        let detail = detail.into();
        let tag = match status {
            Status::Yes => "yes",
            Status::No => "NO",
            Status::Warn => "warn",
            Status::Info => "info",
        };
        // Print as we go so slow probes show progress.
        eprintln!("  [{tag:>4}] {name}");
        self.rows.push(Row { name, status, detail });
    }

    fn print(&self) {
        let w = self.rows.iter().map(|r| r.name.len()).max().unwrap_or(10);
        println!();
        println!("{:<w$} | {:<4} | detail", "capability", "ok?");
        println!("{}-+------+-{}", "-".repeat(w), "-".repeat(60));
        for r in &self.rows {
            let tag = match r.status {
                Status::Yes => "yes",
                Status::No => "NO",
                Status::Warn => "warn",
                Status::Info => "",
            };
            println!("{:<w$} | {:<4} | {}", r.name, tag, r.detail);
        }
        println!();
    }
}

fn kb(n: usize) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MiB", n as f64 / (1 << 20) as f64)
    } else {
        format!("{:.1} KiB", n as f64 / 1024.0)
    }
}

fn ms(t: Instant) -> String {
    format!("{} ms", t.elapsed().as_millis())
}

fn err_detail(e: &RpcError) -> String {
    let s = e.to_string();
    if s.len() > 120 {
        format!("{}…", &s[..120])
    } else {
        s
    }
}

/// Returns true when everything the explorer *needs* works.
pub async fn run(rpc: &Rpc, network: Network, real_scan: bool) -> bool {
    eprintln!("Probing {} (network {network}) ...", rpc.safe_url());
    let mut r = Report { rows: Vec::new() };
    let mut essential_ok = true;
    let heavy = Duration::from_secs(180);

    // 1. chain + version ------------------------------------------------------
    let info: Value = match rpc.call("getblockchaininfo", &[]).await {
        Ok(v) => v,
        Err(e) => {
            r.add("connect + auth", Status::No, err_detail(&e));
            r.print();
            return false;
        }
    };
    r.add("connect + auth", Status::Yes, rpc.safe_url());
    let chain = info["chain"].as_str().unwrap_or("?").to_string();
    let want = network.to_core_arg();
    let chain_ok = chain == want;
    essential_ok &= chain_ok;
    let tip = info["blocks"].as_u64().unwrap_or(0);
    r.add(
        "chain",
        if chain_ok { Status::Yes } else { Status::No },
        format!("\"{chain}\" (expected \"{want}\"), tip {tip}, headers {}", info["headers"]),
    );
    if info["pruned"].as_bool() == Some(true) {
        r.add("pruned", Status::Warn, format!("pruned node, blocks below {} unavailable", info["pruneheight"]));
    } else {
        r.add("pruned", Status::Yes, "not pruned: full block history available");
    }

    match rpc.call::<Value>("getnetworkinfo", &[]).await {
        Ok(n) => {
            let v = n["version"].as_u64().unwrap_or(0);
            r.add(
                "core version",
                if v >= 230000 { Status::Yes } else { Status::Warn },
                format!("{} ({v})", n["subversion"].as_str().unwrap_or("?")),
            );
        }
        Err(e) => r.add("core version", Status::Warn, format!("getnetworkinfo: {}", err_detail(&e))),
    }

    match rpc.call::<Value>("getindexinfo", &[]).await {
        Ok(v) => {
            let names: Vec<String> = v
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, s)| format!("{k}(synced={}, height={})", s["synced"], s["best_block_height"]))
                        .collect()
                })
                .unwrap_or_default();
            r.add(
                "getindexinfo",
                Status::Info,
                if names.is_empty() { "no optional indexes enabled".into() } else { names.join(", ") },
            );
        }
        Err(e) => r.add("getindexinfo", Status::Info, format!("not available: {}", err_detail(&e))),
    }

    // 2. getblock verbosities on a block 6 deep ---------------------------------
    let deep = tip.saturating_sub(6);
    let deep_hash: String = match rpc.call("getblockhash", &[json!(deep)]).await {
        Ok(h) => h,
        Err(e) => {
            r.add("getblockhash", Status::No, err_detail(&e));
            r.print();
            return false;
        }
    };
    let t = Instant::now();
    match rpc.call::<Value>("getblock", &[json!(deep_hash), json!(1)]).await {
        Ok(b) => r.add(
            "getblock verbosity 1",
            Status::Yes,
            format!(
                "height {deep}: {} txs, {} bytes on-chain, response {} in {}",
                b["nTx"],
                b["size"],
                kb(serde_json::to_vec(&b).map(|v| v.len()).unwrap_or(0)),
                ms(t)
            ),
        ),
        Err(e) => {
            essential_ok = false;
            r.add("getblock verbosity 1", Status::No, err_detail(&e));
        }
    }
    let t = Instant::now();
    match rpc.call_timeout::<Value>("getblock", &[json!(deep_hash), json!(3)], heavy).await {
        Ok(b) => {
            let has_prevout = b["tx"][1]["vin"][0].get("prevout").is_some() || b["tx"].as_array().map(|a| a.len()) == Some(1);
            r.add(
                "getblock verbosity 3",
                if has_prevout { Status::Yes } else { Status::Warn },
                format!(
                    "response {} in {}{}",
                    kb(serde_json::to_vec(&b).map(|v| v.len()).unwrap_or(0)),
                    ms(t),
                    if has_prevout { ", includes prevouts" } else { ", but NO prevouts (Core < 23?)" }
                ),
            );
        }
        Err(e) => r.add("getblock verbosity 3", Status::No, format!("{} after {}", err_detail(&e), ms(t))),
    }
    let t = Instant::now();
    match rpc.call::<String>("getblock", &[json!(deep_hash), json!(0)]).await {
        Ok(hex) => r.add("getblock verbosity 0", Status::Yes, format!("raw hex {} in {}", kb(hex.len()), ms(t))),
        Err(e) => r.add("getblock verbosity 0", Status::No, err_detail(&e)),
    }

    // 3. txindex: an old confirmed tx without a block hint ----------------------
    let old_height = if network == Network::Bitcoin { 57043 } else { (tip / 2).max(1) };
    let old = async {
        let h: String = rpc.call("getblockhash", &[json!(old_height)]).await?;
        let b: Value = rpc.call("getblock", &[json!(h), json!(1)]).await?;
        let txid = b["tx"].as_array().and_then(|a| a.last()).and_then(|t| t.as_str()).unwrap_or("").to_string();
        Ok::<_, RpcError>((h, txid))
    }
    .await;
    match old {
        Ok((bh, txid)) => {
            match rpc.call::<Value>("getrawtransaction", &[json!(txid), json!(true)]).await {
                Ok(_) => r.add("txindex (getrawtransaction)", Status::Yes, format!("found {}… from block {old_height} without a block hint", &txid[..12])),
                Err(e) => r.add("txindex (getrawtransaction)", Status::No, format!("old tx without block hint: {}", err_detail(&e))),
            }
            match rpc.call::<Value>("getrawtransaction", &[json!(txid), json!(true), json!(bh)]).await {
                Ok(_) => r.add("getrawtransaction + blockhash", Status::Yes, "works when the containing block is known"),
                Err(e) => r.add("getrawtransaction + blockhash", Status::No, err_detail(&e)),
            }
            let t = Instant::now();
            match rpc.call::<Value>("getrawtransaction", &[json!(txid), json!(2), json!(bh)]).await {
                Ok(v) => {
                    let pv = v["vin"][0].get("prevout").is_some();
                    r.add(
                        "getrawtx verbosity 2 + blockhash",
                        if pv { Status::Yes } else { Status::Warn },
                        format!("{} in {}", if pv { "prevouts + fee for one tx" } else { "works, but no prevouts" }, ms(t)),
                    );
                }
                Err(e) => r.add("getrawtx verbosity 2 + blockhash", Status::No, err_detail(&e)),
            }
            match rpc.call::<Value>("gettxout", &[json!(txid), json!(0)]).await {
                Ok(v) => r.add("gettxout", Status::Yes, if v.is_null() { "allowed (output spent)" } else { "allowed (output unspent)" }),
                Err(e) => r.add("gettxout", Status::No, err_detail(&e)),
            }
        }
        Err(e) => r.add("txindex (getrawtransaction)", Status::Warn, format!("could not pick an old tx: {}", err_detail(&e))),
    }

    // 4. getblockstats -----------------------------------------------------------
    let t = Instant::now();
    match rpc.call::<Value>("getblockstats", &[json!(deep_hash), json!(["totalfee", "txs"])]).await {
        Ok(s) => r.add("getblockstats", Status::Yes, format!("totalfee {} sat, {} txs, {}", s["totalfee"], s["txs"], ms(t))),
        Err(e) => r.add("getblockstats", Status::No, err_detail(&e)),
    }

    // 5. mempool -------------------------------------------------------------------
    match rpc.call::<Value>("getmempoolinfo", &[]).await {
        Ok(m) => r.add("getmempoolinfo", Status::Yes, format!("{} txs, {} bytes", m["size"], m["bytes"])),
        Err(e) => r.add("getmempoolinfo", Status::No, err_detail(&e)),
    }
    match rpc.call::<Vec<String>>("getrawmempool", &[json!(false)]).await {
        Ok(ids) => {
            r.add("getrawmempool", Status::Yes, format!("{} txids", ids.len()));
            if let Some(id) = ids.first() {
                match rpc.call::<Value>("getrawtransaction", &[json!(id), json!(2)]).await {
                    Ok(v) => {
                        let pv = v["vin"][0].get("prevout").is_some();
                        r.add("getrawtx verbosity 2 (mempool)", if pv { Status::Yes } else { Status::Warn }, if pv { "includes prevouts" } else { "no prevouts for mempool txs" });
                    }
                    Err(e) => r.add("getrawtx verbosity 2 (mempool)", Status::No, err_detail(&e)),
                }
                match rpc.call::<Value>("getmempoolentry", &[json!(id)]).await {
                    Ok(v) => r.add("getmempoolentry", Status::Yes, format!("fee {} BTC, vsize {}", v["fees"]["base"], v["vsize"])),
                    Err(e) => r.add("getmempoolentry", Status::No, err_detail(&e)),
                }
            }
        }
        Err(e) => r.add("getrawmempool", Status::No, err_detail(&e)),
    }
    for (name, method, params) in [
        ("getblockheader", "getblockheader", vec![json!(deep_hash)]),
        ("getchaintips", "getchaintips", vec![]),
        ("estimatesmartfee", "estimatesmartfee", vec![json!(6)]),
    ] {
        match rpc.call::<Value>(method, &params).await {
            Ok(_) => r.add(name, Status::Yes, "allowed"),
            Err(e) => r.add(name, Status::No, err_detail(&e)),
        }
    }

    // 6. scantxoutset ---------------------------------------------------------------
    match rpc.call::<Value>("scantxoutset", &[json!("status")]).await {
        Ok(v) => r.add(
            "scantxoutset (status)",
            Status::Yes,
            if v.is_null() { "method allowed, no scan running".to_string() } else { format!("allowed, scan in progress: {v}") },
        ),
        Err(e) => r.add("scantxoutset (status)", Status::No, err_detail(&e)),
    }
    if real_scan {
        // Genesis coinbase address on mainnet; any valid descriptor elsewhere.
        let desc = if network == Network::Bitcoin { "addr(1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa)" } else { "raw(51)" };
        let t = Instant::now();
        match rpc.call_timeout::<Value>("scantxoutset", &[json!("start"), json!([desc])], Duration::from_secs(600)).await {
            Ok(v) => r.add("scantxoutset (real scan)", Status::Yes, format!("{} UTXOs, {} BTC, took {}", v["unspents"].as_array().map(|a| a.len()).unwrap_or(0), v["total_amount"], ms(t))),
            Err(e) => r.add("scantxoutset (real scan)", Status::No, format!("{} after {}", err_detail(&e), ms(t))),
        }
    } else {
        r.add("scantxoutset (real scan)", Status::Info, "skipped; pass --check-scan to time a real scan");
    }

    // 7. JSON-RPC batching and rate limit -------------------------------------------
    let before = rpc.rate_limit();
    let calls: Vec<(&str, Vec<Value>)> = (0..5).map(|i| ("getblockhash", vec![json!(i)])).collect();
    match rpc.batch::<String>(&calls, rpc.timeout()).await {
        Ok(res) => {
            let ok = res.iter().filter(|x| x.is_ok()).count();
            let cost = match (before, rpc.rate_limit()) {
                (Some(b), Some(a)) if a.remaining <= b.remaining => format!(", cost {} rate-limit unit(s) for 5 calls", b.remaining - a.remaining),
                _ => String::new(),
            };
            r.add("JSON-RPC batch", if ok == 5 { Status::Yes } else { Status::Warn }, format!("{ok}/5 results{cost}"));
        }
        Err(e) => r.add("JSON-RPC batch", Status::No, err_detail(&e)),
    }
    let calls: Vec<(&str, Vec<Value>)> = (0..100).map(|i| ("getblockhash", vec![json!(i)])).collect();
    match rpc.batch::<String>(&calls, rpc.timeout()).await {
        Ok(res) => {
            let ok = res.iter().filter(|x| x.is_ok()).count();
            r.add("JSON-RPC batch of 100", if ok == 100 { Status::Yes } else { Status::Warn }, format!("{ok}/100 results"));
        }
        Err(e) => r.add("JSON-RPC batch of 100", Status::No, err_detail(&e)),
    }
    match rpc.rate_limit() {
        Some(l) => r.add("provider rate limit", Status::Info, format!("{} req per window, {} remaining, resets in {}s", l.limit, l.remaining, l.reset_secs)),
        None => r.add("provider rate limit", Status::Info, "no X-RateLimit headers"),
    }

    r.print();
    let failures: Vec<&str> = r.rows.iter().filter(|x| x.status == Status::No).map(|x| x.name).collect();
    if failures.is_empty() {
        println!("All probes passed.");
    } else {
        println!("Not supported: {}", failures.join(", "));
    }
    essential_ok
}
