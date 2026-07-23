//! Local BLS key generation, storage, and standard-wallet derivation.
//!
//! No security is applied: the master key is stored as raw hex on disk. This is by
//! design for a throwaway demo wallet, and is called out loudly in the README.

use std::path::Path;

use anyhow::{Context, Result};
use chia_wallet_sdk::chia::bls::{master_to_wallet_unhardened, PublicKey, SecretKey};
use chia_wallet_sdk::chia::puzzle_types::{standard::StandardArgs, DeriveSynthetic};
use chia_wallet_sdk::prelude::{Address, Bytes32, StandardLayer};
use rand::RngCore;

use crate::MAINNET_PREFIX;

/// A standard wallet derived from a locally stored master key.
///
/// This uses the first unhardened child (index 0) and its synthetic key, which is
/// what the standard puzzle (`p2_delegated_puzzle_or_hidden_puzzle`) expects.
#[derive(Debug, Clone)]
pub struct Wallet {
    master_sk: SecretKey,
    synthetic_sk: SecretKey,
    synthetic_pk: PublicKey,
    puzzle_hash: Bytes32,
}

impl Wallet {
    /// Derives a [`Wallet`] from a master secret key using wallet index 0.
    pub fn from_master(master_sk: SecretKey) -> Self {
        let child = master_to_wallet_unhardened(&master_sk, 0);
        let synthetic_sk = child.derive_synthetic();
        let synthetic_pk = synthetic_sk.public_key();
        let puzzle_hash = StandardArgs::curry_tree_hash(synthetic_pk).into();

        Self {
            master_sk,
            synthetic_sk,
            synthetic_pk,
            puzzle_hash,
        }
    }

    /// Generates a fresh random master key and derives a wallet from it.
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        rand::rng().fill_bytes(&mut seed);
        Self::from_master(SecretKey::from_seed(&seed))
    }

    /// The synthetic public key used to construct the standard puzzle.
    pub fn synthetic_public_key(&self) -> PublicKey {
        self.synthetic_pk
    }

    /// The synthetic secret key used to sign standard spends.
    pub fn synthetic_secret_key(&self) -> &SecretKey {
        &self.synthetic_sk
    }

    /// The standard puzzle hash for this wallet.
    pub fn puzzle_hash(&self) -> Bytes32 {
        self.puzzle_hash
    }

    /// A [`StandardLayer`] for building/spending standard coins owned by this wallet.
    pub fn standard_layer(&self) -> StandardLayer {
        StandardLayer::new(self.synthetic_pk)
    }

    /// The mainnet `xch` address for this wallet.
    pub fn address(&self) -> Result<String> {
        Address::new(self.puzzle_hash, MAINNET_PREFIX.to_string())
            .encode()
            .context("failed to encode wallet address")
    }

    /// The raw master key bytes, for persistence.
    pub fn master_key_bytes(&self) -> [u8; 32] {
        self.master_sk.to_bytes()
    }
}

/// Loads a master key from a hex file and derives the wallet.
pub fn load_wallet(path: &Path) -> Result<Wallet> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read key file {}", path.display()))?;
    let trimmed = contents.trim();
    let normalized = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    let bytes = hex::decode(normalized).context("key file is not valid hex")?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("key file must contain exactly 32 bytes"))?;
    let master = SecretKey::from_bytes(&bytes).context("invalid BLS secret key")?;
    Ok(Wallet::from_master(master))
}

/// Writes a master key to a hex file, refusing to overwrite an existing key.
pub fn save_new_wallet(path: &Path, wallet: &Wallet) -> Result<()> {
    if path.exists() {
        anyhow::bail!(
            "key file {} already exists; refusing to overwrite it",
            path.display()
        );
    }
    let hex = hex::encode(wallet.master_key_bytes());
    std::fs::write(path, format!("{hex}\n"))
        .with_context(|| format!("failed to write key file {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_deterministic() {
        let master = SecretKey::from_seed(&[7u8; 32]);
        let a = Wallet::from_master(master.clone());
        let b = Wallet::from_master(master);
        assert_eq!(a.puzzle_hash(), b.puzzle_hash());
        assert_eq!(a.synthetic_public_key(), b.synthetic_public_key());
    }

    #[test]
    fn address_roundtrips_to_puzzle_hash() {
        let wallet = Wallet::from_master(SecretKey::from_seed(&[3u8; 32]));
        let address = wallet.address().unwrap();
        assert!(address.starts_with("xch1"));
        let decoded = Address::decode(&address).unwrap();
        assert_eq!(decoded.puzzle_hash, wallet.puzzle_hash());
    }

    #[test]
    fn generated_wallets_differ() {
        let a = Wallet::generate();
        let b = Wallet::generate();
        assert_ne!(a.master_key_bytes(), b.master_key_bytes());
    }
}
