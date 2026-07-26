//! Pins the per-transaction p2_singleton coin caps against real mempool cost.
//!
//! The mempool rejects any single transaction whose CLVM cost exceeds
//! `MAX_BLOCK_COST_CLVM / 2 = 5,500,000,000`. Each co-spent p2_singleton coin adds a fixed
//! chunk of cost, so there is a hard limit on how many can be swept in one transaction. The
//! [`MAX_SWEEP_COINS`](pringle_wallet::p2_singleton::MAX_SWEEP_COINS) and
//! [`MAX_EXERCISE_SWEEP_COINS`](pringle_wallet::p2_singleton::MAX_EXERCISE_SWEEP_COINS)
//! constants encode that limit; these tests build sweeps at the cap and one past it and
//! confirm the first fits under the mempool cost cap while the second exceeds it.
//!
//! Costs are computed exactly as the mempool does, with
//! `chia_consensus::spendbundle_conditions::run_spendbundle`, so the constants cannot drift
//! silently when the SDK or the underlying puzzles change.

use anyhow::Result;
use chia_wallet_sdk::chia::consensus::consensus_constants::TEST_CONSTANTS;
use chia_wallet_sdk::chia::consensus::spendbundle_conditions::run_spendbundle;
use chia_wallet_sdk::chia::puzzle_types::nft::NftMetadata;
use chia_wallet_sdk::clvmr::Allocator;
use chia_wallet_sdk::prelude::{
    Bytes32, Coin, CoinSpend, Conditions, Signature, Simulator, SpendBundle, SpendContext,
    StandardLayer,
};

use pringle_wallet::nft;
use pringle_wallet::option;
use pringle_wallet::p2_singleton::{self, MAX_EXERCISE_SWEEP_COINS, MAX_SWEEP_COINS};
use pringle_wallet::state::{OptionKind, Phase};
use pringle_wallet::wallet::select_for;

/// The mempool's single-transaction cost cap: half the max block cost.
const MEMPOOL_COST_CAP: u64 = 5_500_000_000;

/// The value used for every synthetic p2_singleton coin. Even, so the payout stays even and
/// no odd-mojo donation perturbs the cost.
const P2_COIN_VALUE: u64 = 1_000;

fn change_coin(parent: Coin, wallet_puzzle_hash: Bytes32, change: u64) -> Coin {
    Coin::new(parent.coin_id(), wallet_puzzle_hash, change)
}

/// Synthesizes `count` distinct p2_singleton coins at the address controlled by `launcher_id`.
///
/// These coins never need to exist on-chain: [`run_spendbundle`] prices a bundle by running
/// each puzzle and validating its conditions, and does not check coin existence or signatures.
fn synthetic_p2_coins(launcher_id: Bytes32, count: usize) -> Vec<Coin> {
    let puzzle_hash = p2_singleton::puzzle_hash(launcher_id);
    (0..count)
        .map(|i| {
            let mut parent = [0u8; 32];
            parent[..8].copy_from_slice(&((i as u64) + 1).to_be_bytes());
            Coin::new(Bytes32::new(parent), puzzle_hash, P2_COIN_VALUE)
        })
        .collect()
}

/// Prices `coin_spends` exactly as the mempool would.
fn mempool_cost(coin_spends: Vec<CoinSpend>) -> u64 {
    // run_spendbundle does not verify signatures (it returns the required pairs for the caller
    // to verify separately), so a default signature is fine for pure cost measurement.
    let bundle = SpendBundle::new(coin_spends, Signature::default());
    let mut allocator = Allocator::new();
    let (conditions, _) = run_spendbundle(
        &mut allocator,
        &bundle,
        TEST_CONSTANTS.max_block_cost_clvm,
        0,
        &TEST_CONSTANTS,
    )
    .expect("the sweep bundle must be valid so its cost can be measured");
    conditions.cost
}

/// Builds a plain `nft sweep` of `coin_count` p2_singleton coins and returns its mempool cost.
fn plain_sweep_cost(coin_count: usize) -> Result<u64> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();
    let alice = sim.bls(1_000_000);
    let layer = StandardLayer::new(alice.pk);
    let wallet_ph = alice.puzzle_hash;

    let mint_selection = select_for(vec![alice.coin], nft::NFT_MINT_OUTPUT_VALUE, 0)?;
    let metadata = NftMetadata::default();
    let minted = nft::build_mint(ctx, &layer, wallet_ph, &mint_selection, &metadata, 0, 0)?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    let launcher_id = minted.info.launcher_id;

    let coins = synthetic_p2_coins(launcher_id, coin_count);
    let destination = Bytes32::new([7u8; 32]);
    let _ = p2_singleton::build_sweep(ctx, &layer, minted, launcher_id, &coins, destination, 0)?;

    Ok(mempool_cost(ctx.take()))
}

/// Builds a sweep-on-exercise spending `coin_count` p2_singleton coins and returns its cost.
fn exercise_sweep_cost(coin_count: usize) -> Result<u64> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();
    let alice = sim.bls(1_000_000);
    let layer = StandardLayer::new(alice.pk);
    let wallet_ph = alice.puzzle_hash;

    let strike = 1_000u64;
    let expiration = 4_000_000_000u64;

    // Mint the NFT.
    let mint_selection = select_for(vec![alice.coin], nft::NFT_MINT_OUTPUT_VALUE, 0)?;
    let metadata = NftMetadata::default();
    let minted = nft::build_mint(ctx, &layer, wallet_ph, &mint_selection, &metadata, 0, 0)?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    let launcher_id = minted.info.launcher_id;
    let after_mint = change_coin(alice.coin, wallet_ph, mint_selection.change);

    // Create the sweep option.
    let option_selection = select_for(vec![after_mint], option::OPTION_OUTPUT_VALUE, 0)?;
    let outcome = option::build_create_sweep(
        ctx,
        &layer,
        minted,
        &option_selection,
        strike,
        expiration,
        wallet_ph,
        wallet_ph,
        wallet_ph,
        0,
    )?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    let after_option = change_coin(after_mint, wallet_ph, option_selection.change);

    // Reconstruct the option and locked NFT the way the CLI does before exercising.
    let option_record = option::option_to_record(
        &outcome,
        strike,
        expiration,
        wallet_ph,
        wallet_ph,
        OptionKind::Sweep,
        Phase::Confirmed,
    );
    let contract = option::option_from_record(&option_record)?;
    let nft_record = nft::nft_to_record(&outcome.locked_nft, &metadata, Phase::Superseded);
    let locked_nft = nft::nft_from_record(ctx, &nft_record)?;

    let coins = synthetic_p2_coins(launcher_id, coin_count);
    let strike_selection = select_for(vec![after_option], strike, 0)?;
    let _ = option::build_sweep_exercise(
        ctx,
        &layer,
        contract,
        locked_nft,
        wallet_ph,
        expiration,
        strike,
        wallet_ph,
        &coins,
        &strike_selection,
        wallet_ph,
        0,
        Conditions::new(),
    )?;

    Ok(mempool_cost(ctx.take()))
}

#[test]
fn plain_sweep_at_the_cap_fits_the_mempool() -> Result<()> {
    let cost = plain_sweep_cost(MAX_SWEEP_COINS)?;
    assert!(
        cost <= MEMPOOL_COST_CAP,
        "a plain sweep of MAX_SWEEP_COINS={MAX_SWEEP_COINS} costs {cost}, over the cap {MEMPOOL_COST_CAP}"
    );
    Ok(())
}

#[test]
fn plain_sweep_one_over_the_cap_is_rejected() -> Result<()> {
    let cost = plain_sweep_cost(MAX_SWEEP_COINS + 1)?;
    assert!(
        cost > MEMPOOL_COST_CAP,
        "a plain sweep of MAX_SWEEP_COINS+1={} costs {cost}, still under the cap {MEMPOOL_COST_CAP}; the cap could be raised",
        MAX_SWEEP_COINS + 1
    );
    Ok(())
}

#[test]
fn exercise_sweep_at_the_cap_fits_the_mempool() -> Result<()> {
    let cost = exercise_sweep_cost(MAX_EXERCISE_SWEEP_COINS)?;
    assert!(
        cost <= MEMPOOL_COST_CAP,
        "a sweep-exercise of MAX_EXERCISE_SWEEP_COINS={MAX_EXERCISE_SWEEP_COINS} costs {cost}, over the cap {MEMPOOL_COST_CAP}"
    );
    Ok(())
}

#[test]
fn exercise_sweep_one_over_the_cap_is_rejected() -> Result<()> {
    let cost = exercise_sweep_cost(MAX_EXERCISE_SWEEP_COINS + 1)?;
    assert!(
        cost > MEMPOOL_COST_CAP,
        "a sweep-exercise of MAX_EXERCISE_SWEEP_COINS+1={} costs {cost}, still under the cap {MEMPOOL_COST_CAP}; the cap could be raised",
        MAX_EXERCISE_SWEEP_COINS + 1
    );
    Ok(())
}
