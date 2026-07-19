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
