//! The "sweep" option kind: exercising claims the income accumulated at the NFT's
//! p2_singleton address instead of taking ownership of the NFT.
//!
//! Both option kinds lock the NFT into the *identical* [`OptionUnderlying`] puzzle — its
//! merkle tree commits only to the option launcher id (exercise path) and the expiry plus
//! creator (clawback path), never to the delegated puzzle. The two kinds differ solely in
//! the delegated puzzle the option singleton commits to (via its curried
//! `underlying_delegated_puzzle_hash`), which is what the option sends to the NFT on
//! exercise.
//!
//! The standard ("transfer") delegated puzzle pushes the NFT into the settlement puzzle so
//! the holder can claim it. The sweep delegated puzzle instead forces the NFT straight back
//! to the creator and lets the holder drain the p2_singleton coins to themselves. It is
//! composed entirely from audited stock puzzles (`augmented_condition`) wrapped around the
//! CLVM identity puzzle `1`, so there is no bespoke Chialisp to compile:
//!
//! ```text
//! AC(ASSERT_BEFORE_SECONDS_ABSOLUTE expiry,
//!  AC(ASSERT_PUZZLE_ANNOUNCEMENT strike_paid,
//!   AC(CREATE_COIN creator 1 (hint),
//!      1)))            ; the identity tail returns the holder-supplied conditions
//! ```
//!
//! The forced odd `CREATE_COIN` back to the creator is the safety mechanism: the singleton
//! top layer (and the NFT ownership layer) permit exactly one odd output, so the holder can
//! neither redirect nor melt the NFT, and the even payout they add is capped by the
//! p2_singleton coins actually co-spent in the bundle.

use chia_wallet_sdk::chia::puzzle_types::Memos;
use chia_wallet_sdk::clvm_utils::TreeHasher;
use chia_wallet_sdk::driver::{InnerPuzzleSpend, Layer, MipsSpend, P2OneOfManyLayer};
use chia_wallet_sdk::prelude::{
    Bytes32, Conditions, DriverError, NodePtr, OptionType, OptionUnderlying, Spend, SpendContext,
    ToTreeHash, TreeHash,
};
use chia_wallet_sdk::puzzles::SETTLEMENT_PAYMENT_HASH;
use chia_wallet_sdk::types::conditions::{AssertBeforeSecondsAbsolute, CreateCoin};
use chia_wallet_sdk::types::payment_assertion;
use chia_wallet_sdk::types::puzzles::{
    AugmentedConditionArgs, AugmentedConditionSolution, P2OneOfManySolution, SingletonMember,
    SingletonMemberSolution,
};

/// The immutable terms a sweep option's delegated puzzle commits to. These are exactly the
/// parameters of the standard [`OptionUnderlying`] (this CLI only handles XCH strikes), so
/// the NFT lock is identical between the two kinds.
#[derive(Debug, Clone, Copy)]
pub struct SweepTerms {
    /// The option launcher id.
    pub launcher_id: Bytes32,
    /// The puzzle hash that receives the strike payment and the returned NFT.
    pub creator_puzzle_hash: Bytes32,
    /// Absolute expiration, unix seconds (exercise is impossible from then on).
    pub expiration_seconds: u64,
    /// The underlying NFT's coin amount (always 1 for a singleton NFT).
    pub underlying_amount: u64,
    /// The XCH strike (in mojos) paid to the creator on exercise.
    pub strike_amount: u64,
}

impl SweepTerms {
    /// The [`OptionUnderlying`] the NFT is locked into. Identical to the standard kind, so
    /// the lock puzzle hash, clawback path, and strike settlement are all shared.
    pub fn underlying(&self) -> OptionUnderlying {
        OptionUnderlying::new(
            self.launcher_id,
            self.creator_puzzle_hash,
            self.expiration_seconds,
            self.underlying_amount,
            OptionType::Xch {
                amount: self.strike_amount,
            },
        )
    }
}

/// Builds the sweep delegated puzzle for `terms`. The puzzle is fully determined by the
/// terms; the holder's per-exercise choices (which coins, where the payout goes) live in the
/// solution built by [`sweep_exercise_spend`], not here.
pub fn delegated_puzzle(
    ctx: &mut SpendContext,
    terms: &SweepTerms,
) -> Result<NodePtr, DriverError> {
    let underlying = terms.underlying();

    // The strike assertion is identical to the standard delegated puzzle: it forces a
    // settlement coin paying the strike to the creator to exist in the same bundle.
    let payment_hash = underlying
        .requested_payment(&mut TreeHasher)
        .expect("failed to hash requested payment")
        .tree_hash();
    let strike_assertion = payment_assertion(SETTLEMENT_PAYMENT_HASH.into(), payment_hash);

    // The forced odd output: recreate the NFT under the creator's control, hinted so the
    // creator's wallet can discover it (mirrors `Nft::transfer`).
    let hint = ctx.hint(terms.creator_puzzle_hash)?;
    let create_nft = CreateCoin::new(terms.creator_puzzle_hash, terms.underlying_amount, hint);

    // The identity puzzle `1` returns its solution verbatim, which is where the holder's
    // announcements and even payout are fed in during exercise.
    let identity = ctx.alloc(&1)?;

    let ac_create = ctx.curry(AugmentedConditionArgs::<NodePtr, NodePtr>::new(
        create_nft.into(),
        identity,
    ))?;
    let ac_strike = ctx.curry(AugmentedConditionArgs::<NodePtr, NodePtr>::new(
        strike_assertion.into(),
        ac_create,
    ))?;
    let ac_expiry = ctx.curry(AugmentedConditionArgs::<NodePtr, NodePtr>::new(
        AssertBeforeSecondsAbsolute::new(terms.expiration_seconds).into(),
        ac_strike,
    ))?;

    Ok(ac_expiry)
}

/// The tree hash of the sweep delegated puzzle — the value the option singleton commits to
/// as its `underlying_delegated_puzzle_hash`.
pub fn delegated_puzzle_hash(
    ctx: &mut SpendContext,
    terms: &SweepTerms,
) -> Result<Bytes32, DriverError> {
    let puzzle = delegated_puzzle(ctx, terms)?;
    Ok(ctx.tree_hash(puzzle).into())
}

/// Verifies that `terms` reproduce an option's on-chain `underlying_delegated_puzzle_hash`
/// under the sweep construction. Mirrors [`crate::option::verify_terms`] for the sweep kind:
/// only trust recovered terms as "sweep" when this returns true.
pub fn verify_sweep_terms(terms: &SweepTerms, underlying_delegated_puzzle_hash: Bytes32) -> bool {
    let mut ctx = SpendContext::new();
    match delegated_puzzle_hash(&mut ctx, terms) {
        Ok(hash) => hash == underlying_delegated_puzzle_hash,
        Err(_) => false,
    }
}

/// Builds the NFT's exercise [`Spend`] for a sweep option: releases the NFT out of the
/// `OptionUnderlying` via the exercise merkle path, running the sweep delegated puzzle with a
/// solution that emits one puzzle announcement per co-spent p2_singleton coin, an even
/// payout to `payout_puzzle_hash`, and (when the swept total is odd) a 1-mojo fee donation to
/// keep the payout even.
///
/// This mirrors [`OptionUnderlying::exercise_spend`], substituting the sweep delegated puzzle
/// and a non-trivial delegated solution for the hardcoded settlement puzzle and nil solution.
#[allow(clippy::too_many_arguments)]
pub fn sweep_exercise_spend(
    ctx: &mut SpendContext,
    terms: &SweepTerms,
    p2_coin_ids: &[Bytes32],
    payout_puzzle_hash: Bytes32,
    swept_amount: u64,
    odd_donation: u64,
    option_inner_puzzle_hash: Bytes32,
    option_amount: u64,
    extra_tail: Conditions,
) -> Result<Spend, DriverError> {
    let underlying = terms.underlying();
    let merkle_tree = underlying.merkle_tree();
    let custody_hash: TreeHash = underlying.exercise_path_hash().into();
    let merkle_proof = merkle_tree
        .proof(custody_hash.into())
        .ok_or(DriverError::InvalidMerkleProof)?;

    // The holder-supplied tail: authorize each p2_singleton coin by announcing its id (the
    // p2_singleton puzzle asserts this), pay out the even swept balance, and donate any odd
    // remainder to the fee so the singleton layer's single-odd-output rule is satisfied.
    //
    // `extra_tail` is normally empty; it exists so adversarial tests can inject extra
    // conditions (e.g. a second odd CREATE_COIN) and confirm the singleton layer rejects them.
    let mut tail = Conditions::new();
    for &coin_id in p2_coin_ids {
        tail = tail.create_puzzle_announcement(coin_id.into());
    }
    tail = tail.create_coin(payout_puzzle_hash, swept_amount, Memos::None);
    if odd_donation > 0 {
        tail = tail.reserve_fee(odd_donation);
    }
    tail = tail.extend(extra_tail);
    let tail_ptr = ctx.alloc(&tail)?;

    // Nest one AugmentedConditionSolution per augmented_condition wrapper, innermost feeding
    // the tail straight to the identity puzzle.
    let delegated_solution = ctx.alloc(&AugmentedConditionSolution::new(
        AugmentedConditionSolution::new(AugmentedConditionSolution::new(tail_ptr)),
    ))?;

    let delegated_puzzle = delegated_puzzle(ctx, terms)?;
    let delegated_spend = Spend::new(delegated_puzzle, delegated_solution);

    let mut mips = MipsSpend::new(delegated_spend);
    let singleton_member_puzzle = ctx.curry(SingletonMember::new(terms.launcher_id))?;
    let singleton_member_solution = ctx.alloc(&SingletonMemberSolution::new(
        option_inner_puzzle_hash,
        option_amount,
    ))?;
    mips.members.insert(
        custody_hash,
        InnerPuzzleSpend::new(
            0,
            Vec::new(),
            Spend::new(singleton_member_puzzle, singleton_member_solution),
        ),
    );

    let spend = mips.spend(ctx, custody_hash)?;

    P2OneOfManyLayer::new(merkle_tree.root()).construct_spend(
        ctx,
        P2OneOfManySolution::new(merkle_proof, spend.puzzle, spend.solution),
    )
}
