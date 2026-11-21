//! Calendar arithmetic for the cold paths: the manifest, the date table, an `ingest_ts`
//! outside the table. Nothing here runs per row on generated data.

pub const SECOND: i64 = 1_000;
pub const DAY: i64 = 86_400_000;
pub const WEEK: i64 = 7 * DAY;

/// Days since 1970-01-01 of a proleptic Gregorian date (Hinnant's `days_from_civil`).
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year - 719_468
}

/// `(year, month, day)` of a day number (Hinnant's `civil_from_days`).
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day_of_year - (153 * month_index + 2) / 5 + 1)
}

/// Form F1, `2026-10-03T23:58:10Z`, strictly: epoch milliseconds, or `None`.
pub fn parse_f1(text: &[u8]) -> Option<i64> {
    if text.len() != 20 {
        return None;
    }
    let two = |at: usize| {
        let (tens, units) = (text[at].wrapping_sub(b'0'), text[at + 1].wrapping_sub(b'0'));
        (tens < 10 && units < 10).then(|| i64::from(tens * 10 + units))
    };
    let punctuation = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':'), (19, b'Z')];
    if punctuation.iter().any(|&(at, byte)| text[at] != byte) {
        return None;
    }
    let (year, month, day) = (two(0)? * 100 + two(2)?, two(5)?, two(8)?);
    let (hour, minute, second) = (two(11)?, two(14)?, two(17)?);
    // A date is real exactly when it survives the round trip.
    let days = days_from_civil(year, month, day);
    if !(1..=12).contains(&month) || civil_from_days(days) != (year, month, day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(days * DAY + (hour * 3_600 + minute * 60 + second) * SECOND)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f1_parses_and_rejects() {
        assert_eq!(parse_f1(b"1970-01-02T00:00:00Z"), Some(DAY));
        assert_eq!(parse_f1(b"2026-10-03T23:58:10Z"), Some(1_791_071_890_000));
        assert_eq!(parse_f1(b"2028-02-29T00:00:00Z"), Some(days_from_civil(2028, 2, 29) * DAY));
        for bad in [
            "2027-02-29T00:00:00Z", "2026-02-30T00:00:00Z", "2026-13-01T00:00:00Z",
            "2026-00-10T00:00:00Z", "2026-10-00T00:00:00Z", "2026-10-03T24:00:00Z",
            "2026-10-03T23:60:00Z", "2026-10-03T23:59:60Z", "2026-10-03t23:58:10Z",
            "2026-10-03T23:58:10z", "2026-10-03 23:58:10Z", "2026-10-03T23:58:10", "",
            "2026-10-03T23:58:10+00:00", "2026-1x-03T23:58:10Z",
        ] {
            assert_eq!(parse_f1(bad.as_bytes()), None, "{bad}");
        }
        for days in [-1, 0, 59, 20_000, 21_244, 21_245] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days);
        }
    }
}
