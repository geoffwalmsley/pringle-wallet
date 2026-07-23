//! A thin async wrapper around the coinset.org mainnet RPC.
//!
//! Only the handful of endpoints this CLI needs are exposed: discovering wallet coins,
//! looking up a specific coin, and pushing a signed spend bundle. Every call checks the
//! `success` flag and surfaces the server-provided error rather than silently proceeding.

use std::collections::HashMap;

use anyhow::{bail, Result};
use chia_wallet_sdk::coinset::{ChiaRpcClient, CoinRecord, CoinsetClient, PushTxResponse};
use chia_wallet_sdk::prelude::{Bytes32, Coin, CoinSpend, SpendBundle};

use crate::chain::ChainStatus;

/// Returns true if a coin record represents a confirmed, currently-unspent coin.
pub fn record_is_unspent(record: &CoinRecord) -> bool {
    !record.spent && record.confirmed_block_index > 0
}

/// Filters coin records down to the confirmed, unspent coins.
pub fn filter_unspent(records: Vec<CoinRecord>) -> Vec<Coin> {
    records
        .into_iter()
        .filter(record_is_unspent)
        .map(|record| record.coin)
        .collect()
}

/// Interprets a `push_tx` response, returning an error if the bundle was rejected.
pub fn interpret_push(response: PushTxResponse) -> Result<()> {
    if !response.success {
        bail!(
            "coinset push_tx rejected the spend bundle (status: {:?}): {}",
            response.status,
            response
                .error
                .unwrap_or_else(|| "unknown error".to_string())
        );
    }
    Ok(())
}

/// Mainnet coinset client wrapper.
#[derive(Debug, Clone)]
pub struct Coinset {
    client: CoinsetClient,
}

impl Default for Coinset {
    fn default() -> Self {
        Self::mainnet()
    }
}

impl Coinset {
    /// Creates a client pointed at the mainnet coinset endpoint.
    pub fn mainnet() -> Self {
        Self {
            client: CoinsetClient::mainnet(),
        }
    }

    /// Returns all coin records (spent and unspent) for a puzzle hash.
    pub async fn coin_records_by_puzzle_hash(
        &self,
        puzzle_hash: Bytes32,
    ) -> Result<Vec<CoinRecord>> {
        let response = self
            .client
            .get_coin_records_by_puzzle_hash(puzzle_hash, None, None, Some(true), None)
            .await?;
        if !response.success {
            bail!(
                "coinset get_coin_records_by_puzzle_hash failed: {}",
                response
                    .error
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }
        Ok(response.coin_records.unwrap_or_default())
    }

    /// Returns confirmed, currently-unspent coins for a puzzle hash.
    pub async fn unspent_coins(&self, puzzle_hash: Bytes32) -> Result<Vec<Coin>> {
        Ok(filter_unspent(
            self.coin_records_by_puzzle_hash(puzzle_hash).await?,
        ))
    }

    /// Looks up a single coin record by its coin id.
    pub async fn coin_record(&self, coin_id: Bytes32) -> Result<Option<CoinRecord>> {
        let response = self.client.get_coin_record_by_name(coin_id).await?;
        if !response.success {
            bail!(
                "coinset get_coin_record_by_name failed: {}",
                response
                    .error
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }
        Ok(response.coin_record)
    }

    /// Returns true if the coin exists on-chain and has not been spent.
    pub async fn is_unspent(&self, coin_id: Bytes32) -> Result<bool> {
        Ok(self
            .coin_record(coin_id)
            .await?
            .map(|record| record_is_unspent(&record))
            .unwrap_or(false))
    }

    /// Classifies a coin's on-chain status, keeping RPC failures explicit (never conflated
    /// with "spent"). Use this instead of [`Coinset::is_unspent`] when a lookup failure must
    /// be distinguished from a genuine chain fact.
    pub async fn classify(&self, coin_id: Bytes32) -> ChainStatus {
        ChainStatus::from_lookup(self.coin_record(coin_id).await.map_err(|e| e.to_string()))
    }

    /// Batch-classifies many coins in a single RPC round-trip. Coins missing from the
    /// response are reported as [`ChainStatus::NotFound`]; a failed RPC reports every
    /// requested coin as [`ChainStatus::LookupFailed`].
    pub async fn classify_many(&self, coin_ids: &[Bytes32]) -> HashMap<Bytes32, ChainStatus> {
        let mut out = HashMap::new();
        if coin_ids.is_empty() {
            return out;
        }
        match self
            .client
            .get_coin_records_by_names(coin_ids.to_vec(), None, None, Some(true), None)
            .await
        {
            Ok(response) if response.success => {
                let records = response.coin_records.unwrap_or_default();
                let by_id: HashMap<Bytes32, CoinRecord> =
                    records.into_iter().map(|r| (r.coin.coin_id(), r)).collect();
                for &id in coin_ids {
                    let status = match by_id.get(&id) {
                        Some(rec) => ChainStatus::from_record(rec),
                        None => ChainStatus::NotFound,
                    };
                    out.insert(id, status);
                }
            }
            Ok(response) => {
                let error = response
                    .error
                    .unwrap_or_else(|| "unknown error".to_string());
                for &id in coin_ids {
                    out.insert(
                        id,
                        ChainStatus::LookupFailed {
                            error: error.clone(),
                        },
                    );
                }
            }
            Err(err) => {
                let error = err.to_string();
                for &id in coin_ids {
                    out.insert(
                        id,
                        ChainStatus::LookupFailed {
                            error: error.clone(),
                        },
                    );
                }
            }
        }
        out
    }

    /// Fetches the spend (puzzle reveal + solution) of an already-spent coin.
    ///
    /// Returns `None` if the coin does not exist or has not been spent yet.
    pub async fn coin_spend(&self, coin_id: Bytes32) -> Result<Option<CoinSpend>> {
        let Some(record) = self.coin_record(coin_id).await? else {
            return Ok(None);
        };
        if !record.spent {
            return Ok(None);
        }
        let response = self
            .client
            .get_puzzle_and_solution(coin_id, Some(record.spent_block_index))
            .await?;
        if !response.success {
            bail!(
                "coinset get_puzzle_and_solution failed: {}",
                response
                    .error
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }
        Ok(response.coin_solution)
    }

    /// Pushes a signed spend bundle to the mempool, returning an error on rejection.
    pub async fn push_tx(&self, spend_bundle: SpendBundle) -> Result<()> {
        interpret_push(self.client.push_tx(spend_bundle).await?)
    }
}
