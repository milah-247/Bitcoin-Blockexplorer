use std::str::FromStr;

use bitcoin::{Address, Network};
use serde_json::Value;

use crate::error::AppError;

/// Longest input we accept anywhere (addresses are at most 90 chars, hashes 64).
pub const MAX_INPUT_LEN: usize = 128;

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

/// New coins created by a block at `height` (the coinbase may claim this plus fees).
pub fn block_subsidy(height: u64, network: Network) -> u64 {
    let interval = if network == Network::Regtest { 150 } else { 210_000 };
    let halvings = height / interval;
    if halvings >= 64 {
        0
    } else {
        (50 * 100_000_000u64) >> halvings
    }
}

#[derive(Debug, Clone, PartialEq)]
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

/// Validate a txid / hash path segment and normalise it to lowercase.
pub fn parse_hash(s: &str, what: &str) -> Result<String, AppError> {
    if is_hash(s) {
        Ok(s.to_ascii_lowercase())
    } else {
        Err(AppError::InvalidInput(format!("{what} must be 64 hex characters")))
    }
}

pub fn parse_address(s: &str, net: Network) -> Result<Address, AppError> {
    if s.len() > MAX_INPUT_LEN {
        return Err(AppError::InvalidInput("not a valid Bitcoin address".into()));
    }
    Address::from_str(s)
        .map_err(|_| AppError::InvalidInput("not a valid Bitcoin address".into()))?
        .require_network(net)
        .map_err(|_| AppError::InvalidInput(format!("address is not valid for network {net}")))
}
