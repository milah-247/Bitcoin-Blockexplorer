//! Cached access to chain data. Handlers call these instead of raw RPC.
//!
//! Everything here is shaped by what the hosted RPC allows (see `--check`):
//! no `getblockheader`, `getblockstats` or txindex, but `getblock` v1,
//! `getrawtransaction` verbosity 2 with a block hash, and cheap JSON-RPC batches.

use std::{sync::Arc, time::Duration};

use serde_json::{json, Value};

use crate::{
    cache::{Ttl, CONFIRMED_DEPTH},
    error::AppError,
    state::AppState,
    util::{block_subsidy, sats},
};

#[derive(Debug, Clone)]
pub struct Tip {
    pub height: u64,
    pub hash: String,
    pub chain: String,
}

/// Batches of `getblock` v1 for mainnet blocks are several MB.
const HEAVY_TIMEOUT: Duration = Duration::from_secs(90);

fn depth_ttl(tip: u64, height: u64) -> Ttl {
    if tip.saturating_sub(height) >= CONFIRMED_DEPTH {
        Ttl::Long
    } else {
        Ttl::Short
    }
}

/// The fields we keep from `getblock` verbosity 1. `confirmations` and
/// `nextblockhash` are left out on purpose: they change as the chain grows.
fn block_from_v1(b: &Value) -> Value {
    json!({
        "hash": b["hash"],
        "height": b["height"],
        "version": b["version"],
        "time": b["time"],
        "median_time": b["mediantime"],
        "size": b["size"],
        "stripped_size": b["strippedsize"],
        "weight": b["weight"],
        "tx_count": b["nTx"],
        "merkle_root": b["merkleroot"],
        "bits": b["bits"],
        "nonce": b["nonce"],
        "difficulty": b["difficulty"],
        "previous_hash": b["previousblockhash"],
        "txids": b["tx"],
    })
}

impl AppState {
    pub async fn tip(&self) -> Result<Tip, AppError> {
        let v = self
            .cache
            .get_or("tip", async {
                let info: Value = self.rpc.call("getblockchaininfo", &[]).await?;
                Ok((
                    json!({ "height": info["blocks"], "hash": info["bestblockhash"], "chain": info["chain"] }),
                    Ttl::Tip,
                ))
            })
            .await?;
        Ok(Tip {
            height: v["height"].as_u64().unwrap_or(0),
            hash: v["hash"].as_str().unwrap_or_default().to_string(),
            chain: v["chain"].as_str().unwrap_or_default().to_string(),
        })
    }

    /// Block hashes for heights that must all be <= `tip`. Cache misses are
    /// fetched in a single JSON-RPC batch.
    pub async fn hashes_at(&self, tip: u64, heights: &[u64]) -> Result<Vec<String>, AppError> {
        let mut out: Vec<Option<String>> = vec![None; heights.len()];
        let mut missing = Vec::new();
        for (i, h) in heights.iter().enumerate() {
            match self.cache.get(&format!("h:{h}")).await {
                Some(v) => out[i] = v.as_str().map(String::from),
                None => missing.push(i),
            }
        }
        if !missing.is_empty() {
            let calls: Vec<(&str, Vec<Value>)> =
                missing.iter().map(|&i| ("getblockhash", vec![json!(heights[i])])).collect();
            let res = self.rpc.batch::<String>(&calls, self.rpc.timeout()).await?;
            for (&i, r) in missing.iter().zip(res) {
                let hash = r?;
                let h = heights[i];
                self.cache.put(format!("h:{h}"), json!(hash), depth_ttl(tip, h)).await;
                out[i] = Some(hash);
            }
        }
        Ok(out.into_iter().map(Option::unwrap_or_default).collect())
    }

    /// Hash at `height`, or None if above the tip.
    pub async fn hash_at(&self, height: u64) -> Result<Option<String>, AppError> {
        let tip = self.tip().await?;
        if height > tip.height {
            return Ok(None);
        }
        Ok(self.hashes_at(tip.height, &[height]).await?.pop())
    }

    /// Block summary + txids. Block content is immutable by hash, so it is
    /// cached long regardless of depth.
    pub async fn block(&self, hash: &str) -> Result<Arc<Value>, AppError> {
        self.cache
            .get_or(&format!("b:{hash}"), async {
                let b: Value = self
                    .rpc
                    .call_timeout("getblock", &[json!(hash), json!(1)], HEAVY_TIMEOUT)
                    .await
                    .map_err(|e| match AppError::from(e) {
                        AppError::NotFound(_) => AppError::NotFound("block not found".into()),
                        e => e,
                    })?;
                Ok((block_from_v1(&b), Ttl::Long))
            })
            .await
    }

    /// Several blocks; cache misses go out as one batch request.
    pub async fn blocks(&self, hashes: &[String]) -> Result<Vec<Arc<Value>>, AppError> {
        let mut out: Vec<Option<Arc<Value>>> = vec![None; hashes.len()];
        let mut missing = Vec::new();
        for (i, h) in hashes.iter().enumerate() {
            match self.cache.get(&format!("b:{h}")).await {
                Some(v) => out[i] = Some(v),
                None => missing.push(i),
            }
        }
        if !missing.is_empty() {
            let calls: Vec<(&str, Vec<Value>)> =
                missing.iter().map(|&i| ("getblock", vec![json!(hashes[i]), json!(1)])).collect();
            let res = self.rpc.batch::<Value>(&calls, HEAVY_TIMEOUT).await?;
            for (&i, r) in missing.iter().zip(res) {
                let v = self.cache.put(format!("b:{}", hashes[i]), block_from_v1(&r?), Ttl::Long).await;
                out[i] = Some(v);
            }
        }
        Ok(out.into_iter().flatten().collect())
    }

    /// Total fees of a block = coinbase outputs - subsidy (the provider does not
    /// allow `getblockstats`). Under-reports if a miner claimed less than allowed,
    /// which is rare. Returns None rather than failing the whole page.
    pub async fn block_fees(&self, block: &Value) -> Option<u64> {
        let height = block["height"].as_u64()?;
        if height == 0 {
            return Some(0); // genesis coinbase is not a retrievable transaction
        }
        let hash = block["hash"].as_str()?;
        let cb = block["txids"][0].as_str()?;
        let res = self
            .cache
            .get_or(&format!("fees:{hash}"), async {
                let tx: Value = self.rpc.call("getrawtransaction", &[json!(cb), json!(1), json!(hash)]).await?;
                let claimed: u64 = tx["vout"].as_array().map(|a| a.iter().map(|o| sats(&o["value"])).sum()).unwrap_or(0);
                Ok((json!(claimed.saturating_sub(block_subsidy(height, self.network))), Ttl::Long))
            })
            .await;
        match res {
            Ok(v) => v.as_u64(),
            Err(e) => {
                tracing::warn!(block = hash, error = ?e, "could not compute block fees");
                None
            }
        }
    }

    /// Confirmations for a block at `height` with `hash`; -1 if it is not on the
    /// active chain (stale), mirroring Bitcoin Core.
    pub async fn confirmations(&self, tip: &Tip, height: u64, hash: &str) -> Result<i64, AppError> {
        if height > tip.height {
            return Ok(-1);
        }
        let active = self.hashes_at(tip.height, &[height]).await?.pop();
        Ok(if active.as_deref() == Some(hash) { (tip.height - height + 1) as i64 } else { -1 })
    }

    /// A transaction in node JSON form (verbosity 2) with `prevout` on inputs where
    /// we could find it, plus `height`/`time` when confirmed.
    ///
    /// Without a txindex, confirmed transactions can only be fetched when we know
    /// their block, so `block_hint` (from the caller) is required for those.
    pub async fn tx(&self, txid: &str, block_hint: Option<String>) -> Result<Arc<Value>, AppError> {
        let key = format!("tx:{txid}");
        if let Some(v) = self.cache.get(&key).await {
            return Ok(v);
        }
        let mut params = vec![json!(txid), json!(2)];
        if let Some(bh) = &block_hint {
            params.push(json!(bh));
        }
        let mut tx: Value = match self.rpc.call("getrawtransaction", &params).await {
            Ok(v) => v,
            Err(e) if e.code() == Some(-5) => {
                return Err(AppError::NotFound(if block_hint.is_some() {
                    "transaction not found in the given block".into()
                } else {
                    "transaction not found in the mempool. This node has no transaction index, \
                     so a confirmed transaction can only be looked up together with its block \
                     (open it from the block page, or add ?block=<height or hash>)"
                        .into()
                }));
            }
            Err(e) => return Err(e.into()),
        };

        let mut ttl = Ttl::Short;
        if let Some(bh) = tx["blockhash"].as_str().map(String::from) {
            let block = self.block(&bh).await?;
            let height = block["height"].as_u64().unwrap_or(0);
            tx["height"] = json!(height);
            if tx["time"].is_null() {
                tx["time"] = block["time"].clone();
            }
            ttl = depth_ttl(self.tip().await?.height, height);
        } else {
            self.fill_mempool_prevouts(&mut tx).await;
        }
        Ok(self.cache.put(key, tx, ttl).await)
    }

    /// Mempool transactions come back without prevouts. Fetch parents that are
    /// themselves retrievable (in the mempool, or any tx on a txindex node) in one
    /// batch, then fall back to the mempool entry for the fee.
    async fn fill_mempool_prevouts(&self, tx: &mut Value) {
        let Some(vin) = tx["vin"].as_array() else { return };
        let wanted: Vec<(usize, String, u64)> = vin
            .iter()
            .enumerate()
            .filter(|(_, i)| i.get("prevout").is_none() && i.get("coinbase").is_none())
            .filter_map(|(n, i)| Some((n, i["txid"].as_str()?.to_string(), i["vout"].as_u64()?)))
            .collect();
        if !wanted.is_empty() {
            let calls: Vec<(&str, Vec<Value>)> =
                wanted.iter().map(|(_, p, _)| ("getrawtransaction", vec![json!(p), json!(1)])).collect();
            if let Ok(res) = self.rpc.batch::<Value>(&calls, self.rpc.timeout()).await {
                for ((n, _, vout), parent) in wanted.iter().zip(res) {
                    if let Ok(p) = parent {
                        let o = &p["vout"][*vout as usize];
                        tx["vin"][*n]["prevout"] = json!({ "value": o["value"], "scriptPubKey": o["scriptPubKey"] });
                    }
                }
            }
        }
        let complete = tx["vin"].as_array().is_some_and(|v| v.iter().all(|i| i.get("prevout").is_some() || i.get("coinbase").is_some()));
        if !complete && tx.get("fee").is_none() {
            if let Some(id) = tx["txid"].as_str().map(String::from) {
                if let Ok(e) = self.rpc.call::<Value>("getmempoolentry", &[json!(id)]).await {
                    tx["fee"] = e["fees"]["base"].clone();
                }
            }
        }
    }
}
