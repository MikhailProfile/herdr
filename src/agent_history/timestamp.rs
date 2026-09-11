use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch for `time`, clamped at zero for pre-epoch values.
pub fn system_time_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Current wall-clock time in milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    system_time_ms(SystemTime::now())
}

/// Parses an RFC 3339 timestamp such as `2026-02-06T05:59:33.827Z` or
/// `2026-02-06T05:59:33+02:00` into milliseconds since the Unix epoch.
///
/// The `time` crate is compiled without its parsing feature, so this is a small
/// hand-rolled parser covering exactly the shapes agents write.
pub fn parse_iso_ms(value: &str) -> Option<i64> {
    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() < 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    if !matches!(bytes[10], b'T' | b't' | b' ') {
        return None;
    }
    let year = digits(&bytes[0..4])?;
    let month = digits(&bytes[5..7])?;
    let day = digits(&bytes[8..10])?;
    let hour = digits(&bytes[11..13])?;
    let minute = digits(&bytes[14..16])?;
    let second = digits(&bytes[17..19])?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut rest = &value[19..];
    let mut millis: i64 = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let end = fraction
            .bytes()
            .position(|byte| !byte.is_ascii_digit())
            .unwrap_or(fraction.len());
        let digits_part = &fraction[..end];
        if digits_part.is_empty() {
            return None;
        }
        let mut scaled = 0i64;
        for (index, byte) in digits_part.bytes().take(3).enumerate() {
            scaled += i64::from(byte - b'0') * 10i64.pow(2 - index as u32);
        }
        millis = scaled;
        rest = &fraction[end..];
    }

    let offset_seconds: i64 = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let offset = &rest[1..];
            let (offset_hours, offset_minutes) = match offset.len() {
                5 if offset.as_bytes()[2] == b':' => (
                    digits(&offset.as_bytes()[0..2])?,
                    digits(&offset.as_bytes()[3..5])?,
                ),
                4 => (
                    digits(&offset.as_bytes()[0..2])?,
                    digits(&offset.as_bytes()[2..4])?,
                ),
                2 => (digits(offset.as_bytes())?, 0),
                _ => return None,
            };
            sign * (offset_hours * 3600 + offset_minutes * 60)
        }
    };

    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_seconds;
    Some(seconds * 1000 + millis)
}

/// Formats milliseconds since the Unix epoch as a UTC calendar date `YYYY-MM-DD`.
pub fn format_date_ms(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn digits(bytes: &[u8]) -> Option<i64> {
    let mut value = 0i64;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + i64::from(byte - b'0');
    }
    Some(value)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_style_utc_timestamps() {
        assert_eq!(
            parse_iso_ms("2026-02-06T05:59:33.827Z"),
            Some(1_770_357_573_827)
        );
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_ms("1970-01-02T00:00:00.5Z"), Some(86_400_500));
    }

    #[test]
    fn parses_offsets_and_missing_fraction() {
        assert_eq!(parse_iso_ms("1970-01-01T02:00:00+02:00"), Some(0));
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00-0130"), Some(5_400_000));
        assert_eq!(parse_iso_ms("1970-01-01 00:00:10"), Some(10_000));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_iso_ms(""), None);
        assert_eq!(parse_iso_ms("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso_ms("2026-02-06T05:59:33.Z"), None);
        assert_eq!(parse_iso_ms("2026-02-06T05:59:33X"), None);
        assert_eq!(parse_iso_ms("not a date"), None);
    }

    #[test]
    fn formats_dates_round_trip() {
        assert_eq!(format_date_ms(0), "1970-01-01");
        assert_eq!(format_date_ms(1_770_357_573_827), "2026-02-06");
        assert_eq!(format_date_ms(-1), "1969-12-31");
        for (year, month, day) in [(2000, 2, 29), (2024, 12, 31), (1999, 3, 1), (2100, 1, 1)] {
            let ms = days_from_civil(year, month, day) * 86_400_000;
            assert_eq!(format_date_ms(ms), format!("{year:04}-{month:02}-{day:02}"));
        }
    }

    #[test]
    fn system_time_conversion_is_monotonic_enough() {
        let now = now_ms();
        assert!(now > 1_600_000_000_000);
        assert_eq!(system_time_ms(UNIX_EPOCH), 0);
    }
}
