//! Minimal async JSON-RPC client for Bitcoin Core (or a hosted proxy in front of it).
//!
//! Provider-friendly by construction:
//! - every call goes through a concurrency limit (semaphore) and an optional
//!   client-side rate limiter (token bucket),
//! - `X-RateLimit-*` / `Retry-After` headers from the provider pause all callers,
//! - transient failures (429, 5xx, connection errors, Core warming up) are retried
//!   with exponential backoff. Timeouts are *not* retried: the node may still be
//!   working on the request, and repeating it only adds load.

use std::{
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE, RETRY_AFTER};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use tokio::sync::Semaphore;

/// How to authenticate against the RPC endpoint.
#[derive(Clone)]
pub enum Auth {
    /// HTTP Basic (rpcuser/rpcpassword or the `.cookie` file).
    Basic { user: String, pass: String },
    /// API key in a request header (hosted providers), e.g. `X-API-Key`.
    Header { name: HeaderName, value: HeaderValue },
}

/// Never print secrets, even in debug output.
impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::Basic { user, .. } => write!(f, "basic(user={user}, pass=***)"),
            Auth::Header { name, .. } => write!(f, "header({name}: ***)"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RpcSettings {
    pub url: String,
    pub auth: Auth,
    /// Default per-request timeout.
    pub timeout: Duration,
    pub max_concurrency: usize,
    pub max_retries: u32,
    /// Client-side cap on requests per minute; 0 disables it.
    pub rate_per_min: u32,
}

#[derive(Debug, Clone)]
pub enum RpcError {
    /// The node answered with a JSON-RPC error object.
    Rpc { code: i64, message: String },
    /// Non-success HTTP status without a JSON-RPC body (provider 401/403/404/429, 5xx...).
    Http { status: u16, message: String },
    Timeout,
    Transport(String),
    Decode(String),
}

impl RpcError {
    pub fn code(&self) -> Option<i64> {
        match self {
            RpcError::Rpc { code, .. } => Some(*code),
            _ => None,
        }
    }

    fn is_transient(&self) -> bool {
        match self {
            // -28: RPC in warmup
            RpcError::Rpc { code, .. } => *code == -28,
            RpcError::Http { status, .. } => *status == 429 || *status >= 500,
            RpcError::Transport(_) => true,
            RpcError::Timeout | RpcError::Decode(_) => false,
        }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcError::Rpc { code, message } => write!(f, "RPC error {code}: {message}"),
            RpcError::Http { status, message } => write!(f, "HTTP {status}: {message}"),
            RpcError::Timeout => write!(f, "request to node timed out"),
            RpcError::Transport(m) => write!(f, "cannot reach node: {m}"),
            RpcError::Decode(m) => write!(f, "unexpected response from node: {m}"),
        }
    }
}

impl std::error::Error for RpcError {}

/// Rate-limit state as last reported by the provider.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct RateLimitInfo {
    pub limit: u64,
    pub remaining: u64,
    pub reset_secs: u64,
}

/// Token bucket plus a "pause until" set from provider headers.
struct RateLimiter {
    per_sec: f64,
    capacity: f64,
    state: Mutex<(f64, Instant)>,
}

impl RateLimiter {
    fn new(per_min: u32) -> Self {
        let per_sec = per_min as f64 / 60.0;
        // Small bursts only, so a fixed provider window can't see ~2x the rate.
        let capacity = (per_min as f64 / 4.0).max(1.0);
        RateLimiter { per_sec, capacity, state: Mutex::new((capacity, Instant::now())) }
    }

    async fn acquire(&self) {
        loop {
            let wait = {
                let mut s = self.state.lock().unwrap();
                let now = Instant::now();
                s.0 = (s.0 + now.duration_since(s.1).as_secs_f64() * self.per_sec).min(self.capacity);
                s.1 = now;
                if s.0 >= 1.0 {
                    s.0 -= 1.0;
                    return;
                }
                Duration::from_secs_f64((1.0 - s.0) / self.per_sec)
            };
            tokio::time::sleep(wait).await;
        }
    }
}

pub struct Rpc {
    http: reqwest::Client,
    url: String,
    basic: Option<(String, String)>,
    timeout: Duration,
    max_retries: u32,
    sem: Semaphore,
    limiter: Option<RateLimiter>,
    paused_until: Mutex<Option<Instant>>,
    last_rate: Mutex<Option<RateLimitInfo>>,
    next_id: AtomicU64,
}

/// Longest we will ever wait because the provider asked us to.
const MAX_PROVIDER_PAUSE: Duration = Duration::from_secs(65);

impl Rpc {
    pub fn new(s: RpcSettings) -> Result<Rpc, String> {
        let mut headers = HeaderMap::new();
        let mut basic = None;
        match s.auth {
            Auth::Basic { user, pass } => basic = Some((user, pass)),
            Auth::Header { name, mut value } => {
                value.set_sensitive(true);
                headers.insert(name, value);
            }
        }
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .timeout(s.timeout)
            .pool_max_idle_per_host(s.max_concurrency)
            .user_agent(concat!("block-explorer/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("cannot build HTTP client: {e}"))?;
        Ok(Rpc {
            http,
            url: s.url,
            basic,
            timeout: s.timeout,
            max_retries: s.max_retries,
            sem: Semaphore::new(s.max_concurrency.max(1)),
            limiter: (s.rate_per_min > 0).then(|| RateLimiter::new(s.rate_per_min)),
            paused_until: Mutex::new(None),
            last_rate: Mutex::new(None),
            next_id: AtomicU64::new(1),
        })
    }

    /// Endpoint URL with any userinfo/query stripped, safe to log.
    pub fn safe_url(&self) -> String {
        match reqwest::Url::parse(&self.url) {
            Ok(mut u) => {
                let _ = u.set_username("");
                let _ = u.set_password(None);
                u.set_query(None);
                u.to_string()
            }
            Err(_) => "<invalid url>".into(),
        }
    }

    pub fn rate_limit(&self) -> Option<RateLimitInfo> {
        *self.last_rate.lock().unwrap()
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: &[Value]) -> Result<T, RpcError> {
        self.call_timeout(method, params, self.timeout).await
    }

    pub async fn call_timeout<T: DeserializeOwned>(
        &self,
        method: &str,
        params: &[Value],
        timeout: Duration,
    ) -> Result<T, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({ "jsonrpc": "1.0", "id": id, "method": method, "params": params });
        let v = self.send(method, &body, timeout).await?;
        decode_result(v)
    }

    /// JSON-RPC batch: one HTTP request (one rate-limit token) for many calls.
    /// Results come back in the order of `calls`.
    pub async fn batch<T: DeserializeOwned>(
        &self,
        calls: &[(&str, Vec<Value>)],
        timeout: Duration,
    ) -> Result<Vec<Result<T, RpcError>>, RpcError> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        let body: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (m, p))| json!({ "jsonrpc": "1.0", "id": i, "method": m, "params": p }))
            .collect();
        let v = self.send("batch", &Value::Array(body), timeout).await?;
        let Value::Array(items) = v else {
            return Err(RpcError::Decode("batch response is not an array".into()));
        };
        let mut out: Vec<Option<Result<T, RpcError>>> = (0..calls.len()).map(|_| None).collect();
        for item in items {
            let Some(i) = item["id"].as_u64().map(|i| i as usize).filter(|i| *i < calls.len()) else {
                continue;
            };
            out[i] = Some(decode_result(item));
        }
        Ok(out
            .into_iter()
            .map(|r| r.unwrap_or_else(|| Err(RpcError::Decode("missing batch item".into()))))
            .collect())
    }

    async fn send(&self, method: &str, body: &Value, timeout: Duration) -> Result<Value, RpcError> {
        let bytes = serde_json::to_vec(body).map_err(|e| RpcError::Decode(e.to_string()))?;
        let mut attempt = 0;
        loop {
            self.wait_for_pause().await;
            if let Some(l) = &self.limiter {
                l.acquire().await;
            }
            let res = {
                let _permit = self.sem.acquire().await.expect("semaphore closed");
                self.send_once(&bytes, timeout).await
            };
            match res {
                Ok(v) => return Ok(v),
                Err((e, hint)) if e.is_transient() && attempt < self.max_retries => {
                    let backoff = Duration::from_millis(250 * 2u64.pow(attempt) + jitter_ms());
                    let delay = hint.unwrap_or(backoff).min(MAX_PROVIDER_PAUSE);
                    tracing::warn!(method, attempt = attempt + 1, delay_ms = delay.as_millis() as u64, error = %e, "transient RPC failure, retrying");
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err((e, _)) => return Err(e),
            }
        }
    }

    async fn wait_for_pause(&self) {
        let until = *self.paused_until.lock().unwrap();
        if let Some(t) = until {
            let now = Instant::now();
            if t > now {
                tokio::time::sleep(t - now).await;
            }
        }
    }

    /// One HTTP round trip. On error, also returns how long the server asked us to wait.
    async fn send_once(&self, body: &[u8], timeout: Duration) -> Result<Value, (RpcError, Option<Duration>)> {
        let mut req = self
            .http
            .post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .timeout(timeout)
            .body(body.to_vec());
        if let Some((u, p)) = &self.basic {
            req = req.basic_auth(u, Some(p));
        }
        // `without_url()` keeps any credentials in the URL out of error messages and logs.
        let resp = req.send().await.map_err(|e| (map_reqwest(e), None))?;
        let status = resp.status();
        let wait = self.record_rate_limit(resp.headers(), status.as_u16());
        let text = resp.bytes().await.map_err(|e| (map_reqwest(e), None))?;

        // Bitcoin Core replies to RPC errors with HTTP 500/404 *and* a JSON-RPC body,
        // so look at the body before the status code.
        if let Ok(v) = serde_json::from_slice::<Value>(&text) {
            if is_jsonrpc(&v) {
                if let Some(code) = v["error"]["code"].as_i64().filter(|c| *c == -28) {
                    let message = v["error"]["message"].as_str().unwrap_or("").to_string();
                    return Err((RpcError::Rpc { code, message }, None));
                }
                return Ok(v);
            }
            if !status.is_success() {
                let message = v["message"].as_str().or(v["error"].as_str()).unwrap_or("").to_string();
                return Err((RpcError::Http { status: status.as_u16(), message }, wait));
            }
            return Err((RpcError::Decode("response is not JSON-RPC".into()), None));
        }
        if !status.is_success() {
            let message = status.canonical_reason().unwrap_or("").to_string();
            return Err((RpcError::Http { status: status.as_u16(), message }, wait));
        }
        Err((RpcError::Decode("response body is not JSON".into()), None))
    }

    /// Remember provider rate-limit headers. When the budget is exhausted (or we got
    /// a 429), pause all callers until the window resets; returns that wait.
    fn record_rate_limit(&self, h: &HeaderMap, status: u16) -> Option<Duration> {
        let num = |name: &str| h.get(name)?.to_str().ok()?.trim().parse::<u64>().ok();
        let info = match (num("x-ratelimit-limit"), num("x-ratelimit-remaining"), num("x-ratelimit-reset")) {
            (Some(limit), Some(remaining), Some(reset_secs)) => {
                let i = RateLimitInfo { limit, remaining, reset_secs };
                *self.last_rate.lock().unwrap() = Some(i);
                Some(i)
            }
            _ => None,
        };
        let retry_after = h.get(RETRY_AFTER).and_then(|v| v.to_str().ok()?.trim().parse::<u64>().ok());
        let wait = match (status, info, retry_after) {
            (_, _, Some(s)) if status == 429 || status == 503 => Some(s),
            (429, Some(i), _) => Some(i.reset_secs.max(1)),
            (_, Some(i), _) if i.remaining == 0 => Some(i.reset_secs.max(1)),
            _ => None,
        }
        .map(|s| Duration::from_secs(s).min(MAX_PROVIDER_PAUSE));
        if let Some(w) = wait {
            tracing::warn!(wait_secs = w.as_secs(), status, "provider rate limit reached, pausing RPC calls");
            *self.paused_until.lock().unwrap() = Some(Instant::now() + w);
        }
        wait
    }
}

fn map_reqwest(e: reqwest::Error) -> RpcError {
    if e.is_timeout() {
        RpcError::Timeout
    } else {
        RpcError::Transport(e.without_url().to_string())
    }
}

fn is_jsonrpc(v: &Value) -> bool {
    v.is_array() || v.get("result").is_some() || v["error"].get("code").is_some()
}

fn decode_result<T: DeserializeOwned>(mut v: Value) -> Result<T, RpcError> {
    let err = &v["error"];
    if !err.is_null() {
        return Err(RpcError::Rpc {
            code: err["code"].as_i64().unwrap_or(0),
            message: err["message"].as_str().unwrap_or("").to_string(),
        });
    }
    serde_json::from_value(v["result"].take()).map_err(|e| RpcError::Decode(e.to_string()))
}

fn jitter_ms() -> u64 {
    // Cheap jitter without pulling in a RNG crate.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos as u64 % 200
}
