//! Reading the Pot Potato game from the chain.
//!
//! A potato coin is a `p2_1_of_n` coin with three spend paths: pass it to a new holder
//! (only valid before the holder's 24-hour deadline), or, once the deadline has passed,
//! let the holder take the pot. Every pass mints the successor potato exactly one
//! [`PRICE`] richer, so a round is a coin lineage that can be followed in both directions
//! with nothing but coin records and spends.
//!
//! A round therefore contains no claims by construction: the lineage only exists because
//! every holder so far passed the potato on in time. The claim is the event that ends it.

use anyhow::{bail, Context, Result};
use chia_wallet_sdk::coinset::CoinRecord;
use chia_wallet_sdk::prelude::{
    run_puzzle, Address, Allocator, Bytes32, Coin, CoinSpend, Condition, FromClvm, ToClvm,
};

use crate::coinset::Coinset;
use crate::output::AppError;
use crate::state::{from_hex, to_hex, CoinJson, HoldJson};
use crate::{MAINNET_PREFIX, MOJOS_PER_XCH};

/// Paid by each new holder; the pot grows by exactly this much on every pass.
pub const PRICE: u64 = MOJOS_PER_XCH;

/// How long a holder must keep the potato to win the pot.
pub const TIME_LOCK: u64 = 86_400;

/// The potato coin to follow when no anchor has been cached yet.
pub const DEFAULT_ANCHOR: &str =
    "0x1e54b89f681cad9858161cccce3691e9da085ade64275be21e73f81f7c810f0e";

/// Refuses to follow an absurdly long chain rather than hammering the RPC forever.
const MAX_HOPS: usize = 5_000;

/// The successor potato minted by a pass, plus the holder who bought it.
#[derive(Debug, Clone)]
pub struct Pass {
    /// Puzzle hash of the holder who took the potato.
    pub holder: Bytes32,
    /// The timestamp the buyer asserted, i.e. when they took it.
    pub acquired_at: u64,
    /// The successor potato coin.
    pub coin: Coin,
    /// The royalty coin paying out earlier holders, when the spend created one.
    pub royalty: Option<Coin>,
}

/// A spend that took the pot instead of passing the potato on.
#[derive(Debug, Clone)]
pub struct Claim {
    /// The deadline that had to pass before the pot could be taken.
    pub deadline: u64,
    /// The coins the claim paid out.
    pub payouts: Vec<Coin>,
    /// Block timestamp of the claim, when it could be determined.
    pub claimed_at: Option<u64>,
}

/// What a potato coin's spend did.
#[derive(Debug, Clone)]
pub enum PotatoSpend {
    /// Passed to a new holder before the deadline.
    Passed(Pass),
    /// Spent after the deadline without minting a successor: the pot was taken.
    Claimed(Claim),
}

/// One holder's tenure with the potato.
#[derive(Debug, Clone)]
pub struct Hold {
    /// The potato coin this holder owned.
    pub coin: Coin,
    /// The holder's puzzle hash.
    pub holder: Bytes32,
    /// When this holder took the potato (unix seconds).
    pub acquired_at: u64,
    /// When the next holder took it; `None` while this holder still has it.
    pub sold_at: Option<u64>,
}

impl Hold {
    /// Creates a hold that is still open (no successor observed yet).
    pub fn new(coin: Coin, holder: Bytes32, acquired_at: u64) -> Self {
        Self {
            coin,
            holder,
            acquired_at,
            sold_at: None,
        }
    }

    /// The instant this holder becomes able to take the pot.
    pub fn deadline(&self) -> u64 {
        self.acquired_at.saturating_add(TIME_LOCK)
    }

    /// How long the potato was held, or has been held so far.
    pub fn held_for(&self, now: u64) -> u64 {
        self.sold_at.unwrap_or(now).saturating_sub(self.acquired_at)
    }

    /// The holder's mainnet address.
    pub fn address(&self) -> Result<String> {
        Address::new(self.holder, MAINNET_PREFIX.to_string())
            .encode()
            .context("failed to encode holder address")
    }

    /// Converts to the cacheable form. `sold_at` is left out because it is recomputed from
    /// the neighbouring entries by [`relink`].
    pub fn to_json(&self) -> HoldJson {
        HoldJson {
            coin: CoinJson::from_coin(self.coin),
            holder: to_hex(self.holder),
            acquired_at: self.acquired_at,
        }
    }

    /// Restores a cached hold.
    pub fn from_json(json: &HoldJson) -> Result<Self> {
        Ok(Self::new(
            json.coin.to_coin()?,
            from_hex(&json.holder)?,
            json.acquired_at,
        ))
    }
}

/// The state of a round as of the last chain read.
#[derive(Debug, Clone)]
pub struct Game {
    /// Holder tenures, newest first. While the pot is live the first entry is the current
    /// holder.
    pub holds: Vec<Hold>,
    /// Set once the pot has been taken, which ends the round.
    pub claim: Option<Claim>,
}

impl Game {
    /// The holder of the potato at the head of the lineage.
    pub fn latest(&self) -> Option<&Hold> {
        self.holds.first()
    }

    /// The size of the pot in mojos.
    pub fn pot(&self) -> u64 {
        self.latest().map_or(0, |hold| hold.coin.amount)
    }
}

/// Interprets a potato coin's spend.
///
/// The spend is executed rather than matched against precomputed puzzle hashes, so this
/// works for all three spend paths: the resulting conditions themselves say whether a
/// successor potato was minted.
pub fn parse_spend(spend: &CoinSpend) -> Result<PotatoSpend> {
    let mut allocator = Allocator::new();
    let puzzle = spend.puzzle_reveal.to_clvm(&mut allocator)?;
    let solution = spend.solution.to_clvm(&mut allocator)?;
    let output = run_puzzle(&mut allocator, puzzle, solution)
        .context("failed to run the potato coin's puzzle")?;
    let conditions = Vec::<Condition>::from_clvm(&allocator, output)
        .context("potato spend did not produce a condition list")?;

    let spent_id = spend.coin.coin_id();
    let mut created: Vec<Coin> = Vec::new();
    let mut holder: Option<Bytes32> = None;
    let mut seconds: Option<u64> = None;

    for condition in conditions {
        match condition {
            Condition::CreateCoin(create) => {
                created.push(Coin::new(spent_id, create.puzzle_hash, create.amount));
            }
            // A pass announces the new holder's puzzle hash.
            Condition::CreateCoinAnnouncement(announcement) => {
                holder = <[u8; 32]>::try_from(announcement.message.as_ref())
                    .ok()
                    .map(Bytes32::new);
            }
            // A pass asserts the buyer's timestamp; a claim asserts the deadline.
            Condition::AssertSecondsAbsolute(assertion) => seconds = Some(assertion.seconds),
            _ => {}
        }
    }

    // Only a pass mints the successor potato, and it is always one PRICE richer.
    let successor_amount = spend.coin.amount.saturating_add(PRICE);
    let Some(index) = created.iter().position(|c| c.amount == successor_amount) else {
        return Ok(PotatoSpend::Claimed(Claim {
            deadline: seconds.unwrap_or_default(),
            payouts: created,
            claimed_at: None,
        }));
    };

    let coin = created.remove(index);
    let (Some(holder), Some(acquired_at)) = (holder, seconds) else {
        bail!(
            "potato spend {} minted a successor but announced no holder or timestamp",
            to_hex(spent_id)
        );
    };

    Ok(PotatoSpend::Passed(Pass {
        holder,
        acquired_at,
        coin,
        royalty: created.into_iter().next(),
    }))
}

/// Follows the potato from `anchor` to the head of its lineage, then walks backwards until
/// `history` previous holders have been resolved.
///
/// `cached` holds (newest first) are trusted as already-resolved history; pass an empty
/// vector to resolve everything from the chain. The anchor must be the coin of the newest
/// cached hold, or any potato coin in the same round when there is no cache.
pub async fn refresh(
    coinset: &Coinset,
    anchor: Bytes32,
    cached: Vec<Hold>,
    history: usize,
) -> Result<Game> {
    let mut record = require_record(coinset, anchor).await?;
    let mut discovered: Vec<Hold> = Vec::new();
    let mut claim: Option<Claim> = None;

    // Forward: every spent potato hands us its successor's holder for free.
    for hop in 0.. {
        if hop > MAX_HOPS {
            bail!(
                "potato lineage from {} is longer than {MAX_HOPS} hops; the anchor looks wrong",
                to_hex(anchor)
            );
        }
        if !record.spent {
            break;
        }
        let Some(spend) = coinset
            .coin_spend_at(record.coin.coin_id(), record.spent_block_index)
            .await?
        else {
            break;
        };
        match parse_spend(&spend)? {
            PotatoSpend::Claimed(taken) => {
                claim = Some(resolve_claim_time(coinset, taken).await);
                break;
            }
            PotatoSpend::Passed(pass) => {
                let next = pass.coin.coin_id();
                discovered.push(Hold::new(pass.coin, pass.holder, pass.acquired_at));
                record = require_record(coinset, next).await?;
            }
        }
    }

    discovered.reverse();
    let mut holds = discovered;
    holds.extend(cached);

    // The anchor's own hold is described by its parent's spend, so the forward walk never
    // produces it.
    if holds.is_empty() {
        if let Some(hold) = hold_for(coinset, &record).await? {
            holds.push(hold);
        }
    }

    // Backward: two lookups per generation, until we have enough or reach the round's start.
    while holds.len() <= history {
        let Some(oldest) = holds.last() else { break };
        let Some(parent) = coinset.coin_record(oldest.coin.parent_coin_info).await? else {
            break;
        };
        let Some(hold) = hold_for(coinset, &parent).await? else {
            break;
        };
        holds.push(hold);
    }

    relink(&mut holds);
    Ok(Game { holds, claim })
}

/// Resolves who owned `record`'s coin and when they took it, by reading the spend of its
/// parent. Returns `None` at the start of a round, where the parent is an ordinary coin
/// rather than a potato.
async fn hold_for(coinset: &Coinset, record: &CoinRecord) -> Result<Option<Hold>> {
    // A potato's parent is always spent in the very block that creates the potato, so the
    // child's confirmation height is the parent's spent height.
    let Some(spend) = coinset
        .coin_spend_at(record.coin.parent_coin_info, record.confirmed_block_index)
        .await?
    else {
        return Ok(None);
    };
    let PotatoSpend::Passed(pass) = parse_spend(&spend)? else {
        return Ok(None);
    };
    if pass.coin.coin_id() != record.coin.coin_id() {
        return Ok(None);
    }
    Ok(Some(Hold::new(record.coin, pass.holder, pass.acquired_at)))
}

/// Dates a claim by the block that confirmed one of its payout coins.
async fn resolve_claim_time(coinset: &Coinset, mut claim: Claim) -> Claim {
    let Some(payout) = claim.payouts.first() else {
        return claim;
    };
    // Best-effort: an unreachable RPC just leaves the claim undated.
    if let Ok(Some(record)) = coinset.coin_record(payout.coin_id()).await {
        claim.claimed_at = Some(record.timestamp);
    }
    claim
}

/// Each holder sold at the exact moment the next holder bought, so the newer entry's
/// acquisition time closes the older entry's tenure.
fn relink(holds: &mut [Hold]) {
    for index in (1..holds.len()).rev() {
        holds[index].sold_at = Some(holds[index - 1].acquired_at);
    }
    if let Some(newest) = holds.first_mut() {
        newest.sold_at = None;
    }
}

async fn require_record(coinset: &Coinset, coin_id: Bytes32) -> Result<CoinRecord> {
    coinset.coin_record(coin_id).await?.ok_or_else(|| {
        AppError::recoverable(format!("potato coin {} is not on-chain", to_hex(coin_id)))
            .why("the anchor coin id does not exist on mainnet")
            .next("pass a known potato coin id with `--coin <id>`")
            .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hold(amount: u64, acquired_at: u64) -> Hold {
        Hold::new(
            Coin::new(Bytes32::new([1; 32]), Bytes32::new([2; 32]), amount),
            Bytes32::new([3; 32]),
            acquired_at,
        )
    }

    #[test]
    fn relink_closes_each_tenure_with_the_next_purchase() {
        let mut holds = vec![hold(3, 300), hold(2, 200), hold(1, 100)];
        relink(&mut holds);
        assert_eq!(holds[0].sold_at, None);
        assert_eq!(holds[1].sold_at, Some(300));
        assert_eq!(holds[2].sold_at, Some(200));
    }

    #[test]
    fn held_for_measures_against_now_while_still_held() {
        let mut holds = vec![hold(2, 200), hold(1, 100)];
        relink(&mut holds);
        assert_eq!(holds[0].held_for(500), 300);
        assert_eq!(holds[1].held_for(500), 100);
    }

    #[test]
    fn deadline_is_one_time_lock_after_acquisition() {
        assert_eq!(hold(1, 1_000).deadline(), 1_000 + TIME_LOCK);
    }
}
