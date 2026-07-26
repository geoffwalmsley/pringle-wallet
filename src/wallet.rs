//! Standard-wallet coin selection and funding-spend construction.
//!
//! These helpers are network-agnostic: they take already-fetched coins and add coin
//! spends to a [`SpendContext`]. The value in a Chia spend bundle balances across the
//! whole bundle, so all outputs (target coins, change, and reserved fee) are emitted
//! from the first selected coin while any additional inputs are bound to it with a coin
//! announcement.

use anyhow::{bail, Result};
use chia_wallet_sdk::chia::puzzle_types::Memos;
use chia_wallet_sdk::prelude::{
    select_coins, Bytes, Bytes32, Coin, Conditions, SpendContext, StandardLayer,
};
use chia_wallet_sdk::types::announcement_id;

/// The announcement message used to bind multiple wallet inputs into one atomic spend.
const BIND_NONCE: &[u8] = b"pringle";

/// The result of selecting wallet coins to cover an output plus fee.
#[derive(Debug, Clone)]
pub struct Selection {
    /// The coins to spend. The first coin is used as the parent for created singletons.
    pub coins: Vec<Coin>,
    /// The summed value of all selected coins.
    pub total: u64,
    /// The change to return to the wallet (`total - output_value - fee`).
    pub change: u64,
}

/// Result of sending a fixed amount to a destination, with the rest returned as change.
#[derive(Debug, Clone)]
pub struct SendOutcome {
    /// The amount paid to the destination.
    pub sent: u64,
    /// The destination coin created by the spend.
    pub output_coin: Coin,
    /// The change coin returned to the wallet, if any was left over.
    pub change_coin: Option<Coin>,
    /// Input coins consumed by the spend.
    pub spent_coins: Vec<Coin>,
}

/// Result of spending every supplied standard-wallet coin into one output.
#[derive(Debug, Clone)]
pub struct SpendAllOutcome {
    /// Total value of all input coins.
    pub total: u64,
    /// Amount in the destination output (`total - fee`).
    pub sent: u64,
    /// The single destination coin created by the spend.
    pub output_coin: Coin,
    /// Input coins consumed by the spend.
    pub spent_coins: Vec<Coin>,
}

/// Selects coins to cover `output_value + fee` and computes the change.
pub fn select_for(coins: Vec<Coin>, output_value: u64, fee: u64) -> Result<Selection> {
    let needed = output_value
        .checked_add(fee)
        .ok_or_else(|| anyhow::anyhow!("output value plus fee overflows u64"))?;
    if needed == 0 {
        bail!("refusing to build a spend with zero output and zero fee");
    }

    let selected = select_coins(coins, needed)?;
    let total: u64 = selected.iter().map(|coin| coin.amount).sum();
    let change = total - needed;

    Ok(Selection {
        coins: selected,
        total,
        change,
    })
}

/// Spends the selected coins, emitting `primary` outputs plus change and reserved fee.
///
/// `primary` should contain the "real" outputs of the transaction (e.g. the launcher
/// creation conditions, or a `create_coin` for a funding target). Change and fee are
/// appended here so callers do not have to repeat that logic.
pub fn spend_selection(
    ctx: &mut SpendContext,
    layer: &StandardLayer,
    selection: &Selection,
    primary: Conditions,
    change_puzzle_hash: Bytes32,
    fee: u64,
) -> Result<()> {
    let coins = &selection.coins;
    if coins.is_empty() {
        bail!("no coins selected to spend");
    }

    let mut first = primary;
    if selection.change > 0 {
        first = first.create_coin(change_puzzle_hash, selection.change, Memos::None);
    }
    if fee > 0 {
        first = first.reserve_fee(fee);
    }

    if coins.len() == 1 {
        layer.spend(ctx, coins[0], first)?;
        return Ok(());
    }

    let message: Bytes = BIND_NONCE.to_vec().into();
    first = first.create_coin_announcement(message);
    layer.spend(ctx, coins[0], first)?;

    let ann = announcement_id(coins[0].coin_id(), BIND_NONCE);
    for coin in &coins[1..] {
        layer.spend(ctx, *coin, Conditions::new().assert_coin_announcement(ann))?;
    }

    Ok(())
}

/// Sends `amount` to `destination` from the selected coins, returning the change.
///
/// Unlike [`spend_all`], the fee is paid on top of the amount rather than out of it, so the
/// destination receives exactly what was asked for. The selection must already cover
/// `amount + fee` (see [`select_for`]).
pub fn build_send(
    ctx: &mut SpendContext,
    layer: &StandardLayer,
    selection: &Selection,
    destination: Bytes32,
    amount: u64,
    change_puzzle_hash: Bytes32,
    fee: u64,
) -> Result<SendOutcome> {
    if amount == 0 {
        bail!("refusing to send zero mojos");
    }
    if selection.coins.is_empty() {
        bail!("no coins selected to spend");
    }

    let primary = Conditions::new().create_coin(destination, amount, Memos::None);
    spend_selection(ctx, layer, selection, primary, change_puzzle_hash, fee)?;

    // Both outputs are created by the first selected coin, which is what `spend_selection`
    // emits them from.
    let parent = selection.coins[0].coin_id();
    Ok(SendOutcome {
        sent: amount,
        output_coin: Coin::new(parent, destination, amount),
        change_coin: (selection.change > 0)
            .then(|| Coin::new(parent, change_puzzle_hash, selection.change)),
        spent_coins: selection.coins.clone(),
    })
}

/// Spends all supplied standard-wallet coins into one destination coin.
///
/// This powers both consolidation (destination is the wallet itself) and send-all. The fee
/// is deducted from the output, so callers do not need an additional coin to pay it.
pub fn spend_all(
    ctx: &mut SpendContext,
    layer: &StandardLayer,
    coins: Vec<Coin>,
    destination: Bytes32,
    fee: u64,
) -> Result<SpendAllOutcome> {
    if coins.is_empty() {
        bail!("no spendable XCH coins found");
    }

    let total = coins.iter().try_fold(0u64, |sum, coin| {
        sum.checked_add(coin.amount)
            .ok_or_else(|| anyhow::anyhow!("wallet balance overflows u64"))
    })?;
    let sent = total
        .checked_sub(fee)
        .filter(|amount| *amount > 0)
        .ok_or_else(|| anyhow::anyhow!("fee must be less than the spendable XCH balance"))?;

    let output_coin = Coin::new(coins[0].coin_id(), destination, sent);
    let selection = Selection {
        coins: coins.clone(),
        total,
        change: 0,
    };
    let primary = Conditions::new().create_coin(destination, sent, Memos::None);
    spend_selection(ctx, layer, &selection, primary, destination, fee)?;

    Ok(SpendAllOutcome {
        total,
        sent,
        output_coin,
        spent_coins: coins,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coin(amount: u64) -> Coin {
        Coin::new(Bytes32::new([1u8; 32]), Bytes32::new([2u8; 32]), amount)
    }

    #[test]
    fn selects_and_computes_change() {
        let selection = select_for(vec![coin(1000)], 600, 100).unwrap();
        assert_eq!(selection.total, 1000);
        assert_eq!(selection.change, 300);
    }

    #[test]
    fn rejects_zero_spend() {
        assert!(select_for(vec![coin(1000)], 0, 0).is_err());
    }

    #[test]
    fn insufficient_balance_errors() {
        assert!(select_for(vec![coin(50)], 600, 100).is_err());
    }

    #[test]
    fn send_pays_the_fee_on_top_and_returns_change() {
        let mut ctx = SpendContext::new();
        let layer = StandardLayer::new(
            chia_wallet_sdk::chia::bls::SecretKey::from_seed(&[1; 32]).public_key(),
        );
        let change_ph = Bytes32::new([2; 32]);
        let destination = Bytes32::new([3; 32]);

        let selection = select_for(vec![coin(1_000)], 600, 25).unwrap();
        let outcome = build_send(
            &mut ctx,
            &layer,
            &selection,
            destination,
            600,
            change_ph,
            25,
        )
        .unwrap();

        // The destination gets exactly the requested amount; the fee comes out of the change.
        assert_eq!(outcome.sent, 600);
        assert_eq!(outcome.output_coin.amount, 600);
        assert_eq!(outcome.output_coin.puzzle_hash, destination);
        assert_eq!(outcome.change_coin.unwrap().amount, 375);
        assert_eq!(ctx.take().len(), 1);
    }

    #[test]
    fn send_without_change_creates_no_change_coin() {
        let mut ctx = SpendContext::new();
        let layer = StandardLayer::new(
            chia_wallet_sdk::chia::bls::SecretKey::from_seed(&[1; 32]).public_key(),
        );
        let change_ph = Bytes32::new([2; 32]);

        let selection = select_for(vec![coin(1_000)], 975, 25).unwrap();
        let outcome = build_send(
            &mut ctx,
            &layer,
            &selection,
            Bytes32::new([3; 32]),
            975,
            change_ph,
            25,
        )
        .unwrap();

        assert!(outcome.change_coin.is_none());
    }

    #[test]
    fn send_rejects_zero_and_empty_selections() {
        let mut ctx = SpendContext::new();
        let layer = StandardLayer::new(
            chia_wallet_sdk::chia::bls::SecretKey::from_seed(&[1; 32]).public_key(),
        );
        let destination = Bytes32::new([3; 32]);
        let selection = select_for(vec![coin(1_000)], 600, 0).unwrap();
        let empty = Selection {
            coins: Vec::new(),
            total: 0,
            change: 0,
        };

        assert!(build_send(&mut ctx, &layer, &selection, destination, 0, destination, 0).is_err());
        assert!(build_send(&mut ctx, &layer, &empty, destination, 600, destination, 0).is_err());
    }

    #[test]
    fn spend_all_deducts_fee_and_creates_one_output() {
        let mut ctx = SpendContext::new();
        let layer = StandardLayer::new(
            chia_wallet_sdk::chia::bls::SecretKey::from_seed(&[1; 32]).public_key(),
        );
        let destination = Bytes32::new([3; 32]);
        let outcome = spend_all(
            &mut ctx,
            &layer,
            vec![coin(400), coin(600)],
            destination,
            25,
        )
        .unwrap();

        assert_eq!(outcome.total, 1_000);
        assert_eq!(outcome.sent, 975);
        assert_eq!(outcome.spent_coins.len(), 2);
        assert_eq!(outcome.output_coin.amount, 975);
        assert_eq!(outcome.output_coin.puzzle_hash, destination);
        assert_eq!(ctx.take().len(), 2);
    }

    #[test]
    fn spend_all_rejects_empty_wallet_and_excessive_fee() {
        let mut ctx = SpendContext::new();
        let layer = StandardLayer::new(
            chia_wallet_sdk::chia::bls::SecretKey::from_seed(&[1; 32]).public_key(),
        );
        let destination = Bytes32::new([3; 32]);

        assert!(spend_all(&mut ctx, &layer, vec![], destination, 0).is_err());
        assert!(spend_all(&mut ctx, &layer, vec![coin(100)], destination, 100).is_err());
    }
}
