//! Time source and `xs:dateTime` handling.
//!
//! All arithmetic is on `i64` seconds since the Unix epoch and proleptic Gregorian civil dates
//! (Howard Hinnant's `days_from_civil` / `civil_from_days`).

use std::fmt::Debug;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

/// Source of the current time, injectable so replay and skew logic is testable.
pub trait Clock: Send + Sync + Debug {
    /// Seconds since the Unix epoch.
    fn now_unix(&self) -> i64;
}

/// The system wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }
}

/// A settable clock for tests and benchmarks.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    /// A clock reading `unix` seconds.
    #[must_use]
    pub fn new(unix: i64) -> Self {
        Self(AtomicI64::new(unix))
    }

    /// Set the reading.
    pub fn set(&self, unix: i64) {
        self.0.store(unix, Ordering::SeqCst);
    }

    /// Advance the reading by `seconds`.
    pub fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_unix(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A broken-down UTC date and time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    /// Full year.
    pub year: i64,
    /// Month, 1 to 12.
    pub month: i64,
    /// Day of month, 1 to 31.
    pub day: i64,
    /// Hour, 0 to 23.
    pub hour: i64,
    /// Minute, 0 to 59.
    pub minute: i64,
    /// Second, 0 to 59.
    pub second: i64,
}

/// Why an `xs:dateTime` was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("invalid xs:dateTime")]
pub struct TimeError;

/// Days since 1970-01-01 for a civil date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date for days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Break `unix` seconds into UTC civil fields.
#[must_use]
pub fn civil_from_unix(unix: i64) -> Civil {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Civil {
        year,
        month,
        day,
        hour: secs / 3600,
        minute: (secs % 3600) / 60,
        second: secs % 60,
    }
}

/// Exactly `n` ASCII digits, as a number.
fn digits(s: &str, n: usize) -> Result<i64, TimeError> {
    if s.len() != n || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TimeError);
    }
    s.parse::<i64>().map_err(|_| TimeError)
}

/// Parse `xs:dateTime` (`YYYY-MM-DDThh:mm:ss[.f+](Z|+hh:mm|-hh:mm)`) to Unix seconds.
///
/// A missing zone is rejected: `Created` must say what zone it is in. Fractional seconds are
/// accepted (1 to 9 digits) and discarded. Total: never panics on any input.
///
/// # Errors
///
/// Returns [`TimeError`] for any string that is not a valid `xs:dateTime` in years 0001 to 9999.
pub fn parse_datetime(s: &str) -> Result<i64, TimeError> {
    let (date, rest) = s.split_once('T').ok_or(TimeError)?;
    let mut dp = date.split('-');
    let year = digits(dp.next().ok_or(TimeError)?, 4)?;
    let month = digits(dp.next().ok_or(TimeError)?, 2)?;
    let day = digits(dp.next().ok_or(TimeError)?, 2)?;
    if dp.next().is_some() || !(1..=12).contains(&month) || year < 1 {
        return Err(TimeError);
    }
    if day < 1 || day > days_in_month(year, month) {
        return Err(TimeError);
    }

    let (clock, offset) = if let Some(clock) = rest.strip_suffix('Z') {
        (clock, 0)
    } else {
        let idx = rest.rfind(['+', '-']).ok_or(TimeError)?;
        let (clock, zone) = rest.split_at_checked(idx).ok_or(TimeError)?;
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let zone = zone.get(1..).ok_or(TimeError)?;
        let (zh, zm) = zone.split_once(':').ok_or(TimeError)?;
        let (zh, zm) = (digits(zh, 2)?, digits(zm, 2)?);
        if zh > 23 || zm > 59 {
            return Err(TimeError);
        }
        (clock, sign * (zh * 3600 + zm * 60))
    };

    let mut cp = clock.split(':');
    let hour = digits(cp.next().ok_or(TimeError)?, 2)?;
    let minute = digits(cp.next().ok_or(TimeError)?, 2)?;
    let sec_part = cp.next().ok_or(TimeError)?;
    if cp.next().is_some() {
        return Err(TimeError);
    }
    let second = match sec_part.split_once('.') {
        Some((whole, frac)) => {
            if frac.is_empty() || frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return Err(TimeError);
            }
            digits(whole, 2)?
        }
        None => digits(sec_part, 2)?,
    };
    if hour > 23 || minute > 59 || second > 59 {
        return Err(TimeError);
    }

    let days = days_from_civil(year, month, day);
    Ok(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// Format Unix seconds as `xs:dateTime` in UTC, e.g. `2025-01-02T03:04:05Z`.
#[must_use]
pub fn format_datetime(unix: i64) -> String {
    let c = civil_from_unix(unix);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        c.year, c.month, c.day, c.hour, c.minute, c.second
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn epoch_and_known_instants() {
        assert_eq!(parse_datetime("1970-01-01T00:00:00Z"), Ok(0));
        assert_eq!(parse_datetime("2024-02-29T12:00:00Z"), Ok(1_709_208_000));
        assert_eq!(format_datetime(1_709_208_000), "2024-02-29T12:00:00Z");
    }

    #[test]
    fn offsets_and_fractions() {
        let utc = parse_datetime("2025-06-01T10:00:00Z").unwrap();
        assert_eq!(parse_datetime("2025-06-01T12:00:00+02:00"), Ok(utc));
        assert_eq!(parse_datetime("2025-06-01T05:30:00-04:30"), Ok(utc));
        assert_eq!(parse_datetime("2025-06-01T10:00:00.123456Z"), Ok(utc));
    }

    #[test]
    fn rejects_invalid() {
        for bad in [
            "",
            "2025-06-01",
            "2025-06-01T10:00:00",
            "2025-13-01T10:00:00Z",
            "2025-02-29T10:00:00Z",
            "2025-06-01T24:00:00Z",
            "2025-06-01T10:60:00Z",
            "2025-06-01T10:00:60Z",
            "2025-06-01T10:00:00.Z",
            "2025-6-1T10:00:00Z",
            "2025-06-01T10:00:00+25:00",
            "+2025-06-01T10:00:00Z",
            "0000-01-01T00:00:00Z",
        ] {
            assert_eq!(parse_datetime(bad), Err(TimeError), "{bad}");
        }
    }

    proptest! {
        #[test]
        fn roundtrip(t in 0_i64..253_402_300_799) {
            prop_assert_eq!(parse_datetime(&format_datetime(t)), Ok(t));
        }

        #[test]
        fn parse_never_panics(s in ".*") {
            let _ = parse_datetime(&s);
        }
    }
}
