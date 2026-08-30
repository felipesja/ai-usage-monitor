//! Cursor Business limits. Two methods in `cursor.json`: `admin_key` (team
//! admin API, preferred) or `dashboard_cookie` (internal dashboard endpoint,
//! may break if Cursor changes it). Port of `collect_cursor`.

use base64::Engine;
use serde_json::{json, Value};

use super::config::{cursor_config, read_json};
use super::date::{epoch_to_iso, iso_to_epoch, next_month_iso};
use super::{Meter, Provider};

const EVENTS_URL: &str = "https://cursor.com/api/dashboard/get-filtered-usage-events";
/// Cursor caps pageSize at 1000 (400 above that); 5 pages covers any real cycle.
const EVENTS_PAGE_SIZE: i64 = 1000;
const EVENTS_MAX_PAGES: i64 = 5;
const WEEK_SECONDS: i64 = 7 * 24 * 3600;

pub fn collect() -> Provider {
    let path = cursor_config();
    if !path.exists() {
        return Provider::with_error(
            "Cursor",
            "Business",
            "set it up with: ai-usage cursor-cookie or cursor-admin".into(),
        );
    }
    match read_json(&path).and_then(|config| run(&config)) {
        Ok(provider) => provider,
        Err(err) => Provider::with_error("Cursor", "Business", err),
    }
}

fn run(config: &Value) -> Result<Provider, String> {
    match config.get("method").and_then(Value::as_str) {
        Some("dashboard_cookie") => by_cookie(config),
        _ => by_admin_key(config),
    }
}

fn by_cookie(config: &Value) -> Result<Provider, String> {
    let token = config
        .get("session_cookie")
        .and_then(Value::as_str)
        .ok_or("cursor.json has no session_cookie")?;
    let cookie = format!("WorkosCursorSessionToken={token}");
    let client = super::http_client()?;

    let data: Value = client
        .get("https://cursor.com/api/usage-summary")
        .header("Cookie", &cookie)
        .header("User-Agent", "ai-usage-monitor")
        .send()
        .map_err(|err| err.to_string())?
        .error_for_status()
        .map_err(|err| err.to_string())?
        .json()
        .map_err(|err| err.to_string())?;

    let email = client
        .get("https://cursor.com/api/auth/me")
        .header("Cookie", &cookie)
        .header("User-Agent", "ai-usage-monitor")
        .send()
        .ok()
        .and_then(|response| response.json::<Value>().ok())
        .and_then(|me| me.get("email").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();

    let membership = title_case(data.get("membershipType").and_then(Value::as_str).unwrap_or("Team"));
    let mut provider = Provider::new("Cursor", "Business", &membership, &email);

    let plan = data
        .get("individualUsage")
        .and_then(|usage| usage.get("plan"))
        .cloned()
        .unwrap_or(Value::Null);
    let (used, total, included) = plan_usage(&plan);
    let percent = if total > 0.0 { Some(used * 100.0 / total) } else { None };

    let billing_end = data.get("billingCycleEnd").and_then(Value::as_str).map(str::to_string);
    let usage_meter = Meter::new("Usage", percent, billing_end.clone());
    provider.meters.push(usage_meter);

    // Pace gauge over the cycle-aligned week. Best effort: the events endpoint
    // is undocumented, so a failure just drops the meter and leaves the monthly
    // bar alone.
    if let (Some(cycle_start), Some(cycle_end)) = (
        data.get("billingCycleStart").and_then(Value::as_str).and_then(iso_to_epoch),
        billing_end.as_deref().and_then(iso_to_epoch),
    ) {
        if let Ok(events) = fetch_events(&client, &cookie, cycle_start, cycle_end) {
            if let Some(week) = weekly_meter(cycle_start, cycle_end, now_epoch(), total, &events) {
                provider.meters.push(week);
            }
        }
    }

    if used - included > 0.0 {
        provider.details.push(format!("Extra usage: {}", display_money(used - included)));
    }

    if let Some(auto) = plan.get("autoPercentUsed").and_then(Value::as_f64) {
        if auto != 0.0 {
            provider.meters.push(Meter::new("Auto usage", Some(auto), billing_end));
        }
    }
    let demand = data
        .get("individualUsage")
        .and_then(|usage| usage.get("onDemand"))
        .cloned()
        .unwrap_or(Value::Null);
    if demand.get("enabled").and_then(Value::as_bool).unwrap_or(false) {
        let used = demand.get("used").and_then(Value::as_f64).unwrap_or(0.0) / 100.0;
        provider.details.push(format!("On demand: ${used:.2}"));
    }
    Ok(provider)
}

fn by_admin_key(config: &Value) -> Result<Provider, String> {
    let key = config
        .get("admin_key")
        .and_then(Value::as_str)
        .ok_or("cursor.json has no admin_key")?;
    let email = config.get("email").and_then(Value::as_str).unwrap_or("");
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{key}:"));

    let data: Value = super::http_client()?
        .post("https://api.cursor.com/teams/spend")
        .header("Authorization", format!("Basic {auth}"))
        .header("Accept", "application/json")
        .json(&json!({"searchTerm": email, "page": 1, "pageSize": 10}))
        .send()
        .map_err(|err| err.to_string())?
        .error_for_status()
        .map_err(|err| err.to_string())?
        .json()
        .map_err(|err| err.to_string())?;

    let members = data
        .get("teamMemberSpend")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let member = members
        .iter()
        .find(|item| {
            item.get("email")
                .and_then(Value::as_str)
                .is_some_and(|value| value.eq_ignore_ascii_case(email))
        })
        .or_else(|| members.first())
        .ok_or("user not found in the team response")?;

    let reset = data
        .get("subscriptionCycleStart")
        .and_then(Value::as_f64)
        .map(next_month_iso);
    let mut provider = Provider::new("Cursor", "Business", "Team", email);
    for (key_name, label) in [("totalPercentUsed", "Total usage"), ("autoPercentUsed", "Auto")] {
        if let Some(percent) = member.get(key_name).and_then(Value::as_f64) {
            provider.meters.push(Meter::new(label, Some(percent), reset.clone()));
        }
    }
    let spent = member.get("spendCents").and_then(Value::as_f64).unwrap_or(0.0) / 100.0;
    let limit = member
        .get("monthlyLimitDollars")
        .or_else(|| member.get("hardLimitOverrideDollars"))
        .and_then(Value::as_f64);
    provider.details.push(match limit {
        Some(value) => format!("Spend: ${spent:.2} / ${value:.2}"),
        None => format!("Spend: ${spent:.2}"),
    });
    Ok(provider)
}

/// Cursor's `plan.used`/`limit` saturate at the included allowance, so an
/// account with bonus credits sits at a permanent 100%. `breakdown` carries the
/// real balance (included + bonus) and `totalPercentUsed` the real share of it.
/// Returns (used_cents, total_cents, included_cents); falls back to the plain
/// used/limit pair when the payload has no breakdown.
fn plan_usage(plan: &Value) -> (f64, f64, f64) {
    let breakdown = plan.get("breakdown");
    let included = breakdown
        .and_then(|value| value.get("included"))
        .and_then(Value::as_f64)
        .or_else(|| plan.get("limit").and_then(Value::as_f64))
        .unwrap_or(0.0);
    let total = breakdown
        .and_then(|value| value.get("total"))
        .and_then(Value::as_f64)
        .unwrap_or(included);
    let percent = plan.get("totalPercentUsed").and_then(Value::as_f64);
    let used = match percent {
        Some(percent) if total > 0.0 => percent / 100.0 * total,
        _ => plan.get("used").and_then(Value::as_f64).unwrap_or(0.0),
    };
    (used.min(total), total, included)
}

/// Cursor's included-usage values are USD cents.
fn display_money(cents: f64) -> String {
    let formatted = if cents.fract() == 0.0 && cents % 100.0 == 0.0 {
        format!("${:.0}", cents / 100.0)
    } else {
        format!("${:.2}", cents / 100.0)
    };
    formatted
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

/// (epoch seconds, raw cost in cents) for every usage event in the window.
/// Undocumented dashboard endpoint: it rejects the request without an `Origin`
/// header (403 "Invalid origin for state-changing request") — `Referer` alone
/// does not satisfy it.
fn fetch_events(
    client: &reqwest::blocking::Client,
    cookie: &str,
    start: i64,
    end: i64,
) -> Result<Vec<(i64, f64)>, String> {
    let mut events: Vec<(i64, f64)> = Vec::new();
    let mut page: i64 = 1;
    while page <= EVENTS_MAX_PAGES {
        let payload: Value = client
            .post(EVENTS_URL)
            .header("Cookie", cookie)
            .header("User-Agent", "ai-usage-monitor")
            .header("Origin", "https://cursor.com")
            .json(&json!({
                "startDate": (start * 1000).to_string(),
                "endDate": (end * 1000).to_string(),
                "page": page,
                "pageSize": EVENTS_PAGE_SIZE,
            }))
            .send()
            .map_err(|err| err.to_string())?
            .error_for_status()
            .map_err(|err| err.to_string())?
            .json()
            .map_err(|err| err.to_string())?;

        for item in payload
            .get("usageEventsDisplay")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            // `timestamp` is epoch ms inside a string; `totalCents` is a float.
            let moment = item
                .get("timestamp")
                .and_then(|value| {
                    value
                        .as_str()
                        .and_then(|text| text.parse::<i64>().ok())
                        .or_else(|| value.as_f64().map(|number| number as i64))
                })
                .map(|ms| ms / 1000);
            let cost = item
                .get("tokenUsage")
                .and_then(|usage| usage.get("totalCents"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            if let Some(moment) = moment {
                events.push((moment, cost));
            }
        }

        let total = payload
            .get("totalUsageEventsCount")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if page * EVENTS_PAGE_SIZE >= total {
            break;
        }
        page += 1;
    }
    Ok(events)
}

/// The cycle-aligned week `now` falls in. Aligning to the cycle keeps the
/// weekly budget an exact share of the monthly quota; the trailing week is
/// short and gets a proportionally smaller budget.
fn week_window(cycle_start: i64, cycle_end: i64, now: i64) -> (i64, i64) {
    let index = (now - cycle_start).max(0) / WEEK_SECONDS;
    let start = cycle_start + WEEK_SECONDS * index;
    (start, (start + WEEK_SECONDS).min(cycle_end))
}

/// Pace gauge: this cycle-week's raw spend against its share of the monthly
/// limit. Before the plan overflows, the limit is charged in the same raw
/// model-cost units the events endpoint reports, so no quota-dollar conversion
/// is needed — `used` is simply the window's `tokenUsage.totalCents`. The
/// percent is deliberately not clamped — over 100% is the signal that the burn
/// rate is above what the cycle can sustain.
fn weekly_meter(
    cycle_start: i64,
    cycle_end: i64,
    now: i64,
    plan_total: f64,
    events: &[(i64, f64)],
) -> Option<Meter> {
    if cycle_end <= cycle_start || plan_total <= 0.0 {
        return None;
    }
    let (start, end) = week_window(cycle_start, cycle_end, now);
    if end <= start {
        return None;
    }
    let budget = plan_total * (end - start) as f64 / (cycle_end - cycle_start) as f64;
    if budget <= 0.0 {
        return None;
    }
    let used: f64 = events
        .iter()
        .filter(|(moment, _)| *moment >= start && *moment < end)
        .map(|(_, cost)| cost)
        .sum();
    Some(Meter::new("Weekly", Some(used * 100.0 / budget), Some(epoch_to_iso(end as f64))))
}

#[cfg(test)]
mod tests {
    use super::{display_money, plan_usage, week_window, weekly_meter};
    use serde_json::json;

    #[test]
    fn formats_included_usage_cents_as_dollars() {
        assert_eq!(display_money(35.0), "$0.35");
        assert_eq!(display_money(2_000.0), "$20");
    }

    #[test]
    fn scales_the_real_balance_by_the_total_percent_used() {
        let plan = json!({
            "used": 2000,
            "limit": 2000,
            "remaining": 0,
            "breakdown": { "included": 2000, "bonus": 5252, "total": 7252 },
            "autoPercentUsed": 0,
            "apiPercentUsed": 90.65,
            "totalPercentUsed": 90.65
        });
        let (used, total, included) = plan_usage(&plan);
        assert!((used - 6_573.938).abs() < 0.01, "used = {used}");
        assert!((total - 7_252.0).abs() < 0.01, "total = {total}");
        assert!((included - 2_000.0).abs() < 0.01, "included = {included}");
    }

    #[test]
    fn falls_back_to_used_and_limit_without_a_breakdown() {
        let plan = json!({ "used": 1234, "limit": 2000 });
        let (used, total, included) = plan_usage(&plan);
        assert!((used - 1_234.0).abs() < 0.01, "used = {used}");
        assert!((total - 2_000.0).abs() < 0.01, "total = {total}");
        assert!((included - 2_000.0).abs() < 0.01, "included = {included}");
    }

    // 2026-08-01T00:00:00Z .. 2026-09-01T00:00:00Z (31 days).
    const CYCLE_START: i64 = 1_785_542_400;
    const CYCLE_END: i64 = CYCLE_START + 31 * 86_400;

    #[test]
    fn aligns_weeks_to_the_billing_cycle() {
        // Day 0 → the first week.
        assert_eq!(
            week_window(CYCLE_START, CYCLE_END, CYCLE_START),
            (CYCLE_START, CYCLE_START + 7 * 86_400)
        );
        // Day 17 → the third week (index 2).
        let day17 = CYCLE_START + 17 * 86_400;
        assert_eq!(
            week_window(CYCLE_START, CYCLE_END, day17),
            (CYCLE_START + 14 * 86_400, CYCLE_START + 21 * 86_400)
        );
        // The trailing week is clipped to the cycle end (31 = 4*7 + 3).
        let day29 = CYCLE_START + 29 * 86_400;
        assert_eq!(
            week_window(CYCLE_START, CYCLE_END, day29),
            (CYCLE_START + 28 * 86_400, CYCLE_END)
        );
    }

    #[test]
    fn scales_the_weekly_budget_by_the_window_length() {
        // A full week of a 31-day cycle: 70 / (7/31 * 3100) = 10%.
        let events = [(CYCLE_START + 3_600, 70.0)];
        let meter = weekly_meter(CYCLE_START, CYCLE_END, CYCLE_START, 3_100.0, &events).unwrap();
        assert!((meter.percent.unwrap() - 10.0).abs() < 0.01, "percent = {:?}", meter.percent);
        assert_eq!(meter.used, None);
        assert_eq!(meter.limit, None);
        // The trailing 3-day window gets 3/31: 70 / 300 ≈ 23.33%.
        let events = [(CYCLE_START + 29 * 86_400, 70.0)];
        let meter =
            weekly_meter(CYCLE_START, CYCLE_END, CYCLE_START + 29 * 86_400, 3_100.0, &events)
                .unwrap();
        assert!(
            (meter.percent.unwrap() - 70.0 / 300.0 * 100.0).abs() < 0.01,
            "percent = {:?}",
            meter.percent
        );
    }

    #[test]
    fn sums_raw_event_cost_inside_the_week_window() {
        // Only the two events inside week 0 count: 100 + 60 = 160 cents
        // against a 7/31 * 3100 = 700 cent budget → 22.86%. Dollars stay off
        // the meter (only Extra usage shows money).
        let events = [
            (CYCLE_START + 3_600, 100.0),
            (CYCLE_START + 2 * 86_400, 60.0),
            (CYCLE_START + 10 * 86_400, 240.0),
        ];
        let meter =
            weekly_meter(CYCLE_START, CYCLE_END, CYCLE_START + 86_400, 3_100.0, &events).unwrap();
        assert_eq!(meter.used, None);
        assert_eq!(meter.limit, None);
        assert!(
            (meter.percent.unwrap() - 22.857).abs() < 0.01,
            "percent = {:?}",
            meter.percent
        );
    }

    #[test]
    fn does_not_clamp_a_burn_rate_above_the_sustainable_pace() {
        // Everything spent in week 0: 1400 raw cents against a 700 cent budget
        // is 200%, and must stay 200%.
        let events = [(CYCLE_START + 3_600, 1_400.0)];
        let meter =
            weekly_meter(CYCLE_START, CYCLE_END, CYCLE_START + 86_400, 3_100.0, &events).unwrap();
        assert!(
            (meter.percent.unwrap() - 200.0).abs() < 0.01,
            "percent = {:?}",
            meter.percent
        );
    }

    #[test]
    fn refuses_a_degenerate_cycle() {
        assert!(weekly_meter(CYCLE_END, CYCLE_START, CYCLE_START, 100.0, &[]).is_none());
        assert!(weekly_meter(CYCLE_START, CYCLE_END, CYCLE_START, 0.0, &[]).is_none());
    }
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => String::new(),
    }
}
