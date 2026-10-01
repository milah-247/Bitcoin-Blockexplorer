//! SQLite address index over a configurable height range.
//!
//! The provider has no address index, no txindex and forbids `scantxoutset`, so
//! we build our own from raw blocks. Indexing all of mainnet over a remote,
//! rate-limited RPC is impractical, so the index starts at `START_HEIGHT` and
//! every answer derived from it states the covered range.
//!
//! Tables (txids/hashes stored as 32-byte BLOBs in internal byte order):
//!   blocks(height, hash, header, sizes, tx_count, fees)
//!   txs(id, txid, height, pos)                 -- also a txid -> block index
//!   scripts(id, script)                        -- scriptPubKey, deduplicated
//!   outputs(tx_id, vout, script_id, value, height)
//!   spends(tx_id, vout, spender_id, height)    -- only for outputs we indexed
//! Every row carries a height, so a reorg rollback is `DELETE ... WHERE height >= h`.

use std::{
    path::Path,
    str::FromStr,
    sync::{Arc, Mutex, RwLock},
};

use bitcoin::{
    block::Header, consensus, hashes::Hash, Address, Block, BlockHash, Network, ScriptBuf, Txid,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

use crate::{error::AppError, util::block_subsidy};

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS blocks (
    height INTEGER PRIMARY KEY,
    hash BLOB NOT NULL UNIQUE,
    header BLOB NOT NULL,
    size INTEGER NOT NULL,
    stripped_size INTEGER NOT NULL,
    weight INTEGER NOT NULL,
    tx_count INTEGER NOT NULL,
    fees INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS txs (
    id INTEGER PRIMARY KEY,
    txid BLOB NOT NULL UNIQUE,
    height INTEGER NOT NULL,
    pos INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS txs_height ON txs(height, pos);
CREATE TABLE IF NOT EXISTS scripts (id INTEGER PRIMARY KEY, script BLOB NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS outputs (
    tx_id INTEGER NOT NULL,
    vout INTEGER NOT NULL,
    script_id INTEGER NOT NULL,
    value INTEGER NOT NULL,
    height INTEGER NOT NULL,
    PRIMARY KEY (tx_id, vout)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS outputs_script ON outputs(script_id);
CREATE INDEX IF NOT EXISTS outputs_height ON outputs(height);
CREATE TABLE IF NOT EXISTS spends (
    tx_id INTEGER NOT NULL,
    vout INTEGER NOT NULL,
    spender_id INTEGER NOT NULL,
    height INTEGER NOT NULL,
    PRIMARY KEY (tx_id, vout)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS spends_spender ON spends(spender_id);
CREATE INDEX IF NOT EXISTS spends_height ON spends(height);
";

/// Live indexer progress, shared with the API.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IndexStatus {
    pub start_height: u64,
    /// Highest block written; None until the first block is indexed.
    pub indexed_height: Option<u64>,
    pub node_tip: Option<u64>,
    pub synced: bool,
    pub last_error: Option<String>,
}

pub struct Index {
    reader: Mutex<Connection>,
    pub network: Network,
    pub status: RwLock<IndexStatus>,
}

/// A block decoded and reduced to what the index stores.
pub struct IndexedBlock {
    pub height: u64,
    pub block: Block,
    pub size: usize,
}

fn db_err(e: rusqlite::Error) -> AppError {
    AppError::Upstream(format!("index database error: {e}"))
}

pub fn open_writer(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let c = Connection::open(path)?;
    c.execute_batch(SCHEMA)?;
    c.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA cache_size = -65536;")?;
    Ok(c)
}

/// Read the stored start height, or record `wanted` on first use.
/// Changing START_HEIGHT later needs a fresh database (we cannot backfill downwards).
pub fn init_meta(c: &Connection, network: Network, wanted: u64) -> Result<u64, String> {
    let get = |k: &str| -> Option<String> {
        c.query_row("SELECT value FROM meta WHERE key = ?1", [k], |r| r.get(0)).optional().ok().flatten()
    };
    if let Some(n) = get("network") {
        if n != network.to_string() {
            return Err(format!("index database is for network {n}, not {network}"));
        }
    }
    if let Some(s) = get("start_height") {
        let s: u64 = s.parse().map_err(|_| "corrupt start_height in index meta")?;
        if s != wanted {
            tracing::warn!(stored = s, requested = wanted, "START_HEIGHT differs from the existing index; keeping {s}. Delete the index file to change it");
        }
        return Ok(s);
    }
    c.execute("INSERT INTO meta(key, value) VALUES ('network', ?1), ('start_height', ?2)", params![network.to_string(), wanted.to_string()])
        .map_err(|e| e.to_string())?;
    Ok(wanted)
}

pub fn tip(c: &Connection) -> rusqlite::Result<Option<(u64, BlockHash)>> {
    c.query_row("SELECT height, hash FROM blocks ORDER BY height DESC LIMIT 1", [], |r| {
        Ok((r.get::<_, u64>(0)?, r.get::<_, Vec<u8>>(1)?))
    })
    .optional()
    .map(|o| o.and_then(|(h, b)| Some((h, BlockHash::from_slice(&b).ok()?))))
}

/// Remove everything at or above `height` (reorg rollback).
pub fn rollback_from(c: &mut Connection, height: u64) -> rusqlite::Result<()> {
    let t = c.transaction()?;
    for table in ["spends", "outputs", "txs", "blocks"] {
        t.execute(&format!("DELETE FROM {table} WHERE height >= ?1"), [height])?;
    }
    t.commit()
}

/// Write one block atomically.
pub fn write_block(c: &mut Connection, b: &IndexedBlock, network: Network) -> rusqlite::Result<()> {
    let t = c.transaction()?;
    {
        let block = &b.block;
        let h = b.height;
        let mut ins_tx = t.prepare_cached("INSERT OR IGNORE INTO txs(txid, height, pos) VALUES (?1, ?2, ?3)")?;
        let mut get_tx = t.prepare_cached("SELECT id FROM txs WHERE txid = ?1")?;
        let mut ins_script = t.prepare_cached("INSERT OR IGNORE INTO scripts(script) VALUES (?1)")?;
        let mut get_script = t.prepare_cached("SELECT id FROM scripts WHERE script = ?1")?;
        let mut ins_out = t.prepare_cached("INSERT OR IGNORE INTO outputs(tx_id, vout, script_id, value, height) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        let mut ins_spend = t.prepare_cached("INSERT OR IGNORE INTO spends(tx_id, vout, spender_id, height) VALUES (?1, ?2, ?3, ?4)")?;

        let mut coinbase_out = 0u64;
        for (pos, tx) in block.txdata.iter().enumerate() {
            let txid = tx.compute_txid();
            ins_tx.execute(params![txid.as_byte_array().as_slice(), h, pos as i64])?;
            let tx_id: i64 = get_tx.query_row([txid.as_byte_array().as_slice()], |r| r.get(0))?;

            if tx.is_coinbase() {
                coinbase_out = tx.output.iter().map(|o| o.value.to_sat()).sum();
            } else {
                for i in &tx.input {
                    let prev = i.previous_output;
                    // Only outputs created inside the indexed range are known.
                    if let Some(prev_id) = get_tx.query_row([prev.txid.as_byte_array().as_slice()], |r| r.get::<_, i64>(0)).optional()? {
                        ins_spend.execute(params![prev_id, prev.vout, tx_id, h])?;
                    }
                }
            }
            for (vout, o) in tx.output.iter().enumerate() {
                if o.script_pubkey.is_op_return() {
                    continue;
                }
                let s = o.script_pubkey.as_bytes();
                ins_script.execute([s])?;
                let script_id: i64 = get_script.query_row([s], |r| r.get(0))?;
                ins_out.execute(params![tx_id, vout as i64, script_id, o.value.to_sat() as i64, h])?;
            }
        }
        let weight = block.weight().to_wu() as usize;
        let stripped = (weight - b.size) / 3;
        let fees = coinbase_out.saturating_sub(block_subsidy(h, network));
        t.execute(
            "INSERT OR REPLACE INTO blocks(height, hash, header, size, stripped_size, weight, tx_count, fees)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                h,
                block.block_hash().as_byte_array().as_slice(),
                consensus::serialize(&block.header),
                b.size as i64,
                stripped as i64,
                weight as i64,
                block.txdata.len() as i64,
                fees as i64
            ],
        )?;
    }
    t.commit()
}

/// Per-address figures over the indexed range.
#[derive(Debug, Default, Serialize)]
pub struct AddressSummary {
    pub received_sat: u64,
    pub sent_sat: u64,
    pub balance_sat: u64,
    pub tx_count: u64,
    pub utxo_count: u64,
    pub unspents: Vec<Value>,
}

impl Index {
    pub fn open_reader(path: &Path, network: Network, status: IndexStatus) -> rusqlite::Result<Index> {
        let c = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        Ok(Index { reader: Mutex::new(c), network, status: RwLock::new(status) })
    }

    pub fn status(&self) -> IndexStatus {
        self.status.read().unwrap().clone()
    }

    /// Coverage block included in every index-backed response.
    pub fn coverage(&self) -> Value {
        let s = self.status();
        json!({
            "start_height": s.start_height,
            "indexed_height": s.indexed_height,
            "node_tip": s.node_tip,
            "synced": s.synced,
            "complete_history": s.start_height == 0,
        })
    }

    /// Run a read query off the async runtime.
    pub async fn read<T, F>(self: &Arc<Self>, f: F) -> Result<T, AppError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection, Network) -> rusqlite::Result<T> + Send + 'static,
    {
        let me = self.clone();
        tokio::task::spawn_blocking(move || {
            let c = me.reader.lock().unwrap();
            f(&c, me.network)
        })
        .await
        .map_err(|e| AppError::Upstream(format!("index task failed: {e}")))?
        .map_err(db_err)
    }

    /// Height and block hash of an indexed transaction.
    pub async fn tx_location(self: &Arc<Self>, txid: &str) -> Result<Option<(u64, String)>, AppError> {
        let Ok(t) = Txid::from_str(txid) else { return Ok(None) };
        self.read(move |c, _| {
            c.query_row(
                "SELECT t.height, b.hash FROM txs t JOIN blocks b ON b.height = t.height WHERE t.txid = ?1",
                [t.as_byte_array().as_slice()],
                |r| Ok((r.get::<_, u64>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
        })
        .await
        .map(|o| o.and_then(|(h, b)| Some((h, BlockHash::from_slice(&b).ok()?.to_string()))))
    }

    /// Value and scriptPubKey of an indexed output, shaped like Core's `prevout`.
    pub async fn prevouts(self: &Arc<Self>, outpoints: Vec<(String, u64)>) -> Result<Vec<Option<Value>>, AppError> {
        self.read(move |c, net| {
            let mut q = c.prepare_cached(
                "SELECT o.value, s.script FROM txs t JOIN outputs o ON o.tx_id = t.id JOIN scripts s ON s.id = o.script_id
                 WHERE t.txid = ?1 AND o.vout = ?2",
            )?;
            outpoints
                .iter()
                .map(|(txid, vout)| {
                    let Ok(t) = Txid::from_str(txid) else { return Ok(None) };
                    let row = q
                        .query_row(params![t.as_byte_array().as_slice(), *vout as i64], |r| {
                            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
                        })
                        .optional()?;
                    Ok(row.map(|(v, s)| {
                        let script = ScriptBuf::from_bytes(s);
                        let addr = Address::from_script(&script, net).ok().map(|a| a.to_string());
                        json!({ "value": v as f64 / 1e8, "scriptPubKey": { "address": addr } })
                    }))
                })
                .collect()
        })
        .await
    }

    /// Block summary in the same shape `chain::block_from_v1` produces, plus fees.
    pub async fn block_by_hash(self: &Arc<Self>, hash: &str) -> Result<Option<Value>, AppError> {
        let Ok(bh) = BlockHash::from_str(hash) else { return Ok(None) };
        self.read(move |c, _| {
            let row = c
                .query_row(
                    "SELECT height, header, size, stripped_size, weight, tx_count, fees FROM blocks WHERE hash = ?1",
                    [bh.as_byte_array().as_slice()],
                    |r| {
                        Ok((
                            r.get::<_, u64>(0)?,
                            r.get::<_, Vec<u8>>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, i64>(5)?,
                            r.get::<_, i64>(6)?,
                        ))
                    },
                )
                .optional()?;
            let Some((height, header, size, stripped, weight, n, fees)) = row else { return Ok(None) };
            let Ok(hd) = consensus::deserialize::<Header>(&header) else { return Ok(None) };
            let mut txids = Vec::with_capacity(n as usize);
            let mut q = c.prepare_cached("SELECT txid FROM txs WHERE height = ?1 ORDER BY pos")?;
            for t in q.query_map([height], |r| r.get::<_, Vec<u8>>(0))? {
                if let Ok(id) = Txid::from_slice(&t?) {
                    txids.push(id.to_string());
                }
            }
            // Median time past needs the 11 blocks ending here.
            let mut times: Vec<i64> = c
                .prepare_cached("SELECT header FROM blocks WHERE height BETWEEN ?1 AND ?2")?
                .query_map(params![height.saturating_sub(10), height], |r| r.get::<_, Vec<u8>>(0))?
                .filter_map(|h| consensus::deserialize::<Header>(&h.ok()?).ok().map(|h| h.time as i64))
                .collect();
            times.sort_unstable();
            let median = (times.len() == 11 || height < 11 && times.len() as u64 == height + 1).then(|| times[times.len() / 2]);
            Ok(Some(json!({
                "hash": hash_str(&hd),
                "height": height,
                "version": hd.version.to_consensus(),
                "time": hd.time,
                "median_time": median,
                "size": size,
                "stripped_size": stripped,
                "weight": weight,
                "tx_count": n,
                "merkle_root": hd.merkle_root.to_string(),
                "bits": format!("{:08x}", hd.bits.to_consensus()),
                "nonce": hd.nonce,
                "difficulty": hd.difficulty_float(),
                "previous_hash": if height == 0 { Value::Null } else { json!(hd.prev_blockhash.to_string()) },
                "txids": txids,
                "fees_sat": fees,
            })))
        })
        .await
    }

    pub async fn address_summary(self: &Arc<Self>, script: ScriptBuf, max_utxos: usize) -> Result<AddressSummary, AppError> {
        self.read(move |c, _| {
            let Some(sid) = script_id(c, &script)? else { return Ok(AddressSummary::default()) };
            let (received, n_out): (i64, i64) =
                c.query_row("SELECT COALESCE(SUM(value), 0), COUNT(*) FROM outputs WHERE script_id = ?1", [sid], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let sent: i64 = c.query_row(
                "SELECT COALESCE(SUM(o.value), 0) FROM outputs o JOIN spends s ON s.tx_id = o.tx_id AND s.vout = o.vout WHERE o.script_id = ?1",
                [sid],
                |r| r.get(0),
            )?;
            let tx_count: i64 = c.query_row(
                "SELECT COUNT(*) FROM (SELECT tx_id FROM outputs WHERE script_id = ?1
                 UNION SELECT s.spender_id FROM outputs o JOIN spends s ON s.tx_id = o.tx_id AND s.vout = o.vout WHERE o.script_id = ?1)",
                [sid],
                |r| r.get(0),
            )?;
            let spent_count: i64 = c.query_row(
                "SELECT COUNT(*) FROM outputs o JOIN spends s ON s.tx_id = o.tx_id AND s.vout = o.vout WHERE o.script_id = ?1",
                [sid],
                |r| r.get(0),
            )?;
            let mut q = c.prepare_cached(
                "SELECT t.txid, o.vout, o.height, o.value FROM outputs o JOIN txs t ON t.id = o.tx_id
                 LEFT JOIN spends s ON s.tx_id = o.tx_id AND s.vout = o.vout
                 WHERE o.script_id = ?1 AND s.tx_id IS NULL ORDER BY o.height DESC, t.pos DESC LIMIT ?2",
            )?;
            let unspents = q
                .query_map(params![sid, max_utxos as i64], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
                })?
                .filter_map(Result::ok)
                .map(|(t, vout, h, v)| {
                    json!({ "txid": Txid::from_slice(&t).map(|t| t.to_string()).unwrap_or_default(), "vout": vout, "height": h, "amount_sat": v })
                })
                .collect();
            Ok(AddressSummary {
                received_sat: received as u64,
                sent_sat: sent as u64,
                balance_sat: (received - sent).max(0) as u64,
                tx_count: tx_count as u64,
                utxo_count: (n_out - spent_count).max(0) as u64,
                unspents,
            })
        })
        .await
    }

    /// Newest-first transactions touching the address, with per-tx amounts.
    pub async fn address_txs(self: &Arc<Self>, script: ScriptBuf, start: u64, limit: u64) -> Result<Vec<Value>, AppError> {
        self.read(move |c, _| {
            let Some(sid) = script_id(c, &script)? else { return Ok(Vec::new()) };
            let mut q = c.prepare_cached(
                "WITH recv AS (SELECT tx_id AS t, SUM(value) AS v FROM outputs WHERE script_id = ?1 GROUP BY tx_id),
                      sent AS (SELECT s.spender_id AS t, SUM(o.value) AS v FROM outputs o
                               JOIN spends s ON s.tx_id = o.tx_id AND s.vout = o.vout
                               WHERE o.script_id = ?1 GROUP BY s.spender_id),
                      ids AS (SELECT t FROM recv UNION SELECT t FROM sent)
                 SELECT tx.txid, tx.height, b.header, b.hash, COALESCE(recv.v, 0), COALESCE(sent.v, 0)
                 FROM ids JOIN txs tx ON tx.id = ids.t
                 JOIN blocks b ON b.height = tx.height
                 LEFT JOIN recv ON recv.t = ids.t LEFT JOIN sent ON sent.t = ids.t
                 ORDER BY tx.height DESC, tx.pos DESC LIMIT ?2 OFFSET ?3",
            )?;
            let rows = q
                .query_map(params![sid, limit as i64, start as i64], |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, u64>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                })?
                .filter_map(Result::ok)
                .map(|(t, h, header, bh, recv, sent)| {
                    let time = consensus::deserialize::<Header>(&header).map(|h| h.time).ok();
                    json!({
                        "txid": Txid::from_slice(&t).map(|t| t.to_string()).unwrap_or_default(),
                        "block_height": h,
                        "block_hash": BlockHash::from_slice(&bh).map(|b| b.to_string()).ok(),
                        "block_time": time,
                        "received_sat": recv,
                        "sent_sat": sent,
                        "net_sat": recv - sent,
                    })
                })
                .collect();
            Ok(rows)
        })
        .await
    }
}

fn hash_str(h: &Header) -> String {
    h.block_hash().to_string()
}

fn script_id(c: &Connection, script: &ScriptBuf) -> rusqlite::Result<Option<i64>> {
    c.query_row("SELECT id FROM scripts WHERE script = ?1", [script.as_bytes()], |r| r.get(0)).optional()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{
        absolute::LockTime, block::Version as BlockVersion, transaction::Version, Amount, CompactTarget, OutPoint,
        Sequence, Transaction, TxIn, TxMerkleNode, TxOut, Witness,
    };

    const COIN: u64 = 100_000_000;

    fn script(tag: u8) -> ScriptBuf {
        let mut b = vec![0x00, 0x14];
        b.extend([tag; 20]);
        ScriptBuf::from_bytes(b)
    }

    fn tx(inputs: Vec<OutPoint>, outputs: Vec<(ScriptBuf, u64)>, tag: u8) -> Transaction {
        let coinbase = inputs.is_empty();
        let input = if coinbase {
            vec![TxIn { previous_output: OutPoint::null(), script_sig: ScriptBuf::from_bytes(vec![1, tag]), sequence: Sequence::MAX, witness: Witness::new() }]
        } else {
            inputs.into_iter().map(|p| TxIn { previous_output: p, script_sig: ScriptBuf::new(), sequence: Sequence::MAX, witness: Witness::new() }).collect()
        };
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input,
            output: outputs.into_iter().map(|(s, v)| TxOut { value: Amount::from_sat(v), script_pubkey: s }).collect(),
        }
    }

    fn block(height: u64, prev: BlockHash, txdata: Vec<Transaction>) -> IndexedBlock {
        let header = Header {
            version: BlockVersion::TWO,
            prev_blockhash: prev,
            merkle_root: TxMerkleNode::all_zeros(),
            time: 1_700_000_000 + height as u32,
            bits: CompactTarget::from_consensus(0x207fffff),
            nonce: height as u32,
        };
        let block = Block { header, txdata };
        IndexedBlock { height, size: consensus::serialize(&block).len(), block }
    }

    #[tokio::test]
    async fn index_balances_history_and_rollback() {
        let dir = std::env::temp_dir().join(format!("be-index-test-{}", std::process::id()));
        let path = dir.join("t.sqlite");
        let _ = std::fs::remove_file(&path);
        let mut w = open_writer(&path).unwrap();
        assert_eq!(init_meta(&w, Network::Regtest, 1).unwrap(), 1);
        assert!(init_meta(&w, Network::Bitcoin, 1).is_err(), "network mismatch must be refused");

        let (a, b, miner) = (script(0xaa), script(0xbb), script(0xcc));
        let cb1 = tx(vec![], vec![(a.clone(), 50 * COIN)], 1);
        let b1 = block(1, BlockHash::all_zeros(), vec![cb1.clone()]);
        // block 2: A spends 50 BTC -> 49 to B, 0.5 change to A, 0.5 fee to the miner
        let spend = tx(vec![OutPoint::new(cb1.compute_txid(), 0)], vec![(b.clone(), 49 * COIN), (a.clone(), COIN / 2)], 0);
        let cb2 = tx(vec![], vec![(miner.clone(), 50 * COIN + COIN / 2)], 2);
        let b2 = block(2, b1.block.block_hash(), vec![cb2, spend.clone()]);
        let b2_hash = b2.block.block_hash();
        write_block(&mut w, &b1, Network::Regtest).unwrap();
        write_block(&mut w, &b2, Network::Regtest).unwrap();
        assert_eq!(tip(&w).unwrap().unwrap(), (2, b2_hash));

        let idx = Arc::new(Index::open_reader(&path, Network::Regtest, IndexStatus::default()).unwrap());
        let sa = idx.address_summary(a.clone(), 10).await.unwrap();
        assert_eq!((sa.received_sat, sa.sent_sat, sa.balance_sat), (50 * COIN + COIN / 2, 50 * COIN, COIN / 2));
        assert_eq!((sa.tx_count, sa.utxo_count), (2, 1));
        assert_eq!(sa.unspents[0]["txid"], spend.compute_txid().to_string());
        assert_eq!(idx.address_summary(b.clone(), 10).await.unwrap().balance_sat, 49 * COIN);

        let hist = idx.address_txs(a.clone(), 0, 10).await.unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0]["txid"], spend.compute_txid().to_string()); // newest first
        assert_eq!(hist[0]["net_sat"], -(49 * COIN as i64 + COIN as i64 / 2));
        assert_eq!(hist[1]["net_sat"], 50 * COIN as i64);
        assert_eq!(idx.address_txs(a.clone(), 1, 10).await.unwrap().len(), 1);

        let loc = idx.tx_location(&spend.compute_txid().to_string()).await.unwrap();
        assert_eq!(loc, Some((2, b2_hash.to_string())));
        let pv = idx.prevouts(vec![(cb1.compute_txid().to_string(), 0), ("00".repeat(32), 0)]).await.unwrap();
        assert_eq!(pv[0].as_ref().unwrap()["value"], 50.0);
        assert!(pv[1].is_none());

        let blk = idx.block_by_hash(&b2_hash.to_string()).await.unwrap().unwrap();
        assert_eq!(blk["height"], 2);
        assert_eq!(blk["tx_count"], 2);
        assert_eq!(blk["fees_sat"], COIN / 2);
        assert_eq!(blk["previous_hash"], b1.block.block_hash().to_string());

        // Reorg: drop block 2. A gets its 50 BTC back, B never received anything.
        rollback_from(&mut w, 2).unwrap();
        let sa = idx.address_summary(a.clone(), 10).await.unwrap();
        assert_eq!((sa.balance_sat, sa.tx_count, sa.utxo_count), (50 * COIN, 1, 1));
        assert_eq!(idx.address_summary(b, 10).await.unwrap().tx_count, 0);
        assert!(idx.tx_location(&spend.compute_txid().to_string()).await.unwrap().is_none());
        assert_eq!(tip(&w).unwrap().unwrap().0, 1);

        drop(w);
        let _ = std::fs::remove_dir_all(dir);
    }
}
