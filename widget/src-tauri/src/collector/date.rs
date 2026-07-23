//! ISO-8601 UTC date conversions, enough for the reset fields — avoids pulling
//! in the `chrono` crate just for this. Civil algorithms by Howard Hinnant.

pub fn epoch_to_iso(seconds: f64) -> String {
    let total = seconds as i64;
    let days = total.div_euclid(86_400);
    let secs = total.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    iso(year, month, day, secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// Same day next month (clamped to the last day), from an epoch in ms.
/// Port of the Python collector's `next_month` — used for Cursor's billing cycle.
pub fn next_month_iso(epoch_ms: f64) -> String {
    let total = (epoch_ms / 1000.0) as i64;
    let days = total.div_euclid(86_400);
    let secs = total.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let day = day.min(days_in_month(next_year, next_month));
    iso(next_year, next_month, day, secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// ISO-8601 → epoch seconds. Accepts the shapes the providers actually emit:
/// `YYYY-MM-DDTHH:MM:SS`, with optional fractional seconds and an optional
/// `Z` / `±HH:MM` / `±HHMM` offset (absent means UTC). Returns `None` on
/// anything it does not recognize, so callers can decide the fallback.
pub fn iso_to_epoch(text: &str) -> Option<i64> {
    let (date, rest) = text.split_once(['T', 't', ' '])?;
    let mut parts = date.splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;

    let (clock, offset) = match rest.find(['Z', 'z', '+', '-']) {
        Some(index) => rest.split_at(index),
        None => (rest, ""),
    };
    let clock = clock.split('.').next()?;
    let mut units = clock.splitn(3, ':');
    let hour: i64 = units.next()?.parse().ok()?;
    let minute: i64 = units.next()?.parse().ok()?;
    let second: i64 = units.next().unwrap_or("0").parse().ok()?;

    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    Some(seconds - offset_seconds(offset)?)
}

fn offset_seconds(offset: &str) -> Option<i64> {
    let sign = match offset.as_bytes().first() {
        None | Some(b'Z') | Some(b'z') => return Some(0),
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return None,
    };
    let digits: String = offset[1..].chars().filter(char::is_ascii_digit).collect();
    let (hours, minutes) = match digits.len() {
        2 => (digits.parse::<i64>().ok()?, 0),
        4 => (digits[..2].parse::<i64>().ok()?, digits[2..].parse::<i64>().ok()?),
        _ => return None,
    };
    Some(sign * (hours * 3600 + minutes * 60))
}

/// (year, month, day) → days since 1970-01-01. Inverse of `civil_from_days`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn iso(year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64) -> String {
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::iso_to_epoch;

    #[test]
    fn parses_the_shapes_the_providers_emit() {
        // Claude: fractional seconds and an explicit +00:00.
        assert_eq!(iso_to_epoch("2026-07-21T17:20:00.179248+00:00"), Some(1_784_654_400));
        // Cursor: milliseconds and a Z suffix.
        assert_eq!(iso_to_epoch("2026-08-13T15:15:26.000Z"), Some(1_786_634_126));
        // Bare local-looking timestamp is read as UTC.
        assert_eq!(iso_to_epoch("2026-07-21T17:20:00"), Some(1_784_654_400));
        // Non-zero offsets shift back to UTC.
        assert_eq!(iso_to_epoch("2026-07-21T14:20:00-03:00"), Some(1_784_654_400));
        assert_eq!(iso_to_epoch("2026-07-21T20:20:00+0300"), Some(1_784_654_400));
        // Epoch itself, and a leap day.
        assert_eq!(iso_to_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_to_epoch("2024-02-29T00:00:00Z"), Some(1_709_164_800));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(iso_to_epoch(""), None);
        assert_eq!(iso_to_epoch("2026-07-21"), None);
        assert_eq!(iso_to_epoch("not a date"), None);
        assert_eq!(iso_to_epoch("2026-07-21T17:20:00+bogus"), None);
    }
}

/// Days since 1970-01-01 → (year, month, day).
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}
