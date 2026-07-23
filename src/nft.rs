//! NFT minting and (de)serialization of the NFT for later re-spending.

use anyhow::Result;
use chia_wallet_sdk::chia::puzzle_types::nft::NftMetadata;
use chia_wallet_sdk::driver::DriverError;
use chia_wallet_sdk::prelude::{
    Allocator, Bytes32, CoinSpend, FromClvm, Launcher, Nft, NftInfo, NftMint, Puzzle, SpendContext,
    StandardLayer, ToClvm,
};

use crate::state::{from_hex, to_hex, CoinJson, MetadataJson, NftRecord, Phase, ProofJson};
use crate::wallet::{spend_selection, Selection};

/// Mints a singleton NFT owned by `wallet_puzzle_hash`, funded by `selection`.
///
/// The launcher is parented to the first selected coin (amount 1 for the singleton),
/// and no DID owner is assigned. Returns the resulting live NFT.
pub fn build_mint(
    ctx: &mut SpendContext,
    layer: &StandardLayer,
    wallet_puzzle_hash: Bytes32,
    selection: &Selection,
    metadata: &NftMetadata,
    royalty_basis_points: u16,
    fee: u64,
) -> Result<Nft> {
    let parent = selection.coins[0];
    let metadata_ptr = ctx.alloc_hashed(metadata)?;

    let launcher = Launcher::new(parent.coin_id(), 1);
    let (mint_conditions, nft) = launcher.mint_nft(
        ctx,
        &NftMint::new(metadata_ptr, wallet_puzzle_hash, royalty_basis_points, None),
    )?;

    spend_selection(
        ctx,
        layer,
        selection,
        mint_conditions,
        wallet_puzzle_hash,
        fee,
    )?;

    Ok(nft)
}

/// The output value (launcher/singleton amount) an NFT mint requires from the wallet.
pub const NFT_MINT_OUTPUT_VALUE: u64 = 1;

/// Converts an [`NftMetadata`] into its serializable form.
pub fn metadata_to_json(metadata: &NftMetadata) -> MetadataJson {
    MetadataJson {
        edition_number: metadata.edition_number,
        edition_total: metadata.edition_total,
        data_uris: metadata.data_uris.clone(),
        data_hash: metadata.data_hash.map(to_hex),
        metadata_uris: metadata.metadata_uris.clone(),
        metadata_hash: metadata.metadata_hash.map(to_hex),
        license_uris: metadata.license_uris.clone(),
        license_hash: metadata.license_hash.map(to_hex),
    }
}

/// Reconstructs an [`NftMetadata`] from its serializable form.
pub fn metadata_from_json(json: &MetadataJson) -> Result<NftMetadata> {
    Ok(NftMetadata {
        edition_number: json.edition_number,
        edition_total: json.edition_total,
        data_uris: json.data_uris.clone(),
        data_hash: json.data_hash.as_deref().map(from_hex).transpose()?,
        metadata_uris: json.metadata_uris.clone(),
        metadata_hash: json.metadata_hash.as_deref().map(from_hex).transpose()?,
        license_uris: json.license_uris.clone(),
        license_hash: json.license_hash.as_deref().map(from_hex).transpose()?,
    })
}

/// Builds a persistable [`NftRecord`] from a live NFT and its metadata.
pub fn nft_to_record(nft: &Nft, metadata: &NftMetadata, phase: Phase) -> NftRecord {
    NftRecord {
        launcher_id: to_hex(nft.info.launcher_id),
        coin: CoinJson::from_coin(nft.coin),
        proof: ProofJson::from_proof(nft.proof),
        metadata: metadata_to_json(metadata),
        metadata_updater_puzzle_hash: to_hex(nft.info.metadata_updater_puzzle_hash),
        current_owner: nft.info.current_owner.map(to_hex),
        royalty_puzzle_hash: to_hex(nft.info.royalty_puzzle_hash),
        royalty_basis_points: nft.info.royalty_basis_points,
        p2_puzzle_hash: to_hex(nft.info.p2_puzzle_hash),
        phase,
    }
}

/// Parses the NFT-singleton child (if any) created by a coin spend.
///
/// Returns `Ok(None)` when the spend did not create an NFT child (e.g. the coin was melted).
/// Used to follow the NFT singleton forward on-chain during a state sync. The transfer
/// program and metadata updater are applied automatically, so the returned NFT carries the
/// correct owner, p2 puzzle, and metadata.
pub fn nft_child_from_spend(parent_spend: &CoinSpend) -> Result<Option<Nft>> {
    let mut allocator = Allocator::new();
    let puzzle_ptr = parent_spend.puzzle_reveal.to_clvm(&mut allocator)?;
    let puzzle = Puzzle::parse(&allocator, puzzle_ptr);
    let solution_ptr = parent_spend.solution.to_clvm(&mut allocator)?;

    match Nft::parse_child(&mut allocator, parent_spend.coin, puzzle, solution_ptr) {
        Ok(child) => Ok(child),
        // A melt produces no singleton child (unusual for an NFT, but handle it gracefully).
        Err(DriverError::MissingChild) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Reconstructs an [`NftRecord`] (including decoded metadata) for the NFT child created by a
/// parent coin spend. Used to recover a locked underlying NFT from the chain.
///
/// Returns `Ok(None)` if the spend did not create an NFT child.
pub fn nft_record_from_parent_spend(
    parent_spend: &CoinSpend,
    phase: Phase,
) -> Result<Option<NftRecord>> {
    let mut allocator = Allocator::new();
    let puzzle_ptr = parent_spend.puzzle_reveal.to_clvm(&mut allocator)?;
    let puzzle = Puzzle::parse(&allocator, puzzle_ptr);
    let solution_ptr = parent_spend.solution.to_clvm(&mut allocator)?;

    let nft = match Nft::parse_child(&mut allocator, parent_spend.coin, puzzle, solution_ptr) {
        Ok(Some(nft)) => nft,
        Ok(None) => return Ok(None),
        Err(DriverError::MissingChild) => return Ok(None),
        Err(err) => return Err(err.into()),
    };

    let metadata = NftMetadata::from_clvm(&allocator, nft.info.metadata.ptr())
        .map_err(|e| anyhow::anyhow!("failed to decode recovered NFT metadata: {e}"))?;
    Ok(Some(nft_to_record(&nft, &metadata, phase)))
}

/// Reconstructs a live [`Nft`] from a persisted record so it can be re-spent.
pub fn nft_from_record(ctx: &mut SpendContext, record: &NftRecord) -> Result<Nft> {
    let metadata = metadata_from_json(&record.metadata)?;
    let metadata_ptr = ctx.alloc_hashed(&metadata)?;

    let info = NftInfo::new(
        from_hex(&record.launcher_id)?,
        metadata_ptr,
        from_hex(&record.metadata_updater_puzzle_hash)?,
        record.current_owner.as_deref().map(from_hex).transpose()?,
        from_hex(&record.royalty_puzzle_hash)?,
        record.royalty_basis_points,
        from_hex(&record.p2_puzzle_hash)?,
    );

    Ok(Nft::new(
        record.coin.to_coin()?,
        record.proof.to_proof()?,
        info,
    ))
}
