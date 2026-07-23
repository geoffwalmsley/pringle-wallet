//! Explicit on-chain status classification.
//!
//! The CLI must never confuse "the RPC call failed" with "the coin is spent": doing so
//! would silently corrupt local state during a sync. This module defines an explicit
//! [`ChainStatus`] enum that keeps lookup failures distinct from genuine chain facts, so
//! every caller has to decide what to do about a failure rather than defaulting to
//! "gone".

use chia_wallet_sdk::coinset::CoinRecord;

/// The classified on-chain status of a single coin.
#[derive(Debug, Clone)]
pub enum ChainStatus {
    /// The coin is confirmed on-chain and currently unspent.
    ConfirmedUnspent { confirmed_height: u32 },
    /// The coin was confirmed and has since been spent.
    Spent { spent_height: u32 },
    /// The coin was never seen on-chain (still pending in the mempool, or never existed).
    NotFound,
    /// The lookup itself failed (network/RPC error). This is NOT a statement about the
    /// coin; callers must treat it as "unknown", never as "spent".
    LookupFailed { error: String },
}

impl ChainStatus {
    /// Classifies a coin-record lookup result into an explicit status.
    ///
    /// `record` is the coin record if the RPC succeeded (`None` meaning not found), or the
    /// error string if the RPC itself failed.
    pub fn from_lookup(record: Result<Option<CoinRecord>, String>) -> Self {
        match record {
            Ok(None) => ChainStatus::NotFound,
            Ok(Some(rec)) => Self::from_record(&rec),
            Err(error) => ChainStatus::LookupFailed { error },
        }
    }

    /// Classifies a present coin record.
    pub fn from_record(record: &CoinRecord) -> Self {
        if record.spent {
            ChainStatus::Spent {
                spent_height: record.spent_block_index,
            }
        } else if record.confirmed_block_index > 0 {
            ChainStatus::ConfirmedUnspent {
                confirmed_height: record.confirmed_block_index,
            }
        } else {
            // Present but not yet in a block (shouldn't normally happen for a returned
            // record, but be explicit rather than guessing).
            ChainStatus::NotFound
        }
    }

    /// True only when the coin is confirmed and unspent.
    pub fn is_confirmed_unspent(&self) -> bool {
        matches!(self, ChainStatus::ConfirmedUnspent { .. })
    }

    /// True when the coin was definitively observed as spent.
    pub fn is_spent(&self) -> bool {
        matches!(self, ChainStatus::Spent { .. })
    }

    /// True when the lookup failed (status is unknown, not a chain fact).
    pub fn is_lookup_failure(&self) -> bool {
        matches!(self, ChainStatus::LookupFailed { .. })
    }

    /// A short human label for the status.
    pub fn label(&self) -> String {
        match self {
            ChainStatus::ConfirmedUnspent { .. } => "confirmed & unspent".to_string(),
            ChainStatus::Spent { spent_height } => format!("spent at block {spent_height}"),
            ChainStatus::NotFound => "not found (pending or never seen)".to_string(),
            ChainStatus::LookupFailed { error } => format!("lookup failed: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chia_wallet_sdk::coinset::CoinRecord;
    use chia_wallet_sdk::prelude::{Bytes32, Coin};

    fn record(spent: bool, confirmed: u32, spent_block: u32) -> CoinRecord {
        CoinRecord {
            coin: Coin::new(Bytes32::new([1u8; 32]), Bytes32::new([2u8; 32]), 1),
            coinbase: false,
            confirmed_block_index: confirmed,
            spent,
            spent_block_index: spent_block,
            timestamp: 0,
        }
    }

    #[test]
    fn classifies_confirmed_unspent() {
        let s = ChainStatus::from_lookup(Ok(Some(record(false, 100, 0))));
        assert!(s.is_confirmed_unspent());
        assert!(!s.is_spent());
    }

    #[test]
    fn classifies_spent() {
        let s = ChainStatus::from_lookup(Ok(Some(record(true, 100, 150))));
        assert!(s.is_spent());
    }

    #[test]
    fn classifies_not_found() {
        let s = ChainStatus::from_lookup(Ok(None));
        assert!(matches!(s, ChainStatus::NotFound));
    }

    #[test]
    fn lookup_failure_is_not_spent() {
        let s = ChainStatus::from_lookup(Err("boom".to_string()));
        assert!(s.is_lookup_failure());
        assert!(!s.is_spent());
        assert!(!s.is_confirmed_unspent());
    }
}
