//! Simulator-backed lifecycle test: key-owned XCH -> NFT mint -> p2_singleton funding
//! -> NFT-backed option mint, all driven through the same builders the CLI uses.
//!
//! No mainnet access is performed; everything runs against the in-memory simulator.

use anyhow::Result;
use chia_wallet_sdk::chia::puzzle_types::nft::NftMetadata;
use chia_wallet_sdk::driver::{decode_offer, encode_offer, P2SingletonLayer};
use chia_wallet_sdk::prelude::{
    Coin, Conditions, Offer, OptionType, Simulator, SpendBundle, SpendContext, StandardLayer,
};
use chia_wallet_sdk::test::sign_transaction;

use pringle_wallet::nft;
use pringle_wallet::option;
use pringle_wallet::p2_singleton;
use pringle_wallet::state::Phase;
use pringle_wallet::wallet::{build_send, select_for, spend_all};

/// Helper: the deterministic wallet change coin produced by spending `parent`.
fn change_coin(
    parent: Coin,
    wallet_puzzle_hash: chia_wallet_sdk::prelude::Bytes32,
    change: u64,
) -> Coin {
    Coin::new(parent.coin_id(), wallet_puzzle_hash, change)
}

#[test]
fn standard_wallet_consolidates_and_sends_all_coins() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();
    let alice = sim.bls(1_000_000);
    let layer = StandardLayer::new(alice.pk);

    // Split the initial wallet coin so the spend-all builder has multiple inputs.
    layer.spend(
        ctx,
        alice.coin,
        Conditions::new()
            .create_coin(
                alice.puzzle_hash,
                400_000,
                chia_wallet_sdk::chia::puzzle_types::Memos::None,
            )
            .create_coin(
                alice.puzzle_hash,
                600_000,
                chia_wallet_sdk::chia::puzzle_types::Memos::None,
            ),
    )?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    let inputs = vec![
        Coin::new(alice.coin.coin_id(), alice.puzzle_hash, 400_000),
        Coin::new(alice.coin.coin_id(), alice.puzzle_hash, 600_000),
    ];

    let destination = chia_wallet_sdk::prelude::Bytes32::new([7; 32]);
    let outcome = spend_all(ctx, &layer, inputs.clone(), destination, 1_000)?;
    assert_eq!(outcome.total, 1_000_000);
    assert_eq!(outcome.sent, 999_000);
    assert_eq!(outcome.spent_coins, inputs);

    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    let output = sim
        .coin_state(outcome.output_coin.coin_id())
        .expect("single destination output exists");
    assert_eq!(output.coin.puzzle_hash, destination);
    assert_eq!(output.coin.amount, 999_000);
    assert!(inputs.iter().all(|coin| {
        sim.coin_state(coin.coin_id())
            .is_some_and(|state| state.spent_height.is_some())
    }));
    Ok(())
}

#[test]
fn standard_wallet_sends_an_amount_and_keeps_the_change() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();
    let alice = sim.bls(1_000_000);
    let layer = StandardLayer::new(alice.pk);

    let destination = chia_wallet_sdk::prelude::Bytes32::new([7; 32]);
    let selection = select_for(vec![alice.coin], 600_000, 1_000)?;
    let outcome = build_send(
        ctx,
        &layer,
        &selection,
        destination,
        600_000,
        alice.puzzle_hash,
        1_000,
    )?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;

    // The destination receives exactly the requested amount, and the fee comes out of what
    // is left rather than out of the payment.
    let paid = sim
        .coin_state(outcome.output_coin.coin_id())
        .expect("destination coin exists");
    assert_eq!(paid.coin.amount, 600_000);
    assert_eq!(paid.coin.puzzle_hash, destination);

    let change = outcome.change_coin.expect("change was left over");
    assert_eq!(change.amount, 399_000);
    assert!(sim.coin_state(change.coin_id()).is_some());
    assert!(sim
        .coin_state(alice.coin.coin_id())
        .expect("input coin exists")
        .spent_height
        .is_some());
    Ok(())
}

#[test]
fn nft_mint_fund_and_option_mint() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(1_000_000_000);
    let layer = StandardLayer::new(alice.pk);
    let wallet_ph = alice.puzzle_hash;

    // ---- 1. Mint the NFT ----
    let mint_selection = select_for(vec![alice.coin], nft::NFT_MINT_OUTPUT_VALUE, 0)?;
    let metadata = NftMetadata::default();
    let minted = nft::build_mint(ctx, &layer, wallet_ph, &mint_selection, &metadata, 300, 0)?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;

    // The NFT coin now exists on the simulated chain.
    assert!(sim.coin_state(minted.coin.coin_id()).is_some());
    let launcher_id = minted.info.launcher_id;

    // Round-trip the NFT through the persisted record and confirm it reconstructs.
    let record = nft::nft_to_record(&minted, &metadata, Phase::Confirmed);
    let restored = nft::nft_from_record(ctx, &record)?;
    assert_eq!(restored.coin, minted.coin);
    assert_eq!(restored.info.launcher_id, minted.info.launcher_id);
    assert_eq!(restored.info.p2_puzzle_hash, minted.info.p2_puzzle_hash);

    // Wallet change coin after the mint.
    let after_mint = change_coin(alice.coin, wallet_ph, mint_selection.change);

    // ---- 2. Fund the p2_singleton controlled by the NFT ----
    let fund_amount = 500_000_000;
    let fund_selection = select_for(vec![after_mint], fund_amount, 0)?;
    let funded = p2_singleton::build_fund(
        ctx,
        &layer,
        launcher_id,
        fund_amount,
        &fund_selection,
        wallet_ph,
        0,
    )?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;

    // The funded p2_singleton coin exists and sits at the derived puzzle hash.
    assert!(sim.coin_state(funded.coin_id()).is_some());
    assert_eq!(funded.puzzle_hash, p2_singleton::puzzle_hash(launcher_id));

    let after_fund = change_coin(after_mint, wallet_ph, fund_selection.change);

    // ---- 3. p2_singleton cannot be spent without co-spending the NFT ----
    {
        let p2 = P2SingletonLayer::new(launcher_id);
        p2.spend_coin(ctx, funded, wallet_ph)?;
        let result = sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk));
        assert!(
            result.is_err(),
            "p2_singleton must not be spendable without the NFT authorization"
        );
    }
    // The funded coin should still be unspent after the rejected attempt.
    let state = sim
        .coin_state(funded.coin_id())
        .expect("funded coin still exists");
    assert!(state.spent_height.is_none());

    // ---- 4. Create the NFT-backed option ----
    let option_selection = select_for(vec![after_fund], option::OPTION_OUTPUT_VALUE, 0)?;
    let strike = 5_000_000_000_000;
    let expiration = 4_000_000_000; // far in the future
    let outcome = option::build_create(
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

    // The option singleton exists, and the NFT is now locked into the option underlying
    // (its inner puzzle is no longer the wallet's standard puzzle).
    assert!(sim.coin_state(outcome.option.coin.coin_id()).is_some());
    assert_ne!(outcome.locked_nft.info.p2_puzzle_hash, wallet_ph);

    // The option record captures the terms.
    let option_record = option::option_to_record(
        &outcome,
        strike,
        expiration,
        wallet_ph,
        wallet_ph,
        Phase::Pending,
    );
    assert_eq!(option_record.strike_amount, strike);
    assert_eq!(option_record.expiration_seconds, expiration);

    Ok(())
}

#[test]
fn option_offer_roundtrip_and_take() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    // ---- Maker (alice): mint an NFT and create an option owned by alice. ----
    let alice = sim.bls(1_000_000_000);
    let layer = StandardLayer::new(alice.pk);
    let wallet_ph = alice.puzzle_hash;

    let mint_selection = select_for(vec![alice.coin], nft::NFT_MINT_OUTPUT_VALUE, 0)?;
    let metadata = NftMetadata::default();
    let minted = nft::build_mint(ctx, &layer, wallet_ph, &mint_selection, &metadata, 300, 0)?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    let after_mint = change_coin(alice.coin, wallet_ph, mint_selection.change);

    let option_selection = select_for(vec![after_mint], option::OPTION_OUTPUT_VALUE, 0)?;
    let strike = 5_000_000_000_000;
    let expiration = 4_000_000_000;
    let outcome = option::build_create(
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
    assert!(sim.coin_state(outcome.option.coin.coin_id()).is_some());

    // ---- Build an offer selling the option for XCH, via the persisted record path. ----
    let request = 250_000_000u64;
    let record = option::option_to_record(
        &outcome,
        strike,
        expiration,
        wallet_ph,
        wallet_ph,
        Phase::Confirmed,
    );
    let contract = option::option_from_record(&record)?;
    assert_eq!(contract.coin, outcome.option.coin);
    assert_eq!(contract.info.launcher_id, outcome.launcher_id);

    let parts = option::build_offer(ctx, &layer, contract, wallet_ph, request)?;
    let coin_spends = ctx.take();
    let signature = sign_transaction(&coin_spends, std::slice::from_ref(&alice.sk))?;
    let partial = SpendBundle::new(coin_spends, signature);
    let full = option::finalize_offer(ctx, partial, parts)?;

    // ---- Serialize to an `offer1...` string and parse it back. ----
    let offer_text = encode_offer(&full)?;
    assert!(offer_text.starts_with("offer1"));

    let decoded = decode_offer(&offer_text)?;
    // Parse with the same context the taker will spend through, so notarized-payment
    // memo node pointers resolve against a single allocator.
    let offer = Offer::from_spend_bundle(ctx, &decoded)?;
    assert!(offer
        .offered_coins()
        .options
        .contains_key(&outcome.launcher_id));
    assert_eq!(offer.requested_payments().amounts().xch, request);

    // ---- The offer summary the CLI inspects before buying. ----
    let offered = option::offered_option(&offer)?;
    assert_eq!(offered.launcher_id, outcome.launcher_id);
    assert_eq!(offered.request_mojos, request);
    // The maker's option coin has to stay unspent for the offer to settle.
    assert_eq!(offered.maker_coin_id, outcome.option.coin.coin_id());
    assert_eq!(
        offered.underlying_coin_id,
        outcome.locked_nft.coin.coin_id()
    );

    // Terms are not carried in the offer, but the offer commits to them: only the real
    // strike and expiration reproduce the underlying's delegated puzzle hash, which is what
    // makes chain-recovered terms trustworthy enough to show a buyer.
    assert!(option::verify_terms(
        offered.launcher_id,
        wallet_ph,
        expiration,
        outcome.locked_nft.coin.amount,
        OptionType::Xch { amount: strike },
        offered.underlying_delegated_puzzle_hash,
    ));
    assert!(!option::verify_terms(
        offered.launcher_id,
        wallet_ph,
        expiration,
        outcome.locked_nft.coin.amount,
        OptionType::Xch { amount: strike - 1 },
        offered.underlying_delegated_puzzle_hash,
    ));

    // ---- Taker (bob) accepts via the same builder the CLI uses. ----
    let bob = sim.bls(request);
    let take = option::build_take(
        ctx,
        &offer,
        std::slice::from_ref(&bob.coin),
        bob.puzzle_hash,
        bob.pk,
        0,
    )?;
    assert_eq!(take.launcher_id, outcome.launcher_id);
    assert_eq!(take.paid_mojos, request);
    // The option is now owned by bob.
    assert_eq!(take.option.info.p2_puzzle_hash, bob.puzzle_hash);

    let coin_spends = ctx.take();
    let signature = sign_transaction(&coin_spends, std::slice::from_ref(&bob.sk))?;
    let bundle = offer.take(SpendBundle::new(coin_spends, signature));
    sim.new_transaction(bundle)?;

    // The original option coin was consumed by the settled offer.
    let state = sim
        .coin_state(outcome.option.coin.coin_id())
        .expect("option coin exists");
    assert!(state.spent_height.is_some());

    Ok(())
}

#[test]
fn option_exercise_returns_nft() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    // ---- Mint an NFT and create an option owned (and created) by alice. ----
    let alice = sim.bls(1_000_000);
    let layer = StandardLayer::new(alice.pk);
    let wallet_ph = alice.puzzle_hash;

    let mint_selection = select_for(vec![alice.coin], nft::NFT_MINT_OUTPUT_VALUE, 0)?;
    let metadata = NftMetadata::default();
    let minted = nft::build_mint(ctx, &layer, wallet_ph, &mint_selection, &metadata, 300, 0)?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    let after_mint = change_coin(alice.coin, wallet_ph, mint_selection.change);

    let option_selection = select_for(vec![after_mint], option::OPTION_OUTPUT_VALUE, 0)?;
    let strike = 1_000u64;
    let expiration = 4_000_000_000; // far in the future, so exercise is still valid
    let outcome = option::build_create(
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

    // ---- Reconstruct the option and locked NFT from their persisted records. ----
    let option_record = option::option_to_record(
        &outcome,
        strike,
        expiration,
        wallet_ph,
        wallet_ph,
        Phase::Confirmed,
    );
    let nft_record = nft::nft_to_record(&outcome.locked_nft, &metadata, Phase::Superseded);
    let contract = option::option_from_record(&option_record)?;
    let locked_nft = nft::nft_from_record(ctx, &nft_record)?;

    // ---- Exercise: pay the strike to the creator, receive the NFT. ----
    let strike_selection = select_for(vec![after_option], strike, 0)?;
    let exercise = option::build_exercise(
        ctx,
        &layer,
        contract,
        locked_nft,
        wallet_ph, // creator (strike recipient)
        expiration,
        strike,
        wallet_ph, // owner (NFT recipient)
        &strike_selection,
        wallet_ph,
        0,
    )?;
    // The exerciser receives the NFT (same launcher) back under their own puzzle.
    assert_eq!(exercise.nft.info.p2_puzzle_hash, wallet_ph);
    assert_eq!(
        exercise.nft.info.launcher_id,
        outcome.locked_nft.info.launcher_id
    );

    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;

    // The option singleton was melted, and the locked NFT coin was consumed.
    assert!(sim
        .coin_state(outcome.option.coin.coin_id())
        .expect("option coin exists")
        .spent_height
        .is_some());
    assert!(sim
        .coin_state(outcome.locked_nft.coin.coin_id())
        .expect("locked nft coin exists")
        .spent_height
        .is_some());

    // The freed NFT now lives at a fresh, unspent coin owned by alice.
    let freed = sim
        .coin_state(exercise.nft.coin.coin_id())
        .expect("freed nft coin exists");
    assert!(freed.spent_height.is_none());

    Ok(())
}

#[test]
fn option_clawback_reclaims_expired_nft() -> Result<()> {
    // A creator can reclaim the underlying NFT of an expired option: the clawback fails
    // before the deadline, succeeds after it, recreates the NFT under the creator, and leaves
    // the option singleton untouched. Afterwards the reclaimed NFT can sweep its p2-singleton.
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(1_000_000);
    let layer = StandardLayer::new(alice.pk);
    let wallet_ph = alice.puzzle_hash;

    // ---- Mint the NFT and create an option (alice is creator + owner). ----
    let mint_selection = select_for(vec![alice.coin], nft::NFT_MINT_OUTPUT_VALUE, 0)?;
    let metadata = NftMetadata::default();
    let minted = nft::build_mint(ctx, &layer, wallet_ph, &mint_selection, &metadata, 0, 0)?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    let launcher_id = minted.info.launcher_id;
    let after_mint = change_coin(alice.coin, wallet_ph, mint_selection.change);

    // Fund the p2-singleton so we can sweep it after reclaiming the NFT.
    let fund = select_for(vec![after_mint], 100_000, 0)?;
    let p2_coin = p2_singleton::build_fund(ctx, &layer, launcher_id, 100_000, &fund, wallet_ph, 0)?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    let after_fund = change_coin(after_mint, wallet_ph, fund.change);

    let option_selection = select_for(vec![after_fund], option::OPTION_OUTPUT_VALUE, 0)?;
    let strike = 1_000u64;
    let expiration = 2_000u64;
    let outcome = option::build_create(
        ctx,
        &layer,
        minted,
        &option_selection,
        strike,
        expiration,
        wallet_ph, // creator
        wallet_ph, // owner
        wallet_ph, // change
        0,
    )?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    assert!(sim.coin_state(outcome.option.coin.coin_id()).is_some());

    // ---- Clawback before expiry must be rejected on-chain. ----
    {
        let early = option::build_clawback(
            ctx,
            &layer,
            outcome.launcher_id,
            outcome.locked_nft,
            wallet_ph,
            expiration,
            strike,
            wallet_ph,
            None,
            wallet_ph,
            0,
        )?;
        // Keep it well before the deadline.
        sim.set_next_timestamp(expiration - 500)?;
        let result = sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk));
        assert!(
            result.is_err(),
            "clawback must not succeed before the option's expiration deadline"
        );
        let _ = early;
    }

    // The locked NFT must still be unspent after the rejected early clawback.
    assert!(sim
        .coin_state(outcome.locked_nft.coin.coin_id())
        .expect("locked nft exists")
        .spent_height
        .is_none());

    // ---- Clawback after expiry succeeds. ----
    sim.set_next_timestamp(expiration + 1)?;
    let clawback = option::build_clawback(
        ctx,
        &layer,
        outcome.launcher_id,
        outcome.locked_nft,
        wallet_ph,
        expiration,
        strike,
        wallet_ph,
        None,
        wallet_ph,
        0,
    )?;
    assert_eq!(clawback.nft.info.p2_puzzle_hash, wallet_ph);
    assert_eq!(clawback.nft.info.launcher_id, launcher_id);
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;

    // The locked NFT coin was consumed and the reclaimed NFT is live under alice.
    assert!(sim
        .coin_state(outcome.locked_nft.coin.coin_id())
        .expect("locked nft coin exists")
        .spent_height
        .is_some());
    let reclaimed = sim
        .coin_state(clawback.nft.coin.coin_id())
        .expect("reclaimed nft coin exists");
    assert!(reclaimed.spent_height.is_none());
    assert_eq!(reclaimed.coin.puzzle_hash, clawback.nft.coin.puzzle_hash);

    // The option singleton is untouched: clawback only spends the underlying.
    assert!(sim
        .coin_state(outcome.option.coin.coin_id())
        .expect("option coin exists")
        .spent_height
        .is_none());

    // ---- The reclaimed NFT can now sweep the accumulated p2-singleton income. ----
    let sweep = p2_singleton::build_sweep(
        ctx,
        &layer,
        clawback.nft,
        launcher_id,
        &[p2_coin],
        wallet_ph,
        0,
    )?;
    assert_eq!(sweep.total, 100_000);
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    assert_eq!(
        sim.coin_state(sweep.swept_coin.coin_id())
            .expect("payout exists")
            .coin
            .amount,
        100_000
    );

    Ok(())
}

#[test]
fn option_clawback_with_fee_pays_from_separate_coins() -> Result<()> {
    // A clawback fee is funded from separate regular-XCH inputs bound to the NFT spend.
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(1_000_000);
    let layer = StandardLayer::new(alice.pk);
    let wallet_ph = alice.puzzle_hash;

    let mint_selection = select_for(vec![alice.coin], nft::NFT_MINT_OUTPUT_VALUE, 0)?;
    let metadata = NftMetadata::default();
    let minted = nft::build_mint(ctx, &layer, wallet_ph, &mint_selection, &metadata, 0, 0)?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    let after_mint = change_coin(alice.coin, wallet_ph, mint_selection.change);

    let option_selection = select_for(vec![after_mint], option::OPTION_OUTPUT_VALUE, 0)?;
    let strike = 1_000u64;
    let expiration = 2_000u64;
    let outcome = option::build_create(
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

    sim.set_next_timestamp(expiration + 1)?;
    let fee = 500u64;
    let fee_selection = select_for(vec![after_option], 0, fee)?;
    let clawback = option::build_clawback(
        ctx,
        &layer,
        outcome.launcher_id,
        outcome.locked_nft,
        wallet_ph,
        expiration,
        strike,
        wallet_ph,
        Some(&fee_selection),
        wallet_ph,
        fee,
    )?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;

    assert_eq!(
        clawback.nft.info.launcher_id,
        outcome.locked_nft.info.launcher_id
    );
    assert!(sim
        .coin_state(clawback.nft.coin.coin_id())
        .expect("reclaimed nft exists")
        .spent_height
        .is_none());
    // The fee input coin was consumed.
    assert!(sim
        .coin_state(fee_selection.coins[0].coin_id())
        .expect("fee coin exists")
        .spent_height
        .is_some());

    Ok(())
}

#[test]
fn p2_singleton_sweep_moves_full_balance() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(1_000_000);
    let layer = StandardLayer::new(alice.pk);
    let wallet_ph = alice.puzzle_hash;

    // ---- Mint the NFT (the controller of the p2_singleton). ----
    let mint_selection = select_for(vec![alice.coin], nft::NFT_MINT_OUTPUT_VALUE, 0)?;
    let metadata = NftMetadata::default();
    let minted = nft::build_mint(ctx, &layer, wallet_ph, &mint_selection, &metadata, 0, 0)?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    let launcher_id = minted.info.launcher_id;
    let after_mint = change_coin(alice.coin, wallet_ph, mint_selection.change);

    // ---- Fund the p2_singleton twice, producing two separate coins. ----
    let fund1 = select_for(vec![after_mint], 100_000, 0)?;
    let p2_coin1 =
        p2_singleton::build_fund(ctx, &layer, launcher_id, 100_000, &fund1, wallet_ph, 0)?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    let after_fund1 = change_coin(after_mint, wallet_ph, fund1.change);

    let fund2 = select_for(vec![after_fund1], 50_000, 0)?;
    let p2_coin2 =
        p2_singleton::build_fund(ctx, &layer, launcher_id, 50_000, &fund2, wallet_ph, 0)?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;

    assert!(sim.coin_state(p2_coin1.coin_id()).is_some());
    assert!(sim.coin_state(p2_coin2.coin_id()).is_some());

    // ---- Sweep the entire balance to a destination in one transaction. ----
    let bob_ph = chia_wallet_sdk::prelude::Bytes32::new([7u8; 32]);
    let sweep = p2_singleton::build_sweep(
        ctx,
        &layer,
        minted,
        launcher_id,
        &[p2_coin1, p2_coin2],
        bob_ph,
        0,
    )?;
    assert_eq!(sweep.total, 150_000);
    assert_eq!(sweep.swept_amount, 150_000);
    assert_eq!(sweep.coins_spent, 2);
    assert_eq!(sweep.new_nft.info.p2_puzzle_hash, wallet_ph);

    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;

    // Both p2 coins were consumed.
    assert!(sim
        .coin_state(p2_coin1.coin_id())
        .unwrap()
        .spent_height
        .is_some());
    assert!(sim
        .coin_state(p2_coin2.coin_id())
        .unwrap()
        .spent_height
        .is_some());

    // The full balance landed in a single coin at the destination.
    let payout = sim
        .coin_state(sweep.swept_coin.coin_id())
        .expect("payout coin exists");
    assert!(payout.spent_height.is_none());
    assert_eq!(payout.coin.amount, 150_000);
    assert_eq!(payout.coin.puzzle_hash, bob_ph);

    // The NFT was recreated and remains wallet-controlled and unspent.
    assert!(sim
        .coin_state(sweep.new_nft.coin.coin_id())
        .expect("recreated nft exists")
        .spent_height
        .is_none());

    Ok(())
}

#[test]
fn p2_singleton_sweep_donates_odd_mojo() -> Result<()> {
    // When the post-fee balance is odd, the singleton layer forces the payout to be even, so
    // exactly one mojo is donated to the fee. Verify the accounting reports it separately.
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
    let after_mint = change_coin(alice.coin, wallet_ph, mint_selection.change);

    // Fund two coins summing to an odd total (100_000 + 1 = 100_001).
    let fund1 = select_for(vec![after_mint], 100_000, 0)?;
    let p2_coin1 =
        p2_singleton::build_fund(ctx, &layer, launcher_id, 100_000, &fund1, wallet_ph, 0)?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    let after_fund1 = change_coin(after_mint, wallet_ph, fund1.change);

    let fund2 = select_for(vec![after_fund1], 1, 0)?;
    let p2_coin2 = p2_singleton::build_fund(ctx, &layer, launcher_id, 1, &fund2, wallet_ph, 0)?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;

    let bob_ph = chia_wallet_sdk::prelude::Bytes32::new([7u8; 32]);
    let sweep = p2_singleton::build_sweep(
        ctx,
        &layer,
        minted,
        launcher_id,
        &[p2_coin1, p2_coin2],
        bob_ph,
        0,
    )?;
    assert_eq!(sweep.total, 100_001);
    assert_eq!(sweep.requested_fee, 0);
    assert_eq!(sweep.odd_donation, 1);
    assert_eq!(sweep.swept_amount, 100_000);

    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    let payout = sim
        .coin_state(sweep.swept_coin.coin_id())
        .expect("payout coin exists");
    assert_eq!(payout.coin.amount, 100_000);
    Ok(())
}

#[test]
fn option_recovery_helpers_parse_terms() -> Result<()> {
    use chia_wallet_sdk::prelude::OptionType;

    // Recover an option's terms (expiration, strike, creator) purely from the coin spends the
    // mint produces, then verify they reproduce the on-chain delegated puzzle hash.
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(1_000_000);
    let layer = StandardLayer::new(alice.pk);
    let wallet_ph = alice.puzzle_hash;

    let mint_selection = select_for(vec![alice.coin], nft::NFT_MINT_OUTPUT_VALUE, 0)?;
    let metadata = NftMetadata::default();
    let minted = nft::build_mint(ctx, &layer, wallet_ph, &mint_selection, &metadata, 0, 0)?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;
    let after_mint = change_coin(alice.coin, wallet_ph, mint_selection.change);

    let option_selection = select_for(vec![after_mint], option::OPTION_OUTPUT_VALUE, 0)?;
    let strike = 5_000_000_000_000u64;
    let expiration = 4_000_000_000u64;
    let outcome = option::build_create(
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

    let coin_spends = ctx.take();

    // The launcher coin spend carries the metadata (expiration + strike type).
    let launcher_spend = coin_spends
        .iter()
        .find(|cs| cs.coin.coin_id() == outcome.launcher_id)
        .expect("launcher spend present");
    let terms = option::terms_from_launcher_spend(launcher_spend)?;
    assert_eq!(terms.expiration_seconds, expiration);
    match terms.strike_type {
        OptionType::Xch { amount } => assert_eq!(amount, strike),
        other => panic!("unexpected strike type: {other:?}"),
    }

    // The launcher's parent (funding) spend carries the creator hint memo.
    let parent_spend = coin_spends
        .iter()
        .find(|cs| cs.coin == option_selection.coins[0])
        .expect("launcher parent spend present");
    let creator = option::creator_from_launcher_creation(parent_spend, outcome.launcher_id)?
        .expect("creator recovered");
    assert_eq!(creator, wallet_ph);

    // The recovered terms must reproduce the on-chain delegated puzzle hash.
    assert!(option::verify_terms(
        outcome.launcher_id,
        creator,
        expiration,
        1,
        terms.strike_type,
        outcome.underlying_delegated_puzzle_hash,
    ));
    // A wrong creator must NOT verify.
    assert!(!option::verify_terms(
        outcome.launcher_id,
        chia_wallet_sdk::prelude::Bytes32::new([9u8; 32]),
        expiration,
        1,
        terms.strike_type,
        outcome.underlying_delegated_puzzle_hash,
    ));

    sim.spend_coins(coin_spends, std::slice::from_ref(&alice.sk))?;
    Ok(())
}

#[test]
fn insufficient_funds_is_rejected() {
    // Selecting more than the wallet holds should fail before any spend is built.
    let coin = Coin::new(
        chia_wallet_sdk::prelude::Bytes32::new([1u8; 32]),
        chia_wallet_sdk::prelude::Bytes32::new([2u8; 32]),
        10,
    );
    assert!(select_for(vec![coin], 1_000, 100).is_err());
}

#[test]
fn transfer_without_conditions_keeps_singleton_amount() -> Result<()> {
    // A minted NFT keeps its singleton amount of 1 when transferred, which the option
    // underlying relies on. This also exercises the plain transfer path.
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(1_000_000);
    let layer = StandardLayer::new(alice.pk);

    let selection = select_for(vec![alice.coin], nft::NFT_MINT_OUTPUT_VALUE, 0)?;
    let metadata = NftMetadata::default();
    let minted = nft::build_mint(ctx, &layer, alice.puzzle_hash, &selection, &metadata, 0, 0)?;
    sim.spend_coins(ctx.take(), std::slice::from_ref(&alice.sk))?;

    let moved = minted.transfer(ctx, &layer, alice.puzzle_hash, Conditions::new())?;
    sim.spend_coins(ctx.take(), &[alice.sk])?;
    assert_eq!(moved.coin.amount, 1);

    Ok(())
}
