use std::{env, time::Duration};

use bitcoin::Network;
use bitcoincore_rpc::{
    jsonrpc::{self, simple_http::SimpleHttpTransport},
    Client,
};
use reqwest::header::{HeaderName, HeaderValue};

use crate::rpc::{Auth, RpcSettings};

type BoxError = Box<dyn std::error::Error>;

/// Everything read from environment variables (and `.env`) at startup.
pub struct Config {
    pub rpc: RpcSettings,
    pub network: Network,
    pub bind: String,
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

        let bind = env::var("BIND").unwrap_or_else(|_| "127.0.0.1:3000".into());
        Ok(Config { rpc, network, bind })
    }

    /// Blocking bitcoincore-rpc client used by the handlers (plain HTTP + Basic auth only).
    pub fn legacy_client(&self) -> Result<Client, BoxError> {
        let Auth::Basic { user, pass } = &self.rpc.auth else {
            return Err("the HTTP handlers do not support API-key auth yet; only --check does".into());
        };
        // Long timeout: scantxoutset can take minutes on mainnet.
        let transport = SimpleHttpTransport::builder()
            .url(&self.rpc.url)?
            .auth(user.clone(), Some(pass.clone()))
            .timeout(Duration::from_secs(600))
            .build();
        Ok(Client::from_jsonrpc(jsonrpc::Client::with_transport(transport)))
    }
}
