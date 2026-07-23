//! Shared chain reconciliation.
//!
//! This is the single engine used by both `pringle sync` (explicit repair) and
//! `pringle status` (refresh-by-default). It follows tracked singletons forward to their
//! live tips, refreshes p2_singleton balances, and prunes settled transactions.
//!
//! It is deliberately fault-tolerant: an RPC failure for one asset is recorded as a
//! warning and never deletes or downgrades local state, and a missing (pending) watch coin
//! keeps its transaction in the log rather than pretending it settled.

use std::collections::HashSet;

use anyhow::{bail, Result};
use chia_wallet_sdk::prelude::{Bytes32, Coin, CoinSpend};

use crate::chain::ChainStatus;
use crate::coinset::Coinset;
use crate::nft;
use crate::option as option_contract;
use crate::p2_singleton;
use crate::state::{from_hex, to_hex, CoinJson, Phase, ProofJson, State};

/// The result of following a tracked singleton forward on-chain.
pub enum SingletonSync<T> {
    /// The tracked coin is not yet visible on-chain (still pending / in mempool).
    Unconfirmed,
    /// The tracked coin is still the live, unspent tip.
    StillLive,
    /// The singleton advanced to a new live tip, reconstructed here (coin + proof + info).
    Advanced(T),
    /// The singleton was spent without producing a singleton child (melted / exercised).
    Gone,
}

/// Follows a singleton from its tracked coin to its current live tip.
///
/// At each hop the tip is reconstructed via `parse_child` (which walks the transfer
/// program, so the returned value carries the correct proof, owner, and inner puzzle).
pub async fn advance_singleton<T>(
    coinset: &Coinset,
    tracked: Coin,
    parse_child: impl Fn(&CoinSpend) -> Result<Option<T>>,
    child_coin: impl Fn(&T) -> Coin,
) -> Result<SingletonSync<T>> {
    let tracked_id = tracked.coin_id();
    let Some(record) = coinset.coin_record(tracked_id).await? else {
        return Ok(SingletonSync::Unconfirmed);
    };
    if !record.spent {
        return Ok(SingletonSync::StillLive);
    }

    // The tracked coin is spent; walk its children until we reach an unspent tip (or a
    // spend that produced no singleton child, meaning the singleton was melted).
    let mut current_id = tracked_id;
    let mut latest: Option<T> = None;
    let mut guard = 0;
    loop {
        guard += 1;
        if guard > 1000 {
            bail!(
                "singleton {} has an unexpectedly long spend chain; aborting sync",
                to_hex(tracked_id)
            );
        }
        // `coin_spend` returns `None` when the coin is unspent (the tip) or absent.
        let Some(spend) = coinset.coin_spend(current_id).await? else {
            break;
        };
        let Some(child) = parse_child(&spend)? else {
            return Ok(SingletonSync::Gone);
        };
        current_id = child_coin(&child).coin_id();
        latest = Some(child);
    }

    Ok(match latest {
        Some(tip) => SingletonSync::Advanced(tip),
        None => SingletonSync::Gone,
    })
}

/// The outcome of a reconciliation pass.
#[derive(Debug, Default)]
pub struct ReconcileReport {
    /// Whether local state was modified.
    pub changed: bool,
    /// Informational messages (per asset), suitable for stderr progress.
    pub messages: Vec<String>,
    /// Non-fatal warnings (e.g. RPC failures that were skipped, not treated as spent).
    pub warnings: Vec<String>,
}

impl ReconcileReport {
    fn msg(&mut self, m: impl Into<String>) {
        self.messages.push(m.into());
    }
    fn warn(&mut self, m: impl Into<String>) {
        self.warnings.push(m.into());
    }
}

/// Reconciles the entire multi-asset state against the chain.
///
/// Mutates `state` in place; the caller is responsible for saving. Never returns an error
/// for a single asset's RPC failure — those become warnings so other assets still sync.
pub async fn reconcile(coinset: &Coinset, state: &mut State) -> Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let wallet_ph = from_hex(&state.wallet_puzzle_hash).ok();

    reconcile_nfts(coinset, state, wallet_ph, &mut report).await;
    reconcile_options(coinset, state, wallet_ph, &mut report).await;
    backfill_nft_p2_singletons(state, &mut report);
    reconcile_p2_singletons(coinset, state, &mut report).await;
    prune_transactions(coinset, state, &mut report).await;

    Ok(report)
}

/// Adds missing p2-singleton tracking for every tracked NFT.
///
/// A p2-singleton address exists deterministically for every NFT launcher, regardless of
/// whether this CLI funded it. Balance reconciliation runs immediately afterward, so funds
/// sent externally appear in the same status/sync invocation.
fn backfill_nft_p2_singletons(state: &mut State, report: &mut ReconcileReport) {
    let launcher_ids: Vec<String> = state
        .nfts
        .iter()
        .map(|nft| nft.launcher_id.clone())
        .filter(|launcher| state.p2_by_launcher(launcher).is_none())
        .collect();

    for launcher in launcher_ids {
        let launcher_id = match from_hex(&launcher) {
            Ok(id) => id,
            Err(error) => {
                report.warn(format!(
                    "p2 singleton for NFT {launcher}: invalid launcher id ({error})."
                ));
                continue;
            }
        };
        match p2_singleton::tracking_record(launcher_id, Vec::new(), Phase::Confirmed) {
            Ok(record) => {
                state.upsert_p2_singleton(record);
                report.changed = true;
                report.msg(format!(
                    "p2 singleton for NFT {launcher}: added to tracking."
                ));
            }
            Err(error) => report.warn(format!(
                "p2 singleton for NFT {launcher}: could not derive address ({error})."
            )),
        }
    }
}

async fn reconcile_nfts(
    coinset: &Coinset,
    state: &mut State,
    wallet_ph: Option<Bytes32>,
    report: &mut ReconcileReport,
) {
    let nfts = state.nfts.clone();
    for nft in nfts {
        let tracked = match nft.coin.to_coin() {
            Ok(c) => c,
            Err(e) => {
                report.warn(format!(
                    "NFT {}: unreadable coin ({e}); skipped.",
                    nft.launcher_id
                ));
                continue;
            }
        };
        let sync = advance_singleton(coinset, tracked, nft::nft_child_from_spend, |n| n.coin).await;
        match sync {
            Err(e) => report.warn(format!(
                "NFT {}: could not sync ({e}); left unchanged.",
                nft.launcher_id
            )),
            Ok(SingletonSync::Unconfirmed) => {
                report.msg(format!("NFT {}: still pending on-chain.", nft.launcher_id));
            }
            Ok(SingletonSync::StillLive) => {
                if let Some(rec) = state.nft_mut(&nft.launcher_id) {
                    if rec.phase == Phase::Pending {
                        rec.phase = Phase::Confirmed;
                        report.changed = true;
                        report.msg(format!("NFT {}: confirmed on-chain.", nft.launcher_id));
                    }
                }
            }
            Ok(SingletonSync::Advanced(live)) => {
                let controlled = wallet_ph == Some(live.info.p2_puzzle_hash);
                if let Some(rec) = state.nft_mut(&nft.launcher_id) {
                    rec.coin = CoinJson::from_coin(live.coin);
                    rec.proof = ProofJson::from_proof(live.proof);
                    rec.p2_puzzle_hash = to_hex(live.info.p2_puzzle_hash);
                    rec.current_owner = live.info.current_owner.map(to_hex);
                    rec.phase = if controlled {
                        Phase::Confirmed
                    } else {
                        Phase::Superseded
                    };
                }
                report.changed = true;
                report.msg(format!(
                    "NFT {}: advanced to live coin {} ({}).",
                    nft.launcher_id,
                    to_hex(live.coin.coin_id()),
                    if controlled {
                        "wallet-controlled"
                    } else {
                        "locked / not this wallet"
                    }
                ));
            }
            Ok(SingletonSync::Gone) => {
                if let Some(rec) = state.nft_mut(&nft.launcher_id) {
                    if rec.phase != Phase::Superseded {
                        rec.phase = Phase::Superseded;
                        report.changed = true;
                    }
                }
                report.msg(format!(
                    "NFT {}: no longer a live singleton.",
                    nft.launcher_id
                ));
            }
        }
    }
}

async fn reconcile_options(
    coinset: &Coinset,
    state: &mut State,
    wallet_ph: Option<Bytes32>,
    report: &mut ReconcileReport,
) {
    let options = state.options.clone();
    for option in options {
        let tracked = match option.coin.to_coin() {
            Ok(c) => c,
            Err(e) => {
                report.warn(format!(
                    "Option {}: unreadable coin ({e}); skipped.",
                    option.launcher_id
                ));
                continue;
            }
        };
        let sync = advance_singleton(
            coinset,
            tracked,
            option_contract::option_child_from_spend,
            |o| o.coin,
        )
        .await;
        match sync {
            Err(e) => report.warn(format!(
                "Option {}: could not sync ({e}); left unchanged.",
                option.launcher_id
            )),
            Ok(SingletonSync::Unconfirmed) => {
                report.msg(format!(
                    "Option {}: still pending on-chain.",
                    option.launcher_id
                ));
            }
            Ok(SingletonSync::StillLive) => {
                if let Some(rec) = state.option_mut(&option.launcher_id) {
                    if rec.phase == Phase::Pending {
                        rec.phase = Phase::Confirmed;
                        report.changed = true;
                        report.msg(format!(
                            "Option {}: confirmed on-chain.",
                            option.launcher_id
                        ));
                    }
                }
            }
            Ok(SingletonSync::Advanced(live)) => {
                let owner = live.info.p2_puzzle_hash;
                let controlled = wallet_ph == Some(owner);
                if let Some(rec) = state.option_mut(&option.launcher_id) {
                    rec.coin = CoinJson::from_coin(live.coin);
                    rec.proof = Some(ProofJson::from_proof(live.proof));
                    rec.owner_puzzle_hash = to_hex(owner);
                    rec.phase = if controlled {
                        Phase::Confirmed
                    } else {
                        Phase::Superseded
                    };
                }
                report.changed = true;
                report.msg(format!(
                    "Option {}: advanced to live coin {} ({}).",
                    option.launcher_id,
                    to_hex(live.coin.coin_id()),
                    if controlled {
                        "this wallet"
                    } else {
                        "not this wallet"
                    }
                ));
            }
            Ok(SingletonSync::Gone) => {
                if let Some(rec) = state.option_mut(&option.launcher_id) {
                    if rec.phase != Phase::Superseded {
                        rec.phase = Phase::Superseded;
                        report.changed = true;
                    }
                }
                report.msg(format!(
                    "Option {}: closed (exercised or otherwise melted).",
                    option.launcher_id
                ));
            }
        }
    }
}

async fn reconcile_p2_singletons(
    coinset: &Coinset,
    state: &mut State,
    report: &mut ReconcileReport,
) {
    let p2s = state.p2_singletons.clone();
    for p2 in p2s {
        let puzzle_hash = match from_hex(&p2.puzzle_hash) {
            Ok(ph) => ph,
            Err(e) => {
                report.warn(format!(
                    "p2_singleton {}: unreadable puzzle hash ({e}); skipped.",
                    p2.launcher_id
                ));
                continue;
            }
        };

        let live = match coinset.unspent_coins(puzzle_hash).await {
            Ok(coins) => coins,
            Err(e) => {
                report.warn(format!(
                    "p2_singleton {}: balance lookup failed ({e}); left unchanged.",
                    p2.launcher_id
                ));
                continue;
            }
        };
        let live_ids: HashSet<Bytes32> = live.iter().map(|c| c.coin_id()).collect();

        // Retain any locally-recorded coins that are still pending (not yet confirmed and
        // not observed spent). This keeps freshly-funded coins visible before they confirm.
        let mut kept_pending: Vec<Coin> = Vec::new();
        for coin_json in &p2.funded_coins {
            let Ok(coin) = coin_json.to_coin() else {
                continue;
            };
            if live_ids.contains(&coin.coin_id()) {
                continue; // already in `live`
            }
            match coinset.classify(coin.coin_id()).await {
                ChainStatus::NotFound => kept_pending.push(coin), // still pending
                ChainStatus::LookupFailed { .. } => kept_pending.push(coin), // unknown; keep
                _ => {} // confirmed-but-not-live is impossible here; spent → drop
            }
        }

        let mut funded: Vec<Coin> = live.clone();
        funded.extend(kept_pending.iter().copied());

        let target_phase = if !live.is_empty() {
            Phase::Confirmed
        } else if !kept_pending.is_empty() || p2.phase == Phase::Pending {
            // No confirmed coins yet, but something is still in flight (a pending fund or
            // an unconfirmed sweep). Don't prematurely declare it empty.
            Phase::Pending
        } else {
            Phase::Superseded
        };

        let new_funded: Vec<CoinJson> = funded.iter().map(|c| CoinJson::from_coin(*c)).collect();
        let old_ids: HashSet<Bytes32> = p2
            .funded_coins
            .iter()
            .filter_map(|c| c.to_coin().ok())
            .map(|c| c.coin_id())
            .collect();
        let new_ids: HashSet<Bytes32> = funded.iter().map(|c| c.coin_id()).collect();

        if old_ids != new_ids || p2.phase != target_phase {
            if let Some(rec) = state.p2_mut(&p2.launcher_id) {
                rec.funded_coins = new_funded;
                rec.phase = target_phase;
            }
            report.changed = true;
        }
        let total: u64 = live.iter().map(|c| c.amount).sum();
        report.msg(format!(
            "p2_singleton {}: {} confirmed coin(s) totaling {} mojos{}.",
            p2.launcher_id,
            live.len(),
            total,
            if kept_pending.is_empty() {
                String::new()
            } else {
                format!(", {} pending", kept_pending.len())
            }
        ));
    }
}

async fn prune_transactions(coinset: &Coinset, state: &mut State, report: &mut ReconcileReport) {
    if state.transactions.is_empty() {
        return;
    }
    let watch_ids: Vec<Bytes32> = state
        .transactions
        .iter()
        .filter_map(|tx| from_hex(&tx.watch_coin_id).ok())
        .collect();
    let statuses = coinset.classify_many(&watch_ids).await;

    let before = state.transactions.len();
    let kept: Vec<_> = std::mem::take(&mut state.transactions)
        .into_iter()
        .filter(|tx| {
            let Ok(id) = from_hex(&tx.watch_coin_id) else {
                // Unparseable watch id: drop it (nothing we can track).
                return false;
            };
            // Keep the transaction unless its watch coin is definitively on-chain. A missing
            // (pending) coin or a lookup failure keeps the record, never silently drops it.
            !matches!(
                statuses.get(&id),
                Some(ChainStatus::ConfirmedUnspent { .. }) | Some(ChainStatus::Spent { .. })
            )
        })
        .collect();
    state.transactions = kept;

    let pruned = before - state.transactions.len();
    if pruned > 0 {
        report.changed = true;
        report.msg(format!(
            "Pruned {pruned} settled transaction(s) from the log."
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{MetadataJson, NftRecord, P2SingletonRecord};

    fn coin_json(seed: u8) -> CoinJson {
        CoinJson {
            parent_coin_info: to_hex(Bytes32::new([seed; 32])),
            puzzle_hash: to_hex(Bytes32::new([seed.wrapping_add(1); 32])),
            amount: 1,
        }
    }

    #[test]
    fn backfills_p2_singleton_for_every_tracked_nft() {
        let nft_launcher = Bytes32::new([7; 32]);
        let mut state = State {
            nfts: vec![NftRecord {
                launcher_id: to_hex(nft_launcher),
                coin: coin_json(4),
                proof: ProofJson::Eve {
                    parent_parent_coin_info: to_hex(Bytes32::new([5; 32])),
                    parent_amount: 1,
                },
                metadata: MetadataJson {
                    edition_number: 1,
                    edition_total: 1,
                    data_uris: Vec::new(),
                    data_hash: None,
                    metadata_uris: Vec::new(),
                    metadata_hash: None,
                    license_uris: Vec::new(),
                    license_hash: None,
                },
                metadata_updater_puzzle_hash: to_hex(Bytes32::new([6; 32])),
                current_owner: None,
                royalty_puzzle_hash: to_hex(Bytes32::new([8; 32])),
                royalty_basis_points: 0,
                p2_puzzle_hash: to_hex(Bytes32::new([9; 32])),
                phase: Phase::Confirmed,
            }],
            ..Default::default()
        };
        let mut report = ReconcileReport::default();

        backfill_nft_p2_singletons(&mut state, &mut report);

        assert!(report.changed);
        assert_eq!(state.p2_singletons.len(), 1);
        let record: &P2SingletonRecord = &state.p2_singletons[0];
        assert_eq!(record.launcher_id, to_hex(nft_launcher));
        assert_eq!(
            record.puzzle_hash,
            to_hex(p2_singleton::puzzle_hash(nft_launcher))
        );
        assert_eq!(record.address, p2_singleton::address(nft_launcher).unwrap());
        assert!(record.funded_coins.is_empty());

        // Reconciliation is idempotent and must not overwrite an existing tracked record.
        backfill_nft_p2_singletons(&mut state, &mut report);
        assert_eq!(state.p2_singletons.len(), 1);
    }
}
