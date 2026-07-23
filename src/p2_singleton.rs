//! Derivation and funding of the p2_singleton controlled by the NFT.
//!
//! A `p2_singleton` coin can only be spent by co-spending the singleton (here, the NFT)
//! whose launcher id it is curried with. Funding it is a plain `create_coin` to the
//! derived puzzle hash, hinted with the launcher id so the coin is discoverable.

use anyhow::{bail, Context, Result};
use chia_wallet_sdk::chia::puzzle_types::Memos;
use chia_wallet_sdk::driver::P2SingletonLayer;
use chia_wallet_sdk::prelude::{
    Address, Bytes32, Coin, Conditions, Nft, SingletonInfo, SpendContext, StandardLayer, ToTreeHash,
};

use crate::state::{CoinJson, P2SingletonRecord, Phase};
use crate::wallet::{spend_selection, Selection};
use crate::MAINNET_PREFIX;

/// The puzzle hash of the p2_singleton controlled by `launcher_id`.
pub fn puzzle_hash(launcher_id: Bytes32) -> Bytes32 {
    P2SingletonLayer::new(launcher_id).tree_hash().into()
}

/// The mainnet `xch` address of the p2_singleton controlled by `launcher_id`.
pub fn address(launcher_id: Bytes32) -> Result<String> {
    Address::new(puzzle_hash(launcher_id), MAINNET_PREFIX.to_string())
        .encode()
        .context("failed to encode p2_singleton address")
}

/// Creates the persisted tracking entry for an NFT's p2 singleton.
///
/// Every NFT launcher deterministically controls one p2-singleton puzzle, even when its
/// balance is currently empty. This is also used when recovering an NFT received through
/// an option offer so subsequent status/sync calls discover any existing funds.
pub fn tracking_record(
    launcher_id: Bytes32,
    funded_coins: Vec<Coin>,
    phase: Phase,
) -> Result<P2SingletonRecord> {
    Ok(P2SingletonRecord {
        launcher_id: crate::state::to_hex(launcher_id),
        puzzle_hash: crate::state::to_hex(puzzle_hash(launcher_id)),
        address: address(launcher_id)?,
        funded_coins: funded_coins.into_iter().map(CoinJson::from_coin).collect(),
        phase,
    })
}

/// Funds the p2_singleton with `amount` mojos and returns the created coin.
pub fn build_fund(
    ctx: &mut SpendContext,
    layer: &StandardLayer,
    launcher_id: Bytes32,
    amount: u64,
    selection: &Selection,
    change_puzzle_hash: Bytes32,
    fee: u64,
) -> Result<Coin> {
    let target = puzzle_hash(launcher_id);
    let hint = ctx.hint(launcher_id)?;
    let primary = Conditions::new().create_coin(target, amount, hint);

    spend_selection(ctx, layer, selection, primary, change_puzzle_hash, fee)?;

    Ok(Coin::new(selection.coins[0].coin_id(), target, amount))
}

/// The result of sweeping the p2_singleton balance to a destination.
#[derive(Debug, Clone)]
pub struct SweepOutcome {
    /// The NFT after being re-spent to authorize the sweep (still wallet-controlled).
    pub new_nft: Nft,
    /// The single output coin paying the destination.
    pub swept_coin: Coin,
    /// The amount paid to the destination.
    pub swept_amount: u64,
    /// The total p2_singleton balance consumed.
    pub total: u64,
    /// How many p2_singleton coins were swept.
    pub coins_spent: usize,
    /// The fee the caller requested (reserved for the network).
    pub requested_fee: u64,
    /// The extra 1 mojo donated to the fee to keep the payout even (0 or 1). The singleton
    /// top layer requires exactly one odd output (the recreated NFT), so the payout must be
    /// even; any odd remainder becomes an unavoidable 1-mojo fee donation.
    pub odd_donation: u64,
}

/// Sweeps every p2_singleton coin into one payout coin for `destination_puzzle_hash`.
///
/// A p2_singleton coin can only be spent by co-spending the controlling singleton (the NFT),
/// which authorizes each coin by creating a puzzle announcement of its id. All the coins'
/// value is captured by a single `create_coin` emitted from the NFT's inner puzzle, and the
/// NFT is recreated unchanged under the wallet's control.
///
/// The `fee` (plus any odd remainder, since the singleton layer requires exactly one odd
/// output — the recreated NFT) is taken out of the swept balance.
pub fn build_sweep(
    ctx: &mut SpendContext,
    owner_layer: &StandardLayer,
    nft: Nft,
    launcher_id: Bytes32,
    p2_coins: &[Coin],
    destination_puzzle_hash: Bytes32,
    fee: u64,
) -> Result<SweepOutcome> {
    if p2_coins.is_empty() {
        bail!("no p2_singleton coins to sweep");
    }

    let total = p2_coins
        .iter()
        .try_fold(0u64, |acc, coin| acc.checked_add(coin.amount))
        .ok_or_else(|| anyhow::anyhow!("p2_singleton balance overflows u64"))?;

    let after_fee = total
        .checked_sub(fee)
        .ok_or_else(|| anyhow::anyhow!("fee {fee} exceeds the p2_singleton balance {total}"))?;
    // The singleton top layer requires exactly one odd-amount output (the recreated NFT), so
    // the payout must be even; any odd remainder becomes an unavoidable 1-mojo fee donation.
    let odd_donation = after_fee % 2;
    let swept = after_fee - odd_donation;
    if swept == 0 {
        bail!("nothing left to sweep after the fee");
    }
    // The whole balance must be accounted for: payout + reserved fee == total.
    let reserved_fee = fee + odd_donation;

    let p2 = P2SingletonLayer::new(launcher_id);
    let nft_inner_puzzle_hash: Bytes32 = nft.info.inner_puzzle_hash().into();
    let nft_p2_puzzle_hash = nft.info.p2_puzzle_hash;
    let nft_coin_id = nft.coin.coin_id();

    // Spend every p2_singleton coin; the NFT will authorize each by announcing its id.
    let mut extra = Conditions::new();
    for coin in p2_coins {
        p2.spend_coin(ctx, *coin, nft_inner_puzzle_hash)?;
        extra = extra.create_puzzle_announcement(coin.coin_id().into());
    }

    // The NFT captures the whole balance into the payout coin and reserves the fee (plus any
    // odd-mojo donation), while recreating itself unchanged (the odd output).
    extra = extra.create_coin(destination_puzzle_hash, swept, Memos::None);
    if reserved_fee > 0 {
        extra = extra.reserve_fee(reserved_fee);
    }

    let new_nft = nft.transfer(ctx, owner_layer, nft_p2_puzzle_hash, extra)?;

    Ok(SweepOutcome {
        new_nft,
        swept_coin: Coin::new(nft_coin_id, destination_puzzle_hash, swept),
        swept_amount: swept,
        total,
        coins_spent: p2_coins.len(),
        requested_fee: fee,
        odd_donation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn puzzle_hash_is_deterministic() {
        let launcher_id = Bytes32::new([4u8; 32]);
        assert_eq!(puzzle_hash(launcher_id), puzzle_hash(launcher_id));
    }

    #[test]
    fn address_uses_mainnet_prefix() {
        let launcher_id = Bytes32::new([4u8; 32]);
        assert!(address(launcher_id).unwrap().starts_with("xch1"));
    }

    #[test]
    fn tracking_record_derives_address_and_puzzle_hash() {
        let launcher_id = Bytes32::new([9u8; 32]);
        let record = tracking_record(launcher_id, Vec::new(), Phase::Confirmed).unwrap();

        assert_eq!(record.launcher_id, crate::state::to_hex(launcher_id));
        assert_eq!(
            record.puzzle_hash,
            crate::state::to_hex(puzzle_hash(launcher_id))
        );
        assert_eq!(record.address, address(launcher_id).unwrap());
        assert!(record.funded_coins.is_empty());
        assert_eq!(record.phase, Phase::Confirmed);
    }
}
