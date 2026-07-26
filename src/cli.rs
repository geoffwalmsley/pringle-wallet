//! Command-line argument definitions.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// A simple mainnet Chia CLI: keys, XCH, NFTs, and NFT-backed options.
///
/// All amounts are in mojos (1 XCH = 1_000_000_000_000 mojos). This wallet operates on
/// mainnet and submits real transactions; destructive commands preview the action and (in
/// an interactive terminal) ask for confirmation unless `--yes` is given.
///
/// Common workflow:
///   pringle init
///   pringle nft mint
///   pringle status                 # refreshes against the chain
///   pringle nft address            # income address controlled by the NFT
///   pringle option create --strike 5000000000000 --expiration <unix>
///   pringle option offer --request 250000000 -o my.offer
#[derive(Debug, Parser)]
#[command(name = "pringle", version, about, long_about = None)]
pub struct Cli {
    /// Path to the local hex-encoded master key file.
    #[arg(long, global = true, default_value = "pringle-key.hex")]
    pub key_file: PathBuf,

    /// Path to the local JSON state file.
    #[arg(long, global = true, default_value = "pringle-state.json")]
    pub state_file: PathBuf,

    /// Emit machine-readable JSON instead of human output.
    #[arg(long, global = true)]
    pub json: bool,

    /// Show extra detail (full ids, puzzle hashes, raw phases).
    #[arg(long, global = true)]
    pub verbose: bool,

    /// Suppress non-essential output (errors and primary values only).
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Skip interactive confirmation prompts for destructive mainnet actions.
    #[arg(long, global = true)]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Generate a new local key and initialize fresh state.
    Init,
    /// Regular XCH wallet operations.
    Xch {
        #[command(subcommand)]
        command: XchCommand,
    },
    /// NFT operations.
    Nft {
        #[command(subcommand)]
        command: NftCommand,
    },
    /// Option contract operations.
    Option {
        #[command(subcommand)]
        command: OptionCommand,
    },
    /// Show the Pot Potato game: the pot, who holds it, and the most recent holders.
    ///
    /// The potato is a coin lineage with no fixed puzzle hash, so it has to be followed
    /// hop by hop. The newest coin found is cached in the state file, which keeps repeat
    /// runs to a single lookup.
    Potato {
        /// How many previous holders to list.
        #[arg(long, default_value_t = 5)]
        holders: usize,
        /// Follow the lineage from this potato coin id instead of the cached anchor.
        #[arg(long)]
        coin: Option<String>,
    },
    /// Show the local lifecycle state, refreshed against the chain by default.
    Status {
        /// Show the local snapshot without contacting the network.
        #[arg(long)]
        cached: bool,
    },
    /// Reconcile local state against the blockchain (follow singletons to their live coins,
    /// refresh funded coins, update phases, and prune settled transactions).
    Sync,
}

#[derive(Debug, Subcommand)]
pub enum XchCommand {
    /// Print the wallet's mainnet address.
    Address,
    /// List confirmed, unspent coins owned by the wallet.
    Coins,
    /// Show details for a specific coin id.
    Coin {
        /// The coin id (hex, with or without `0x`).
        coin_id: String,
    },
    /// Combine every spendable standard-wallet XCH coin into one coin.
    Consolidate {
        /// Transaction fee in mojos (deducted from the consolidated output).
        #[arg(long, default_value_t = 0)]
        fee: u64,
    },
    /// Send the entire spendable standard-wallet XCH balance to an address.
    SendAll {
        /// Destination mainnet XCH address.
        address: String,
        /// Transaction fee in mojos (deducted from the sent amount).
        #[arg(long, default_value_t = 0)]
        fee: u64,
    },
}

#[derive(Debug, Subcommand)]
pub enum NftCommand {
    /// Mint a singleton NFT owned by the wallet.
    Mint {
        /// Transaction fee in mojos.
        #[arg(long, default_value_t = 0)]
        fee: u64,
        /// Royalty in hundredths of a percent (e.g. 300 = 3%).
        #[arg(long, default_value_t = 0)]
        royalty_basis_points: u16,
        /// One or more data URIs for the NFT metadata.
        #[arg(long = "data-uri")]
        data_uris: Vec<String>,
        /// The SHA-256 hash of the data (hex), if using data URIs.
        #[arg(long)]
        data_hash: Option<String>,
    },
    /// Print the income address controlled by the NFT (its p2 singleton).
    Address {
        /// Which NFT to use, by launcher id (required only when several are tracked).
        #[arg(long)]
        launcher: Option<String>,
    },
    /// Sweep the NFT's entire accumulated income (all coins) to an address in one transaction.
    Sweep {
        /// Destination address (defaults to the wallet's own address).
        #[arg(long)]
        address: Option<String>,
        /// Transaction fee in mojos (taken out of the swept balance).
        #[arg(long, default_value_t = 0)]
        fee: u64,
        /// Which NFT's income to sweep, by launcher id (required only when several
        /// are tracked).
        #[arg(long)]
        launcher: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum OptionCommand {
    /// List wallet-owned open options with full launcher ids and exercise commands.
    #[command(name = "show-all")]
    ShowAll {
        /// Include expired, exercised, closed, and transferred options.
        #[arg(long)]
        include_closed: bool,
        /// Use local state without refreshing from the blockchain.
        #[arg(long)]
        cached: bool,
    },
    /// Create an option contract with the NFT as the underlying and an XCH strike.
    Create {
        /// The XCH strike amount in mojos (paid by the exerciser to the creator).
        #[arg(long)]
        strike: u64,
        /// Absolute expiration as a Unix timestamp in seconds.
        #[arg(long)]
        expiration: u64,
        /// Address that receives the strike payment (defaults to the wallet address).
        #[arg(long)]
        creator_address: Option<String>,
        /// Address that initially owns the option (defaults to the wallet address).
        #[arg(long)]
        owner_address: Option<String>,
        /// Transaction fee in mojos.
        #[arg(long, default_value_t = 0)]
        fee: u64,
        /// Which NFT to use as the underlying (required only when several are tracked).
        #[arg(long)]
        launcher: Option<String>,
    },
    /// Create an offer file that sells the option coin for XCH.
    Offer {
        /// XCH amount (in mojos) requested in exchange for the option.
        #[arg(long)]
        request: u64,
        /// Address to receive the requested XCH (defaults to the wallet address).
        #[arg(long)]
        receive_address: Option<String>,
        /// Where to write the `.offer` file (defaults to `option.offer`).
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Which option to sell, by launcher id (required only when several are tracked).
        #[arg(long)]
        launcher: Option<String>,
    },
    /// Accept an option offer file, paying the requested XCH to receive the option.
    Take {
        /// Path to the `.offer` file to accept.
        offer_file: PathBuf,
        /// Transaction fee in mojos.
        #[arg(long, default_value_t = 0)]
        fee: u64,
    },
    /// Exercise the option: pay the XCH strike to the creator and receive the NFT.
    Exercise {
        /// Transaction fee in mojos.
        #[arg(long, default_value_t = 0)]
        fee: u64,
        /// Which option to exercise, by launcher id (required only when several are tracked).
        #[arg(long)]
        launcher: Option<String>,
    },
    /// Recover a purchased option's terms (strike/expiration/creator) and underlying NFT
    /// from the chain, backfilling options taken by older versions.
    Recover {
        /// Which option to recover, by launcher id (required only when several are tracked).
        #[arg(long)]
        launcher: Option<String>,
    },
    /// Reclaim the underlying NFT of an expired option you created (creator-only clawback).
    ///
    /// Only works after the option's expiration deadline has passed. The expired option coin
    /// is left untouched; once the reclaimed NFT confirms, use `nft sweep` to
    /// withdraw its accumulated income.
    Clawback {
        /// Destination address for the reclaimed NFT owner (defaults to the wallet address).
        #[arg(long)]
        address: Option<String>,
        /// Transaction fee in mojos (funded from separate regular-XCH coins).
        #[arg(long, default_value_t = 0)]
        fee: u64,
        /// Which option to claw back, by launcher id (required only when several are tracked).
        #[arg(long)]
        launcher: Option<String>,
    },
}
