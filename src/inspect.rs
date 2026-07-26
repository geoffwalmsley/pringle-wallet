//! Resolving an option's real terms and backing value from the chain.
//!
//! An option coin — whether it is tracked in local state or merely referenced by an offer
//! file someone sent you — carries no terms of its own. The strike, expiration, and creator
//! are committed to by the underlying's delegated puzzle hash, so they have to be read back
//! out of the launcher spend and then verified against that commitment. Everything here is
//! read-only: it looks things up and hands back facts, and never touches local state.

use anyhow::Result;
use chia_wallet_sdk::prelude::{Bytes32, Coin, OptionType};

use crate::coinset::Coinset;
use crate::nft;
use crate::option as option_contract;
use crate::output::AppError;
use crate::p2_singleton;
use crate::state::{NftRecord, Phase};

/// An option's verified terms and underlying, read from the chain.
#[derive(Debug, Clone)]
pub struct OptionDetails {
    /// The XCH strike (in mojos) the exerciser pays the creator.
    pub strike_amount: u64,
    /// Absolute expiration, unix seconds. Exercise is impossible from then on.
    pub expiration_seconds: u64,
    /// The puzzle hash that receives the strike payment.
    pub creator_puzzle_hash: Bytes32,
    /// The live underlying NFT coin, still locked in the option underlying puzzle.
    pub underlying_coin: Coin,
    /// The reconstructed underlying NFT (launcher id, metadata, proof).
    pub underlying_nft: NftRecord,
}

/// Reads an option's terms from the chain and verifies them against the on-chain contract.
///
/// The three inputs are everything an option coin knows about itself; they are available
/// both from a persisted [`crate::state::OptionRecord`] and from an option sitting inside an
/// offer. The recovered terms are only returned once they reproduce
/// `underlying_delegated_puzzle_hash`, so a successful result is trustworthy.
pub async fn recover_option_details(
    coinset: &Coinset,
    launcher_id: Bytes32,
    underlying_coin_id: Bytes32,
    underlying_delegated_puzzle_hash: Bytes32,
) -> Result<OptionDetails> {
    // 1. Read the launcher metadata (expiration + strike type).
    let launcher_spend = coinset.coin_spend(launcher_id).await?.ok_or_else(|| {
        AppError::recoverable("option launcher coin has no recorded spend on-chain")
            .why("the option may not be confirmed yet")
            .next("wait for confirmation, then try again")
    })?;
    let terms = option_contract::terms_from_launcher_spend(&launcher_spend)?;
    let strike_amount = match terms.strike_type {
        OptionType::Xch { amount } => amount,
        _ => {
            return Err(AppError::recoverable(
                "this option has a non-XCH strike, which this CLI does not support",
            )
            .into())
        }
    };

    // 2. Recover the creator puzzle hash from the launcher creation memo.
    let launcher_record = coinset
        .coin_record(launcher_id)
        .await?
        .ok_or_else(|| AppError::recoverable("option launcher coin not found on-chain"))?;
    let parent_spend = coinset
        .coin_spend(launcher_record.coin.parent_coin_info)
        .await?
        .ok_or_else(|| {
            AppError::recoverable(
                "could not fetch the launcher's parent spend to recover the creator",
            )
        })?;
    let creator = option_contract::creator_from_launcher_creation(&parent_spend, launcher_id)?
        .ok_or_else(|| {
            AppError::recoverable("could not recover the option creator from the launcher memo")
        })?;

    // 3. Reconstruct the locked underlying NFT from its parent spend.
    let underlying_record = coinset
        .coin_record(underlying_coin_id)
        .await?
        .ok_or_else(|| AppError::recoverable("underlying NFT coin not found on-chain"))?;
    let underlying_parent_spend = coinset
        .coin_spend(underlying_record.coin.parent_coin_info)
        .await?
        .ok_or_else(|| {
            AppError::recoverable("could not fetch the underlying NFT's parent spend")
        })?;
    let underlying_nft =
        nft::nft_record_from_parent_spend(&underlying_parent_spend, Phase::Superseded)?
            .ok_or_else(|| AppError::recoverable("could not reconstruct the underlying NFT"))?;

    // 4. Only trust the terms once they reproduce the on-chain delegated puzzle hash.
    if !option_contract::verify_terms(
        launcher_id,
        creator,
        terms.expiration_seconds,
        underlying_record.coin.amount,
        terms.strike_type,
        underlying_delegated_puzzle_hash,
    ) {
        return Err(AppError::recoverable(
            "recovered option terms failed verification against the on-chain contract",
        )
        .into());
    }

    Ok(OptionDetails {
        strike_amount,
        expiration_seconds: terms.expiration_seconds,
        creator_puzzle_hash: creator,
        underlying_coin: underlying_record.coin,
        underlying_nft,
    })
}

/// The confirmed balance sitting at the p2 singleton an NFT controls.
#[derive(Debug, Clone)]
pub struct P2SingletonBalance {
    /// The income address funds are sent to.
    pub address: String,
    /// The confirmed, unspent coins held there.
    pub coins: Vec<Coin>,
    /// Their combined value in mojos.
    pub total_mojos: u64,
}

/// Looks up the income an NFT's p2 singleton is currently holding.
///
/// This is the value that comes with the NFT when an option is exercised, so it is the
/// number that matters most when deciding what an option is worth.
pub async fn p2_singleton_balance(
    coinset: &Coinset,
    nft_launcher_id: Bytes32,
) -> Result<P2SingletonBalance> {
    let coins = coinset
        .unspent_coins(p2_singleton::puzzle_hash(nft_launcher_id))
        .await?;
    let total_mojos = coins
        .iter()
        .try_fold(0u64, |sum, coin| sum.checked_add(coin.amount))
        .ok_or_else(|| anyhow::anyhow!("p2_singleton balance overflows u64"))?;

    Ok(P2SingletonBalance {
        address: p2_singleton::address(nft_launcher_id)?,
        coins,
        total_mojos,
    })
}
