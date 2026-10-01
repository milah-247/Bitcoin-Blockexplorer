use std::str::FromStr;

use bitcoin::{Address, Network};
use bitcoincore_rpc::{Client, RpcApi};
use serde_json::{json, Value};

use crate::error::AppError;
use crate::state::RpcResult;

pub fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// BTC (float from JSON) -> satoshis
pub fn sats(v: &Value) -> u64 {
    (v.as_f64().unwrap_or(0.0) * 1e8).round() as u64
}

pub enum BlockId {
    Height(u64),
    Hash(String),
}

pub fn parse_block_id(s: &str) -> Result<BlockId, AppError> {
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
pub fn resolve_block(c: &Client, id: &BlockId) -> RpcResult<Option<String>> {
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

pub fn parse_address(s: &str, net: Network) -> Result<Address, AppError> {
    Address::from_str(s)
        .map_err(|_| AppError::InvalidInput("not a valid Bitcoin address".into()))?
        .require_network(net)
        .map_err(|_| AppError::InvalidInput(format!("address is not valid for network {net}")))
}