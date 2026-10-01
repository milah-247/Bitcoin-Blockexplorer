//! In-memory response cache (moka) with a TTL chosen per entry.
//!
//! What is immutable and what is not:
//! - block *content* by hash never changes, so it is cached long;
//! - height -> hash and transaction placement can change in a reorg, so they are
//!   cached long only once buried `CONFIRMED_DEPTH` blocks deep;
//! - tip and mempool data get a few seconds.

use std::{future::Future, sync::Arc, time::Duration};

use moka::{future::Cache as Moka, Expiry};
use serde_json::Value;

use crate::error::AppError;

/// Blocks this deep (or deeper) are treated as final.
pub const CONFIRMED_DEPTH: u64 = 6;

#[derive(Debug, Clone, Copy)]
pub enum Ttl {
    /// Content that cannot change (deep blocks/txs, block content by hash)
    Long,
    /// Near the tip, mempool
    Short,
    /// Chain tip
    Tip,
}

#[derive(Debug, Clone)]
pub struct CacheSettings {
    pub max_bytes: u64,
    pub long: Duration,
    pub short: Duration,
    pub tip: Duration,
}

#[derive(Clone)]
struct Entry {
    value: Arc<Value>,
    ttl: Duration,
    weight: u32,
}

struct PerEntry;

impl Expiry<String, Entry> for PerEntry {
    fn expire_after_create(&self, _k: &String, v: &Entry, _now: std::time::Instant) -> Option<Duration> {
        Some(v.ttl)
    }
}

pub struct Cache {
    inner: Moka<String, Entry>,
    s: CacheSettings,
}

impl Cache {
    pub fn new(s: CacheSettings) -> Cache {
        let inner = Moka::builder()
            .max_capacity(s.max_bytes)
            .weigher(|_k: &String, v: &Entry| v.weight)
            .expire_after(PerEntry)
            .build();
        Cache { inner, s }
    }

    fn ttl(&self, t: Ttl) -> Duration {
        match t {
            Ttl::Long => self.s.long,
            Ttl::Short => self.s.short,
            Ttl::Tip => self.s.tip,
        }
    }

    fn entry(&self, value: Value, ttl: Ttl) -> Entry {
        // Approximate memory use by serialized size.
        let weight = serde_json::to_vec(&value).map(|b| b.len()).unwrap_or(1024);
        Entry { value: Arc::new(value), ttl: self.ttl(ttl), weight: weight.min(u32::MAX as usize) as u32 }
    }

    pub async fn get(&self, key: &str) -> Option<Arc<Value>> {
        self.inner.get(key).await.map(|e| e.value)
    }

    pub async fn put(&self, key: String, value: Value, ttl: Ttl) -> Arc<Value> {
        let e = self.entry(value, ttl);
        let v = e.value.clone();
        self.inner.insert(key, e).await;
        v
    }

    /// Return the cached value or compute it. Concurrent callers for the same key
    /// share one computation, so a burst of identical requests costs one RPC.
    pub async fn get_or<F>(&self, key: &str, f: F) -> Result<Arc<Value>, AppError>
    where
        F: Future<Output = Result<(Value, Ttl), AppError>>,
    {
        self.inner
            .try_get_with_by_ref(key, async { f.await.map(|(v, t)| self.entry(v, t)) })
            .await
            .map(|e| e.value)
            .map_err(|e: Arc<AppError>| (*e).clone())
    }

    pub fn stats(&self) -> (u64, u64) {
        (self.inner.entry_count(), self.inner.weighted_size())
    }
}
