use std::{env, time::Duration};

use bitcoin::Network;
use reqwest::header::{HeaderName, HeaderValue};

use crate::cache::CacheSettings;
use crate::indexer::{IndexerSettings, StartHeight};
use crate::router::HttpSettings;
use crate::rpc::{Auth, RpcSettings};

type BoxError = Box<dyn std::error::Error>;

/// Everything read from environment variables (and `.env`) at startup.
pub struct Config {
    pub rpc: RpcSettings,
    pub network: Network,
    pub bind: String,
    pub http: HttpSettings,
    pub cache: CacheSettings,
    /// None when INDEX_ENABLED=false
    pub index: Option<IndexerSettings>,
}

/// Load `.env` (or the file named by `ENV_FILE`; `ENV_FILE=none` skips it).
/// Variables already set in the environment win over the file.
pub fn load_dotenv() {
    match env::var("ENV_FILE").as_deref() {
        Ok("none") => {}
        Ok(path) => {
            if let Err(e) = dotenvy::from_filename(path) {
                eprintln!("warning: cannot read ENV_FILE {path}: {e}");
            }
        }
        Err(_) => {
            let _ = dotenvy::dotenv();
        }
    }
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> Result<T, BoxError> {
    match env::var(name) {
        Ok(v) if !v.trim().is_empty() => v.trim().parse().map_err(|_| format!("invalid {name}: `{v}`").into()),
        _ => Ok(default),
    }
}

impl Config {
    pub fn from_env() -> Result<Config, BoxError> {
        let (network, default_port) =
            match env::var("NETWORK").unwrap_or_else(|_| "regtest".into()).as_str() {
                "mainnet" | "bitcoin" | "main" => (Network::Bitcoin, 8332),
                "testnet" => (Network::Testnet, 18332),
                "signet" => (Network::Signet, 38332),
                "regtest" => (Network::Regtest, 18443),
                other => return Err(format!("unknown NETWORK `{other}`").into()),
            };
        let url = env::var("RPC_URL").unwrap_or_else(|_| format!("http://127.0.0.1:{default_port}"));

        let auth = if let Ok(key) = env::var("RPC_API_KEY") {
            let header = env::var("RPC_API_KEY_HEADER").unwrap_or_else(|_| "X-API-Key".into());
            Auth::Header {
                name: HeaderName::try_from(header.as_str()).map_err(|_| "invalid RPC_API_KEY_HEADER")?,
                value: HeaderValue::try_from(key.trim()).map_err(|_| "invalid RPC_API_KEY")?,
            }
        } else if let Ok(path) = env::var("RPC_COOKIE") {
            let s = std::fs::read_to_string(path)?;
            let (u, p) = s.trim().split_once(':').ok_or("bad cookie file")?;
            Auth::Basic { user: u.to_string(), pass: p.to_string() }
        } else {
            const MSG: &str = "set RPC_API_KEY, RPC_USER/RPC_PASS or RPC_COOKIE";
            Auth::Basic {
                user: env::var("RPC_USER").map_err(|_| MSG)?,
                pass: env::var("RPC_PASS").map_err(|_| MSG)?,
            }
        };

        let rpc = RpcSettings {
            url,
            auth,
            timeout: Duration::from_secs(env_or("RPC_TIMEOUT_SECS", 30u64)?),
            max_concurrency: env_or("RPC_MAX_CONCURRENCY", 4usize)?,
            max_retries: env_or("RPC_MAX_RETRIES", 3u32)?,
            rate_per_min: env_or("RPC_RATE_LIMIT_PER_MIN", 0u32)?,
        };

        let http = HttpSettings {
            request_timeout: Duration::from_secs(env_or("REQUEST_TIMEOUT_SECS", 120u64)?),
            frontend_dir: env::var("FRONTEND_DIR").unwrap_or_else(|_| "frontend".into()),
            cors_origins: env::var("CORS_ORIGINS").ok().and_then(|v| {
                let list: Vec<String> =
                    v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                (!list.is_empty()).then_some(list)
            }),
        };
        let cache = CacheSettings {
            max_bytes: env_or("CACHE_MAX_MB", 256u64)? * 1024 * 1024,
            long: Duration::from_secs(env_or("CACHE_TTL_LONG_SECS", 86_400u64)?),
            short: Duration::from_secs(env_or("CACHE_TTL_SHORT_SECS", 15u64)?),
            tip: Duration::from_secs(env_or("CACHE_TTL_TIP_SECS", 5u64)?),
        };

        let index = if env_or("INDEX_ENABLED", true)? {
            let default_start = if network == Network::Regtest { "0" } else { "tip-144" };
            let start = env::var("START_HEIGHT").unwrap_or_else(|_| default_start.into());
            Some(IndexerSettings {
                path: env::var("INDEX_DB_PATH")
                    .unwrap_or_else(|_| format!("data/index-{network}.sqlite"))
                    .into(),
                start: parse_start(&start)?,
                concurrency: env_or("INDEX_CONCURRENCY", 2usize)?.clamp(1, 8),
                poll: Duration::from_secs(env_or("INDEX_POLL_SECS", 15u64)?),
                max_reorg: env_or("INDEX_MAX_REORG", 100u64)?,
            })
        } else {
            None
        };

        let bind = env::var("BIND").unwrap_or_else(|_| "127.0.0.1:3000".into());
        Ok(Config { rpc, network, bind, http, cache, index })
    }
}

/// `START_HEIGHT=840000` or `START_HEIGHT=tip-1000`.
pub fn parse_start(s: &str) -> Result<StartHeight, BoxError> {
    let s = s.trim();
    let bad = || format!("invalid START_HEIGHT `{s}` (use a height or tip-N)");
    match s.strip_prefix("tip-") {
        Some(n) => n.parse().map(StartHeight::FromTip).map_err(|_| bad().into()),
        None => s.parse().map(StartHeight::Fixed).map_err(|_| bad().into()),
    }
}
