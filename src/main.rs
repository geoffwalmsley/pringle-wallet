//! The `pringle` binary: wires the network-agnostic builders to coinset I/O.

mod cli;

use anyhow::{Context, Result};
use chia_wallet_sdk::driver::{decode_offer, encode_offer};
use chia_wallet_sdk::prelude::{
    Address, Bytes32, Coin, Conditions, Offer, OptionContract, SpendContext,
};
use clap::Parser;
use serde_json::{json, Value};

use pringle_wallet::chain::ChainStatus;
use pringle_wallet::coinset::Coinset;
use pringle_wallet::confirm::ActionPreview;
use pringle_wallet::format;
use pringle_wallet::inspect;
use pringle_wallet::key::{self, Wallet};
use pringle_wallet::nft;
use pringle_wallet::option::{self as option_contract, OfferedOption};
use pringle_wallet::output::{self, AppError, Report};
use pringle_wallet::p2_singleton;
use pringle_wallet::potato;
use pringle_wallet::signing::sign_spend_bundle;
use pringle_wallet::state::{
    from_hex, to_hex, CoinJson, OptionKind, OptionRecord, Phase, PotatoCache, ProofJson, State,
    TxRecord,
};
use pringle_wallet::status_view;
use pringle_wallet::sync;
use pringle_wallet::wallet::{build_send, select_for, spend_all, Selection};
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
                cmd_xch_consolidate(&key_file, &state_file, fee, yes).await
            }
            XchCommand::Send {
                address,
                amount,
                all,
                fee,
            } => cmd_xch_send(&key_file, &state_file, address, amount, all, fee, yes).await,
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
                kind,
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
                    kind.into(),
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
            OptionCommand::Inspect { offer_file, fee } => {
                cmd_option_inspect(&key_file, &state_file, &offer_file, fee, yes).await
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
        Command::Potato { holders, coin } => {
            cmd_potato(&state_file, holders, coin.as_deref()).await
        }
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

/// Fetches the coins a spend can draw on, refusing to continue when there are none.
///
/// Pending transaction inputs recorded in local state are excluded to avoid accidental
/// double-spend submissions. p2-singleton funds are at a different puzzle hash and are
/// therefore never included.
async fn spendable_or_error(
    coinset: &Coinset,
    wallet: &Wallet,
    state: &State,
) -> Result<(Vec<Coin>, u64)> {
    let coins = wallet_spendable_coins(coinset, wallet, state).await?;
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
    Ok((coins, total))
}

/// Combines every spendable standard-wallet coin into a single wallet coin.
async fn cmd_xch_consolidate(
    key_file: &Path,
    state_file: &Path,
    fee: u64,
    assume_yes: bool,
) -> Result<()> {
    let wallet = load_wallet(key_file)?;
    let mut state = State::load(state_file)?;
    let coinset = Coinset::mainnet();
    let (coins, total) = spendable_or_error(&coinset, &wallet, &state).await?;

    if coins.len() == 1 {
        return Err(
            AppError::recoverable("wallet already has one spendable XCH coin")
                .why("there is nothing to consolidate")
                .next("use `pringle xch coins` to inspect the wallet balance")
                .into(),
        );
    }
    let output = total_after_fee(total, fee)?;

    let preview = ActionPreview::new("Consolidate XCH coins")
        .detail("Network", "mainnet")
        .detail("Input coins", coins.len().to_string())
        .detail("Balance", format::xch(total))
        .detail("Fee", format::xch(fee))
        .detail("Output", format::xch(output));
    if !confirm_or_abort(&preview, assume_yes)? {
        return Ok(());
    }

    let mut ctx = SpendContext::new();
    let outcome = spend_all(
        &mut ctx,
        &wallet.standard_layer(),
        coins,
        wallet.puzzle_hash(),
        fee,
    )?;
    let bundle = sign_spend_bundle(ctx.take(), &[wallet.synthetic_secret_key().clone()])?;
    coinset.push_tx(bundle).await?;

    state.transactions.push(TxRecord::new(
        "xch_consolidate",
        outcome
            .spent_coins
            .iter()
            .map(|coin| to_hex(coin.coin_id()))
            .collect(),
        to_hex(outcome.output_coin.coin_id()),
    ));
    state.save(state_file)?;

    let mut report = Report::new("xch_consolidate", "XCH consolidation submitted.");
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
        .field("Destination", wallet.address()?, "destination")
        .field(
            "Pending output coin",
            display_id(to_hex(outcome.output_coin.coin_id())),
            "output_coin_id",
        )
        .note("Submitted to the mempool; run `pringle status` to check confirmation.");
    report.emit();
    Ok(())
}

/// What is left of `total` once `fee` is reserved, refusing a fee that swallows it whole.
fn total_after_fee(total: u64, fee: u64) -> Result<u64> {
    total
        .checked_sub(fee)
        .filter(|amount| *amount > 0)
        .ok_or_else(|| {
            AppError::recoverable("fee must be less than the spendable XCH balance")
                .why(format!("balance is {total} mojos and fee is {fee} mojos"))
                .next("choose a smaller `--fee`")
                .into()
        })
}

/// Sends XCH to an address: either a fixed `amount`, or the whole balance minus the fee.
///
/// A fixed amount is funded from just enough coins to cover it plus the fee, with the rest
/// returned as change. `--all` spends every coin and takes the fee out of what is sent, so
/// the wallet is left empty.
#[allow(clippy::too_many_arguments)]
async fn cmd_xch_send(
    key_file: &Path,
    state_file: &Path,
    address: String,
    amount: Option<u64>,
    all: bool,
    fee: u64,
    assume_yes: bool,
) -> Result<()> {
    let wallet = load_wallet(key_file)?;
    let mut state = State::load(state_file)?;
    let destination = parse_address(&address)?;
    let coinset = Coinset::mainnet();
    let (coins, total) = spendable_or_error(&coinset, &wallet, &state).await?;

    // Work out what will be sent, and which coins pay for it, before asking to confirm: an
    // unaffordable amount should be an error rather than a prompt the user has to decline.
    // A `None` selection means "spend every coin", which is what `--all` does.
    let (sent, selection) = match (amount, all) {
        (Some(0), _) => {
            return Err(AppError::recoverable("--amount must be greater than zero")
                .next("use `--all` to send the whole balance")
                .into())
        }
        (Some(amount), _) => {
            let selection = select_for(coins.clone(), amount, fee).map_err(|err| {
                AppError::recoverable(format!("cannot send {}: {err}", format::xch(amount)))
                    .why(format!(
                        "the wallet holds {} across {} spendable coin(s), and the fee is {}",
                        format::xch(total),
                        coins.len(),
                        format::xch(fee)
                    ))
                    .next("lower --amount, or use --all to send everything")
            })?;
            (amount, Some(selection))
        }
        (None, true) => (total_after_fee(total, fee)?, None),
        (None, false) => {
            return Err(AppError::recoverable("no amount to send")
                .next("pass `--amount <mojos>`, or `--all` to send the whole spendable balance")
                .into())
        }
    };

    let inputs = selection.as_ref().map_or(coins.len(), |s| s.coins.len());
    let preview = ActionPreview::new("Send XCH")
        .detail("Network", "mainnet")
        .detail("Destination", address.clone())
        .detail("Amount", format::xch(sent))
        .detail("Fee", format::xch(fee))
        .detail("Input coins", inputs.to_string())
        .detail("Balance", format::xch(total));
    if !confirm_or_abort(&preview, assume_yes)? {
        return Ok(());
    }

    let mut ctx = SpendContext::new();
    let layer = wallet.standard_layer();
    let (output_coin, change, spent_coins) = match &selection {
        Some(selection) => {
            let outcome = build_send(
                &mut ctx,
                &layer,
                selection,
                destination,
                sent,
                wallet.puzzle_hash(),
                fee,
            )?;
            (
                outcome.output_coin,
                selection.change,
                outcome.spent_coins.clone(),
            )
        }
        None => {
            let outcome = spend_all(&mut ctx, &layer, coins, destination, fee)?;
            (outcome.output_coin, 0, outcome.spent_coins)
        }
    };

    let bundle = sign_spend_bundle(ctx.take(), &[wallet.synthetic_secret_key().clone()])?;
    coinset.push_tx(bundle).await?;

    state.transactions.push(TxRecord::new(
        "xch_send",
        spent_coins
            .iter()
            .map(|coin| to_hex(coin.coin_id()))
            .collect(),
        to_hex(output_coin.coin_id()),
    ));
    state.save(state_file)?;

    let mut report = Report::new("xch_send", "XCH send submitted.");
    report
        .field_json(
            "Amount",
            format::xch(sent),
            "amount_mojos",
            Value::from(sent),
        )
        .primary()
        .field_json("Fee", format::xch(fee), "fee_mojos", Value::from(fee))
        .field("Destination", address, "destination")
        .field_json(
            "Input coins",
            spent_coins.len().to_string(),
            "input_count",
            Value::from(spent_coins.len()),
        )
        .field_json(
            "Change",
            format::xch(change),
            "change_mojos",
            Value::from(change),
        )
        .field(
            "Pending output coin",
            display_id(to_hex(output_coin.coin_id())),
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
// Pot Potato
// ---------------------------------------------------------------------------

/// Keeps the cached lineage from growing without bound, while still covering whatever
/// depth of history the caller asked for.
fn potato_cache_limit(holders: usize) -> usize {
    100.max(holders + 1)
}

async fn cmd_potato(state_file: &Path, holders: usize, coin: Option<&str>) -> Result<()> {
    let mut state = State::load(state_file)?;
    let cached: Vec<potato::Hold> = state
        .potato
        .iter()
        .flat_map(|cache| cache.holds.iter())
        .map(potato::Hold::from_json)
        .collect::<Result<_>>()?;

    // An explicit --coin re-anchors the walk, so history cached for the old anchor no
    // longer applies.
    let (anchor, cached) = match coin {
        Some(id) => (from_hex(id)?, Vec::new()),
        None => match cached.first() {
            Some(newest) => (newest.coin.coin_id(), cached),
            None => (from_hex(potato::DEFAULT_ANCHOR)?, Vec::new()),
        },
    };

    output::progress("Following the potato on-chain...");
    let coinset = Coinset::mainnet();
    let game = potato::refresh(&coinset, anchor, cached, holders).await?;

    let Some(current) = game.latest() else {
        return Err(AppError::chain("no potato holder could be resolved")
            .why("the lineage walk produced no holders")
            .next("re-anchor with `pringle potato --coin <potato coin id>`")
            .into());
    };

    // Only the default lineage is worth remembering; an ad-hoc --coin walk may be an
    // entirely different round.
    if coin.is_none() {
        let mut holds: Vec<_> = game.holds.iter().map(potato::Hold::to_json).collect();
        holds.truncate(potato_cache_limit(holders));
        state.potato = Some(PotatoCache { holds });
        state.save(state_file)?;
    }

    let now = now_seconds();
    let pot = game.pot();
    let held_for = current.held_for(now);

    let title = match &game.claim {
        Some(_) => format!("Pot Potato — pot claimed, {} XCH", format::xch_only(pot)),
        None => format!("Pot Potato — {} XCH in the pot", format::xch_only(pot)),
    };
    let mut report = Report::new("potato", title);
    report
        .field_json("Pot", format::xch(pot), "pot_mojos", Value::from(pot))
        .primary()
        .field(
            "Coin",
            display_id(to_hex(current.coin.coin_id())),
            "coin_id",
        )
        .field("Holder", display_address(&current.address()?), "holder")
        .json_only("holder_puzzle_hash", Value::from(to_hex(current.holder)))
        .field_json(
            "Took it",
            format::utc_datetime(current.acquired_at),
            "acquired_at",
            Value::from(current.acquired_at),
        )
        .field_json(
            "Held for",
            format::duration(held_for),
            "held_seconds",
            Value::from(held_for),
        );

    match &game.claim {
        Some(claim) => {
            report
                .field_json(
                    "Claimed",
                    claim
                        .claimed_at
                        .map_or_else(|| "yes".to_string(), format::utc_datetime),
                    "claimed_at",
                    claim.claimed_at.map_or(Value::Null, Value::from),
                )
                .json_only("claimed", Value::Bool(true));
        }
        None => {
            report
                .field_json(
                    "Claimable",
                    claimable_at(current.deadline(), now),
                    "deadline",
                    Value::from(current.deadline()),
                )
                .json_only("claimed", Value::Bool(false));
        }
    }

    // The cache can hold more history than was asked for, so trim to the requested depth.
    let previous = game.holds.get(1..).unwrap_or_default();
    let previous = &previous[..previous.len().min(holders)];
    report.json_only(
        "previous_holders",
        Value::Array(
            previous
                .iter()
                .map(|hold| {
                    json!({
                        "coin_id": to_hex(hold.coin.coin_id()),
                        "pot_mojos": hold.coin.amount,
                        "holder": hold.address().unwrap_or_default(),
                        "holder_puzzle_hash": to_hex(hold.holder),
                        "acquired_at": hold.acquired_at,
                        "sold_at": hold.sold_at,
                        "held_seconds": hold.held_for(now),
                    })
                })
                .collect(),
        ),
    );

    if previous.is_empty() {
        report.note("No earlier holders found; this is the start of the lineage.");
    } else {
        report.note(previous_holders_table(previous, now)?);
    }

    report.emit();
    Ok(())
}

/// Renders the deadline plus how far away it is, in either direction.
fn claimable_at(deadline: u64, now: u64) -> String {
    let when = format::utc_datetime(deadline);
    if deadline > now {
        format!("{when} (in {})", format::duration(deadline - now))
    } else {
        format!(
            "{when} (claimable now, {} ago)",
            format::duration(now - deadline)
        )
    }
}

/// Renders previous holders as an aligned block, newest first.
fn previous_holders_table(holds: &[potato::Hold], now: u64) -> Result<String> {
    let mut out = format!(
        "Last {} holder{}",
        holds.len(),
        if holds.len() == 1 { "" } else { "s" }
    );
    for hold in holds {
        out.push_str(&format!(
            "\n  {:>8}  {}  held {:<8}  {}",
            format!("{} XCH", format::xch_only(hold.coin.amount)),
            format::utc_datetime(hold.acquired_at),
            format::duration(hold.held_for(now)),
            display_address(&hold.address()?),
        ));
    }
    Ok(out)
}

/// Abbreviates an address for human output, keeping it exact for verbose and JSON.
fn display_address(address: &str) -> String {
    if output::is_verbose() || output::is_json() || address.len() <= 20 {
        return address.to_string();
    }
    format!("{}…{}", &address[..10], &address[address.len() - 6..])
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
    let all_p2_coins = coinset.unspent_coins(p2_puzzle_hash).await?;
    if all_p2_coins.is_empty() {
        return Err(AppError::recoverable(
            "the p2 singleton has no confirmed, unspent coins to sweep",
        )
        .into());
    }

    // A single transaction can only co-spend so many p2_singleton coins before hitting the
    // mempool cost cap. Select the highest-value coins first and report any left behind.
    let plan = p2_singleton::plan_sweep(&all_p2_coins, p2_singleton::MAX_SWEEP_COINS);
    let p2_coins = plan.selected.clone();

    let total: u64 = plan.selected_total;
    let mut preview = ActionPreview::new("Sweep p2 singleton")
        .detail("Coins", p2_coins.len().to_string())
        .detail("Total balance", format::xch(total))
        .detail("Fee", format::xch(fee))
        .detail("Destination", &destination_label);
    if plan.has_skipped() {
        preview = preview.detail(
            "WARNING coins over cap",
            format!(
                "{} coin(s) worth {} exceed the {}-coin per-transaction cap and will be left \
                 behind; sweep them in a follow-up transaction",
                plan.skipped.len(),
                format::xch(plan.skipped_total),
                p2_singleton::MAX_SWEEP_COINS
            ),
        );
    }
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
        );
    if plan.has_skipped() {
        report.field_json(
            "Coins left behind (over cap)",
            format!(
                "{} ({}) — sweep again to collect them",
                plan.skipped.len(),
                format::xch(plan.skipped_total)
            ),
            "skipped_coins",
            Value::from(plan.skipped.len()),
        );
    }
    report.note("Submitted, not yet confirmed. Run `pringle status` to watch it settle.");
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
                    "kind": option.terms_known.then_some(option.kind.label()),
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
            println!(
                "  Kind:       {} ({})",
                option.kind.label(),
                option.kind.exercise_semantics()
            );
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
    kind: OptionKind,
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
        .detail("Kind", kind.label())
        .detail("Exercise", kind.exercise_semantics())
        .detail("Underlying NFT", &nft_record.launcher_id)
        .detail("Strike", format::xch(strike))
        .detail("Expiration", format::expiration(expiration, now_seconds()))
        .detail("Fee", format::xch(fee));
    if !confirm_or_abort(&preview, assume_yes)? {
        return Ok(());
    }

    let mut ctx = SpendContext::new();
    let nft = nft::nft_from_record(&mut ctx, &nft_record)?;
    let outcome = match kind {
        OptionKind::Transfer => option_contract::build_create(
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
        )?,
        OptionKind::Sweep => option_contract::build_create_sweep(
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
        )?,
    };

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
        kind,
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
        .field_json("Kind", kind.label(), "kind", Value::from(kind.label()))
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

/// Reads an offer file and parses it against `ctx`, which must be the same context the
/// offer is later spent through (notarized-payment memos resolve against its allocator).
fn read_offer(ctx: &mut SpendContext, offer_file: &Path) -> Result<Offer> {
    let raw = std::fs::read_to_string(offer_file)
        .with_context(|| format!("failed to read offer file {}", offer_file.display()))?;
    let spend_bundle = decode_offer(raw.trim()).context("failed to decode offer file")?;
    Ok(Offer::from_spend_bundle(ctx, &spend_bundle)?)
}

/// Extracts the single option an offer sells, explaining the CLI's limits on failure.
fn parse_offered_option(offer: &Offer) -> Result<OfferedOption> {
    option_contract::offered_option(offer).map_err(|err| {
        AppError::recoverable(err.to_string())
            .why("this CLI only handles offers that sell one option for XCH")
            .into()
    })
}

/// Everything worth knowing about an offered option before paying for it.
struct OfferInspection {
    /// What the offer sells and asks for.
    offered: OfferedOption,
    /// Status of the maker's option coin. The offer can only settle while it is unspent.
    offer_status: ChainStatus,
    /// The option's chain-verified terms, or the reason they could not be established.
    details: std::result::Result<inspect::OptionDetails, String>,
    /// Income held by the underlying NFT's p2 singleton (looked up only with the terms).
    p2: Option<inspect::P2SingletonBalance>,
    /// The wallet coins available to pay with.
    spendable: Vec<Coin>,
    /// Their combined value.
    spendable_mojos: u64,
}

impl OfferInspection {
    /// The mojos still missing to cover the asking price plus `fee`, if any.
    fn shortfall(&self, fee: u64) -> Option<u64> {
        let needed = self.offered.request_mojos.saturating_add(fee);
        needed.checked_sub(self.spendable_mojos).filter(|m| *m > 0)
    }
}

/// Gathers the option's terms, its underlying's income, and the wallet's ability to pay.
///
/// The terms lookup is best-effort: an option that is not confirmed yet, or whose terms do
/// not verify, is still worth reporting — just not worth buying.
async fn inspect_offered_option(
    coinset: &Coinset,
    wallet: &Wallet,
    state: &State,
    offered: OfferedOption,
) -> Result<OfferInspection> {
    let offer_status = coinset.classify(offered.maker_coin_id).await;

    let details = inspect::recover_option_details(
        coinset,
        offered.launcher_id,
        offered.underlying_coin_id,
        offered.underlying_delegated_puzzle_hash,
    )
    .await
    .map_err(|err| err.to_string());

    let p2 = match &details {
        Ok(details) => {
            let nft_launcher = from_hex(&details.underlying_nft.launcher_id)?;
            Some(inspect::p2_singleton_balance(coinset, nft_launcher).await?)
        }
        Err(_) => None,
    };

    let spendable = wallet_spendable_coins(coinset, wallet, state).await?;
    let spendable_mojos = spendable.iter().try_fold(0u64, |sum, coin| {
        sum.checked_add(coin.amount)
            .ok_or_else(|| anyhow::anyhow!("wallet balance overflows u64"))
    })?;

    Ok(OfferInspection {
        offered,
        offer_status,
        details,
        p2,
        spendable,
        spendable_mojos,
    })
}

/// Requires the maker's option coin to still be spendable, so the offer can settle.
fn require_live_offer(status: &ChainStatus) -> Result<()> {
    match status {
        ChainStatus::ConfirmedUnspent { .. } => Ok(()),
        ChainStatus::NotFound => Err(AppError::recoverable(
            "the offered option coin is not confirmed yet",
        )
        .why("it has not appeared on-chain (still pending in the mempool)")
        .next("wait a bit, then try again")
        .into()),
        ChainStatus::Spent { .. } => Err(AppError::recoverable(
            "this offer can no longer be taken",
        )
        .why("the offered option coin has already been spent, so the offer was taken or cancelled")
        .next("ask the maker for a fresh offer")
        .into()),
        ChainStatus::LookupFailed { error } => Err(AppError::chain(format!(
            "could not look up the offered option coin: {error}"
        ))
        .next("check your network connection and retry")
        .into()),
    }
}

/// Builds the confirmation preview for taking an offer, including what is being bought.
fn take_preview(inspection: &OfferInspection, fee: u64, receive_address: String) -> ActionPreview {
    let mut preview = ActionPreview::new("Take option offer")
        .detail("Pay", format::xch(inspection.offered.request_mojos))
        .detail("Fee", format::xch(fee));
    if let Ok(details) = &inspection.details {
        preview = preview
            .detail("Kind", details.kind.label())
            .detail("Exercise", details.kind.exercise_semantics())
            .detail("Strike", format::xch(details.strike_amount))
            .detail(
                "Expiration",
                format::expiration(details.expiration_seconds, now_seconds()),
            );
    }
    if let Some(p2) = &inspection.p2 {
        preview = preview.detail("Income held by NFT", format::xch(p2.total_mojos));
    }
    preview.detail("Receive option to", receive_address)
}

/// Buys the offered option: settles the offer, records the purchase, and reports it.
#[allow(clippy::too_many_arguments)]
async fn execute_take(
    coinset: &Coinset,
    wallet: &Wallet,
    state: &mut State,
    state_file: &Path,
    ctx: &mut SpendContext,
    offer: Offer,
    offered: &OfferedOption,
    selection: &Selection,
    fee: u64,
) -> Result<()> {
    let outcome = option_contract::build_take(
        ctx,
        &offer,
        &selection.coins,
        wallet.puzzle_hash(),
        wallet.synthetic_public_key(),
        fee,
    )?;

    let signed = sign_spend_bundle(ctx.take(), &[wallet.synthetic_secret_key().clone()])?;
    let full_bundle = offer.take(signed);
    coinset.push_tx(full_bundle).await?;

    let mut spent_ids = selection_spent_ids(selection);
    spent_ids.push(to_hex(offered.maker_coin_id));
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
    let recovered = recover_option_terms(coinset, state, state_file, &launcher_hex)
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

async fn cmd_option_take(
    key_file: &Path,
    state_file: &Path,
    offer_file: &Path,
    fee: u64,
    assume_yes: bool,
) -> Result<()> {
    let wallet = load_wallet(key_file)?;
    let mut state = State::load(state_file)?;

    let mut ctx = SpendContext::new();
    let offer = read_offer(&mut ctx, offer_file)?;
    let offered = parse_offered_option(&offer)?;

    let coinset = Coinset::mainnet();
    let inspection = inspect_offered_option(&coinset, &wallet, &state, offered).await?;
    require_live_offer(&inspection.offer_status)?;

    let selection = select_for(inspection.spendable.clone(), offered.request_mojos, fee)?;

    let preview = take_preview(&inspection, fee, wallet.address()?);
    if !confirm_or_abort(&preview, assume_yes)? {
        return Ok(());
    }

    execute_take(
        &coinset, &wallet, &mut state, state_file, &mut ctx, offer, &offered, &selection, fee,
    )
    .await
}

/// Shows what an offered option is actually worth, then offers to buy it.
///
/// The offer file only names the option, so its terms and the income backing it are read
/// from the chain. Nothing is spent unless the wallet can cover the asking price and the
/// user accepts the prompt.
async fn cmd_option_inspect(
    key_file: &Path,
    state_file: &Path,
    offer_file: &Path,
    fee: u64,
    assume_yes: bool,
) -> Result<()> {
    let wallet = load_wallet(key_file)?;
    let mut state = State::load(state_file)?;

    let mut ctx = SpendContext::new();
    let offer = read_offer(&mut ctx, offer_file)?;
    let offered = parse_offered_option(&offer)?;

    let coinset = Coinset::mainnet();
    let inspection = inspect_offered_option(&coinset, &wallet, &state, offered).await?;
    let shortfall = inspection.shortfall(fee);
    emit_inspection_report(offer_file, &inspection, fee, shortfall);

    // Only offer to buy something that is still for sale, understood, and affordable.
    if !inspection.offer_status.is_confirmed_unspent()
        || inspection.details.is_err()
        || shortfall.is_some()
        || output::is_json()
    {
        return Ok(());
    }

    let selection = select_for(inspection.spendable.clone(), offered.request_mojos, fee)?;
    let preview = take_preview(&inspection, fee, wallet.address()?);
    if !preview.confirm_opt_in(assume_yes)? {
        output::progress("Not taken.");
        return Ok(());
    }

    execute_take(
        &coinset, &wallet, &mut state, state_file, &mut ctx, offer, &offered, &selection, fee,
    )
    .await
}

/// Reports an inspected offer: what it sells, what backs it, and whether it is affordable.
fn emit_inspection_report(
    offer_file: &Path,
    inspection: &OfferInspection,
    fee: u64,
    shortfall: Option<u64>,
) {
    let offered = &inspection.offered;
    let now = now_seconds();
    let mut report = Report::new(
        "option_inspect",
        format!("Option offer in {}", offer_file.display()),
    );

    report
        .field(
            "Offer status",
            inspection.offer_status.label(),
            "offer_status",
        )
        .field(
            "Option launcher",
            display_id(to_hex(offered.launcher_id)),
            "launcher_id",
        )
        .primary();

    match &inspection.details {
        Ok(details) => {
            report
                .field_json(
                    "Kind",
                    format!(
                        "{} ({})",
                        details.kind.label(),
                        details.kind.exercise_semantics()
                    ),
                    "kind",
                    Value::from(details.kind.label()),
                )
                .field_json(
                    "Strike",
                    format::xch(details.strike_amount),
                    "strike_mojos",
                    Value::from(details.strike_amount),
                )
                .field_json(
                    "Expiration",
                    format::expiration(details.expiration_seconds, now),
                    "expiration_seconds",
                    Value::from(details.expiration_seconds),
                )
                .field(
                    "Creator",
                    display_id(to_hex(details.creator_puzzle_hash)),
                    "creator_puzzle_hash",
                )
                .field(
                    "Underlying NFT",
                    display_id(details.underlying_nft.launcher_id.clone()),
                    "nft_launcher_id",
                );
        }
        Err(error) => {
            report.field_json(
                "Terms",
                "unknown",
                "terms_error",
                Value::String(error.clone()),
            );
        }
    }

    if let Some(p2) = &inspection.p2 {
        report
            .field_json(
                "Income held by NFT",
                format!(
                    "{} in {} coin(s)",
                    format::xch(p2.total_mojos),
                    p2.coins.len()
                ),
                "income_mojos",
                Value::from(p2.total_mojos),
            )
            .json_only("income_coins", Value::from(p2.coins.len()))
            .field("Income address", p2.address.clone(), "income_address");
    }

    report.field_json(
        "Asking price",
        format::xch(offered.request_mojos),
        "request_mojos",
        Value::from(offered.request_mojos),
    );
    if fee > 0 {
        report.field_json("Fee", format::xch(fee), "fee_mojos", Value::from(fee));
    }
    report
        .field_json(
            "Your balance",
            format::xch(inspection.spendable_mojos),
            "spendable_mojos",
            Value::from(inspection.spendable_mojos),
        )
        .json_only("affordable", Value::Bool(shortfall.is_none()));

    // Spell out anything standing between the reader and a sound decision. An offer that
    // cannot settle explains itself; there is no point also explaining why its terms are
    // unreadable, since that is the same fact told twice.
    match (&inspection.offer_status, &inspection.details) {
        (ChainStatus::Spent { .. }, _) => {
            report.note(
                "The offered option coin has been spent, so this offer has already been taken\n\
                 or cancelled and can no longer settle.",
            );
        }
        (ChainStatus::NotFound, _) => {
            report.note(
                "The offered option coin is not on-chain yet, so the offer cannot settle until\n\
                 it confirms.",
            );
        }
        (ChainStatus::LookupFailed { error }, _) => {
            report.note(format!(
                "Could not check whether this offer is still live: {error}"
            ));
        }
        (ChainStatus::ConfirmedUnspent { .. }, Err(error)) => {
            report.note(format!(
                "The option's terms could not be verified against the chain, so what this\n\
                 option is worth is unknown: {error}"
            ));
        }
        (ChainStatus::ConfirmedUnspent { .. }, Ok(details)) => {
            if now >= details.expiration_seconds {
                report.note(
                    "This option has already expired: it can no longer be exercised, and its\n\
                     creator can reclaim the underlying NFT at any time.",
                );
            }
            match shortfall {
                Some(missing) => report.note(format!(
                    "Not enough XCH to take this offer: {} short.",
                    format::xch(missing)
                )),
                None if output::is_json() => report.note(format!(
                    "Affordable. Run `pringle option take {}` to accept it.",
                    offer_file.display()
                )),
                None => &mut report,
            };
        }
    }

    report.emit();
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
    let underlying_coin_id = match &record.underlying_coin_id {
        Some(id) => from_hex(id)?,
        None => record.underlying_nft_coin.to_coin()?.coin_id(),
    };

    let details = inspect::recover_option_details(
        coinset,
        launcher_bytes,
        underlying_coin_id,
        from_hex(&record.underlying_delegated_puzzle_hash)?,
    )
    .await?;

    let nft_launcher = details.underlying_nft.launcher_id.clone();
    let nft_launcher_bytes = from_hex(&nft_launcher)?;
    state.upsert_nft(details.underlying_nft);

    // The recovered NFT deterministically controls a p2 singleton. Track it even when empty
    // so status/sync discovers funds that were attached before this wallet bought the option.
    if state.p2_by_launcher(&nft_launcher).is_none() {
        state.upsert_p2_singleton(p2_singleton::tracking_record(
            nft_launcher_bytes,
            Vec::new(),
            Phase::Confirmed,
        )?);
    }

    // Persist the recovered terms and NFT relationship.
    if let Some(rec) = state.option_mut(launcher_id) {
        rec.strike_amount = details.strike_amount;
        rec.expiration_seconds = details.expiration_seconds;
        rec.creator_puzzle_hash = to_hex(details.creator_puzzle_hash);
        rec.underlying_nft_coin = CoinJson::from_coin(details.underlying_coin);
        rec.nft_launcher_id = Some(nft_launcher);
        // The recovered delegated puzzle hash proves which kind of option this is.
        rec.kind = details.kind;
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
            "Kind",
            format!(
                "{} ({})",
                recovered.kind.label(),
                recovered.kind.exercise_semantics()
            ),
            "kind",
            Value::from(recovered.kind.label()),
        )
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

    match option.kind {
        OptionKind::Transfer => {
            let preview = ActionPreview::new("Exercise option")
                .detail("Kind", option.kind.label())
                .detail("Effect", option.kind.exercise_semantics())
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
            report
                .field_json("Kind", option.kind.label(), "kind", Value::from("transfer"))
                .field_json(
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
        OptionKind::Sweep => {
            let nft_launcher_id = from_hex(&nft_record.launcher_id)?;
            let balance = inspect::p2_singleton_balance(&coinset, nft_launcher_id).await?;
            if balance.coins.is_empty() {
                return Err(AppError::recoverable(
                    "this sweep option has no p2_singleton income to claim",
                )
                .why("there is nothing at the NFT's income address to sweep")
                .next("exercising would gain nothing; wait for income to accrue first")
                .into());
            }
            let plan =
                p2_singleton::plan_sweep(&balance.coins, p2_singleton::MAX_EXERCISE_SWEEP_COINS);

            let income_address = wallet.address()?;
            let mut preview = ActionPreview::new("Exercise option (sweep)")
                .detail("Kind", option.kind.label())
                .detail("Effect", option.kind.exercise_semantics())
                .detail("Pay strike", format::xch(option.strike_amount))
                .detail("Strike to", to_hex(creator_puzzle_hash))
                .detail("Sweep income", format::xch(plan.selected_total))
                .detail("Income to", income_address.clone())
                .detail("Coins swept", plan.selected.len().to_string())
                .detail("NFT returns to creator", to_hex(creator_puzzle_hash))
                .detail("Fee", format::xch(fee));
            if plan.has_skipped() {
                preview = preview.detail(
                    "WARNING coins over cap",
                    format!(
                        "{} coin(s) worth {} exceed the {}-coin per-transaction cap and, since a \
                         sweep option is single-use, are forfeited to the creator",
                        plan.skipped.len(),
                        format::xch(plan.skipped_total),
                        p2_singleton::MAX_EXERCISE_SWEEP_COINS
                    ),
                );
            }
            if !confirm_or_abort(&preview, assume_yes)? {
                return Ok(());
            }

            let mut ctx = SpendContext::new();
            let locked_nft = nft::nft_from_record(&mut ctx, &nft_record)?;
            let outcome = option_contract::build_sweep_exercise(
                &mut ctx,
                &wallet.standard_layer(),
                contract,
                locked_nft,
                creator_puzzle_hash,
                option.expiration_seconds,
                option.strike_amount,
                wallet.puzzle_hash(),
                &plan.selected,
                &selection,
                wallet.puzzle_hash(),
                fee,
                Conditions::new(),
            )?;

            let bundle = sign_spend_bundle(ctx.take(), &[wallet.synthetic_secret_key().clone()])?;
            coinset.push_tx(bundle).await?;

            let mut spent_ids = selection_spent_ids(&selection);
            spent_ids.push(to_hex(option_coin.coin_id()));
            spent_ids.push(to_hex(nft_coin.coin_id()));
            for coin in &plan.selected {
                spent_ids.push(to_hex(coin.coin_id()));
            }
            state.transactions.push(TxRecord::new(
                "option_exercise_sweep",
                spent_ids,
                to_hex(outcome.payout_coin.coin_id()),
            ));

            if let Some(rec) = state.option_mut(&option.launcher_id) {
                rec.phase = Phase::Superseded;
            }
            // The NFT has been returned to the creator; track the new live coin.
            if let Some(rec) = state.nft_mut(&nft_record.launcher_id) {
                rec.coin = CoinJson::from_coin(outcome.returned_nft.coin);
                rec.proof = ProofJson::from_proof(outcome.returned_nft.proof);
                rec.p2_puzzle_hash = to_hex(outcome.returned_nft.info.p2_puzzle_hash);
                rec.current_owner = outcome.returned_nft.info.current_owner.map(to_hex);
                rec.phase = Phase::Pending;
            }
            state.save(state_file)?;

            let mut report = Report::new("option_exercise", "Submitted sweep-option exercise.");
            report
                .field_json("Kind", option.kind.label(), "kind", Value::from("sweep"))
                .field_json(
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
                .field_json(
                    "Income swept",
                    format::xch(outcome.swept_amount),
                    "swept_mojos",
                    Value::from(outcome.swept_amount),
                )
                .primary()
                .field(
                    "Income coin id",
                    to_hex(outcome.payout_coin.coin_id()),
                    "payout_coin_id",
                )
                .field_json(
                    "Coins swept",
                    outcome.coins_swept.to_string(),
                    "coins_swept",
                    Value::from(outcome.coins_swept),
                )
                .field("Income to", income_address, "income_address")
                .field(
                    "NFT returned to creator",
                    to_hex(creator_puzzle_hash),
                    "nft_returned_to",
                );
            if outcome.odd_donation > 0 {
                report.field_json(
                    "Odd-mojo fee donation",
                    format::xch(outcome.odd_donation),
                    "odd_donation_mojos",
                    Value::from(outcome.odd_donation),
                );
            }
            if plan.has_skipped() {
                report.field_json(
                    "Coins forfeited to creator (over cap)",
                    format!(
                        "{} ({})",
                        plan.skipped.len(),
                        format::xch(plan.skipped_total)
                    ),
                    "skipped_coins",
                    Value::from(plan.skipped.len()),
                );
            }
            report.emit();
            Ok(())
        }
    }
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
            println!(
                "      kind:       {} ({})",
                option.kind.label(),
                option.kind.exercise_semantics()
            );
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
/// terms known, created by this wallet, past expiry, not yet reclaimed, its NFT linked, and
/// still open. An exercised option (phase `Superseded`) is never eligible: for a transfer
/// option the NFT went to the holder, and for a sweep option the NFT has already come home to
/// the creator — either way there is nothing locked left to reclaim.
fn clawback_eligible(option: &OptionRecord, wallet_ph: Option<Bytes32>, now: u64) -> bool {
    option.terms_known
        && !option.underlying_reclaimed
        && option.phase != Phase::Superseded
        && option.nft_launcher_id.is_some()
        && now >= option.expiration_seconds
        && wallet_ph.is_some()
        && from_hex(&option.creator_puzzle_hash).ok() == wallet_ph
}

/// Abbreviates an id for human display unless verbose mode is on.
fn display_id(id: String) -> String {
    // Abbreviation is a human-reading convenience; JSON consumers need the exact id.
    if output::is_verbose() || output::is_json() {
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
                "kind": option.kind.label(),
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
