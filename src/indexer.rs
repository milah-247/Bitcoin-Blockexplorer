//! Background task that keeps the SQLite address index in step with the node.
//!
//! Each round: check our top block is still on the node's best chain (rolling
//! back one block at a time if not), then fetch the next blocks as raw hex
//! (`getblock` verbosity 0, decoded locally) with a few downloads in flight, and
//! write each block in its own transaction. The highest stored block is the
//! checkpoint, so a restart resumes where it stopped.
//!
//! Why verbosity 0 and not 3: on mainnet a block is ~3 MB as hex (~4 s) versus
//! ~13 MB as verbosity-3 JSON (~25 s). Verbosity 3 would add the scriptPubKey of
//! coins created *before* START_HEIGHT that are spent inside the range, which
//! still would not give a correct balance for such coins. Not worth 5x.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bitcoin::{consensus::encode::deserialize_hex, Block, BlockHash, Network};
use futures::{StreamExt, TryStreamExt};
use rusqlite::Connection;
use serde_json::json;

use crate::index::{self, Index, IndexStatus, IndexedBlock};
use crate::rpc::Rpc;

#[derive(Debug, Clone, Copy)]
pub enum StartHeight {
    Fixed(u64),
    /// N blocks below the node tip at first start.
    FromTip(u64),
}

#[derive(Debug, Clone)]
pub struct IndexerSettings {
    pub path: PathBuf,
    pub start: StartHeight,
    pub concurrency: usize,
    pub poll: Duration,
    pub max_reorg: u64,
}

const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

type Db = Arc<Mutex<Connection>>;

async fn db<T, F>(w: &Db, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
{
    let w = w.clone();
    tokio::task::spawn_blocking(move || f(&mut w.lock().unwrap()))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("index database: {e}"))
}

/// Open the database, start the sync task and return the read handle.
pub fn spawn(rpc: Arc<Rpc>, network: Network, s: IndexerSettings) -> Result<Arc<Index>, String> {
    let w = index::open_writer(&s.path).map_err(|e| format!("cannot open index {}: {e}", s.path.display()))?;
    let stored_start: Option<u64> = w
        .query_row("SELECT value FROM meta WHERE key = 'start_height'", [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|v| v.parse().ok());
    let indexed = index::tip(&w).map_err(|e| e.to_string())?.map(|t| t.0);
    let status = IndexStatus { start_height: stored_start.unwrap_or(0), indexed_height: indexed, ..Default::default() };
    let idx = Arc::new(Index::open_reader(&s.path, network, status).map_err(|e| e.to_string())?);
    tracing::info!(path = %s.path.display(), start = ?stored_start, indexed = ?indexed, "address index opened");
    let task_idx = idx.clone();
    tokio::spawn(async move { run(rpc, task_idx, Arc::new(Mutex::new(w)), network, s).await });
    Ok(idx)
}

fn set_error(idx: &Index, e: Option<String>) {
    idx.status.write().unwrap().last_error = e;
}

async fn run(rpc: Arc<Rpc>, idx: Arc<Index>, w: Db, network: Network, s: IndexerSettings) {
    // Decide the start height once (needs the node tip for `tip-N`).
    let start = loop {
        let wanted = match s.start {
            StartHeight::Fixed(h) => Ok(h),
            StartHeight::FromTip(n) => rpc.call::<u64>("getblockcount", &[]).await.map(|t| t.saturating_sub(n)).map_err(|e| e.to_string()),
        };
        let res = match wanted {
            Ok(h) => db(&w, move |c| Ok(index::init_meta(c, network, h))).await.and_then(|r| r),
            Err(e) => Err(e),
        };
        match res {
            Ok(h) => break h,
            Err(e) => {
                tracing::warn!(error = %e, "address index: cannot initialise, retrying in 30s");
                set_error(&idx, Some(e));
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    };
    idx.status.write().unwrap().start_height = start;
    tracing::info!(start, "address index: syncing");

    let mut rollbacks = 0u64;
    let mut failures = 0u32;
    loop {
        match step(&rpc, &idx, &w, network, start, &s, &mut rollbacks).await {
            Ok(progress) => {
                failures = 0;
                set_error(&idx, None);
                if !progress {
                    tokio::time::sleep(s.poll).await;
                }
            }
            Err(e) => {
                failures += 1;
                let wait = Duration::from_secs((5u64 << failures.min(5)).min(120));
                tracing::warn!(error = %e, retry_in_secs = wait.as_secs(), "address index: sync error");
                set_error(&idx, Some(e));
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// One sync round. Ok(true) if anything changed (run again immediately).
async fn step(
    rpc: &Rpc,
    idx: &Index,
    w: &Db,
    network: Network,
    start: u64,
    s: &IndexerSettings,
    rollbacks: &mut u64,
) -> Result<bool, String> {
    let node_tip: u64 = rpc.call("getblockcount", &[]).await.map_err(|e| e.to_string())?;
    idx.status.write().unwrap().node_tip = Some(node_tip);
    let local = db(w, |c| index::tip(c)).await?;

    // Reorg check: is our top block still on the node's best chain?
    if let Some((h, hash)) = local {
        let on_chain = if h > node_tip {
            false
        } else {
            let node_hash: String = rpc.call("getblockhash", &[json!(h)]).await.map_err(|e| e.to_string())?;
            node_hash == hash.to_string()
        };
        if !on_chain {
            *rollbacks += 1;
            if *rollbacks > s.max_reorg {
                return Err(format!("reorg deeper than INDEX_MAX_REORG ({}); delete the index to rebuild", s.max_reorg));
            }
            tracing::warn!(height = h, "address index: block no longer on best chain, rolling back");
            db(w, move |c| index::rollback_from(c, h)).await?;
            let mut st = idx.status.write().unwrap();
            st.indexed_height = h.checked_sub(1).filter(|p| *p >= start);
            st.synced = false;
            return Ok(true);
        }
    }
    *rollbacks = 0;

    let next = local.map(|(h, _)| h + 1).unwrap_or(start);
    if next > node_tip {
        idx.status.write().unwrap().synced = true;
        return Ok(false);
    }
    let end = node_tip.min(next + (s.concurrency as u64) * 4 - 1);
    let calls: Vec<(&str, Vec<serde_json::Value>)> = (next..=end).map(|h| ("getblockhash", vec![json!(h)])).collect();
    let hashes: Vec<String> = rpc
        .batch::<String>(&calls, rpc.timeout())
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;

    let mut blocks = futures::stream::iter((next..=end).zip(hashes))
        .map(|(height, hash)| fetch_block(rpc, height, hash))
        .buffered(s.concurrency.max(1));

    let mut prev: Option<BlockHash> = local.map(|(_, h)| h);
    while let Some(b) = blocks.try_next().await? {
        if let Some(p) = prev {
            if b.block.header.prev_blockhash != p {
                // The chain moved under us; the next round's reorg check sorts it out.
                tracing::warn!(height = b.height, "address index: parent mismatch, re-checking chain");
                return Ok(true);
            }
        }
        prev = Some(b.block.block_hash());
        let (height, ntx, t) = (b.height, b.block.txdata.len(), Instant::now());
        db(w, move |c| index::write_block(c, &b, network)).await?;
        let mut st = idx.status.write().unwrap();
        st.indexed_height = Some(height);
        st.synced = height == node_tip;
        tracing::info!(height, txs = ntx, write_ms = t.elapsed().as_millis() as u64, behind = node_tip - height, "address index: block indexed");
    }
    Ok(true)
}

async fn fetch_block(rpc: &Rpc, height: u64, hash: String) -> Result<IndexedBlock, String> {
    let hex: String = rpc
        .call_timeout("getblock", &[json!(hash), json!(0)], FETCH_TIMEOUT)
        .await
        .map_err(|e| format!("getblock {height}: {e}"))?;
    tokio::task::spawn_blocking(move || {
        let block: Block = deserialize_hex(&hex).map_err(|e| format!("decode block {height}: {e}"))?;
        if block.block_hash().to_string() != hash {
            return Err(format!("block {height}: hash mismatch"));
        }
        Ok(IndexedBlock { height, size: hex.len() / 2, block })
    })
    .await
    .map_err(|e| e.to_string())?
}
