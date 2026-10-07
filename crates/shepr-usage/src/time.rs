//! The wall-clock formats the endpoints use: RFC 3339 timestamps, epoch
//! seconds and HTTP dates. Parsing is strict; anything else is invalid, never
//! guessed.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Days from 1970-01-01 to the given proleptic Gregorian date, by Howard
/// Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        _ => 28,
    }
}

/// Seconds since the epoch as a wall time; `None` before the epoch.
pub(crate) fn from_epoch_seconds(seconds: i64) -> Option<SystemTime> {
    let seconds = u64::try_from(seconds).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

fn civil_to_epoch(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<i64> {
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(
        days * 86_400
            + i64::from(hour) * 3_600
            + i64::from(minute) * 60
            + i64::from(second.min(59)),
    )
}

fn digits(text: &str) -> Option<u32> {
    (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

/// An RFC 3339 timestamp with a `Z` or `+HH:MM`/`-HH:MM` offset and optional
/// fractional seconds, as the Claude usage endpoint sends.
pub(crate) fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let (date, rest) = text.split_once(['T', 't', ' '])?;
    let mut date_parts = date.split('-');
    let year: i64 = i64::from(digits(date_parts.next()?)?);
    let month = digits(date_parts.next()?)?;
    let day = digits(date_parts.next()?)?;
    if date_parts.next().is_some() {
        return None;
    }
    let (clock, offset_seconds) = if let Some(clock) = rest.strip_suffix(['Z', 'z']) {
        (clock, 0_i64)
    } else {
        let split = rest.rfind(['+', '-'])?;
        let (clock, offset) = rest.split_at(split);
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let (hours, minutes) = offset[1..].split_once(':')?;
        let (hours, minutes) = (digits(hours)?, digits(minutes)?);
        if hours > 23 || minutes > 59 {
            return None;
        }
        (
            clock,
            sign * (i64::from(hours) * 3_600 + i64::from(minutes) * 60),
        )
    };
    let (whole, fraction) = match clock.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (clock, None),
    };
    let mut clock_parts = whole.split(':');
    let hour = digits(clock_parts.next()?)?;
    let minute = digits(clock_parts.next()?)?;
    let second = digits(clock_parts.next()?)?;
    if clock_parts.next().is_some() {
        return None;
    }
    let nanos = match fraction {
        Some(fraction) => {
            if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let mut padded: String = fraction.chars().take(9).collect();
            while padded.len() < 9 {
                padded.push('0');
            }
            padded.parse::<u32>().ok()?
        }
        None => 0,
    };
    let epoch = civil_to_epoch(year, month, day, hour, minute, second)? - offset_seconds;
    from_epoch_seconds(epoch)?.checked_add(Duration::from_nanos(u64::from(nanos)))
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// An IMF-fixdate HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`). The obsolete
/// forms are not accepted; a Retry-After in them is ignored.
pub(crate) fn parse_http_date(text: &str) -> Option<SystemTime> {
    let (_, rest) = text.split_once(", ")?;
    let mut parts = rest.split(' ');
    let day = digits(parts.next()?)?;
    let month_name = parts.next()?;
    let month = u32::try_from(MONTHS.iter().position(|name| *name == month_name)? + 1).ok()?;
    let year = i64::from(digits(parts.next()?)?);
    let mut clock = parts.next()?.split(':');
    let hour = digits(clock.next()?)?;
    let minute = digits(clock.next()?)?;
    let second = digits(clock.next()?)?;
    if parts.next()? != "GMT" || parts.next().is_some() || clock.next().is_some() {
        return None;
    }
    from_epoch_seconds(civil_to_epoch(year, month, day, hour, minute, second)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn rfc3339_handles_offsets_fractions_and_zulu() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(epoch(0)));
        assert_eq!(
            parse_rfc3339("2026-10-07T15:00:00+02:00"),
            Some(epoch(1_791_378_000))
        );
        assert_eq!(
            parse_rfc3339("2026-10-07T13:00:00.5+00:00"),
            Some(epoch(1_791_378_000) + Duration::from_millis(500))
        );
    }

    #[test]
    fn rfc3339_refuses_what_it_cannot_place() {
        for text in [
            "",
            "2026-10-07",
            "2026-10-07T13:00:00",
            "2026-02-30T00:00:00Z",
            "2026-10-07T25:00:00Z",
            "2026-10-07T13:00:00.Z",
            "yesterday",
        ] {
            assert_eq!(parse_rfc3339(text), None, "{text:?}");
        }
    }

    #[test]
    fn http_dates_use_the_fixed_form_only() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(epoch(784_111_777))
        );
        assert_eq!(parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"), None);
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 PST"), None);
    }
}
