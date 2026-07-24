//! Persistent, resumable CLI state.
//!
//! Each lifecycle phase (NFT mint, p2_singleton funding, option creation) records
//! enough information to be reconstructed and to check confirmation later. State is
//! written atomically (temp file + rename) so an interrupted write cannot corrupt it.
//!
//! To avoid coupling the on-disk format to SDK-internal serde features, everything is
//! stored as hex strings / integers and converted to/from SDK types via the helpers here.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chia_wallet_sdk::chia::puzzle_types::{EveProof, LineageProof, Proof};
use chia_wallet_sdk::prelude::{Bytes32, Coin};
use serde::{Deserialize, Serialize};

/// The current on-disk state schema version.
pub const STATE_VERSION: u32 = 2;

/// Status of a tracked lifecycle object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Submitted to the mempool but not yet observed as confirmed on-chain.
    Pending,
    /// Observed as confirmed and unspent on-chain.
    Confirmed,
    /// Superseded (e.g. the NFT was locked into an option), no longer the live coin.
    Superseded,
}

/// The root persisted state document (schema v2, multi-asset).
///
/// v1 state stored a single `nft`/`p2_singleton`/`option`. Those fields are still accepted
/// on load (for backward compatibility) and folded into the v2 collections by
/// [`State::migrate`]; they are never written back out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// On-disk schema version (see [`STATE_VERSION`]).
    #[serde(default)]
    pub version: u32,
    /// Path to the key file this state is associated with (informational).
    #[serde(default)]
    pub key_file: String,
    /// The wallet's standard puzzle hash (hex).
    #[serde(default)]
    pub wallet_puzzle_hash: String,
    /// The wallet's `xch` address.
    #[serde(default)]
    pub wallet_address: String,
    /// A log of submitted transactions.
    #[serde(default)]
    pub transactions: Vec<TxRecord>,
    /// The NFTs tracked by this wallet.
    #[serde(default)]
    pub nfts: Vec<NftRecord>,
    /// The p2_singletons tracked by this wallet (keyed by controlling NFT launcher id).
    #[serde(default)]
    pub p2_singletons: Vec<P2SingletonRecord>,
    /// The options tracked by this wallet (created or purchased).
    #[serde(default)]
    pub options: Vec<OptionRecord>,

    // ---- Legacy v1 single-asset fields (migrated on load, never re-serialized). ----
    #[serde(default, skip_serializing)]
    pub nft: Option<NftRecord>,
    #[serde(default, skip_serializing)]
    pub p2_singleton: Option<P2SingletonRecord>,
    #[serde(default, skip_serializing)]
    pub option: Option<OptionRecord>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            key_file: String::new(),
            wallet_puzzle_hash: String::new(),
            wallet_address: String::new(),
            transactions: Vec::new(),
            nfts: Vec::new(),
            p2_singletons: Vec::new(),
            options: Vec::new(),
            nft: None,
            p2_singleton: None,
            option: None,
        }
    }
}

/// A record of a single submitted transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxRecord {
    /// A human-readable label for the transaction kind.
    pub kind: String,
    /// The coins that were spent (inputs), hex coin ids.
    pub spent_coin_ids: Vec<String>,
    /// The primary coin id created that later phases will look up for confirmation.
    pub watch_coin_id: String,
    /// Unix seconds when the transaction was submitted (best-effort; `None` for records
    /// created before this field existed).
    #[serde(default)]
    pub submitted_at: Option<u64>,
}

impl TxRecord {
    /// Creates a transaction record stamped with the current time.
    pub fn new(
        kind: impl Into<String>,
        spent_coin_ids: Vec<String>,
        watch_coin_id: String,
    ) -> Self {
        let submitted_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .ok();
        Self {
            kind: kind.into(),
            spent_coin_ids,
            watch_coin_id,
            submitted_at,
        }
    }
}

/// Serializable form of a [`Coin`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoinJson {
    pub parent_coin_info: String,
    pub puzzle_hash: String,
    pub amount: u64,
}

impl CoinJson {
    pub fn from_coin(coin: Coin) -> Self {
        Self {
            parent_coin_info: to_hex(coin.parent_coin_info),
            puzzle_hash: to_hex(coin.puzzle_hash),
            amount: coin.amount,
        }
    }

    pub fn to_coin(&self) -> Result<Coin> {
        Ok(Coin::new(
            from_hex(&self.parent_coin_info)?,
            from_hex(&self.puzzle_hash)?,
            self.amount,
        ))
    }
}

/// Serializable form of a singleton [`Proof`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProofJson {
    Eve {
        parent_parent_coin_info: String,
        parent_amount: u64,
    },
    Lineage {
        parent_parent_coin_info: String,
        parent_inner_puzzle_hash: String,
        parent_amount: u64,
    },
}

impl ProofJson {
    pub fn from_proof(proof: Proof) -> Self {
        match proof {
            Proof::Eve(eve) => Self::Eve {
                parent_parent_coin_info: to_hex(eve.parent_parent_coin_info),
                parent_amount: eve.parent_amount,
            },
            Proof::Lineage(lineage) => Self::Lineage {
                parent_parent_coin_info: to_hex(lineage.parent_parent_coin_info),
                parent_inner_puzzle_hash: to_hex(lineage.parent_inner_puzzle_hash),
                parent_amount: lineage.parent_amount,
            },
        }
    }

    pub fn to_proof(&self) -> Result<Proof> {
        Ok(match self {
            Self::Eve {
                parent_parent_coin_info,
                parent_amount,
            } => Proof::Eve(EveProof {
                parent_parent_coin_info: from_hex(parent_parent_coin_info)?,
                parent_amount: *parent_amount,
            }),
            Self::Lineage {
                parent_parent_coin_info,
                parent_inner_puzzle_hash,
                parent_amount,
            } => Proof::Lineage(LineageProof {
                parent_parent_coin_info: from_hex(parent_parent_coin_info)?,
                parent_inner_puzzle_hash: from_hex(parent_inner_puzzle_hash)?,
                parent_amount: *parent_amount,
            }),
        })
    }
}

/// Serializable form of [`chia_puzzle_types::nft::NftMetadata`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataJson {
    pub edition_number: u64,
    pub edition_total: u64,
    pub data_uris: Vec<String>,
    pub data_hash: Option<String>,
    pub metadata_uris: Vec<String>,
    pub metadata_hash: Option<String>,
    pub license_uris: Vec<String>,
    pub license_hash: Option<String>,
}

/// Everything needed to reconstruct and re-spend the wallet's NFT.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NftRecord {
    pub launcher_id: String,
    pub coin: CoinJson,
    pub proof: ProofJson,
    pub metadata: MetadataJson,
    pub metadata_updater_puzzle_hash: String,
    pub current_owner: Option<String>,
    pub royalty_puzzle_hash: String,
    pub royalty_basis_points: u16,
    pub p2_puzzle_hash: String,
    pub phase: Phase,
}

/// The p2_singleton controlled by the NFT, plus the coins funded into it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct P2SingletonRecord {
    pub launcher_id: String,
    pub puzzle_hash: String,
    pub address: String,
    pub funded_coins: Vec<CoinJson>,
    pub phase: Phase,
}

/// How this wallet came to track an option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OptionOrigin {
    /// Created (minted) by this wallet.
    #[default]
    Created,
    /// Purchased by taking an offer.
    Purchased,
}

/// The option contract created on (or purchased for) the NFT.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionRecord {
    pub launcher_id: String,
    pub coin: CoinJson,
    /// Lineage/eve proof needed to re-spend the option singleton (e.g. to make an offer).
    ///
    /// Optional for backward compatibility with options created before this field existed;
    /// when absent it can be recovered from the chain via the parent coin's spend.
    #[serde(default)]
    pub proof: Option<ProofJson>,
    pub underlying_nft_coin: CoinJson,
    pub underlying_delegated_puzzle_hash: String,
    pub strike_amount: u64,
    pub expiration_seconds: u64,
    pub creator_puzzle_hash: String,
    pub owner_puzzle_hash: String,
    pub phase: Phase,
    /// The underlying coin id the option is bound to, when known. Preferred over
    /// `underlying_nft_coin` for reconstructing the option (a purchased option may know the
    /// coin id without the full coin details).
    #[serde(default)]
    pub underlying_coin_id: Option<String>,
    /// Launcher id of the underlying NFT, when known (links the option to its NFT record).
    #[serde(default)]
    pub nft_launcher_id: Option<String>,
    /// Whether this option was created by us or purchased via an offer.
    #[serde(default)]
    pub origin: OptionOrigin,
    /// True when the terms (strike/expiration/creator) are known-good (recovered/verified),
    /// false for a purchased option whose terms have not yet been recovered from the chain.
    #[serde(default = "default_true")]
    pub terms_known: bool,
    /// True once the creator has clawed back the underlying NFT after expiry. The option
    /// singleton is left inert (never melted by clawback), so this flag distinguishes an
    /// expired-but-reclaimed option from one merely awaiting clawback. Defaults to false for
    /// records written before this field existed.
    #[serde(default)]
    pub underlying_reclaimed: bool,
}

fn default_true() -> bool {
    true
}

impl State {
    /// Loads state from disk, returning a default (empty) state if the file is absent.
    ///
    /// Legacy v1 state (single `nft`/`p2_singleton`/`option`) is migrated in-memory into the
    /// v2 collections; the migration is only persisted the next time [`State::save`] runs.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read state file {}", path.display()))?;
        if contents.trim().is_empty() {
            return Ok(Self::default());
        }
        let mut state: State = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse state file {}", path.display()))?;
        state.migrate();
        Ok(state)
    }

    /// Folds any legacy single-asset fields into the v2 collections and stamps the current
    /// schema version. Idempotent.
    pub fn migrate(&mut self) {
        if let Some(nft) = self.nft.take() {
            self.upsert_nft(nft);
        }
        if let Some(p2) = self.p2_singleton.take() {
            self.upsert_p2_singleton(p2);
        }
        if let Some(option) = self.option.take() {
            self.upsert_option(option);
        }
        self.version = STATE_VERSION;
    }

    /// Inserts or replaces an NFT record, keyed by launcher id.
    pub fn upsert_nft(&mut self, record: NftRecord) {
        match self
            .nfts
            .iter_mut()
            .find(|n| n.launcher_id == record.launcher_id)
        {
            Some(existing) => *existing = record,
            None => self.nfts.push(record),
        }
    }

    /// Inserts or replaces a p2_singleton record, keyed by (controlling NFT) launcher id.
    pub fn upsert_p2_singleton(&mut self, record: P2SingletonRecord) {
        match self
            .p2_singletons
            .iter_mut()
            .find(|p| p.launcher_id == record.launcher_id)
        {
            Some(existing) => *existing = record,
            None => self.p2_singletons.push(record),
        }
    }

    /// Inserts or replaces an option record, keyed by launcher id.
    pub fn upsert_option(&mut self, record: OptionRecord) {
        match self
            .options
            .iter_mut()
            .find(|o| o.launcher_id == record.launcher_id)
        {
            Some(existing) => *existing = record,
            None => self.options.push(record),
        }
    }

    /// Finds an NFT record by launcher id (hex, with or without `0x`).
    pub fn nft_by_launcher(&self, launcher_id: &str) -> Option<&NftRecord> {
        let want = normalize_hex(launcher_id);
        self.nfts
            .iter()
            .find(|n| normalize_hex(&n.launcher_id) == want)
    }

    /// Finds an option record by launcher id (hex, with or without `0x`).
    pub fn option_by_launcher(&self, launcher_id: &str) -> Option<&OptionRecord> {
        let want = normalize_hex(launcher_id);
        self.options
            .iter()
            .find(|o| normalize_hex(&o.launcher_id) == want)
    }

    /// Finds a p2_singleton record by its controlling NFT launcher id.
    pub fn p2_by_launcher(&self, launcher_id: &str) -> Option<&P2SingletonRecord> {
        let want = normalize_hex(launcher_id);
        self.p2_singletons
            .iter()
            .find(|p| normalize_hex(&p.launcher_id) == want)
    }

    /// Mutable NFT lookup by launcher id.
    pub fn nft_mut(&mut self, launcher_id: &str) -> Option<&mut NftRecord> {
        let want = normalize_hex(launcher_id);
        self.nfts
            .iter_mut()
            .find(|n| normalize_hex(&n.launcher_id) == want)
    }

    /// Mutable option lookup by launcher id.
    pub fn option_mut(&mut self, launcher_id: &str) -> Option<&mut OptionRecord> {
        let want = normalize_hex(launcher_id);
        self.options
            .iter_mut()
            .find(|o| normalize_hex(&o.launcher_id) == want)
    }

    /// Mutable p2_singleton lookup by controlling NFT launcher id.
    pub fn p2_mut(&mut self, launcher_id: &str) -> Option<&mut P2SingletonRecord> {
        let want = normalize_hex(launcher_id);
        self.p2_singletons
            .iter_mut()
            .find(|p| normalize_hex(&p.launcher_id) == want)
    }

    /// Selects a single NFT, honoring an optional `--launcher` selector.
    ///
    /// With no selector: returns the sole NFT, or errors if there are zero or several.
    /// With a selector: returns the matching NFT, or errors if none matches.
    pub fn select_nft(&self, launcher: Option<&str>) -> Result<NftRecord> {
        select_one(&self.nfts, launcher, |n| &n.launcher_id, "NFT", "nft mint").cloned()
    }

    /// Selects a single option, honoring an optional `--launcher` selector.
    pub fn select_option(&self, launcher: Option<&str>) -> Result<OptionRecord> {
        select_one(
            &self.options,
            launcher,
            |o| &o.launcher_id,
            "option",
            "option create",
        )
        .cloned()
    }

    /// Atomically writes state to disk (temp file + rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).context("failed to serialize state")?;
        let tmp: PathBuf = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)
            .with_context(|| format!("failed to write temp state file {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("failed to replace state file {}", path.display()))?;
        Ok(())
    }
}

/// Normalizes a hex string for comparison (lowercase, no `0x` prefix).
pub fn normalize_hex(s: &str) -> String {
    s.strip_prefix("0x").unwrap_or(s).to_lowercase()
}

/// Selects exactly one record from a slice, honoring an optional launcher-id selector.
///
/// - `launcher = Some(id)`: returns the record whose launcher id matches, else an error.
/// - `launcher = None` with one record: returns it.
/// - `launcher = None` with zero records: an error suggesting `create_hint`.
/// - `launcher = None` with several: an error listing the launcher ids to disambiguate.
fn select_one<'a, T>(
    records: &'a [T],
    launcher: Option<&str>,
    launcher_id: impl Fn(&T) -> &String,
    noun: &str,
    create_hint: &str,
) -> Result<&'a T> {
    if let Some(want) = launcher {
        let want = normalize_hex(want);
        return records
            .iter()
            .find(|r| normalize_hex(launcher_id(r)) == want)
            .ok_or_else(|| anyhow::anyhow!("no tracked {noun} with launcher id {want}"));
    }
    match records.len() {
        0 => anyhow::bail!("no {noun} tracked yet; run `pringle {create_hint}` first"),
        1 => Ok(&records[0]),
        _ => {
            let ids = records
                .iter()
                .map(|r| format!("  --launcher {}", launcher_id(r)))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!("several {noun}s are tracked; select one with --launcher:\n{ids}")
        }
    }
}

/// Encodes a [`Bytes32`] as a `0x`-prefixed hex string.
pub fn to_hex(bytes: Bytes32) -> String {
    format!("0x{}", hex::encode(bytes.to_bytes()))
}

/// Decodes a hex string (with or without `0x`) into a [`Bytes32`].
pub fn from_hex(s: &str) -> Result<Bytes32> {
    let normalized = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(normalized).with_context(|| format!("invalid hex value {s}"))?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected 32 bytes in hex value {s}"))?;
    Ok(Bytes32::new(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let value = Bytes32::new([9u8; 32]);
        let encoded = to_hex(value);
        assert!(encoded.starts_with("0x"));
        assert_eq!(from_hex(&encoded).unwrap(), value);
        // Also accept the un-prefixed form.
        assert_eq!(from_hex(&encoded[2..]).unwrap(), value);
    }

    #[test]
    fn coin_roundtrip() {
        let coin = Coin::new(Bytes32::new([1u8; 32]), Bytes32::new([2u8; 32]), 1234);
        let json = CoinJson::from_coin(coin);
        assert_eq!(json.to_coin().unwrap(), coin);
    }

    #[test]
    fn proof_roundtrip() {
        let eve = Proof::Eve(EveProof {
            parent_parent_coin_info: Bytes32::new([5u8; 32]),
            parent_amount: 7,
        });
        assert!(matches!(
            ProofJson::from_proof(eve).to_proof().unwrap(),
            Proof::Eve(_)
        ));

        let lineage = Proof::Lineage(LineageProof {
            parent_parent_coin_info: Bytes32::new([6u8; 32]),
            parent_inner_puzzle_hash: Bytes32::new([7u8; 32]),
            parent_amount: 3,
        });
        assert!(matches!(
            ProofJson::from_proof(lineage).to_proof().unwrap(),
            Proof::Lineage(_)
        ));
    }

    #[test]
    fn state_save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("pringle-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");

        let mut state = State {
            wallet_address: "xch1example".to_string(),
            ..Default::default()
        };
        state.transactions.push(TxRecord::new(
            "nft_mint",
            vec!["0xabc".to_string()],
            "0xdef".to_string(),
        ));
        state.save(&path).unwrap();

        let loaded = State::load(&path).unwrap();
        assert_eq!(loaded.wallet_address, "xch1example");
        assert_eq!(loaded.transactions.len(), 1);
        assert_eq!(loaded.version, STATE_VERSION);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_returns_default() {
        let path = Path::new("/nonexistent/pringle/state.json");
        let state = State::load(path).unwrap();
        assert!(state.nfts.is_empty());
        assert_eq!(state.version, STATE_VERSION);
    }

    fn sample_nft(launcher: &str) -> NftRecord {
        NftRecord {
            launcher_id: launcher.to_string(),
            coin: CoinJson::from_coin(Coin::new(
                Bytes32::new([1u8; 32]),
                Bytes32::new([2u8; 32]),
                1,
            )),
            proof: ProofJson::from_proof(Proof::Eve(EveProof {
                parent_parent_coin_info: Bytes32::new([3u8; 32]),
                parent_amount: 1,
            })),
            metadata: MetadataJson {
                edition_number: 1,
                edition_total: 1,
                data_uris: vec![],
                data_hash: None,
                metadata_uris: vec![],
                metadata_hash: None,
                license_uris: vec![],
                license_hash: None,
            },
            metadata_updater_puzzle_hash: to_hex(Bytes32::new([4u8; 32])),
            current_owner: None,
            royalty_puzzle_hash: to_hex(Bytes32::new([5u8; 32])),
            royalty_basis_points: 0,
            p2_puzzle_hash: to_hex(Bytes32::new([6u8; 32])),
            phase: Phase::Confirmed,
        }
    }

    #[test]
    fn migrates_legacy_single_nft_into_collection() {
        // A v1-shaped document with a single `nft` field and no `nfts` array.
        let json = serde_json::json!({
            "key_file": "k.hex",
            "wallet_address": "xch1legacy",
            "nft": {
                "launcher_id": "0xaa",
                "coin": {"parent_coin_info": "0x01", "puzzle_hash": "0x02", "amount": 1},
                "proof": {"kind": "eve", "parent_parent_coin_info": "0x03", "parent_amount": 1},
                "metadata": {
                    "edition_number": 1, "edition_total": 1,
                    "data_uris": [], "data_hash": null,
                    "metadata_uris": [], "metadata_hash": null,
                    "license_uris": [], "license_hash": null
                },
                "metadata_updater_puzzle_hash": "0x04",
                "current_owner": null,
                "royalty_puzzle_hash": "0x05",
                "royalty_basis_points": 0,
                "p2_puzzle_hash": "0x06",
                "phase": "confirmed"
            }
        });
        let mut state: State = serde_json::from_value(json).unwrap();
        state.migrate();
        assert_eq!(state.version, STATE_VERSION);
        assert_eq!(state.nfts.len(), 1);
        assert_eq!(state.nfts[0].launcher_id, "0xaa");
        assert!(state.nft.is_none());
    }

    #[test]
    fn upsert_replaces_by_launcher() {
        let mut state = State::default();
        state.upsert_nft(sample_nft("0xaa"));
        state.upsert_nft(sample_nft("0xbb"));
        assert_eq!(state.nfts.len(), 2);
        // Replacing an existing launcher does not add a duplicate.
        state.upsert_nft(sample_nft("0xaa"));
        assert_eq!(state.nfts.len(), 2);
    }

    #[test]
    fn option_record_defaults_underlying_reclaimed_to_false() {
        // A record written before the `underlying_reclaimed` field must still deserialize,
        // defaulting the new field to false (not yet reclaimed).
        let json = serde_json::json!({
            "launcher_id": "0xaa",
            "coin": {"parent_coin_info": "0x01", "puzzle_hash": "0x02", "amount": 1},
            "underlying_nft_coin": {"parent_coin_info": "0x03", "puzzle_hash": "0x04", "amount": 1},
            "underlying_delegated_puzzle_hash": "0x05",
            "strike_amount": 1000,
            "expiration_seconds": 2000,
            "creator_puzzle_hash": "0x06",
            "owner_puzzle_hash": "0x07",
            "phase": "confirmed"
        });
        let record: OptionRecord = serde_json::from_value(json).unwrap();
        assert!(!record.underlying_reclaimed);
        assert!(record.terms_known); // also defaults to true
    }

    #[test]
    fn select_nft_disambiguation() {
        let mut state = State::default();
        assert!(state.select_nft(None).is_err()); // none
        state.upsert_nft(sample_nft("0xaa"));
        assert!(state.select_nft(None).is_ok()); // exactly one
        state.upsert_nft(sample_nft("0xbb"));
        assert!(state.select_nft(None).is_err()); // ambiguous
        assert!(state.select_nft(Some("0xbb")).is_ok()); // explicit
        assert!(state.select_nft(Some("0xzz")).is_err()); // no match
    }
}
