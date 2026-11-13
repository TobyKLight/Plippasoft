//! The one date format this store family uses.
//!
//! `created` is RFC 1123 (`20 Aug 2026 06:48:08 GMT`), not ISO 8601, so it must be
//! parsed rather than compared as a string. There is no chrono dependency here and
//! none is wanted for one field: the civil-date algorithm is a dozen lines.

/// Parse an RFC 1123 timestamp to Unix seconds.
///
/// Accepts the measured shape `20 Aug 2026 06:48:08 GMT`, the same with a weekday
/// prefix (`Thu, 20 Aug 2026 06:48:08 GMT`), and `UTC`/`Z`/`+0000` in place of `GMT`.
/// Anything else — ISO 8601, a bare year, a non-UTC offset, an impossible day — is
/// [`None`]. A date is decoration, so an unparsable one must not fail a listing.
pub fn parse_rfc1123_unix(value: &str) -> Option<i64> {
    let mut parts = value.split_whitespace();

    let mut day_token = parts.next()?;
    if day_token.ends_with(',') {
        // "Thu, 20 Aug ..." — the weekday is redundant.
        day_token = parts.next()?;
    }
    let day: u32 = parse_small_uint(day_token)?;
    let month = month_index(parts.next()?)?;
    let year: i64 = parts.next()?.parse().ok()?;
    let clock = parts.next()?;
    let zone = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if !matches!(zone, "GMT" | "UTC" | "Z" | "+0000" | "-0000")
        && !zone.eq_ignore_ascii_case("gmt")
        && !zone.eq_ignore_ascii_case("utc")
    {
        return None;
    }
    let (hour, minute, second) = parse_clock(clock)?;

    if !(1_000..=9_999).contains(&year) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // A leap second is reported as 60; fold it onto 59 rather than reject the row.
    let second = second.min(59);

    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second))
}

fn parse_small_uint(token: &str) -> Option<u32> {
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    token.parse().ok()
}

fn month_index(token: &str) -> Option<u32> {
    let token = token.to_ascii_lowercase();
    let index = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ]
    .iter()
    .position(|month| *month == token)?;
    Some(index as u32 + 1)
}

fn parse_clock(token: &str) -> Option<(u32, u32, u32)> {
    let mut fields = token.split(':');
    let hour = parse_small_uint(fields.next()?)?;
    let minute = parse_small_uint(fields.next()?)?;
    let second = parse_small_uint(fields.next()?)?;
    if fields.next().is_some() || token.len() != 8 {
        return None;
    }
    Some((hour, minute, second))
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day = i64::from(day);
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era =
        year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_measured_date_parses_to_unix_seconds() {
        // 2026-08-20T06:48:08Z
        assert_eq!(
            parse_rfc1123_unix("20 Aug 2026 06:48:08 GMT"),
            Some(1_787_208_488)
        );
        assert_eq!(
            parse_rfc1123_unix("Thu, 20 Aug 2026 06:48:08 GMT"),
            Some(1_787_208_488)
        );
        assert_eq!(
            parse_rfc1123_unix("01 Jan 1970 00:00:00 GMT"),
            Some(0)
        );
    }

    #[test]
    fn iso_8601_and_junk_are_not_dates() {
        for value in [
            "2026-08-20T06:48:08Z",
            "2026-08-20 06:48:08",
            "20 Aug 2026",
            "20 Aug 2026 06:48 GMT",
            "20 Xxx 2026 06:48:08 GMT",
            "32 Aug 2026 06:48:08 GMT",
            "29 Feb 2026 06:48:08 GMT",
            "20 Aug 2026 24:00:00 GMT",
            "20 Aug 2026 06:48:08 EST",
            "",
            "not a date",
        ] {
            assert_eq!(parse_rfc1123_unix(value), None, "accepted {value:?}");
        }
    }

    #[test]
    fn leap_days_exist_in_leap_years() {
        assert!(parse_rfc1123_unix("29 Feb 2024 00:00:00 GMT").is_some());
        assert!(parse_rfc1123_unix("29 Feb 2000 00:00:00 GMT").is_some());
        assert!(parse_rfc1123_unix("29 Feb 1900 00:00:00 GMT").is_none());
    }
}
