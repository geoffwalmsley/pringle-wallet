//! Translation of internal phases and chain status into user-facing states.
//!
//! Users should not have to reason about `Phase::Superseded` or raw coin ids to understand
//! where an asset is in its lifecycle. This module maps the internal bookkeeping onto a
//! small vocabulary of intuitive states.

use crate::chain::ChainStatus;
use crate::state::Phase;

/// A user-facing lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserState {
    /// Confirmed, unspent, and controlled by this wallet — ready to use.
    Ready,
    /// Submitted but not yet confirmed on-chain.
    PendingConfirmation,
    /// The NFT is locked as the underlying of an option.
    LockedInOption,
    /// The option was exercised (melted) — the underlying was released.
    Exercised,
    /// The option's exercise deadline has passed.
    Expired,
    /// The singleton/coin is closed (spent/melted) and no longer live.
    Closed,
    /// The asset moved to a different owner (no longer this wallet).
    Transferred,
    /// No funds / nothing to show.
    Empty,
    /// The on-chain status could not be determined (RPC failure).
    Unknown,
}

impl UserState {
    /// A short, human-friendly label.
    pub fn label(self) -> &'static str {
        match self {
            UserState::Ready => "Ready",
            UserState::PendingConfirmation => "Pending confirmation",
            UserState::LockedInOption => "Locked in option",
            UserState::Exercised => "Exercised",
            UserState::Expired => "Expired",
            UserState::Closed => "Closed",
            UserState::Transferred => "Transferred",
            UserState::Empty => "Empty",
            UserState::Unknown => "Unknown (lookup failed)",
        }
    }

    /// A stable machine string for JSON output.
    pub fn machine(self) -> &'static str {
        match self {
            UserState::Ready => "ready",
            UserState::PendingConfirmation => "pending_confirmation",
            UserState::LockedInOption => "locked_in_option",
            UserState::Exercised => "exercised",
            UserState::Expired => "expired",
            UserState::Closed => "closed",
            UserState::Transferred => "transferred",
            UserState::Empty => "empty",
            UserState::Unknown => "unknown",
        }
    }
}

/// Derives the user state of an NFT from its phase, chain status, and control.
///
/// `controlled` means the NFT's current p2 puzzle hash is this wallet's.
pub fn nft_state(phase: Phase, chain: &ChainStatus, controlled: bool) -> UserState {
    match chain {
        ChainStatus::LookupFailed { .. } => UserState::Unknown,
        ChainStatus::ConfirmedUnspent { .. } => {
            if controlled {
                UserState::Ready
            } else if phase == Phase::Superseded {
                UserState::LockedInOption
            } else {
                UserState::Transferred
            }
        }
        // Not the live coin: pending if we only just submitted, otherwise the tip moved on.
        ChainStatus::NotFound => {
            if phase == Phase::Pending {
                UserState::PendingConfirmation
            } else {
                UserState::Closed
            }
        }
        ChainStatus::Spent { .. } => UserState::Closed,
    }
}

/// Derives the user state of an option from its phase, chain status, expiration, and owner.
pub fn option_state(
    phase: Phase,
    chain: &ChainStatus,
    controlled: bool,
    expired: bool,
) -> UserState {
    match chain {
        ChainStatus::LookupFailed { .. } => UserState::Unknown,
        ChainStatus::ConfirmedUnspent { .. } => {
            if expired {
                UserState::Expired
            } else if controlled {
                UserState::Ready
            } else {
                UserState::Transferred
            }
        }
        ChainStatus::NotFound => {
            if phase == Phase::Pending {
                UserState::PendingConfirmation
            } else if expired {
                UserState::Expired
            } else {
                UserState::Exercised
            }
        }
        ChainStatus::Spent { .. } => {
            if phase == Phase::Superseded {
                UserState::Exercised
            } else {
                UserState::Closed
            }
        }
    }
}

/// Derives the user state of a p2_singleton from its live balance and phase.
pub fn p2_state(phase: Phase, live_coins: usize, lookup_failed: bool) -> UserState {
    if lookup_failed {
        return UserState::Unknown;
    }
    if live_coins > 0 {
        return UserState::Ready;
    }
    match phase {
        Phase::Pending => UserState::PendingConfirmation,
        _ => UserState::Empty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nft_ready_when_confirmed_and_controlled() {
        let s = nft_state(
            Phase::Confirmed,
            &ChainStatus::ConfirmedUnspent {
                confirmed_height: 1,
            },
            true,
        );
        assert_eq!(s, UserState::Ready);
    }

    #[test]
    fn nft_locked_when_superseded_but_live() {
        let s = nft_state(
            Phase::Superseded,
            &ChainStatus::ConfirmedUnspent {
                confirmed_height: 1,
            },
            false,
        );
        assert_eq!(s, UserState::LockedInOption);
    }

    #[test]
    fn nft_unknown_on_lookup_failure() {
        let s = nft_state(
            Phase::Confirmed,
            &ChainStatus::LookupFailed { error: "x".into() },
            true,
        );
        assert_eq!(s, UserState::Unknown);
    }

    #[test]
    fn option_pending_when_not_found_and_pending() {
        let s = option_state(Phase::Pending, &ChainStatus::NotFound, true, false);
        assert_eq!(s, UserState::PendingConfirmation);
    }

    #[test]
    fn confirmed_expired_option_is_not_ready() {
        let s = option_state(
            Phase::Confirmed,
            &ChainStatus::ConfirmedUnspent {
                confirmed_height: 10,
            },
            true,
            true,
        );
        assert_eq!(s, UserState::Expired);
        assert_eq!(s.machine(), "expired");
    }

    #[test]
    fn p2_ready_with_live_coins() {
        assert_eq!(p2_state(Phase::Confirmed, 2, false), UserState::Ready);
        assert_eq!(p2_state(Phase::Superseded, 0, false), UserState::Empty);
        assert_eq!(p2_state(Phase::Confirmed, 0, true), UserState::Unknown);
    }
}
