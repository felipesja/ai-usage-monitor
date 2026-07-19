//! Collects the limits of a Claude account from a profile stored at
//! `~/.config/ai-usage-monitor/claude/<name>/.credentials.json`, refreshing the
//! OAuth token when needed. Port of `collect_claude`/`refresh_claude`.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::config::{read_json, write_json};
use super::{Meter, Provider};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_millis() as f64)
        .unwrap_or(0.0)
}

pub fn collect(profile_dir: &Path) -> Provider {
    let name = profile_name(profile_dir);
    match run(profile_dir, &name) {
        Ok(provider) => provider,
        Err(err) => Provider::with_error("Claude", &name, err),
    }
}

fn profile_name(profile_dir: &Path) -> String {
    profile_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("claude")
        .to_string()
}

fn run(profile_dir: &Path, name: &str) -> Result<Provider, String> {
    let path = profile_dir.join(".credentials.json");
    let mut data = read_json(&path)?;
    refresh(&path, &mut data)?;

    let oauth = data
        .get("claudeAiOauth")
        .ok_or("the file does not contain a Claude OAuth session")?;
    let token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .ok_or("credential has no accessToken")?
        .to_string();
    let plan = title_case(oauth.get("subscriptionType").and_then(Value::as_str).unwrap_or(""));

    let client = super::http_client()?;
    let usage = get_json(&client, USAGE_URL, &token)?;
    let email = get_json(&client, PROFILE_URL, &token)
        .ok()
        .and_then(|profile| {
            profile
                .get("account")
                .and_then(|account| account.get("email"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();

    let mut provider = Provider::new("Claude", name, &plan, &email);
    for (key, label) in [
        ("five_hour", "Session"),
        ("seven_day", "Weekly"),
        ("seven_day_sonnet", "Weekly Sonnet"),
    ] {
        if let Some(block) = usage.get(key) {
            if !block.is_null() {
                let percent = block.get("utilization").and_then(Value::as_f64).unwrap_or(0.0);
                let reset_at = block.get("resets_at").and_then(Value::as_str).map(str::to_string);
                provider.meters.push(Meter::new(label, Some(percent), reset_at));
            }
        }
    }
    if let Some(extra) = usage.get("extra_usage") {
        if extra.get("is_enabled").and_then(Value::as_bool).unwrap_or(false) {
            let used = extra.get("utilization").and_then(Value::as_f64).unwrap_or(0.0);
            provider.details.push(format!("Extra usage: {used}%"));
        }
    }
    Ok(provider)
}

/// Refreshes the access token if it expires in under 2 min; writes it back.
fn refresh(path: &Path, data: &mut Value) -> Result<(), String> {
    let (refresh_token, scopes) = {
        let oauth = data
            .get("claudeAiOauth")
            .ok_or("the file does not contain a Claude OAuth session")?;
        let expires_at = oauth.get("expiresAt").and_then(Value::as_f64).unwrap_or(0.0);
        if expires_at > now_ms() + 120_000.0 {
            return Ok(());
        }
        let refresh_token = oauth
            .get("refreshToken")
            .and_then(Value::as_str)
            .ok_or("credential has no refreshToken")?
            .to_string();
        let scopes = oauth
            .get("scopes")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        (refresh_token, scopes)
    };

    let client = super::http_client()?;
    let response: Value = client
        .post(TOKEN_URL)
        .header("User-Agent", "claude-code/2")
        .header("Accept", "application/json")
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
            "scope": scopes,
        }))
        .send()
        .map_err(|err| err.to_string())?
        .error_for_status()
        .map_err(|err| err.to_string())?
        .json()
        .map_err(|err| err.to_string())?;

    let oauth = data
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
        .ok_or("invalid claudeAiOauth")?;
    if let Some(access) = response.get("access_token").and_then(Value::as_str) {
        oauth.insert("accessToken".into(), json!(access));
    }
    if let Some(refresh) = response.get("refresh_token").and_then(Value::as_str) {
        oauth.insert("refreshToken".into(), json!(refresh));
    }
    let expires_in = response.get("expires_in").and_then(Value::as_f64).unwrap_or(28_800.0);
    oauth.insert("expiresAt".into(), json!(now_ms() + expires_in * 1000.0));
    if let Some(refresh_expires) = response.get("refresh_token_expires_in").and_then(Value::as_f64) {
        oauth.insert("refreshTokenExpiresAt".into(), json!(now_ms() + refresh_expires * 1000.0));
    }
    if let Some(scope) = response.get("scope").and_then(Value::as_str) {
        oauth.insert("scopes".into(), json!(scope.split(' ').collect::<Vec<_>>()));
    }
    write_json(path, data)
}

fn get_json(client: &reqwest::blocking::Client, url: &str, token: &str) -> Result<Value, String> {
    client
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", "claude-code/2")
        .header("Accept", "application/json")
        .send()
        .map_err(|err| err.to_string())?
        .error_for_status()
        .map_err(|err| err.to_string())?
        .json()
        .map_err(|err| err.to_string())
}

/// Equivalent to Python's `str.title()` for short plan strings.
fn title_case(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
