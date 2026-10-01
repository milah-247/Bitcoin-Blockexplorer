use std::{env, time::Duration};

use bitcoin::Network;
use bitcoincore_rpc::{
    jsonrpc::{self, simple_http::SimpleHttpTransport},
    Client,
};

/// Everything read from environment variables at startup.
pub struct Config {
    pub client: Client,
    pub network: Network,
    pub bind: String,
}

impl Config {
    pub fn from_env() -> Result<Config, Box<dyn std::error::Error>> {
        let (network, default_port) =
            match env::var("NETWORK").unwrap_or_else(|_| "regtest".into()).as_str() {
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

        let bind = env::var("BIND").unwrap_or_else(|_| "127.0.0.1:3000".into());
        Ok(Config { client, network, bind })
    }
}