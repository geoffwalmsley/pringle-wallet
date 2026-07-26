//! Core library for the `pringle` CLI.
//!
//! The modules here are deliberately split so that the transaction-building logic
//! (`nft`, `p2_singleton`, `option`, `wallet`, `signing`) is independent of any
//! network access. Coinset I/O lives in [`coinset`], and the binary in `main.rs`
//! wires everything together. This separation lets the integration tests drive the
//! exact same builders against the in-memory simulator instead of mainnet.

pub mod chain;
pub mod coinset;
pub mod confirm;
pub mod format;
pub mod inspect;
pub mod key;
pub mod nft;
pub mod option;
pub mod output;
pub mod p2_singleton;
pub mod potato;
pub mod signing;
pub mod state;
pub mod status_view;
pub mod sweep_option;
pub mod sync;
pub mod wallet;

/// The number of mojos in one XCH.
pub const MOJOS_PER_XCH: u64 = 1_000_000_000_000;

/// The bech32m prefix used for standard mainnet addresses.
pub const MAINNET_PREFIX: &str = "xch";
