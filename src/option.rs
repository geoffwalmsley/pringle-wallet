//! Creation of an option contract whose underlying asset is the wallet's NFT.
//!
//! The option is a singleton (odd amount) wrapping the owner's standard puzzle. The NFT
//! is locked by transferring it into the option's `OptionUnderlying` puzzle; exercising
//! the option later releases the NFT (and thus control of its p2_singleton funds) to the
//! exerciser in exchange for the XCH strike payment. This module builds the mint only.

use anyhow::Result;
use chia_wallet_sdk::chia::puzzle_types::offer::{
    NotarizedPayment, Payment, SettlementPaymentsSolution,
};
use chia_wallet_sdk::chia::puzzle_types::Memos;
use chia_wallet_sdk::driver::DriverError;
use chia_wallet_sdk::prelude::{
    run_puzzle, Action, Allocator, AssetInfo, Bytes, Bytes32, Coin, CoinSpend, Condition,
    Conditions, FromClvm, Id, Layer, Nft, NodePtr, Offer, OptionContract, OptionInfo,
    OptionLauncher, OptionLauncherInfo, OptionType, OptionUnderlying, PublicKey, Puzzle, Relation,
    RequestedPayments, SettlementLayer, SingletonInfo, SpendBundle, SpendContext,
    SpendWithConditions, Spends, StandardLayer, ToClvm, ToTreeHash,
};
use chia_wallet_sdk::puzzles::SETTLEMENT_PAYMENT_HASH;
use chia_wallet_sdk::types::announcement_id;
use indexmap::IndexMap;

use crate::state::{from_hex, to_hex, CoinJson, OptionOrigin, OptionRecord, Phase, ProofJson};
use crate::wallet::{spend_selection, Selection};

/// The output value (singleton amount, odd) an option mint requires from the wallet.
pub const OPTION_OUTPUT_VALUE: u64 = 1;

/// The result of building an option mint.
#[derive(Debug, Clone)]
pub struct OptionOutcome {
    /// The live option singleton.
    pub option: OptionContract,
    /// The NFT after being locked into the option underlying.
    pub locked_nft: Nft,
    /// The option launcher id.
    pub launcher_id: Bytes32,
    /// The tree hash of the underlying delegated puzzle.
    pub underlying_delegated_puzzle_hash: Bytes32,
}

/// Builds an option mint locking `nft` as the underlying, with an XCH strike.
#[allow(clippy::too_many_arguments)]
pub fn build_create(
    ctx: &mut SpendContext,
    layer: &StandardLayer,
    nft: Nft,
    selection: &Selection,
    strike_amount: u64,
    expiration_seconds: u64,
    creator_puzzle_hash: Bytes32,
    owner_puzzle_hash: Bytes32,
    change_puzzle_hash: Bytes32,
    fee: u64,
) -> Result<OptionOutcome> {
    let parent = selection.coins[0];
    let underlying_amount = nft.coin.amount;

    let info = OptionLauncherInfo::new(
        creator_puzzle_hash,
        owner_puzzle_hash,
        expiration_seconds,
        underlying_amount,
        OptionType::Xch {
            amount: strike_amount,
        },
    );

    let launcher = OptionLauncher::new(ctx, parent.coin_id(), info, OPTION_OUTPUT_VALUE)?;
    let p2_option = launcher.p2_puzzle_hash();

    // Lock the NFT by transferring it into the option's underlying puzzle hash.
    let locked_nft = nft.transfer(ctx, layer, p2_option, Conditions::new())?;

    let launcher = launcher.with_underlying(locked_nft.coin.coin_id());
    let launcher_id = launcher.info().launcher_id;
    let underlying_delegated_puzzle_hash = launcher.info().underlying_delegated_puzzle_hash;

    let (mint_conditions, option) = launcher.mint(ctx)?;
    spend_selection(
        ctx,
        layer,
        selection,
        mint_conditions,
        change_puzzle_hash,
        fee,
    )?;

    Ok(OptionOutcome {
        option,
        locked_nft,
        launcher_id,
        underlying_delegated_puzzle_hash,
    })
}

/// Builds a persistable [`OptionRecord`] from an option outcome and its terms.
#[allow(clippy::too_many_arguments)]
pub fn option_to_record(
    outcome: &OptionOutcome,
    strike_amount: u64,
    expiration_seconds: u64,
    creator_puzzle_hash: Bytes32,
    owner_puzzle_hash: Bytes32,
    phase: Phase,
) -> OptionRecord {
    OptionRecord {
        launcher_id: to_hex(outcome.launcher_id),
        coin: CoinJson::from_coin(outcome.option.coin),
        proof: Some(ProofJson::from_proof(outcome.option.proof)),
        underlying_nft_coin: CoinJson::from_coin(outcome.locked_nft.coin),
        underlying_delegated_puzzle_hash: to_hex(outcome.underlying_delegated_puzzle_hash),
        strike_amount,
        expiration_seconds,
        creator_puzzle_hash: to_hex(creator_puzzle_hash),
        owner_puzzle_hash: to_hex(owner_puzzle_hash),
        phase,
        underlying_coin_id: Some(to_hex(outcome.locked_nft.coin.coin_id())),
        nft_launcher_id: Some(to_hex(outcome.locked_nft.info.launcher_id)),
        origin: OptionOrigin::Created,
        terms_known: true,
        underlying_reclaimed: false,
    }
}

/// Reconstructs the live [`OptionContract`] singleton from its persisted record so it can
/// be re-spent (e.g. to build an offer).
///
/// Requires the record to carry a `proof`. For older records that predate the persisted
/// proof, recover it from the chain with [`option_from_parent_spend`] instead.
pub fn option_from_record(record: &OptionRecord) -> Result<OptionContract> {
    let coin = record.coin.to_coin()?;
    let proof = record
        .proof
        .as_ref()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "option record is missing its singleton proof (created by an older version)"
            )
        })?
        .to_proof()?;
    let launcher_id = from_hex(&record.launcher_id)?;
    let underlying_coin_id = match &record.underlying_coin_id {
        Some(id) => from_hex(id)?,
        None => record.underlying_nft_coin.to_coin()?.coin_id(),
    };
    let underlying_delegated_puzzle_hash = from_hex(&record.underlying_delegated_puzzle_hash)?;
    // The option's inner p2 puzzle is the owner's standard puzzle.
    let p2_puzzle_hash = from_hex(&record.owner_puzzle_hash)?;

    let info = OptionInfo::new(
        launcher_id,
        underlying_coin_id,
        underlying_delegated_puzzle_hash,
        p2_puzzle_hash,
    );
    Ok(OptionContract::new(coin, proof, info))
}

/// Recovers the child [`OptionContract`] created by `parent_spend` (the spend of the
/// option's parent coin), including its lineage proof and info.
///
/// This is used to reconstruct an option whose record predates the persisted proof: fetch
/// the parent coin's puzzle-and-solution from the chain and parse the child from it.
pub fn option_from_parent_spend(parent_spend: &CoinSpend) -> Result<OptionContract> {
    option_child_from_spend(parent_spend)?
        .ok_or_else(|| anyhow::anyhow!("parent coin spend did not create an option child"))
}

/// Parses the option-singleton child (if any) created by a coin spend.
///
/// Returns `Ok(None)` when the spend did not create an option child — e.g. the option was
/// exercised (melted) or otherwise spent to a non-singleton. Used to follow the singleton
/// forward on-chain during a state sync.
pub fn option_child_from_spend(parent_spend: &CoinSpend) -> Result<Option<OptionContract>> {
    let mut allocator = Allocator::new();
    let puzzle_ptr = parent_spend.puzzle_reveal.to_clvm(&mut allocator)?;
    let puzzle = Puzzle::parse(&allocator, puzzle_ptr);
    let solution_ptr = parent_spend.solution.to_clvm(&mut allocator)?;

    match OptionContract::parse_child(&mut allocator, parent_spend.coin, puzzle, solution_ptr) {
        Ok(child) => Ok(child),
        // A melt (e.g. the option was exercised) produces no singleton child.
        Err(DriverError::MissingChild) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Terms recovered from an option launcher's on-chain metadata.
#[derive(Debug, Clone, Copy)]
pub struct RecoveredTerms {
    /// Absolute expiration, unix seconds.
    pub expiration_seconds: u64,
    /// The strike, expressed as the raw option type (this CLI only handles XCH).
    pub strike_type: OptionType,
}

/// Reads an option launcher's metadata (expiration + strike type) from the launcher coin's
/// spend. Used to recover the terms of a purchased option from the chain.
pub fn terms_from_launcher_spend(launcher_spend: &CoinSpend) -> Result<RecoveredTerms> {
    let mut allocator = Allocator::new();
    let solution_ptr = launcher_spend.solution.to_clvm(&mut allocator)?;
    let metadata = OptionContract::parse_metadata(&mut allocator, solution_ptr)?;
    Ok(RecoveredTerms {
        expiration_seconds: metadata.expiration_seconds,
        strike_type: metadata.strike_type,
    })
}

/// Recovers the option creator's puzzle hash from the memo attached to the launcher coin
/// when it was created (the launcher is minted with the creator puzzle hash as a hint).
///
/// `parent_spend` is the spend of the launcher coin's parent; `launcher_id` is the launcher
/// coin id. Returns `Ok(None)` if no matching create-coin/memo is found.
pub fn creator_from_launcher_creation(
    parent_spend: &CoinSpend,
    launcher_id: Bytes32,
) -> Result<Option<Bytes32>> {
    let mut allocator = Allocator::new();
    let puzzle_ptr = parent_spend.puzzle_reveal.to_clvm(&mut allocator)?;
    let solution_ptr = parent_spend.solution.to_clvm(&mut allocator)?;
    let output = run_puzzle(&mut allocator, puzzle_ptr, solution_ptr)?;
    let conditions = Vec::<Condition>::from_clvm(&allocator, output)?;

    for condition in conditions {
        let Some(create_coin) = condition.into_create_coin() else {
            continue;
        };
        let coin = Coin::new(
            parent_spend.coin.coin_id(),
            create_coin.puzzle_hash,
            create_coin.amount,
        );
        if coin.coin_id() != launcher_id {
            continue;
        }
        let Memos::Some(memos) = create_coin.memos else {
            return Ok(None);
        };
        if let Ok((hint, _)) = <(Bytes32, NodePtr)>::from_clvm(&allocator, memos) {
            return Ok(Some(hint));
        }
        return Ok(None);
    }
    Ok(None)
}

/// Verifies that a set of recovered option terms reproduces the option's on-chain
/// `underlying_delegated_puzzle_hash`. Only trust recovered terms when this returns true.
pub fn verify_terms(
    launcher_id: Bytes32,
    creator_puzzle_hash: Bytes32,
    expiration_seconds: u64,
    underlying_amount: u64,
    strike_type: OptionType,
    underlying_delegated_puzzle_hash: Bytes32,
) -> bool {
    let underlying = OptionUnderlying::new(
        launcher_id,
        creator_puzzle_hash,
        expiration_seconds,
        underlying_amount,
        strike_type,
    );
    Bytes32::from(underlying.delegated_puzzle().tree_hash()) == underlying_delegated_puzzle_hash
}

/// Builds a partial [`OptionRecord`] for an option acquired by taking an offer.
///
/// The terms (strike, expiration, creator) are not carried in an offer file, so they are
/// left as placeholders with `terms_known = false` until recovered from the chain.
pub fn purchased_option_record(outcome: &TakeOutcome, owner_puzzle_hash: Bytes32) -> OptionRecord {
    OptionRecord {
        launcher_id: to_hex(outcome.launcher_id),
        coin: CoinJson::from_coin(outcome.option.coin),
        proof: Some(ProofJson::from_proof(outcome.option.proof)),
        underlying_nft_coin: CoinJson::from_coin(Coin::new(
            Bytes32::default(),
            Bytes32::default(),
            0,
        )),
        underlying_delegated_puzzle_hash: to_hex(
            outcome.option.info.underlying_delegated_puzzle_hash,
        ),
        strike_amount: 0,
        expiration_seconds: 0,
        creator_puzzle_hash: to_hex(Bytes32::default()),
        owner_puzzle_hash: to_hex(owner_puzzle_hash),
        phase: Phase::Pending,
        underlying_coin_id: Some(to_hex(outcome.option.info.underlying_coin_id)),
        nft_launcher_id: None,
        origin: OptionOrigin::Purchased,
        terms_known: false,
        underlying_reclaimed: false,
    }
}

/// The requested-payment side of an offer, produced by [`build_offer`] and consumed by
/// [`finalize_offer`] after the maker has signed the offered spend.
#[derive(Debug, Clone)]
pub struct OfferParts {
    /// What the maker wants in return (XCH, here).
    pub requested_payments: RequestedPayments,
    /// Metadata for reconstructing requested-asset puzzle hashes (empty for XCH).
    pub asset_info: AssetInfo,
}

/// Builds the maker side of an offer that sells `option` in exchange for `request_mojos` of
/// XCH paid to `maker_puzzle_hash`.
///
/// This spends the option singleton into the settlement puzzle (locking it into the offer)
/// while asserting the requested XCH payment, and adds the resulting coin spend to `ctx`.
/// The caller must then sign `ctx.take()` and pass the signed bundle to [`finalize_offer`].
pub fn build_offer(
    ctx: &mut SpendContext,
    owner_layer: &StandardLayer,
    option: OptionContract,
    maker_puzzle_hash: Bytes32,
    request_mojos: u64,
) -> Result<OfferParts> {
    // The nonce binds the requested payment to the specific option coin being offered.
    let nonce = Offer::nonce(vec![option.coin.coin_id()]);
    let hint = ctx.hint(maker_puzzle_hash)?;

    let mut requested_payments = RequestedPayments::new();
    requested_payments.xch.push(NotarizedPayment::new(
        nonce,
        vec![Payment::new(maker_puzzle_hash, request_mojos, hint)],
    ));
    let asset_info = AssetInfo::new();

    // Assert the settlement payment so the offered option can only be taken if paid for.
    let assertions = requested_payments.assertions(ctx, &asset_info)?;
    let extra_conditions = Conditions::new().extend(assertions);

    // The offered option is now locked at the settlement puzzle; we don't need the handle.
    let _locked = option.transfer(
        ctx,
        owner_layer,
        SETTLEMENT_PAYMENT_HASH.into(),
        extra_conditions,
    )?;

    Ok(OfferParts {
        requested_payments,
        asset_info,
    })
}

/// Combines the maker's signed offered spend with the requested payments into a complete
/// offer spend bundle (appending the settlement payment placeholder spends). The result is
/// ready to be serialized with `encode_offer`.
pub fn finalize_offer(
    ctx: &mut SpendContext,
    signed_partial: SpendBundle,
    parts: OfferParts,
) -> Result<SpendBundle> {
    let mut allocator = Allocator::new();
    let offer = Offer::from_input_spend_bundle(
        &mut allocator,
        signed_partial,
        parts.requested_payments,
        parts.asset_info,
    )?;
    Ok(offer.to_spend_bundle(ctx)?)
}

/// The single option an offer sells, together with the XCH it asks for.
///
/// An offer file carries the option's identity and its link to the underlying, but not its
/// terms (strike, expiration, creator) — those live on-chain and have to be looked up.
#[derive(Debug, Clone, Copy)]
pub struct OfferedOption {
    /// The offered option's launcher id.
    pub launcher_id: Bytes32,
    /// The option coin as it sits inside the offer, locked at the settlement puzzle.
    pub settlement_coin: Coin,
    /// The maker's option coin that settles the offer. It must still be unspent, otherwise
    /// the offer has already been taken (or cancelled by spending it another way).
    pub maker_coin_id: Bytes32,
    /// The coin id of the underlying the option is bound to.
    pub underlying_coin_id: Bytes32,
    /// The tree hash of the underlying delegated puzzle, which commits to the option terms.
    pub underlying_delegated_puzzle_hash: Bytes32,
    /// The XCH (in mojos) the maker asks for.
    pub request_mojos: u64,
}

/// Extracts the option an offer sells, rejecting offers this CLI cannot handle.
///
/// Only "one option for XCH" offers are supported: anything else (CATs, NFTs, several
/// options) is rejected here rather than partway through building a spend.
pub fn offered_option(offer: &Offer) -> Result<OfferedOption> {
    let requested = offer.requested_payments();
    if !requested.cats.is_empty() || !requested.nfts.is_empty() || !requested.options.is_empty() {
        anyhow::bail!("offer requests non-XCH assets, which this CLI cannot provide");
    }
    let request_mojos = requested.amounts().xch;
    if request_mojos == 0 {
        anyhow::bail!("offer does not request any XCH");
    }

    let offered = offer.offered_coins();
    if !offered.xch.is_empty() || !offered.cats.is_empty() || !offered.nfts.is_empty() {
        anyhow::bail!("offer includes non-option assets, which this CLI cannot receive");
    }
    if offered.options.len() != 1 {
        anyhow::bail!(
            "expected exactly one offered option, found {}",
            offered.options.len()
        );
    }
    let (&launcher_id, option) = offered.options.iter().next().expect("one option");

    Ok(OfferedOption {
        launcher_id,
        settlement_coin: option.coin,
        maker_coin_id: option.coin.parent_coin_info,
        underlying_coin_id: option.info.underlying_coin_id,
        underlying_delegated_puzzle_hash: option.info.underlying_delegated_puzzle_hash,
        request_mojos,
    })
}

/// The result of taking (accepting) an option offer.
#[derive(Debug, Clone)]
pub struct TakeOutcome {
    /// The acquired option singleton, now owned by the taker.
    pub option: OptionContract,
    /// The option launcher id.
    pub launcher_id: Bytes32,
    /// The XCH (in mojos) paid to the maker.
    pub paid_mojos: u64,
}

/// Builds the taker side of an "option for XCH" offer: pays the requested XCH (plus an
/// optional `fee`) from `source_coins` and receives the offered option at
/// `taker_puzzle_hash`.
///
/// The offered spends and the taker's coin spends are added to `ctx`; the caller must sign
/// `ctx.take()` and merge it into the offer via [`Offer::take`] before submitting. Rejects
/// offers that request non-XCH assets or that offer anything other than a single option,
/// since this CLI cannot honor those.
pub fn build_take(
    ctx: &mut SpendContext,
    offer: &Offer,
    source_coins: &[Coin],
    taker_puzzle_hash: Bytes32,
    taker_synthetic_pk: PublicKey,
    fee: u64,
) -> Result<TakeOutcome> {
    let OfferedOption {
        launcher_id,
        request_mojos,
        ..
    } = offered_option(offer)?;

    let mut spends = Spends::new(taker_puzzle_hash);
    spends.add(offer.offered_coins().clone());
    for &coin in source_coins {
        spends.add(coin);
    }

    let mut actions = offer.requested_payments().actions();
    if fee > 0 {
        actions.push(Action::fee(fee));
    }

    let deltas = spends.apply(ctx, &actions)?;

    let mut synthetic_keys = IndexMap::new();
    synthetic_keys.insert(taker_puzzle_hash, taker_synthetic_pk);
    let outputs =
        spends.finish_with_keys(ctx, &deltas, Relation::AssertConcurrent, &synthetic_keys)?;

    let option = outputs
        .options
        .get(&Id::Existing(launcher_id))
        .copied()
        .ok_or_else(|| anyhow::anyhow!("offer settlement did not yield the option output"))?;

    Ok(TakeOutcome {
        option,
        launcher_id,
        paid_mojos: request_mojos,
    })
}

/// The result of exercising an option.
#[derive(Debug, Clone)]
pub struct ExerciseOutcome {
    /// The underlying NFT, now released to and owned by the exerciser.
    pub nft: Nft,
    /// The settlement coin created to pay the strike to the creator.
    pub strike_settlement_coin: Coin,
}

/// Builds an exercise of an XCH-strike option whose underlying is `locked_nft`.
///
/// Exercising atomically: (1) melts the option singleton while authorizing the underlying
/// spend, (2) releases the NFT out of the option underlying puzzle and claims it to
/// `owner_puzzle_hash`, and (3) pays `strike_amount` XCH to the creator from `selection`.
/// All the coin spends are added to `ctx`; the caller signs `ctx.take()` and submits it.
///
/// The exercise is only valid before `expiration_seconds` (enforced on-chain by the
/// underlying puzzle).
#[allow(clippy::too_many_arguments)]
pub fn build_exercise(
    ctx: &mut SpendContext,
    owner_layer: &StandardLayer,
    option: OptionContract,
    locked_nft: Nft,
    creator_puzzle_hash: Bytes32,
    expiration_seconds: u64,
    strike_amount: u64,
    owner_puzzle_hash: Bytes32,
    selection: &Selection,
    change_puzzle_hash: Bytes32,
    fee: u64,
) -> Result<ExerciseOutcome> {
    let underlying_amount = locked_nft.coin.amount;
    let underlying = OptionUnderlying::new(
        option.info.launcher_id,
        creator_puzzle_hash,
        expiration_seconds,
        underlying_amount,
        OptionType::Xch {
            amount: strike_amount,
        },
    );

    // The reconstructed underlying must match the puzzle the NFT is actually locked in.
    if Bytes32::from(underlying.tree_hash()) != locked_nft.info.p2_puzzle_hash {
        anyhow::bail!(
            "reconstructed option underlying does not match the locked NFT; state may be inconsistent"
        );
    }

    let option_inner_puzzle_hash: Bytes32 = option.info.inner_puzzle_hash().into();
    let option_amount = option.coin.amount;

    // 1. Spend the option singleton: melt it and authorize the underlying via a message.
    option.exercise(ctx, owner_layer, Conditions::new())?;

    // 2. Release the NFT out of the underlying puzzle (it lands at the settlement puzzle)...
    let exercise_spend = underlying.exercise_spend(ctx, option_inner_puzzle_hash, option_amount)?;
    let released_nft = locked_nft.spend(ctx, exercise_spend)?;

    // ...then immediately claim it to the exerciser.
    let nft_hint = ctx.hint(owner_puzzle_hash)?;
    let claim_payment = NotarizedPayment::new(
        underlying.launcher_id,
        vec![Payment::new(owner_puzzle_hash, underlying_amount, nft_hint)],
    );
    let nft = released_nft.unlock_settlement(ctx, vec![claim_payment])?;

    // 3. Fund the strike settlement coin from the wallet, then pay it out to the creator.
    let strike_primary =
        Conditions::new().create_coin(SETTLEMENT_PAYMENT_HASH.into(), strike_amount, Memos::None);
    spend_selection(
        ctx,
        owner_layer,
        selection,
        strike_primary,
        change_puzzle_hash,
        fee,
    )?;

    let strike_settlement_coin = Coin::new(
        selection.coins[0].coin_id(),
        SETTLEMENT_PAYMENT_HASH.into(),
        strike_amount,
    );

    let payment = underlying.requested_payment(&mut **ctx)?;
    let strike_spend = SettlementLayer.construct_coin_spend(
        ctx,
        strike_settlement_coin,
        SettlementPaymentsSolution::new(vec![payment]),
    )?;
    ctx.insert(strike_spend);

    Ok(ExerciseOutcome {
        nft,
        strike_settlement_coin,
    })
}

/// The announcement message binding the fee-paying inputs to the NFT clawback spend, so the
/// fee cannot be confirmed on its own without also reclaiming the NFT.
const CLAWBACK_BIND_NONCE: &[u8] = b"pringle-clawback";

/// The result of clawing back an expired option's underlying NFT.
#[derive(Debug, Clone)]
pub struct ClawbackOutcome {
    /// The reclaimed NFT, recreated under the creator's control.
    pub nft: Nft,
}

/// Builds a creator clawback of an expired option's underlying NFT.
///
/// After the option's absolute expiration passes, the creator can reclaim the locked NFT
/// through the underlying puzzle's time-locked clawback path, which enforces
/// `ASSERT_SECONDS_ABSOLUTE` (the expiry deadline) and requires the creator's key. The NFT is
/// recreated at `reclaim_puzzle_hash`, still without an assigned owner.
///
/// The option singleton is deliberately not spent: clawback only touches the underlying, so
/// the expired option coin remains as an inert singleton. Any `fee` is funded from separate
/// regular-XCH `fee_selection` inputs, which are bound to the NFT spend by a coin
/// announcement so they cannot be confirmed without the reclaim.
#[allow(clippy::too_many_arguments)]
pub fn build_clawback(
    ctx: &mut SpendContext,
    creator_layer: &StandardLayer,
    option_launcher_id: Bytes32,
    locked_nft: Nft,
    creator_puzzle_hash: Bytes32,
    expiration_seconds: u64,
    strike_amount: u64,
    reclaim_puzzle_hash: Bytes32,
    fee_selection: Option<&Selection>,
    change_puzzle_hash: Bytes32,
    fee: u64,
) -> Result<ClawbackOutcome> {
    let underlying_amount = locked_nft.coin.amount;
    let underlying = OptionUnderlying::new(
        option_launcher_id,
        creator_puzzle_hash,
        expiration_seconds,
        underlying_amount,
        OptionType::Xch {
            amount: strike_amount,
        },
    );

    // The reconstructed underlying must match the puzzle the NFT is actually locked in.
    if Bytes32::from(underlying.tree_hash()) != locked_nft.info.p2_puzzle_hash {
        anyhow::bail!(
            "reconstructed option underlying does not match the locked NFT; state may be inconsistent"
        );
    }

    // Recreate the NFT under the creator's control. When a fee is paid from separate coins,
    // emit an announcement from this spend so those coins can be bound to it.
    let hint = ctx.hint(reclaim_puzzle_hash)?;
    let mut inner = Conditions::new().create_coin(reclaim_puzzle_hash, underlying_amount, hint);
    if fee > 0 {
        let message: Bytes = CLAWBACK_BIND_NONCE.to_vec().into();
        inner = inner.create_coin_announcement(message);
    }

    // The clawback path wraps the creator's standard spend in the time-locked augmented layer
    // (which prepends the `ASSERT_SECONDS_ABSOLUTE` deadline) and the underlying's 1-of-n.
    let inner_spend = creator_layer.spend_with_conditions(ctx, inner)?;
    let clawback_spend = underlying.clawback_spend(ctx, inner_spend)?;
    let nft = locked_nft.spend(ctx, clawback_spend)?;

    // Fund the fee from separate regular-XCH inputs, bound to the NFT spend.
    if fee > 0 {
        let selection = fee_selection.ok_or_else(|| {
            anyhow::anyhow!("a fee was requested but no coins were provided to pay it")
        })?;
        let ann = announcement_id(locked_nft.coin.coin_id(), CLAWBACK_BIND_NONCE);
        let primary = Conditions::new().assert_coin_announcement(ann);
        spend_selection(
            ctx,
            creator_layer,
            selection,
            primary,
            change_puzzle_hash,
            fee,
        )?;
    }

    Ok(ClawbackOutcome { nft })
}
