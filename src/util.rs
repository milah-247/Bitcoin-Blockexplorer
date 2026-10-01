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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn is_hash_accepts_64_hex_only() {
        assert!(is_hash(&"a".repeat(64)));
        assert!(is_hash(&"AbCdEf0123456789".repeat(4)));
        assert!(!is_hash(&"a".repeat(63)));
        assert!(!is_hash(&"a".repeat(65)));
        assert!(!is_hash(&format!("{}g", "a".repeat(63))));
        assert!(!is_hash(""));
    }

    #[test]
    fn is_digits_rejects_empty_and_signs() {
        assert!(is_digits("0"));
        assert!(is_digits("840000"));
        assert!(!is_digits(""));
        assert!(!is_digits("-1"));
        assert!(!is_digits("1e3"));
        assert!(!is_digits(" 1"));
    }

    #[test]
    fn parse_block_id_height_hash_and_errors() {
        assert_eq!(parse_block_id("0").unwrap(), BlockId::Height(0));
        assert_eq!(parse_block_id("840000").unwrap(), BlockId::Height(840_000));
        let h = "00000000000000000000BD922BB8ADF647E2EBCF605A15DB4705D144A45DFDF8";
        assert_eq!(parse_block_id(h).unwrap(), BlockId::Hash(h.to_ascii_lowercase()));
        // 64 digits is a hash, not a (huge) height
        let digits = "1".repeat(64);
        assert_eq!(parse_block_id(&digits).unwrap(), BlockId::Hash(digits.clone()));
        assert!(matches!(parse_block_id("99999999999999999999999"), Err(AppError::InvalidInput(_))));
        assert!(matches!(parse_block_id("hello"), Err(AppError::InvalidInput(_))));
        assert!(matches!(parse_block_id(""), Err(AppError::InvalidInput(_))));
    }

    #[test]
    fn parse_hash_lowercases() {
        assert_eq!(parse_hash(&"AB".repeat(32), "txid").unwrap(), "ab".repeat(32));
        assert!(parse_hash("abc", "txid").is_err());
    }

    #[test]
    fn sats_rounds_btc_floats() {
        assert_eq!(sats(&json!(1)), 100_000_000);
        assert_eq!(sats(&json!(0.1)), 10_000_000);
        // 0.29 * 1e8 = 28999999.999999996 in f64
        assert_eq!(sats(&json!(0.29)), 29_000_000);
        assert_eq!(sats(&json!(0.00000001)), 1);
        assert_eq!(sats(&json!(20999999.9769)), 2_099_999_997_690_000);
        assert_eq!(sats(&json!(null)), 0);
        assert_eq!(sats(&json!("1")), 0);
    }

    #[test]
    fn subsidy_halves() {
        assert_eq!(block_subsidy(0, Network::Bitcoin), 5_000_000_000);
        assert_eq!(block_subsidy(209_999, Network::Bitcoin), 5_000_000_000);
        assert_eq!(block_subsidy(210_000, Network::Bitcoin), 2_500_000_000);
        assert_eq!(block_subsidy(840_000, Network::Bitcoin), 312_500_000);
        assert_eq!(block_subsidy(64 * 210_000, Network::Bitcoin), 0);
        assert_eq!(block_subsidy(150, Network::Regtest), 2_500_000_000);
    }

    #[test]
    fn parse_address_checks_network_and_length() {
        assert!(parse_address("bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh", Network::Bitcoin).is_ok());
        assert!(parse_address("1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa", Network::Bitcoin).is_ok());
        assert!(parse_address("bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh", Network::Regtest).is_err());
        assert!(parse_address("notanaddress", Network::Bitcoin).is_err());
        assert!(parse_address(&"1".repeat(200), Network::Bitcoin).is_err());
    }
}
