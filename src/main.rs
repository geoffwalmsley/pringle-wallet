//! The `pringle` binary: wires the network-agnostic builders to coinset I/O.

mod cli;

use anyhow::{Context, Result};
use chia_wallet_sdk::driver::{decode_offer, encode_offer};
use chia_wallet_sdk::prelude::{
    Address, Bytes32, Coin, Offer, OptionContract, OptionType, SpendContext,
};
use clap::Parser;
use serde_json::{json, Value};

use pringle_wallet::chain::ChainStatus;
use pringle_wallet::coinset::Coinset;
use pringle_wallet::confirm::ActionPreview;
use pringle_wallet::format;
use pringle_wallet::key::{self, Wallet};
use pringle_wallet::nft;
use pringle_wallet::option as option_contract;
use pringle_wallet::output::{self, AppError, Report};
use pringle_wallet::p2_singleton;
use pringle_wallet::signing::sign_spend_bundle;
use pringle_wallet::state::{
    from_hex, to_hex, CoinJson, OptionRecord, Phase, ProofJson, State, TxRecord,
};
use pringle_wallet::status_view;
use pringle_wallet::sync;
use pringle_wallet::wallet::{select_for, spend_all, Selection};
use pringle_wallet::MAINNET_PREFIX;

use cli::{Cli, Command, NftCommand, OptionCommand, XchCommand};

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    output::init(cli.json, cli.verbose, cli.quiet);

    let result = dispatch(cli).await;
    if let Err(err) = result {
        let code = output::render_error(&err);
        std::process::exit(code);
    }
}

async fn dispatch(cli: Cli) -> Result<()> {
    let key_file = cli.key_file.clone();
    let state_file = cli.state_file.clone();
    let yes = cli.yes;

    match cli.command {
        Command::Init => cmd_init(&key_file, &state_file),
        Command::Xch { command } => match command {
            XchCommand::Address => cmd_address(&key_file, &state_file),
            XchCommand::Coins => cmd_coins(&key_file).await,
            XchCommand::Coin { coin_id } => cmd_coin(&coin_id).await,
            XchCommand::Consolidate { fee } => {
                cmd_xch_spend_all(&key_file, &state_file, None, fee, yes).await
            }
            XchCommand::SendAll { address, fee } => {
                cmd_xch_spend_all(&key_file, &state_file, Some(address), fee, yes).await
            }
        },
        Command::Nft { command } => match command {
            NftCommand::Mint {
                fee,
                royalty_basis_points,
                data_uris,
                data_hash,
            } => {
                cmd_nft_mint(
                    &key_file,
                    &state_file,
                    fee,
                    royalty_basis_points,
                    data_uris,
                    data_hash,
                    yes,
                )
                .await
            }
            NftCommand::Address { launcher } => cmd_nft_address(&state_file, launcher.as_deref()),
            NftCommand::Sweep {
                address,
                fee,
                launcher,
            } => {
                cmd_nft_sweep(
                    &key_file,
                    &state_file,
                    address,
                    fee,
                    launcher.as_deref(),
                    yes,
                )
                .await
            }
        },
        Command::Option { command } => match command {
            OptionCommand::ShowAll {
                include_closed,
                cached,
            } => cmd_option_show_all(&key_file, &state_file, include_closed, cached).await,
            OptionCommand::Create {
                strike,
                expiration,
                creator_address,
                owner_address,
                fee,
                launcher,
            } => {
                cmd_option_create(
                    &key_file,
                    &state_file,
                    strike,
                    expiration,
                    creator_address,
                    owner_address,
                    fee,
                    launcher.as_deref(),
                    yes,
                )
                .await
            }
            OptionCommand::Offer {
                request,
                receive_address,
                output,
                launcher,
            } => {
                cmd_option_offer(
                    &key_file,
                    &state_file,
                    request,
                    receive_address,
                    output,
                    launcher.as_deref(),
                )
                .await
            }
            OptionCommand::Take { offer_file, fee } => {
                cmd_option_take(&key_file, &state_file, &offer_file, fee, yes).await
            }
            OptionCommand::Exercise { fee, launcher } => {
                cmd_option_exercise(&key_file, &state_file, fee, launcher.as_deref(), yes).await
            }
            OptionCommand::Recover { launcher } => {
                cmd_option_recover(&state_file, launcher.as_deref()).await
            }
            OptionCommand::Clawback {
                address,
                fee,
                launcher,
            } => {
                cmd_option_clawback(
                    &key_file,
                    &state_file,
                    address,
                    fee,
                    launcher.as_deref(),
                    yes,
                )
                .await
            }
        },
        Command::Status { cached } => cmd_status(&key_file, &state_file, cached).await,
        Command::Sync => cmd_sync(&key_file, &state_file).await,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Loads the wallet from the key file, with a helpful error if it is missing.
fn load_wallet(key_file: &Path) -> Result<Wallet> {
    if !key_file.exists() {
        return Err(
            AppError::recoverable(format!("no key file at {}", key_file.display()))
                .next("run `pringle init` first")
                .into(),
        );
    }
    key::load_wallet(key_file)
}

fn parse_address(address: &str) -> Result<Bytes32> {
    Address::decode(address)
        .with_context(|| format!("invalid address {address}"))?
        .expect_prefix(MAINNET_PREFIX)
        .with_context(|| format!("address {address} is not a mainnet (xch) address"))
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Fetches wallet coins minus any still referenced by an unconfirmed local transaction.
async fn wallet_spendable_coins(
    coinset: &Coinset,
    wallet: &Wallet,
    state: &State,
) -> Result<Vec<Coin>> {
    let mut coins = coinset.unspent_coins(wallet.puzzle_hash()).await?;
    let pending: HashSet<Bytes32> = state
        .transactions
        .iter()
        .flat_map(|tx| tx.spent_coin_ids.iter())
        .filter_map(|id| from_hex(id).ok())
        .collect();
    coins.retain(|coin| !pending.contains(&coin.coin_id()));
    Ok(coins)
}

fn selection_spent_ids(selection: &Selection) -> Vec<String> {
    selection
        .coins
        .iter()
        .map(|coin| to_hex(coin.coin_id()))
        .collect()
}

/// Requires that a coin be confirmed and unspent, mapping other statuses to actionable
/// errors (RPC failures become chain errors; missing/spent become recoverable guidance).
async fn require_confirmed_unspent(coinset: &Coinset, coin_id: Bytes32, noun: &str) -> Result<()> {
    match coinset.classify(coin_id).await {
        ChainStatus::ConfirmedUnspent { .. } => Ok(()),
        ChainStatus::NotFound => Err(AppError::recoverable(format!(
            "{noun} coin {} is not confirmed yet",
            format::abbrev_bytes(coin_id)
        ))
        .why("it has not appeared on-chain (still pending in the mempool)")
        .next("wait a bit, then run `pringle status`")
        .into()),
        ChainStatus::Spent { .. } => Err(AppError::recoverable(format!(
            "{noun} coin {} has already been spent",
            format::abbrev_bytes(coin_id)
        ))
        .why("your local state is behind the chain")
        .next("run `pringle sync` to catch up, then retry")
        .into()),
        ChainStatus::LookupFailed { error } => Err(AppError::chain(format!(
            "could not look up the {noun} coin: {error}"
        ))
        .next("check your network connection and retry")
        .into()),
    }
}

/// Confirms a destructive mainnet action, printing "Cancelled." and returning `false` if
/// the user declines at an interactive prompt.
fn confirm_or_abort(preview: &ActionPreview, assume_yes: bool) -> Result<bool> {
    if preview.confirm(assume_yes)? {
        Ok(true)
    } else {
        output::progress("Cancelled.");
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// init / address / coins
// ---------------------------------------------------------------------------

fn cmd_init(key_file: &Path, state_file: &Path) -> Result<()> {
    let wallet = Wallet::generate();
    key::save_new_wallet(key_file, &wallet)?;

    // A new key gets fresh lifecycle state (no inherited NFTs/options from an old wallet).
    let state = State {
        key_file: key_file.display().to_string(),
        wallet_puzzle_hash: to_hex(wallet.puzzle_hash()),
        wallet_address: wallet.address()?,
        ..Default::default()
    };
    state.save(state_file)?;

    let mut report = Report::new("init", "Initialized new wallet.");
    report
        .field("Key file", key_file.display().to_string(), "key_file")
        .field("Address", wallet.address()?, "address")
        .primary()
        .note(format!(
            "WARNING: the key in {} is unencrypted. Use only a throwaway key with minimal funds.",
            key_file.display()
        ));
    report.emit();
    Ok(())
}

fn cmd_address(key_file: &Path, state_file: &Path) -> Result<()> {
    let wallet = load_wallet(key_file)?;
    let address = wallet.address()?;

    // Keep the state's cached address in sync.
    let mut state = State::load(state_file)?;
    if state.wallet_address != address {
        state.wallet_puzzle_hash = to_hex(wallet.puzzle_hash());
        state.wallet_address = address.clone();
        state.save(state_file)?;
    }

    if output::is_json() {
        let mut report = Report::new("address", "Wallet address");
        report.field("Address", &address, "address").field(
            "Puzzle hash",
            to_hex(wallet.puzzle_hash()),
            "puzzle_hash",
        );
        report.emit();
        return Ok(());
    }

    // Human/quiet: print only the bare address so it is trivially script-friendly.
    println!("{address}");
    if output::is_verbose() {
        println!("puzzle hash: {}", to_hex(wallet.puzzle_hash()));
    }
    Ok(())
}

async fn cmd_coins(key_file: &Path) -> Result<()> {
    let wallet = load_wallet(key_file)?;
    let coinset = Coinset::mainnet();
    let coins = coinset.unspent_coins(wallet.puzzle_hash()).await?;

    let total: u64 = coins.iter().map(|c| c.amount).sum();
    let mut report = Report::new(
        "coins",
        format!("Confirmed unspent coins for {}", wallet.address()?),
    );
    report.field_json(
        "Count",
        coins.len().to_string(),
        "count",
        Value::from(coins.len()),
    );
    report.field_json(
        "Total",
        format::xch(total),
        "total_mojos",
        Value::from(total),
    );
    report.json_only(
        "coins",
        Value::Array(
            coins
                .iter()
                .map(|c| {
                    json!({
                        "coin_id": to_hex(c.coin_id()),
                        "amount_mojos": c.amount,
                    })
                })
                .collect(),
        ),
    );
    for coin in &coins {
        report.field(
            display_id(to_hex(coin.coin_id())),
            format::xch(coin.amount),
            "coin",
        );
    }
    report.emit();
    Ok(())
}

/// Consolidates all standard-wallet coins, or sends their full value to another address.
///
/// Pending transaction inputs recorded in local state are excluded to avoid accidental
/// double-spend submissions. p2-singleton funds are at a different puzzle hash and are
/// therefore never included.
async fn cmd_xch_spend_all(
    key_file: &Path,
    state_file: &Path,
    destination_address: Option<String>,
    fee: u64,
    assume_yes: bool,
) -> Result<()> {
    let wallet = load_wallet(key_file)?;
    let mut state = State::load(state_file)?;
    let coinset = Coinset::mainnet();
    let coins = wallet_spendable_coins(&coinset, &wallet, &state).await?;

    let is_consolidation = destination_address.is_none();
    if is_consolidation && coins.len() == 1 {
        return Err(
            AppError::recoverable("wallet already has one spendable XCH coin")
                .why("there is nothing to consolidate")
                .next("use `pringle xch coins` to inspect the wallet balance")
                .into(),
        );
    }
    if coins.is_empty() {
        return Err(AppError::recoverable("wallet has no spendable XCH coins")
            .why("the wallet is empty, or its coins are reserved by pending transactions")
            .next("run `pringle status` to refresh pending transaction state")
            .into());
    }

    let total = coins.iter().try_fold(0u64, |sum, coin| {
        sum.checked_add(coin.amount)
            .ok_or_else(|| anyhow::anyhow!("wallet balance overflows u64"))
    })?;
    let sent = total
        .checked_sub(fee)
        .filter(|amount| *amount > 0)
        .ok_or_else(|| {
            AppError::recoverable("fee must be less than the spendable XCH balance")
                .why(format!("balance is {total} mojos and fee is {fee} mojos"))
                .next("choose a smaller `--fee`")
        })?;

    let destination = match destination_address {
        Some(address) => (parse_address(&address)?, address),
        None => (wallet.puzzle_hash(), wallet.address()?),
    };
    let action = if is_consolidation {
        "Consolidate XCH coins"
    } else {
        "Send full XCH balance"
    };
    let preview = ActionPreview::new(action)
        .detail("Network", "mainnet")
        .detail("Input coins", coins.len().to_string())
        .detail("Balance", format::xch(total))
        .detail("Fee", format::xch(fee))
        .detail("Destination", destination.1.clone())
        .detail("Output", format::xch(sent));
    if !confirm_or_abort(&preview, assume_yes)? {
        return Ok(());
    }

    let mut ctx = SpendContext::new();
    let outcome = spend_all(
        &mut ctx,
        &wallet.standard_layer(),
        coins,
        destination.0,
        fee,
    )?;
    let bundle = sign_spend_bundle(ctx.take(), &[wallet.synthetic_secret_key().clone()])?;
    coinset.push_tx(bundle).await?;

    let kind = if is_consolidation {
        "xch_consolidate"
    } else {
        "xch_send_all"
    };
    state.transactions.push(TxRecord::new(
        kind,
        outcome
            .spent_coins
            .iter()
            .map(|coin| to_hex(coin.coin_id()))
            .collect(),
        to_hex(outcome.output_coin.coin_id()),
    ));
    state.save(state_file)?;

    let mut report = Report::new(
        kind,
        if is_consolidation {
            "XCH consolidation submitted."
        } else {
            "Full-balance XCH send submitted."
        },
    );
    report
        .field_json(
            "Input coins",
            outcome.spent_coins.len().to_string(),
            "input_count",
            Value::from(outcome.spent_coins.len()),
        )
        .field_json(
            "Input total",
            format::xch(outcome.total),
            "input_total_mojos",
            Value::from(outcome.total),
        )
        .field_json(
            "Amount",
            format::xch(outcome.sent),
            "amount_mojos",
            Value::from(outcome.sent),
        )
        .field_json("Fee", format::xch(fee), "fee_mojos", Value::from(fee))
        .field("Destination", destination.1, "destination")
        .field(
            "Pending output coin",
            display_id(to_hex(outcome.output_coin.coin_id())),
            "output_coin_id",
        )
        .note("Submitted to the mempool; run `pringle status` to check confirmation.");
    report.emit();
    Ok(())
}

async fn cmd_coin(coin_id: &str) -> Result<()> {
    let coin_id = from_hex(coin_id)?;
    let coinset = Coinset::mainnet();
    match coinset.coin_record(coin_id).await? {
        None => {
            let mut report = Report::new(
                "coin",
                format!("Coin {} not found on-chain", to_hex(coin_id)),
            );
            report.json_only("found", Value::Bool(false));
            report.emit();
        }
        Some(record) => {
            let mut report = Report::new("coin", format!("Coin {}", to_hex(coin_id)));
            report
                .field("Parent", to_hex(record.coin.parent_coin_info), "parent")
                .field("Puzzle", to_hex(record.coin.puzzle_hash), "puzzle_hash")
                .field_json(
                    "Amount",
                    format::xch(record.coin.amount),
                    "amount_mojos",
                    Value::from(record.coin.amount),
                )
                .field_json(
                    "Confirmed",
                    format!("block {}", record.confirmed_block_index),
                    "confirmed_block_index",
                    Value::from(record.confirmed_block_index),
                )
                .field_json(
                    "Spent",
                    record.spent.to_string(),
                    "spent",
                    Value::Bool(record.spent),
                );
            if record.spent {
                report.field_json(
                    "Spent at",
                    format!("block {}", record.spent_block_index),
                    "spent_block_index",
                    Value::from(record.spent_block_index),
                );
            }
            report.emit();
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// NFT
// ---------------------------------------------------------------------------

async fn cmd_nft_mint(
    key_file: &Path,
    state_file: &Path,
    fee: u64,
    royalty_basis_points: u16,
    data_uris: Vec<String>,
    data_hash: Option<String>,
    assume_yes: bool,
) -> Result<()> {
    let wallet = load_wallet(key_file)?;
    let mut state = State::load(state_file)?;

    // Build metadata (default unless data URIs were provided).
    let mut metadata = chia_wallet_sdk::chia::puzzle_types::nft::NftMetadata::default();
    if !data_uris.is_empty() {
        metadata.data_uris = data_uris;
        metadata.data_hash = data_hash.as_deref().map(from_hex).transpose()?;
    } else if data_hash.is_some() {
        return Err(AppError::recoverable("--data-hash requires at least one --data-uri").into());
    }

    let coinset = Coinset::mainnet();
    let coins = wallet_spendable_coins(&coinset, &wallet, &state).await?;
    let selection = select_for(coins, nft::NFT_MINT_OUTPUT_VALUE, fee)?;

    let preview = ActionPreview::new("Mint NFT")
        .detail("Owner", wallet.address()?)
        .detail("Royalty", format!("{royalty_basis_points} bps"))
        .detail("Fee", format::xch(fee));
    if !confirm_or_abort(&preview, assume_yes)? {
        return Ok(());
    }

    let mut ctx = SpendContext::new();
    let minted = nft::build_mint(
        &mut ctx,
        &wallet.standard_layer(),
        wallet.puzzle_hash(),
        &selection,
        &metadata,
        royalty_basis_points,
        fee,
    )?;

    let bundle = sign_spend_bundle(ctx.take(), &[wallet.synthetic_secret_key().clone()])?;
    coinset.push_tx(bundle).await?;

    state.transactions.push(TxRecord::new(
        "nft_mint",
        selection_spent_ids(&selection),
        to_hex(minted.coin.coin_id()),
    ));
    state.upsert_nft(nft::nft_to_record(&minted, &metadata, Phase::Pending));
    state.save(state_file)?;

    let mut report = Report::new("nft_mint", "Submitted NFT mint.");
    report
        .field("Launcher id", to_hex(minted.info.launcher_id), "launcher_id")
        .primary()
        .field("NFT coin id", to_hex(minted.coin.coin_id()), "nft_coin_id")
        .note("Submitted, not yet confirmed. Run `pringle status` to watch it settle, then `pringle nft address` for its income address.");
    report.emit();
    Ok(())
}

// ---------------------------------------------------------------------------
// NFT income (p2 singleton)
// ---------------------------------------------------------------------------

fn cmd_nft_address(state_file: &Path, launcher: Option<&str>) -> Result<()> {
    let state = State::load(state_file)?;
    let nft = state.select_nft(launcher)?;
    let launcher_id = from_hex(&nft.launcher_id)?;

    let mut report = Report::new("p2_singleton_address", "NFT income address");
    report
        .field("Controlling NFT", &nft.launcher_id, "nft_launcher_id")
        .field("Address", p2_singleton::address(launcher_id)?, "address")
        .primary();
    if output::is_verbose() || output::is_json() {
        report.field(
            "Puzzle hash",
            to_hex(p2_singleton::puzzle_hash(launcher_id)),
            "puzzle_hash",
        );
    }
    report.emit();
    Ok(())
}

async fn cmd_nft_sweep(
    key_file: &Path,
    state_file: &Path,
    address: Option<String>,
    fee: u64,
    launcher: Option<&str>,
    assume_yes: bool,
) -> Result<()> {
    let wallet = load_wallet(key_file)?;
    let mut state = State::load(state_file)?;

    let nft_record = state.select_nft(launcher)?;
    let p2_record = state
        .p2_by_launcher(&nft_record.launcher_id)
        .cloned()
        .ok_or_else(|| {
            AppError::recoverable("no income tracked for this NFT")
                .why("no XCH has been sent to the NFT's income address, or state is behind the chain")
                .next("send XCH to `pringle nft address`, then run `pringle sync`")
        })?;
    let launcher_id = from_hex(&nft_record.launcher_id)?;

    let (destination, destination_label) = match &address {
        Some(addr) => (parse_address(addr)?, addr.clone()),
        None => (wallet.puzzle_hash(), wallet.address()?),
    };

    let coinset = Coinset::mainnet();

    // The NFT must be confirmed, unspent, and wallet-controlled to authorize the sweep.
    let nft_coin = nft_record.coin.to_coin()?;
    require_confirmed_unspent(&coinset, nft_coin.coin_id(), "NFT").await?;
    let nft_p2 = from_hex(&nft_record.p2_puzzle_hash)?;
    if nft_p2 != wallet.puzzle_hash() {
        return Err(
            AppError::recoverable("the NFT is not currently controlled by this wallet")
                .why("it may be locked in an option")
                .next("exercise the option first, then run `pringle sync`")
                .into(),
        );
    }

    let p2_puzzle_hash = from_hex(&p2_record.puzzle_hash)?;
    let p2_coins = coinset.unspent_coins(p2_puzzle_hash).await?;
    if p2_coins.is_empty() {
        return Err(AppError::recoverable(
            "the p2 singleton has no confirmed, unspent coins to sweep",
        )
        .into());
    }

    let total: u64 = p2_coins.iter().map(|c| c.amount).sum();
    let preview = ActionPreview::new("Sweep p2 singleton")
        .detail("Coins", p2_coins.len().to_string())
        .detail("Total balance", format::xch(total))
        .detail("Fee", format::xch(fee))
        .detail("Destination", &destination_label);
    if !confirm_or_abort(&preview, assume_yes)? {
        return Ok(());
    }

    let mut ctx = SpendContext::new();
    let nft = nft::nft_from_record(&mut ctx, &nft_record)?;
    let outcome = p2_singleton::build_sweep(
        &mut ctx,
        &wallet.standard_layer(),
        nft,
        launcher_id,
        &p2_coins,
        destination,
        fee,
    )?;

    let bundle = sign_spend_bundle(ctx.take(), &[wallet.synthetic_secret_key().clone()])?;
    coinset.push_tx(bundle).await?;

    let mut spent_ids: Vec<String> = p2_coins.iter().map(|c| to_hex(c.coin_id())).collect();
    spent_ids.push(to_hex(nft_coin.coin_id()));
    state.transactions.push(TxRecord::new(
        "p2_singleton_sweep",
        spent_ids,
        to_hex(outcome.swept_coin.coin_id()),
    ));
    if let Some(rec) = state.nft_mut(&nft_record.launcher_id) {
        rec.coin = CoinJson::from_coin(outcome.new_nft.coin);
        rec.proof = ProofJson::from_proof(outcome.new_nft.proof);
        rec.p2_puzzle_hash = to_hex(outcome.new_nft.info.p2_puzzle_hash);
        rec.current_owner = outcome.new_nft.info.current_owner.map(to_hex);
        rec.phase = Phase::Pending;
    }
    if let Some(rec) = state.p2_mut(&nft_record.launcher_id) {
        // Leave the sweep pending until the payout confirms; sync will mark it empty.
        rec.funded_coins = Vec::new();
        rec.phase = Phase::Pending;
    }
    state.save(state_file)?;

    let mut report = Report::new("p2_singleton_sweep", "Submitted p2 singleton sweep.");
    report
        .field_json(
            "Coins swept",
            outcome.coins_spent.to_string(),
            "coins_swept",
            Value::from(outcome.coins_spent),
        )
        .field_json(
            "Total balance",
            format::xch(outcome.total),
            "total_mojos",
            Value::from(outcome.total),
        )
        .field_json(
            "Requested fee",
            format::xch(outcome.requested_fee),
            "requested_fee_mojos",
            Value::from(outcome.requested_fee),
        );
    if outcome.odd_donation > 0 {
        report.field_json(
            "Odd-mojo donation",
            format!("{} mojo", outcome.odd_donation),
            "odd_donation_mojos",
            Value::from(outcome.odd_donation),
        );
    }
    report
        .field_json(
            "Paid out",
            format::xch(outcome.swept_amount),
            "paid_out_mojos",
            Value::from(outcome.swept_amount),
        )
        .field("Destination", &destination_label, "destination")
        .field(
            "Payout coin",
            to_hex(outcome.swept_coin.coin_id()),
            "payout_coin_id",
        )
        .primary()
        .field(
            "NFT recreated",
            to_hex(outcome.new_nft.coin.coin_id()),
            "new_nft_coin_id",
        )
        .note("Submitted, not yet confirmed. Run `pringle status` to watch it settle.");
    report.emit();
    Ok(())
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

async fn cmd_option_show_all(
    key_file: &Path,
    state_file: &Path,
    include_closed: bool,
    cached: bool,
) -> Result<()> {
    let wallet = load_wallet(key_file)?;
    let mut state = State::load(state_file)?;
    let coinset = Coinset::mainnet();

    if !cached {
        let rec = sync::reconcile(&coinset, &mut state).await?;
        state.save(state_file)?;
        for warning in &rec.warnings {
            output::warn(warning);
        }
    }

    let now = now_seconds();
    let mut shown = Vec::new();
    for option in &state.options {
        let controlled = from_hex(&option.owner_puzzle_hash).ok() == Some(wallet.puzzle_hash());
        let expired = option.terms_known && now >= option.expiration_seconds;
        let open = controlled && option.phase != Phase::Superseded && !expired;
        if !include_closed && !open {
            continue;
        }

        let user_state = if cached {
            if option.underlying_reclaimed {
                status_view::UserState::Reclaimed
            } else if expired {
                status_view::UserState::Expired
            } else if option.phase == Phase::Pending {
                status_view::UserState::PendingConfirmation
            } else if option.phase == Phase::Superseded {
                if controlled {
                    status_view::UserState::Exercised
                } else {
                    status_view::UserState::Transferred
                }
            } else if !controlled {
                status_view::UserState::Transferred
            } else {
                status_view::UserState::Ready
            }
        } else {
            let chain = coinset.classify(option.coin.to_coin()?.coin_id()).await;
            status_view::option_state(
                option.phase,
                &chain,
                controlled,
                expired,
                option.underlying_reclaimed,
            )
        };
        shown.push((option, user_state, open));
    }

    if output::is_json() {
        let options = shown
            .iter()
            .map(|(option, user_state, open)| {
                json!({
                    "launcher_id": option.launcher_id,
                    "state": user_state.machine(),
                    "open": open,
                    "terms_known": option.terms_known,
                    "strike_mojos": option.terms_known.then_some(option.strike_amount),
                    "expiration_seconds": option.terms_known.then_some(option.expiration_seconds),
                    "creator_puzzle_hash": option.terms_known.then_some(&option.creator_puzzle_hash),
                    "owner_puzzle_hash": option.owner_puzzle_hash,
                    "nft_launcher_id": option.nft_launcher_id,
                    "origin": option.origin,
                    "underlying_reclaimed": option.underlying_reclaimed,
                    "clawback_available": clawback_eligible(option, Some(wallet.puzzle_hash()), now),
                })
            })
            .collect::<Vec<_>>();
        let mut report = Report::new("option_show_all", "Options");
        report
            .field_json(
                "Count",
                options.len().to_string(),
                "count",
                Value::from(options.len()),
            )
            .json_only("include_closed", Value::Bool(include_closed))
            .json_only("options", Value::Array(options));
        report.emit();
        return Ok(());
    }

    if output::is_quiet() {
        for (option, _, _) in shown {
            println!("{}", option.launcher_id);
        }
        return Ok(());
    }

    println!(
        "{}:",
        if include_closed {
            "Tracked options"
        } else {
            "Open options"
        }
    );
    if shown.is_empty() {
        println!("  (none)");
        if !include_closed {
            println!("  Use `pringle option show-all --include-closed` to show history.");
        }
        return Ok(());
    }

    for (option, user_state, open) in shown {
        println!("\n  Launcher:   {}", option.launcher_id);
        println!("  State:      {}", user_state.label());
        if option.terms_known {
            println!("  Strike:     {}", format::xch(option.strike_amount));
            println!(
                "  Expiration: {}",
                format::expiration(option.expiration_seconds, now)
            );
        } else {
            println!("  Terms:      unknown (run `pringle option recover --launcher …`)");
        }
        if let Some(nft_launcher) = &option.nft_launcher_id {
            println!("  NFT:        {nft_launcher}");
        }
        if open && option.terms_known {
            println!(
                "  Exercise:   pringle option exercise --launcher {}",
                option.launcher_id
            );
        }
        if clawback_eligible(option, Some(wallet.puzzle_hash()), now) {
            println!(
                "  Clawback:   pringle option clawback --launcher {}",
                option.launcher_id
            );
        }
    }
    if !include_closed {
        println!("\nUse `pringle option show-all --include-closed` to show history.");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_option_create(
    key_file: &Path,
    state_file: &Path,
    strike: u64,
    expiration: u64,
    creator_address: Option<String>,
    owner_address: Option<String>,
    fee: u64,
    launcher: Option<&str>,
    assume_yes: bool,
) -> Result<()> {
    if strike == 0 {
        return Err(AppError::recoverable("--strike must be greater than zero").into());
    }

    let wallet = load_wallet(key_file)?;
    let mut state = State::load(state_file)?;

    let nft_record = state.select_nft(launcher)?;
    if nft_record.phase == Phase::Superseded {
        return Err(AppError::recoverable("the NFT is already locked into an option").into());
    }

    let creator_puzzle_hash = match creator_address {
        Some(address) => parse_address(&address)?,
        None => wallet.puzzle_hash(),
    };
    let owner_puzzle_hash = match owner_address {
        Some(address) => parse_address(&address)?,
        None => wallet.puzzle_hash(),
    };

    let coinset = Coinset::mainnet();
    let nft_coin = nft_record.coin.to_coin()?;
    require_confirmed_unspent(&coinset, nft_coin.coin_id(), "NFT").await?;

    let coins = wallet_spendable_coins(&coinset, &wallet, &state).await?;
    let selection = select_for(coins, option_contract::OPTION_OUTPUT_VALUE, fee)?;

    let preview = ActionPreview::new("Create option")
        .detail("Underlying NFT", &nft_record.launcher_id)
        .detail("Strike", format::xch(strike))
        .detail("Expiration", format::expiration(expiration, now_seconds()))
        .detail("Fee", format::xch(fee));
    if !confirm_or_abort(&preview, assume_yes)? {
        return Ok(());
    }

    let mut ctx = SpendContext::new();
    let nft = nft::nft_from_record(&mut ctx, &nft_record)?;
    let outcome = option_contract::build_create(
        &mut ctx,
        &wallet.standard_layer(),
        nft,
        &selection,
        strike,
        expiration,
        creator_puzzle_hash,
        owner_puzzle_hash,
        wallet.puzzle_hash(),
        fee,
    )?;

    let bundle = sign_spend_bundle(ctx.take(), &[wallet.synthetic_secret_key().clone()])?;
    coinset.push_tx(bundle).await?;

    let mut spent_ids = selection_spent_ids(&selection);
    spent_ids.push(to_hex(nft_coin.coin_id()));
    state.transactions.push(TxRecord::new(
        "option_create",
        spent_ids,
        to_hex(outcome.option.coin.coin_id()),
    ));

    state.upsert_option(option_contract::option_to_record(
        &outcome,
        strike,
        expiration,
        creator_puzzle_hash,
        owner_puzzle_hash,
        Phase::Pending,
    ));
    // The wallet-owned NFT coin is now superseded by the locked one.
    if let Some(nft) = state.nft_mut(&nft_record.launcher_id) {
        nft.phase = Phase::Superseded;
        nft.coin = CoinJson::from_coin(outcome.locked_nft.coin);
        nft.p2_puzzle_hash = to_hex(outcome.locked_nft.info.p2_puzzle_hash);
        nft.proof = ProofJson::from_proof(outcome.locked_nft.proof);
    }
    state.save(state_file)?;

    let mut report = Report::new("option_create", "Submitted option creation.");
    report
        .field(
            "Option launcher id",
            to_hex(outcome.launcher_id),
            "launcher_id",
        )
        .primary()
        .field(
            "Option coin id",
            to_hex(outcome.option.coin.coin_id()),
            "option_coin_id",
        )
        .field("Underlying NFT", &nft_record.launcher_id, "nft_launcher_id")
        .field_json(
            "Strike",
            format::xch(strike),
            "strike_mojos",
            Value::from(strike),
        )
        .field_json(
            "Expiration",
            format::expiration(expiration, now_seconds()),
            "expiration_seconds",
            Value::from(expiration),
        );
    report.emit();
    Ok(())
}

/// Reconstructs a tracked option singleton, recovering (and persisting) its lineage proof
/// from the chain if the record predates the persisted proof.
async fn reconstruct_option(
    coinset: &Coinset,
    state: &mut State,
    state_file: &Path,
    launcher_id: &str,
) -> Result<OptionContract> {
    let record = state
        .option_by_launcher(launcher_id)
        .cloned()
        .ok_or_else(|| AppError::recoverable("no such option"))?;
    let option_coin = record.coin.to_coin()?;

    if record.proof.is_some() {
        return option_contract::option_from_record(&record);
    }

    let parent_id = option_coin.parent_coin_info;
    let parent_spend = coinset.coin_spend(parent_id).await?.ok_or_else(|| {
        AppError::recoverable(format!(
            "cannot recover option proof: parent coin {} has no recorded spend on-chain",
            to_hex(parent_id)
        ))
    })?;
    let recovered = option_contract::option_from_parent_spend(&parent_spend)?;
    if recovered.coin != option_coin {
        return Err(AppError::recoverable(format!(
            "recovered option coin {} does not match the stored option coin {}",
            to_hex(recovered.coin.coin_id()),
            to_hex(option_coin.coin_id())
        ))
        .into());
    }

    if let Some(rec) = state.option_mut(launcher_id) {
        rec.proof = Some(ProofJson::from_proof(recovered.proof));
    }
    state.save(state_file)?;
    output::progress("Recovered the option's singleton proof from the chain and saved it.");
    Ok(recovered)
}

async fn cmd_option_offer(
    key_file: &Path,
    state_file: &Path,
    request: u64,
    receive_address: Option<String>,
    output_path: Option<PathBuf>,
    launcher: Option<&str>,
) -> Result<()> {
    if request == 0 {
        return Err(AppError::recoverable("--request must be greater than zero").into());
    }

    let wallet = load_wallet(key_file)?;
    let mut state = State::load(state_file)?;
    let option = state.select_option(launcher)?;

    let owner_puzzle_hash = from_hex(&option.owner_puzzle_hash)?;
    if owner_puzzle_hash != wallet.puzzle_hash() {
        return Err(AppError::recoverable(
            "this wallet does not own the option; only the owner can offer it",
        )
        .into());
    }

    let receive_puzzle_hash = match receive_address {
        Some(address) => parse_address(&address)?,
        None => wallet.puzzle_hash(),
    };

    let coinset = Coinset::mainnet();
    let option_coin = option.coin.to_coin()?;
    require_confirmed_unspent(&coinset, option_coin.coin_id(), "option").await?;

    let contract =
        reconstruct_option(&coinset, &mut state, state_file, &option.launcher_id).await?;

    let mut ctx = SpendContext::new();
    let parts = option_contract::build_offer(
        &mut ctx,
        &wallet.standard_layer(),
        contract,
        receive_puzzle_hash,
        request,
    )?;

    let signed_partial = sign_spend_bundle(ctx.take(), &[wallet.synthetic_secret_key().clone()])?;
    let full_bundle = option_contract::finalize_offer(&mut ctx, signed_partial, parts)?;
    let offer_text = encode_offer(&full_bundle)?;

    let path = output_path.unwrap_or_else(|| PathBuf::from("option.offer"));
    std::fs::write(&path, format!("{offer_text}\n"))
        .with_context(|| format!("failed to write offer file {}", path.display()))?;

    let mut report = Report::new("option_offer", "Created an offer selling the option.");
    report
        .field("Option launcher", &option.launcher_id, "launcher_id")
        .field_json(
            "Request",
            format::xch(request),
            "request_mojos",
            Value::from(request),
        )
        .field(
            "Receiving XCH to",
            to_hex(receive_puzzle_hash),
            "receive_puzzle_hash",
        )
        .field("Offer file", path.display().to_string(), "offer_file")
        .primary()
        .note(
            "The offer is not on-chain yet; it settles only when a taker accepts it. Until then\n\
             the option coin stays unspent and you can cancel by spending it another way.",
        );
    report.emit();
    Ok(())
}

async fn cmd_option_take(
    key_file: &Path,
    state_file: &Path,
    offer_file: &Path,
    fee: u64,
    assume_yes: bool,
) -> Result<()> {
    let wallet = load_wallet(key_file)?;
    let mut state = State::load(state_file)?;

    let raw = std::fs::read_to_string(offer_file)
        .with_context(|| format!("failed to read offer file {}", offer_file.display()))?;
    let spend_bundle = decode_offer(raw.trim()).context("failed to decode offer file")?;

    let mut ctx = SpendContext::new();
    let offer = Offer::from_spend_bundle(&mut ctx, &spend_bundle)?;

    if offer.offered_coins().options.len() != 1 {
        return Err(AppError::recoverable(
            "this CLI can only take offers that offer exactly one option",
        )
        .into());
    }
    let offered_option = *offer
        .offered_coins()
        .options
        .values()
        .next()
        .expect("one option");
    let request = offer.requested_payments().amounts().xch;
    if request == 0 {
        return Err(AppError::recoverable("offer does not request any XCH").into());
    }

    let coinset = Coinset::mainnet();
    let maker_coin_id = offered_option.coin.parent_coin_info;
    require_confirmed_unspent(&coinset, maker_coin_id, "offered option").await?;

    let coins = wallet_spendable_coins(&coinset, &wallet, &state).await?;
    let selection = select_for(coins, request, fee)?;

    let preview = ActionPreview::new("Take option offer")
        .detail("Pay", format::xch(request))
        .detail("Fee", format::xch(fee))
        .detail("Receive option to", wallet.address()?);
    if !confirm_or_abort(&preview, assume_yes)? {
        return Ok(());
    }

    let outcome = option_contract::build_take(
        &mut ctx,
        &offer,
        &selection.coins,
        wallet.puzzle_hash(),
        wallet.synthetic_public_key(),
        fee,
    )?;

    let signed = sign_spend_bundle(ctx.take(), &[wallet.synthetic_secret_key().clone()])?;
    let full_bundle = offer.take(signed);
    coinset.push_tx(full_bundle).await?;

    let mut spent_ids = selection_spent_ids(&selection);
    spent_ids.push(to_hex(maker_coin_id));
    state.transactions.push(TxRecord::new(
        "option_take",
        spent_ids,
        to_hex(outcome.option.coin.coin_id()),
    ));
    // Persist the acquired option so status/offer/exercise can work for the buyer. Its
    // terms are unknown from the offer alone; recovery (below, best-effort) fills them in.
    state.upsert_option(option_contract::purchased_option_record(
        &outcome,
        wallet.puzzle_hash(),
    ));
    state.save(state_file)?;

    // Best-effort: recover terms + underlying NFT from the chain. It may fail if the option
    // is not yet confirmed; the user can re-run `pringle option recover` later.
    let launcher_hex = to_hex(outcome.launcher_id);
    let recovered = recover_option_terms(&coinset, &mut state, state_file, &launcher_hex)
        .await
        .is_ok();

    let mut report = Report::new("option_take", "Accepted option offer.");
    report.field_json(
        "Paid",
        format::xch(outcome.paid_mojos),
        "paid_mojos",
        Value::from(outcome.paid_mojos),
    );
    if fee > 0 {
        report.field_json("Fee", format::xch(fee), "fee_mojos", Value::from(fee));
    }
    report
        .field(
            "Option launcher",
            to_hex(outcome.launcher_id),
            "launcher_id",
        )
        .primary()
        .field(
            "Option coin",
            to_hex(outcome.option.coin.coin_id()),
            "option_coin_id",
        )
        .field("Now owned by", wallet.address()?, "owner_address")
        .field_json(
            "Terms recovered",
            recovered.to_string(),
            "terms_recovered",
            Value::Bool(recovered),
        );
    if !recovered {
        report.note(
            "Could not yet recover the option's terms from the chain (it may not be confirmed).\n\
             Run `pringle option recover` once it confirms to enable exercising.",
        );
    }
    report.emit();
    Ok(())
}

/// Recovers a purchased option's terms (strike/expiration/creator) and locked underlying
/// NFT from the chain, verifies them, and persists everything.
async fn recover_option_terms(
    coinset: &Coinset,
    state: &mut State,
    state_file: &Path,
    launcher_id: &str,
) -> Result<()> {
    let record = state
        .option_by_launcher(launcher_id)
        .cloned()
        .ok_or_else(|| AppError::recoverable("no such option"))?;
    let launcher_bytes = from_hex(&record.launcher_id)?;

    // 1. Read the launcher metadata (expiration + strike type).
    let launcher_spend = coinset.coin_spend(launcher_bytes).await?.ok_or_else(|| {
        AppError::recoverable("option launcher coin has no recorded spend on-chain")
            .why("the option may not be confirmed yet")
            .next("wait for confirmation, then run `pringle option recover`")
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
        .coin_record(launcher_bytes)
        .await?
        .ok_or_else(|| AppError::recoverable("option launcher coin not found on-chain"))?;
    let launcher_parent = launcher_record.coin.parent_coin_info;
    let parent_spend = coinset.coin_spend(launcher_parent).await?.ok_or_else(|| {
        AppError::recoverable("could not fetch the launcher's parent spend to recover the creator")
    })?;
    let creator = option_contract::creator_from_launcher_creation(&parent_spend, launcher_bytes)?
        .ok_or_else(|| {
        AppError::recoverable("could not recover the option creator from the launcher memo")
    })?;

    // 3. Verify the recovered terms reproduce the on-chain delegated puzzle hash.
    let underlying_delegated = from_hex(&record.underlying_delegated_puzzle_hash)?;
    if !option_contract::verify_terms(
        launcher_bytes,
        creator,
        terms.expiration_seconds,
        1, // NFT underlying amount
        terms.strike_type,
        underlying_delegated,
    ) {
        return Err(AppError::recoverable(
            "recovered option terms failed verification against the on-chain contract",
        )
        .into());
    }

    // 4. Reconstruct the locked underlying NFT from its parent spend.
    let underlying_coin_id = match &record.underlying_coin_id {
        Some(id) => from_hex(id)?,
        None => record.underlying_nft_coin.to_coin()?.coin_id(),
    };
    let ul_record = coinset
        .coin_record(underlying_coin_id)
        .await?
        .ok_or_else(|| AppError::recoverable("underlying NFT coin not found on-chain"))?;
    let ul_parent_spend = coinset
        .coin_spend(ul_record.coin.parent_coin_info)
        .await?
        .ok_or_else(|| {
            AppError::recoverable("could not fetch the underlying NFT's parent spend")
        })?;
    let nft_record = nft::nft_record_from_parent_spend(&ul_parent_spend, Phase::Superseded)?
        .ok_or_else(|| AppError::recoverable("could not reconstruct the underlying NFT"))?;
    let nft_launcher = nft_record.launcher_id.clone();
    let nft_launcher_bytes = from_hex(&nft_launcher)?;
    state.upsert_nft(nft_record);

    // The recovered NFT deterministically controls a p2 singleton. Track it even when empty
    // so status/sync discovers funds that were attached before this wallet bought the option.
    if state.p2_by_launcher(&nft_launcher).is_none() {
        state.upsert_p2_singleton(p2_singleton::tracking_record(
            nft_launcher_bytes,
            Vec::new(),
            Phase::Confirmed,
        )?);
    }

    // 5. Persist the recovered terms and NFT relationship.
    if let Some(rec) = state.option_mut(launcher_id) {
        rec.strike_amount = strike_amount;
        rec.expiration_seconds = terms.expiration_seconds;
        rec.creator_puzzle_hash = to_hex(creator);
        rec.underlying_nft_coin = CoinJson::from_coin(ul_record.coin);
        rec.nft_launcher_id = Some(nft_launcher);
        rec.terms_known = true;
    }
    state.save(state_file)?;
    Ok(())
}

async fn cmd_option_recover(state_file: &Path, launcher: Option<&str>) -> Result<()> {
    let mut state = State::load(state_file)?;
    let option = state.select_option(launcher)?;
    let coinset = Coinset::mainnet();

    recover_option_terms(&coinset, &mut state, state_file, &option.launcher_id).await?;

    let recovered = state
        .option_by_launcher(&option.launcher_id)
        .cloned()
        .expect("option present");
    let mut report = Report::new("option_recover", "Recovered option terms from the chain.");
    report
        .field("Option launcher", &recovered.launcher_id, "launcher_id")
        .primary()
        .field_json(
            "Strike",
            format::xch(recovered.strike_amount),
            "strike_mojos",
            Value::from(recovered.strike_amount),
        )
        .field_json(
            "Expiration",
            format::expiration(recovered.expiration_seconds, now_seconds()),
            "expiration_seconds",
            Value::from(recovered.expiration_seconds),
        )
        .field(
            "Creator",
            recovered.creator_puzzle_hash.clone(),
            "creator_puzzle_hash",
        );
    if let Some(nft) = &recovered.nft_launcher_id {
        report.field("Underlying NFT", nft.clone(), "nft_launcher_id");
    }
    report.emit();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_option_exercise(
    key_file: &Path,
    state_file: &Path,
    fee: u64,
    launcher: Option<&str>,
    assume_yes: bool,
) -> Result<()> {
    let wallet = load_wallet(key_file)?;
    let mut state = State::load(state_file)?;

    let option = state.select_option(launcher)?;
    if option.phase == Phase::Superseded {
        return Err(AppError::recoverable(
            "this option has already been closed (exercised or superseded)",
        )
        .into());
    }
    if !option.terms_known {
        return Err(AppError::recoverable(
            "this option's terms are not known (it was purchased and not yet recovered)",
        )
        .next("run `pringle option recover` first")
        .into());
    }

    let owner_puzzle_hash = from_hex(&option.owner_puzzle_hash)?;
    if owner_puzzle_hash != wallet.puzzle_hash() {
        return Err(AppError::recoverable(
            "this wallet does not own the option; only the owner can exercise it",
        )
        .into());
    }

    let now = now_seconds();
    if now >= option.expiration_seconds {
        return Err(AppError::recoverable(format!(
            "option expired at {} ({})",
            format::utc_datetime(option.expiration_seconds),
            format::relative_time(option.expiration_seconds, now)
        ))
        .why("expired options can no longer be exercised")
        .into());
    }

    // Locate the underlying NFT record (linked by launcher id when known).
    let nft_record = match &option.nft_launcher_id {
        Some(id) => state.nft_by_launcher(id).cloned().ok_or_else(|| {
            AppError::recoverable("the option's underlying NFT is not tracked")
                .next("run `pringle option recover`")
        })?,
        None => {
            return Err(
                AppError::recoverable("the option's underlying NFT is not linked")
                    .next("run `pringle option recover` first")
                    .into(),
            )
        }
    };
    let creator_puzzle_hash = from_hex(&option.creator_puzzle_hash)?;

    let coinset = Coinset::mainnet();
    let option_coin = option.coin.to_coin()?;
    require_confirmed_unspent(&coinset, option_coin.coin_id(), "option").await?;
    let nft_coin = nft_record.coin.to_coin()?;
    require_confirmed_unspent(&coinset, nft_coin.coin_id(), "locked NFT").await?;

    let contract =
        reconstruct_option(&coinset, &mut state, state_file, &option.launcher_id).await?;

    let coins = wallet_spendable_coins(&coinset, &wallet, &state).await?;
    let selection = select_for(coins, option.strike_amount, fee)?;

    let preview = ActionPreview::new("Exercise option")
        .detail("Pay strike", format::xch(option.strike_amount))
        .detail("Strike to", to_hex(creator_puzzle_hash))
        .detail("Receive NFT", nft_record.launcher_id.clone())
        .detail("Fee", format::xch(fee));
    if !confirm_or_abort(&preview, assume_yes)? {
        return Ok(());
    }

    let mut ctx = SpendContext::new();
    let locked_nft = nft::nft_from_record(&mut ctx, &nft_record)?;
    let outcome = option_contract::build_exercise(
        &mut ctx,
        &wallet.standard_layer(),
        contract,
        locked_nft,
        creator_puzzle_hash,
        option.expiration_seconds,
        option.strike_amount,
        wallet.puzzle_hash(),
        &selection,
        wallet.puzzle_hash(),
        fee,
    )?;

    let bundle = sign_spend_bundle(ctx.take(), &[wallet.synthetic_secret_key().clone()])?;
    coinset.push_tx(bundle).await?;

    let mut spent_ids = selection_spent_ids(&selection);
    spent_ids.push(to_hex(option_coin.coin_id()));
    spent_ids.push(to_hex(nft_coin.coin_id()));
    state.transactions.push(TxRecord::new(
        "option_exercise",
        spent_ids,
        to_hex(outcome.nft.coin.coin_id()),
    ));

    if let Some(rec) = state.option_mut(&option.launcher_id) {
        rec.phase = Phase::Superseded;
    }
    if let Some(rec) = state.nft_mut(&nft_record.launcher_id) {
        rec.coin = CoinJson::from_coin(outcome.nft.coin);
        rec.proof = ProofJson::from_proof(outcome.nft.proof);
        rec.p2_puzzle_hash = to_hex(outcome.nft.info.p2_puzzle_hash);
        rec.current_owner = outcome.nft.info.current_owner.map(to_hex);
        rec.phase = Phase::Pending;
    }
    state.save(state_file)?;

    let mut report = Report::new("option_exercise", "Submitted option exercise.");
    report.field_json(
        "Paid strike",
        format::xch(option.strike_amount),
        "strike_mojos",
        Value::from(option.strike_amount),
    );
    if fee > 0 {
        report.field_json("Fee", format::xch(fee), "fee_mojos", Value::from(fee));
    }
    report
        .field(
            "Strike paid to",
            to_hex(creator_puzzle_hash),
            "creator_puzzle_hash",
        )
        .field("NFT received", &nft_record.launcher_id, "nft_launcher_id")
        .primary()
        .field(
            "New NFT coin",
            to_hex(outcome.nft.coin.coin_id()),
            "new_nft_coin_id",
        )
        .field("Now owned by", wallet.address()?, "owner_address");
    report.emit();
    Ok(())
}

async fn cmd_option_clawback(
    key_file: &Path,
    state_file: &Path,
    address: Option<String>,
    fee: u64,
    launcher: Option<&str>,
    assume_yes: bool,
) -> Result<()> {
    let wallet = load_wallet(key_file)?;
    let mut state = State::load(state_file)?;

    let option = state.select_option(launcher)?;
    if option.underlying_reclaimed {
        return Err(AppError::recoverable(
            "the underlying NFT for this option has already been reclaimed",
        )
        .into());
    }
    if !option.terms_known {
        return Err(AppError::recoverable(
            "this option's terms are not known (it was purchased and not yet recovered)",
        )
        .next("run `pringle option recover` first")
        .into());
    }

    // Only the creator can claw back. Being the owner (or having transferred the option to a
    // buyer) is irrelevant: the underlying's clawback path is keyed to the creator.
    let creator_puzzle_hash = from_hex(&option.creator_puzzle_hash)?;
    if creator_puzzle_hash != wallet.puzzle_hash() {
        return Err(AppError::recoverable(
            "this wallet is not the option creator; only the creator can claw back the NFT",
        )
        .into());
    }

    let now = now_seconds();
    if now < option.expiration_seconds {
        return Err(AppError::recoverable(format!(
            "option has not expired yet (expires {})",
            format::expiration(option.expiration_seconds, now)
        ))
        .why("clawback is only allowed after the expiration deadline")
        .next("wait until it expires, or exercise it before then")
        .into());
    }

    // Locate the locked underlying NFT record (linked by launcher id when known).
    let nft_record = match &option.nft_launcher_id {
        Some(id) => state.nft_by_launcher(id).cloned().ok_or_else(|| {
            AppError::recoverable("the option's underlying NFT is not tracked")
                .next("run `pringle option recover`")
        })?,
        None => {
            return Err(
                AppError::recoverable("the option's underlying NFT is not linked")
                    .next("run `pringle option recover` first")
                    .into(),
            )
        }
    };

    let (reclaim_puzzle_hash, reclaim_label) = match &address {
        Some(addr) => (parse_address(addr)?, addr.clone()),
        None => (wallet.puzzle_hash(), wallet.address()?),
    };

    let coinset = Coinset::mainnet();
    let nft_coin = nft_record.coin.to_coin()?;
    require_confirmed_unspent(&coinset, nft_coin.coin_id(), "locked NFT").await?;

    // Fund the fee from separate regular-XCH coins (the NFT clawback provides no value).
    let fee_selection = if fee > 0 {
        let coins = wallet_spendable_coins(&coinset, &wallet, &state).await?;
        Some(select_for(coins, 0, fee)?)
    } else {
        None
    };

    let launcher_id = from_hex(&option.launcher_id)?;
    let preview = ActionPreview::new("Clawback expired option")
        .detail("Option launcher", option.launcher_id.clone())
        .detail("Reclaim NFT", nft_record.launcher_id.clone())
        .detail("NFT owner", reclaim_label.clone())
        .detail("Fee", format::xch(fee));
    if !confirm_or_abort(&preview, assume_yes)? {
        return Ok(());
    }

    let mut ctx = SpendContext::new();
    let locked_nft = nft::nft_from_record(&mut ctx, &nft_record)?;
    let outcome = option_contract::build_clawback(
        &mut ctx,
        &wallet.standard_layer(),
        launcher_id,
        locked_nft,
        creator_puzzle_hash,
        option.expiration_seconds,
        option.strike_amount,
        reclaim_puzzle_hash,
        fee_selection.as_ref(),
        wallet.puzzle_hash(),
        fee,
    )?;

    let bundle = sign_spend_bundle(ctx.take(), &[wallet.synthetic_secret_key().clone()])?;
    coinset.push_tx(bundle).await?;

    let mut spent_ids = vec![to_hex(nft_coin.coin_id())];
    if let Some(selection) = &fee_selection {
        spent_ids.extend(selection_spent_ids(selection));
    }
    state.transactions.push(TxRecord::new(
        "option_clawback",
        spent_ids,
        to_hex(outcome.nft.coin.coin_id()),
    ));

    // The reclaimed NFT is pending under the new owner; mark the option's underlying as
    // reclaimed. The option singleton itself is untouched, so leave its phase alone.
    if let Some(rec) = state.nft_mut(&nft_record.launcher_id) {
        rec.coin = CoinJson::from_coin(outcome.nft.coin);
        rec.proof = ProofJson::from_proof(outcome.nft.proof);
        rec.p2_puzzle_hash = to_hex(outcome.nft.info.p2_puzzle_hash);
        rec.current_owner = outcome.nft.info.current_owner.map(to_hex);
        rec.phase = Phase::Pending;
    }
    if let Some(rec) = state.option_mut(&option.launcher_id) {
        rec.underlying_reclaimed = true;
    }
    state.save(state_file)?;

    let mut report = Report::new("option_clawback", "Submitted expired option clawback.");
    if fee > 0 {
        report.field_json("Fee", format::xch(fee), "fee_mojos", Value::from(fee));
    }
    report
        .field("Option launcher", &option.launcher_id, "launcher_id")
        .field("NFT reclaimed", &nft_record.launcher_id, "nft_launcher_id")
        .primary()
        .field(
            "New NFT coin",
            to_hex(outcome.nft.coin.coin_id()),
            "new_nft_coin_id",
        )
        .field("NFT owner", reclaim_label, "reclaim_address")
        .note(
            "Submitted, not yet confirmed. Once `pringle status` shows the NFT as Ready, run\n\
             `pringle nft sweep` to withdraw its accumulated income.",
        );
    report.emit();
    Ok(())
}

// ---------------------------------------------------------------------------
// status / sync
// ---------------------------------------------------------------------------

async fn cmd_sync(key_file: &Path, state_file: &Path) -> Result<()> {
    let mut state = State::load(state_file)?;
    let coinset = Coinset::mainnet();

    if key_file.exists() {
        let wallet = load_wallet(key_file)?;
        let address = wallet.address()?;
        if state.wallet_address != address {
            state.wallet_puzzle_hash = to_hex(wallet.puzzle_hash());
            state.wallet_address = address;
        }
    }

    let report = sync::reconcile(&coinset, &mut state).await?;
    state.save(state_file)?;

    for msg in &report.messages {
        output::progress(msg);
    }
    for w in &report.warnings {
        output::warn(w);
    }

    let mut out = Report::new(
        "sync",
        if report.changed {
            "Sync complete; local state updated."
        } else {
            "Everything already in sync."
        },
    );
    out.field_json(
        "Changed",
        report.changed.to_string(),
        "changed",
        Value::Bool(report.changed),
    )
    .field_json(
        "Warnings",
        report.warnings.len().to_string(),
        "warnings",
        Value::from(report.warnings.len()),
    );
    out.json_only(
        "messages",
        Value::Array(report.messages.iter().cloned().map(Value::String).collect()),
    );
    out.emit();
    Ok(())
}

async fn cmd_status(key_file: &Path, state_file: &Path, cached: bool) -> Result<()> {
    let mut state = State::load(state_file)?;
    let coinset = Coinset::mainnet();

    if !cached {
        if key_file.exists() {
            if let Ok(wallet) = load_wallet(key_file) {
                if let Ok(address) = wallet.address() {
                    if state.wallet_address != address {
                        state.wallet_puzzle_hash = to_hex(wallet.puzzle_hash());
                        state.wallet_address = address;
                    }
                }
            }
        }
        let rec = sync::reconcile(&coinset, &mut state).await?;
        state.save(state_file)?;
        for w in &rec.warnings {
            output::warn(w);
        }
    }

    let now = now_seconds();
    let wallet_ph = from_hex(&state.wallet_puzzle_hash).ok();

    // Gather per-asset views (classifying on-chain unless cached).
    let mut nft_views = Vec::new();
    for nft in &state.nfts {
        let coin = nft.coin.to_coin()?;
        let status = if cached {
            ChainStatus::NotFound
        } else {
            coinset.classify(coin.coin_id()).await
        };
        let controlled = wallet_ph == from_hex(&nft.p2_puzzle_hash).ok();
        let user = status_view::nft_state(nft.phase, &status, controlled);
        nft_views.push((nft, coin, status, user));
    }

    let mut option_views = Vec::new();
    for option in &state.options {
        let coin = option.coin.to_coin()?;
        let status = if cached {
            ChainStatus::NotFound
        } else {
            coinset.classify(coin.coin_id()).await
        };
        let controlled = wallet_ph == from_hex(&option.owner_puzzle_hash).ok();
        let expired = option.terms_known && now >= option.expiration_seconds;
        let user = status_view::option_state(
            option.phase,
            &status,
            controlled,
            expired,
            option.underlying_reclaimed,
        );
        option_views.push((option, coin, status, user, expired));
    }

    if output::is_json() {
        emit_status_json(&state, &nft_views, &option_views);
        return Ok(());
    }

    // Human output.
    match &state.wallet_address {
        addr if !addr.is_empty() => println!("Wallet: {addr}"),
        _ => println!("Wallet: not initialized (run `pringle init`)"),
    }

    println!("\nNFTs:");
    if nft_views.is_empty() {
        println!("  (none)");
    }
    for (nft, coin, status, user) in &nft_views {
        println!(
            "  - {}  [{}]",
            display_id(nft.launcher_id.clone()),
            user.label()
        );
        if output::is_verbose() {
            println!("      coin:     {}", to_hex(coin.coin_id()));
            println!("      p2 hash:  {}", nft.p2_puzzle_hash);
            println!("      phase:    {:?}", nft.phase);
            println!("      on-chain: {}", status.label());
        }
    }

    println!("\np2 singletons:");
    if state.p2_singletons.is_empty() {
        println!("  (none)");
    }
    for p2 in &state.p2_singletons {
        let total: u64 = p2.funded_coins.iter().map(|c| c.amount).sum();
        let user = status_view::p2_state(p2.phase, p2.funded_coins.len(), false);
        println!(
            "  - {}  {}  [{}]",
            display_id(p2.launcher_id.clone()),
            format::xch(total),
            user.label()
        );
        if output::is_verbose() {
            println!("      address:  {}", p2.address);
            println!("      coins:    {}", p2.funded_coins.len());
            println!("      phase:    {:?}", p2.phase);
        }
    }

    println!("\nOptions:");
    if option_views.is_empty() {
        println!("  (none)");
    }
    for (option, coin, status, user, expired) in &option_views {
        println!(
            "  - {}  [{}]",
            display_id(option.launcher_id.clone()),
            user.label()
        );
        if option.terms_known {
            println!("      strike:     {}", format::xch(option.strike_amount));
            println!(
                "      expiration: {}{}",
                format::expiration(option.expiration_seconds, now),
                if *expired { "  (EXPIRED)" } else { "" }
            );
        } else {
            println!("      terms:      unknown (run `pringle option recover`)");
        }
        if clawback_eligible(option, wallet_ph, now) {
            println!(
                "      clawback:   available — pringle option clawback --launcher {}",
                option.launcher_id
            );
        }
        if output::is_verbose() {
            println!("      coin:       {}", to_hex(coin.coin_id()));
            println!("      owner:      {}", option.owner_puzzle_hash);
            println!("      origin:     {:?}", option.origin);
            println!("      phase:      {:?}", option.phase);
            println!("      reclaimed:  {}", option.underlying_reclaimed);
            println!("      on-chain:   {}", status.label());
        }
    }

    if !state.transactions.is_empty() {
        println!("\nPending transactions:");
        for tx in &state.transactions {
            println!(
                "  - {} (watching {})",
                tx.kind,
                display_id(tx.watch_coin_id.clone())
            );
        }
    }

    Ok(())
}

/// Whether an option is eligible for a creator clawback of its expired underlying NFT:
/// terms known, created by this wallet, past expiry, not yet reclaimed, and its NFT linked.
fn clawback_eligible(option: &OptionRecord, wallet_ph: Option<Bytes32>, now: u64) -> bool {
    option.terms_known
        && !option.underlying_reclaimed
        && option.nft_launcher_id.is_some()
        && now >= option.expiration_seconds
        && wallet_ph.is_some()
        && from_hex(&option.creator_puzzle_hash).ok() == wallet_ph
}

/// Abbreviates an id for human display unless verbose mode is on.
fn display_id(id: String) -> String {
    if output::is_verbose() {
        id
    } else {
        format::abbrev(&id)
    }
}

type NftView<'a> = (
    &'a pringle_wallet::state::NftRecord,
    Coin,
    ChainStatus,
    status_view::UserState,
);
type OptionView<'a> = (
    &'a OptionRecord,
    Coin,
    ChainStatus,
    status_view::UserState,
    bool,
);

fn emit_status_json(state: &State, nfts: &[NftView], options: &[OptionView]) {
    let nfts_json: Vec<Value> = nfts
        .iter()
        .map(|(nft, coin, status, user)| {
            json!({
                "launcher_id": nft.launcher_id,
                "coin_id": to_hex(coin.coin_id()),
                "state": user.machine(),
                "on_chain": status.label(),
                "phase": format!("{:?}", nft.phase).to_lowercase(),
            })
        })
        .collect();

    let p2_json: Vec<Value> = state
        .p2_singletons
        .iter()
        .map(|p2| {
            let total: u64 = p2.funded_coins.iter().map(|c| c.amount).sum();
            let user = status_view::p2_state(p2.phase, p2.funded_coins.len(), false);
            json!({
                "launcher_id": p2.launcher_id,
                "address": p2.address,
                "total_mojos": total,
                "coins": p2.funded_coins.len(),
                "state": user.machine(),
            })
        })
        .collect();

    let now = now_seconds();
    let wallet_ph = from_hex(&state.wallet_puzzle_hash).ok();
    let options_json: Vec<Value> = options
        .iter()
        .map(|(option, coin, status, user, expired)| {
            json!({
                "launcher_id": option.launcher_id,
                "coin_id": to_hex(coin.coin_id()),
                "state": user.machine(),
                "on_chain": status.label(),
                "terms_known": option.terms_known,
                "strike_mojos": option.strike_amount,
                "expiration_seconds": option.expiration_seconds,
                "expired": expired,
                "underlying_reclaimed": option.underlying_reclaimed,
                "clawback_available": clawback_eligible(option, wallet_ph, now),
                "origin": format!("{:?}", option.origin).to_lowercase(),
            })
        })
        .collect();

    let env = json!({
        "schema_version": output::SCHEMA_VERSION,
        "ok": true,
        "kind": "status",
        "wallet_address": state.wallet_address,
        "nfts": nfts_json,
        "p2_singletons": p2_json,
        "options": options_json,
        "pending_transactions": state.transactions.iter().map(|tx| json!({
            "kind": tx.kind,
            "watch_coin_id": tx.watch_coin_id,
        })).collect::<Vec<_>>(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&env).unwrap_or_else(|_| "{}".to_string())
    );
}
