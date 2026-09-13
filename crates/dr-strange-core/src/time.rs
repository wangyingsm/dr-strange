//! Calendar arithmetic without a date-time dependency: RFC-3339 instants and
//! calendar days as unix-epoch milliseconds.

const MS_PER_DAY: i64 = 86_400_000;

/// Days from 1970-01-01 to a proleptic Gregorian date (Hinnant's days-from-civil).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The `YYYY-MM-DD` date at the head of `s`, validated, or `None`.
fn civil_date(s: &str) -> Option<(i64, i64, i64)> {
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    ((1..=12).contains(&month) && (1..=31).contains(&day)).then_some((year, month, day))
}

/// Unix-epoch milliseconds of an RFC-3339 instant
/// (`YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]`), or `None` if `s` is not one.
pub fn rfc3339_to_epoch_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || (b[10] != b'T' && b[10] != b't' && b[10] != b' ') {
        return None;
    }
    let (year, month, day) = civil_date(s)?;
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (hour, min, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if b[13] != b':' || b[16] != b':' || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut millis = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        let ms: String = digits.chars().chain("000".chars()).take(3).collect();
        millis = ms.parse::<i64>().ok()?;
        rest = &rest[1 + digits.len()..];
    }
    let offset_min = match rest.as_bytes().first() {
        Some(b'Z') | Some(b'z') if rest.len() == 1 => 0,
        Some(sign @ (b'+' | b'-')) if rest.len() == 6 && rest.as_bytes()[3] == b':' => {
            let h = rest.get(1..3)?.parse::<i64>().ok()?;
            let m = rest.get(4..6)?.parse::<i64>().ok()?;
            if h > 23 || m > 59 {
                return None;
            }
            if *sign == b'-' {
                -(h * 60 + m)
            } else {
                h * 60 + m
            }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    Some(((days * 86_400 + hour * 3600 + min * 60 + sec - offset_min * 60) * 1000) + millis)
}

/// The last millisecond of a `YYYY-MM-DD` day in UTC, or `None` if `s` is not one.
pub fn date_end_of_day(s: &str) -> Option<i64> {
    if s.len() != 10 {
        return None;
    }
    let (year, month, day) = civil_date(s)?;
    Some((days_from_civil(year, month, day) + 1) * MS_PER_DAY - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_rfc3339_instant_is_epoch_milliseconds() {
        assert_eq!(rfc3339_to_epoch_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            rfc3339_to_epoch_ms("2026-07-01T00:00:00Z"),
            Some(1_782_864_000_000)
        );
        assert_eq!(
            rfc3339_to_epoch_ms("1970-01-01T01:30:00.250+01:30"),
            Some(250)
        );
        assert_eq!(rfc3339_to_epoch_ms("1969-12-31T23:59:59Z"), Some(-1000));
    }

    #[test]
    fn a_malformed_instant_is_none() {
        for bad in [
            "2026-07-01",
            "2026-07-01T00:00:00",
            "2026-13-01T00:00:00Z",
            "2026-07-01X00:00:00Z",
            "2026-07-01T24:00:00Z",
            "2026-07-01T00:00:00.Z",
            "2026-07-01T00:00:00+0100",
        ] {
            assert_eq!(rfc3339_to_epoch_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_day_ends_at_its_last_millisecond_in_utc() {
        assert_eq!(date_end_of_day("1970-01-01"), Some(MS_PER_DAY - 1));
        assert_eq!(
            date_end_of_day("2026-07-01"),
            Some(1_782_864_000_000 + MS_PER_DAY - 1)
        );
        assert_eq!(
            date_end_of_day("2024-02-29"),
            rfc3339_to_epoch_ms("2024-02-29T23:59:59.999Z")
        );
    }

    #[test]
    fn a_malformed_day_is_none() {
        for bad in [
            "2026-7-01",
            "2026-07-1",
            "2026-00-10",
            "2026-07-01T00:00:00Z",
            "abcd-ef-gh",
        ] {
            assert_eq!(date_end_of_day(bad), None, "{bad}");
        }
    }
}
