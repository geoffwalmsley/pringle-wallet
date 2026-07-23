//! Production (mainnet) transaction signing.
//!
//! Given a set of coin spends, this computes the required BLS signatures using the
//! mainnet `AGG_SIG_ME` constants, matches them against the provided secret keys,
//! and returns a fully-signed [`SpendBundle`] ready to submit.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use chia_wallet_sdk::chia::bls::{sign, PublicKey, SecretKey, Signature};
use chia_wallet_sdk::clvmr::Allocator;
use chia_wallet_sdk::prelude::{
    AggSigConstants, CoinSpend, RequiredSignature, SpendBundle, MAINNET_CONSTANTS,
};

/// Signs the given coin spends with the mainnet constants and returns a spend bundle.
///
/// Returns an error if any required BLS signature cannot be satisfied by the provided
/// keys, or if a spend requires a non-BLS (secp) signature (unsupported here).
pub fn sign_spend_bundle(coin_spends: Vec<CoinSpend>, keys: &[SecretKey]) -> Result<SpendBundle> {
    let mut allocator = Allocator::new();
    let constants = AggSigConstants::from(&*MAINNET_CONSTANTS);
    let required = RequiredSignature::from_coin_spends(&mut allocator, &coin_spends, &constants)?;

    let key_pairs: HashMap<PublicKey, &SecretKey> =
        keys.iter().map(|sk| (sk.public_key(), sk)).collect();

    let mut aggregate = Signature::default();
    for requirement in required {
        match requirement {
            RequiredSignature::Bls(bls) => {
                let sk = key_pairs.get(&bls.public_key).ok_or_else(|| {
                    anyhow!(
                        "missing secret key for required signature by {:?}",
                        bls.public_key
                    )
                })?;
                aggregate += &sign(sk, bls.message());
            }
            RequiredSignature::Secp(_) => {
                return Err(anyhow!(
                    "spend requires a secp signature, which this CLI does not support"
                ));
            }
        }
    }

    Ok(SpendBundle::new(coin_spends, aggregate))
}
