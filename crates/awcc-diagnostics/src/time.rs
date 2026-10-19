//! Dependency-free UTC timestamp formatting (RFC 3339 / compact), used for event timestamps and
//! file names. Civil-date conversion follows Howard Hinnant's `civil_from_days`.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch (`0` if the clock is before 1970).
#[must_use]
pub fn unix_millis(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Current wall-clock time as Unix milliseconds.
#[must_use]
pub fn now_unix_millis() -> u64 {
    unix_millis(SystemTime::now())
}

/// Broken-down UTC time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Utc {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub millis: u32,
}

/// Convert Unix milliseconds to broken-down UTC.
#[must_use]
pub fn utc_from_millis(ms: u64) -> Utc {
    let secs = ms / 1000;
    let millis = u32::try_from(ms % 1000).unwrap_or(0);
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = u32::try_from(secs % 86_400).unwrap_or(0);

    // civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = if m <= 2 { y + 1 } else { y };

    Utc {
        year,
        month: m,
        day: d,
        hour: rem / 3600,
        minute: (rem % 3600) / 60,
        second: rem % 60,
        millis,
    }
}

/// `2026-10-19T15:56:14.123Z`
#[must_use]
pub fn rfc3339_millis(ms: u64) -> String {
    let t = utc_from_millis(ms);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year, t.month, t.day, t.hour, t.minute, t.second, t.millis
    )
}

/// `20261019T155614Z` (sortable, safe in file names).
#[must_use]
pub fn compact_utc(ms: u64) -> String {
    let t = utc_from_millis(ms);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

/// MS-DOS date/time words as used by ZIP headers (2-second resolution, 1980 epoch).
#[must_use]
pub fn dos_datetime(ms: u64) -> (u16, u16) {
    let t = utc_from_millis(ms);
    let year = u32::try_from((t.year - 1980).clamp(0, 127)).unwrap_or(0);
    let date = (year << 9) | (t.month << 5) | t.day;
    let time = (t.hour << 11) | (t.minute << 5) | (t.second / 2);
    (
        u16::try_from(date).unwrap_or(0x21),
        u16::try_from(time).unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instants_format_correctly() {
        assert_eq!(rfc3339_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            rfc3339_millis(1_000_000_000_000),
            "2001-09-09T01:46:40.000Z"
        );
        // 2000-02-29 exists (divisible by 400).
        assert_eq!(rfc3339_millis(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(
            rfc3339_millis(4_102_444_800_000),
            "2100-01-01T00:00:00.000Z"
        );
        assert_eq!(
            rfc3339_millis(1_760_000_000_123),
            "2025-10-09T08:53:20.123Z"
        );
        assert_eq!(compact_utc(1_000_000_000_000), "20010909T014640Z");
    }

    #[test]
    fn dos_time_is_packed_with_two_second_resolution() {
        let (date, time) = dos_datetime(1_000_000_000_000);
        assert_eq!(date >> 9, 2001 - 1980);
        assert_eq!((date >> 5) & 0xF, 9);
        assert_eq!(date & 0x1F, 9);
        assert_eq!(time >> 11, 1);
        assert_eq!((time >> 5) & 0x3F, 46);
        assert_eq!(time & 0x1F, 20);
    }
}
