//! Wall-clock helpers.
//!
//! hearth stamps observations and names dated files, which needs epoch
//! milliseconds and a civil date — and nothing else. That isn't worth a date
//! crate, so the one non-trivial piece (turning a Unix day number into a
//! year/month/day) is Howard Hinnant's `civil_from_days`, which is exact across
//! the whole proleptic Gregorian range.

/// Wall clock in epoch milliseconds (UTC). `0` if the clock is before the epoch,
/// which only happens on a badly misconfigured machine.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Today's UTC date as `YYYY-MM-DD` — the stamp in a dated backup's filename.
/// Lexicographic order on these strings is chronological order, which is what
/// lets retention just sort filenames.
pub fn today_utc() -> String {
    ymd(now_ms().div_euclid(1_000).div_euclid(86_400))
}

/// Format a Unix day number as `YYYY-MM-DD`.
pub fn ymd(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert a Unix day number (days since 1970-01-01) to a civil
/// `(year, month, day)`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29)); // leap day
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn ymd_zero_pads_so_names_sort_chronologically() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(19_723), "2024-01-01");
        // Sorting the strings must order the dates.
        let mut names = [ymd(19_800), ymd(19_723), ymd(19_782)];
        names.sort();
        assert_eq!(names, [ymd(19_723), ymd(19_782), ymd(19_800)]);
    }

    #[test]
    fn now_is_plausibly_now() {
        // Sanity: after 2020 and before 2100, i.e. the clock is real.
        let now = now_ms();
        assert!(now > 1_577_836_800_000, "clock is before 2020");
        assert!(now < 4_102_444_800_000, "clock is after 2100");
    }
}
