//! Cursor Business limits. Two methods in `cursor.json`: `admin_key` (team
//! admin API, preferred) or `dashboard_cookie` (internal dashboard endpoint,
//! may break if Cursor changes it). Port of `collect_cursor`.

use base64::Engine;
use serde_json::{json, Value};

use super::config::{cursor_config, read_json};
use super::date::next_month_iso;
use super::{Meter, Provider};

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
    let raw_used = plan.get("used").and_then(Value::as_f64).unwrap_or(0.0);
    let raw_limit = plan.get("limit").and_then(Value::as_f64).unwrap_or(0.0);
    let percent = if raw_limit > 0.0 {
        Some(raw_used * 100.0 / raw_limit)
    } else {
        None
    };

    // The team dashboard shows request units at 1/4 of the internal values
    // returned by usage-summary (576/2000 -> 144/500).
    let scale = if data.get("limitType").and_then(Value::as_str) == Some("team") {
        4.0
    } else {
        1.0
    };
    let billing_end = data.get("billingCycleEnd").and_then(Value::as_str).map(str::to_string);
    let mut usage_meter = Meter::new("Usage", percent, billing_end.clone());
    usage_meter.used = Some(display_number(raw_used / scale));
    usage_meter.limit = Some(display_number(raw_limit / scale));
    provider.meters.push(usage_meter);

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

/// Integer with no decimals when exact (matches Python's `display_number`).
fn display_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => String::new(),
    }
}
