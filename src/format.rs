//! Human-friendly formatting helpers.
//!
//! These are presentation-only: they render amounts, dates, and ids in a compact,
//! consistent way for human output. Full, unabbreviated values are always available
//! through the JSON/verbose paths, so nothing here is ever the source of truth.

use chia_wallet_sdk::prelude::Bytes32;

use crate::MOJOS_PER_XCH;

/// Renders a mojo amount as `X.XXXXXXXXXXXX XCH (<n> mojos)`.
///
/// The XCH portion is trimmed of trailing zeros (but always keeps at least one decimal),
/// and the raw mojo count is grouped with underscores for readability.
pub fn xch(mojos: u64) -> String {
    format!("{} XCH ({} mojos)", xch_only(mojos), grouped(mojos))
}

/// Renders just the decimal-XCH portion of an amount (no mojo suffix).
pub fn xch_only(mojos: u64) -> String {
    let whole = mojos / MOJOS_PER_XCH;
    let frac = mojos % MOJOS_PER_XCH;
    if frac == 0 {
        return format!("{whole}");
    }
    // 12 fractional digits, trimmed of trailing zeros.
    let mut frac_str = format!("{frac:012}");
    while frac_str.ends_with('0') {
        frac_str.pop();
    }
    format!("{whole}.{frac_str}")
}

/// Groups a number with underscores every three digits (e.g. `1_000_000`).
pub fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let len = digits.len();
    for (i, ch) in digits.chars().enumerate() {
        // Insert a separator before a digit that starts a new group of three
        // (counting from the right), except at the very start of the string.
        if i != 0 && (len - i).is_multiple_of(3) {
            out.push('_');
        }
        out.push(ch);
    }
    out
}

/// Abbreviates a hex id string to `0xabcd…wxyz`.
pub fn abbrev(id: &str) -> String {
    let s = id.strip_prefix("0x").unwrap_or(id);
    if s.len() <= 12 {
        return format!("0x{s}");
    }
    format!("0x{}…{}", &s[..6], &s[s.len() - 4..])
}

/// Abbreviates a [`Bytes32`] the same way as [`abbrev`].
pub fn abbrev_bytes(id: Bytes32) -> String {
    abbrev(&hex::encode(id.to_bytes()))
}

/// Renders an absolute unix-seconds expiration as a UTC date plus a relative hint.
///
/// e.g. `2033-05-18 03:33:20 UTC (in ~2y)` or `... (expired 3d ago)`.
pub fn expiration(seconds: u64, now: u64) -> String {
    let date = utc_datetime(seconds);
    let relative = relative_time(seconds, now);
    format!("{date} ({relative})")
}

/// Formats unix seconds as `YYYY-MM-DD HH:MM:SS UTC`.
pub fn utc_datetime(seconds: u64) -> String {
    use chrono::{DateTime, Utc};
    match DateTime::<Utc>::from_timestamp(seconds as i64, 0) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => format!("unix {seconds}"),
    }
}

/// A coarse relative-time description ("in ~3d", "expired 2h ago", "now").
pub fn relative_time(target: u64, now: u64) -> String {
    if target == now {
        return "now".to_string();
    }
    let (delta, future) = if target > now {
        (target - now, true)
    } else {
        (now - target, false)
    };
    let human = humanize_duration(delta);
    if future {
        format!("in ~{human}")
    } else {
        format!("expired {human} ago")
    }
}

/// Renders a duration with up to two units, e.g. `2d 3h`, `4h 32m`, `51m 12s`, `9s`.
///
/// Unlike [`relative_time`]'s coarse single unit, this keeps enough resolution to compare
/// durations that differ by minutes.
pub fn duration(seconds: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    let (major, major_unit, minor, minor_unit) = if seconds >= DAY {
        (seconds / DAY, 'd', (seconds % DAY) / HOUR, 'h')
    } else if seconds >= HOUR {
        (seconds / HOUR, 'h', (seconds % HOUR) / MINUTE, 'm')
    } else if seconds >= MINUTE {
        (seconds / MINUTE, 'm', seconds % MINUTE, 's')
    } else {
        return format!("{seconds}s");
    };

    if minor == 0 {
        format!("{major}{major_unit}")
    } else {
        format!("{major}{major_unit} {minor}{minor_unit}")
    }
}

fn humanize_duration(seconds: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const YEAR: u64 = 365 * DAY;

    if seconds >= YEAR {
        format!("{}y", seconds / YEAR)
    } else if seconds >= DAY {
        format!("{}d", seconds / DAY)
    } else if seconds >= HOUR {
        format!("{}h", seconds / HOUR)
    } else if seconds >= MINUTE {
        format!("{}m", seconds / MINUTE)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xch_only_trims_trailing_zeros() {
        assert_eq!(xch_only(0), "0");
        assert_eq!(xch_only(MOJOS_PER_XCH), "1");
        assert_eq!(xch_only(MOJOS_PER_XCH / 4), "0.25");
        assert_eq!(xch_only(MOJOS_PER_XCH + 1), "1.000000000001");
    }

    #[test]
    fn xch_includes_mojos() {
        assert_eq!(xch(250_000_000_000), "0.25 XCH (250_000_000_000 mojos)");
        assert_eq!(xch(1), "0.000000000001 XCH (1 mojos)");
    }

    #[test]
    fn grouped_inserts_separators() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(100), "100");
        assert_eq!(grouped(1000), "1_000");
        assert_eq!(grouped(1_000_000), "1_000_000");
        assert_eq!(grouped(1234567), "1_234_567");
    }

    #[test]
    fn abbrev_shortens_long_ids() {
        let id = "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(abbrev(id), "0x012345…cdef");
        assert_eq!(abbrev("0xabcd"), "0xabcd");
    }

    #[test]
    fn duration_uses_at_most_two_units() {
        assert_eq!(duration(9), "9s");
        assert_eq!(duration(60), "1m");
        assert_eq!(duration(3072), "51m 12s");
        assert_eq!(duration(16_320), "4h 32m");
        assert_eq!(duration(7_200), "2h");
        assert_eq!(duration(97_200), "1d 3h");
        assert_eq!(duration(86_400), "1d");
    }

    #[test]
    fn expiration_reports_relative() {
        let s = expiration(1_000, 100);
        assert!(s.contains("UTC"));
        assert!(s.contains("in ~"));
        let past = expiration(100, 1_000);
        assert!(past.contains("ago"));
    }
}
