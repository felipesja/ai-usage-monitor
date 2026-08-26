//! SuperGrok / Grok Build limits. Reads the Grok CLI session at
//! `~/.grok/auth.json`, refreshes the OIDC token when needed, and queries the
//! CLI-proxy billing API. Port of `collect_grok`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::config::{grok_auth, grok_home, read_json, write_json};
use super::date::{epoch_to_iso, iso_to_epoch};
use super::{Meter, Provider};

const BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const SETTINGS_URL: &str = "https://cli-chat-proxy.grok.com/v1/settings";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const REFRESH_BUFFER_SECS: i64 = 120;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn collect() -> Provider {
    let path = grok_auth();
    if !path.exists() {
        return Provider::with_error("Grok", "SuperGrok", "no local session found".into());
    }
    match run(&path) {
        Ok(provider) => provider,
        Err(err) => Provider::with_error("Grok", "SuperGrok", err),
    }
}

fn run(path: &Path) -> Result<Provider, String> {
    let mut data = read_json(path)?;
    let session_key = session_key(&data)?;
    refresh(path, &mut data, &session_key, false)?;

    let mut token = session_token(&data, &session_key)?;
    let email = session_email(&data, &session_key);
    let client = super::http_client()?;

    let billing = match get_json(&client, BILLING_URL, &token) {
        Ok(value) => value,
        Err(err) if err.contains("401") => {
            refresh(path, &mut data, &session_key, true)?;
            token = session_token(&data, &session_key)?;
            get_json(&client, BILLING_URL, &token)?
        }
        Err(err) => return Err(err),
    };

    let plan = get_json(&client, SETTINGS_URL, &token)
        .ok()
        .and_then(|settings| {
            settings
                .get("subscription_tier_display")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "SuperGrok".into());

    let config = billing.get("config").cloned().unwrap_or(billing);
    Ok(from_billing(&config, &email, &plan))
}

fn from_billing(config: &Value, email: &str, plan: &str) -> Provider {
    let mut provider = Provider::new("Grok", "SuperGrok", plan, email);
    let period = config.get("currentPeriod").cloned().unwrap_or(Value::Null);
    let period_type = period
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase();
    let label = if period_type.contains("WEEKLY") {
        "Weekly"
    } else if period_type.contains("MONTHLY") {
        "Monthly"
    } else {
        "Credits"
    };

    let mut percent = config.get("creditUsagePercent").and_then(Value::as_f64);
    if percent.is_none() {
        if let Some(items) = config.get("productUsage").and_then(Value::as_array) {
            percent = items.iter().find_map(|item| {
                (item.get("product").and_then(Value::as_str) == Some("GrokBuild"))
                    .then(|| item.get("usagePercent").and_then(Value::as_f64))
                    .flatten()
            });
        }
    }
    if percent.is_none() {
        let cap = numeric(config.get("onDemandCap"));
        let used = numeric(config.get("onDemandUsed"));
        if cap > 0.0 {
            percent = Some(used / cap * 100.0);
        } else if !period.is_null() {
            percent = Some(0.0);
        }
    }

    let reset = period
        .get("end")
        .or_else(|| config.get("billingPeriodEnd"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(percent) = percent {
        provider.meters.push(Meter::new(label, Some(percent), reset));
    }

    let on_used = numeric(config.get("onDemandUsed"));
    let on_cap = numeric(config.get("onDemandCap"));
    if on_used > 0.0 {
        provider.details.push(if on_cap > 0.0 {
            format!("On demand: {on_used} / {on_cap}")
        } else {
            format!("On demand: {on_used}")
        });
    }
    let prepaid = numeric(config.get("prepaidBalance"));
    if prepaid > 0.0 {
        provider.details.push(format!("Prepaid: {prepaid}"));
    }
    provider
}

fn numeric(value: Option<&Value>) -> f64 {
    let Some(value) = value else { return 0.0 };
    value
        .get("val")
        .and_then(Value::as_f64)
        .or_else(|| value.as_f64())
        .unwrap_or(0.0)
}

fn session_key(data: &Value) -> Result<String, String> {
    let obj = data.as_object().ok_or("no local session found")?;
    let mut best: Option<(String, i64)> = None;
    for (key, account) in obj {
        if account
            .get("key")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            continue;
        }
        let expires = account
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(iso_to_epoch)
            .unwrap_or(0);
        if best.as_ref().is_none_or(|(_, stamp)| expires >= *stamp) {
            best = Some((key.clone(), expires));
        }
    }
    best.map(|(key, _)| key).ok_or_else(|| "no local session found".into())
}

fn session_token(data: &Value, key: &str) -> Result<String, String> {
    data.get(key)
        .and_then(|account| account.get("key"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "Grok session has no access token".into())
}

fn session_email(data: &Value, key: &str) -> String {
    data.get(key)
        .and_then(|account| account.get("email"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Renews the access token when it expires in under 2 min (or when `force`),
/// and writes the rotated pair back so the Grok CLI keeps a usable refresh
/// token — OIDC refresh tokens are single-use.
fn refresh(path: &Path, data: &mut Value, session_key: &str, force: bool) -> Result<(), String> {
    let (refresh_token, client_id, expires) = {
        let account = data.get(session_key).ok_or("Grok session disappeared")?;
        let expires = account
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(iso_to_epoch);
        if !force && expires.is_some_and(|stamp| stamp > now_epoch() + REFRESH_BUFFER_SECS) {
            return Ok(());
        }
        let refresh_token = account
            .get("refresh_token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let client_id = account
            .get("oidc_client_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        (refresh_token, client_id, expires)
    };
    if refresh_token.is_empty() || client_id.is_empty() {
        if expires.is_some_and(|stamp| stamp <= now_epoch()) {
            return Err("Grok session expired; run grok login".into());
        }
        return Ok(());
    }

    let client = super::http_client()?;
    let body = form_body(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
        ("client_id", client_id.as_str()),
    ]);
    let response: Value = client
        .post(TOKEN_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", "ai-usage-monitor")
        .body(body)
        .send()
        .map_err(|err| err.to_string())?
        .error_for_status()
        .map_err(|err| err.to_string())?
        .json()
        .map_err(|err| err.to_string())?;

    let access = response
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or("Grok token refresh returned no access_token")?;
    let account = data
        .get_mut(session_key)
        .and_then(Value::as_object_mut)
        .ok_or("Grok session disappeared")?;
    account.insert("key".into(), json!(access));
    if let Some(rotated) = response.get("refresh_token").and_then(Value::as_str) {
        account.insert("refresh_token".into(), json!(rotated));
    }
    let expires_in = response.get("expires_in").and_then(Value::as_f64).unwrap_or(28_800.0);
    account.insert(
        "expires_at".into(),
        json!(epoch_to_iso(now_epoch() as f64 + expires_in)),
    );
    write_json(path, data)
}

fn get_json(client: &reqwest::blocking::Client, url: &str, token: &str) -> Result<Value, String> {
    client
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .header("x-xai-token-auth", "xai-grok-cli")
        .header("User-Agent", "ai-usage-monitor")
        .send()
        .map_err(|err| err.to_string())?
        .error_for_status()
        .map_err(|err| err.to_string())?
        .json()
        .map_err(|err| err.to_string())
}

/// Runs `grok logout`. If the CLI is missing, drop `auth.json` so the provider
/// hides the same way a never-configured one does.
pub fn logout() -> Result<(), String> {
    match grok_command() {
        Ok(mut command) => {
            command.arg("logout");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(CREATE_NO_WINDOW);
            }
            let output = command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .map_err(|err| format!("grok logout did not start: {err}"))?;
            if !output.status.success() {
                let text = String::from_utf8_lossy(&output.stderr);
                let text = if text.trim().is_empty() {
                    String::from_utf8_lossy(&output.stdout)
                } else {
                    text
                };
                return Err(format!("grok logout failed: {}", text.trim()));
            }
            Ok(())
        }
        Err(_) => {
            let path = grok_auth();
            if path.exists() {
                std::fs::remove_file(&path).map_err(|err| format!("{}: {err}", path.display()))?;
            }
            Ok(())
        }
    }
}

fn grok_command() -> Result<Command, String> {
    let binary = find_grok().ok_or("grok CLI not found")?;
    Ok(Command::new(binary))
}

fn find_grok() -> Option<PathBuf> {
    let home = grok_home();
    #[cfg(windows)]
    let names = ["grok.exe", "grok.cmd"];
    #[cfg(not(windows))]
    let names = ["grok"];

    let mut candidates = Vec::new();
    for name in names {
        candidates.push(home.join("bin").join(name));
    }
    if let Some(found) = candidates.iter().find(|candidate| candidate.is_file()) {
        return Some(found.clone());
    }
    if let Some(found) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
            .find(|candidate| candidate.is_file())
    }) {
        return Some(found);
    }
    #[cfg(not(windows))]
    {
        let user = super::config::home();
        for candidate in [
            user.join(".local").join("bin").join("grok"),
            user.join("bin").join("grok"),
            PathBuf::from("/opt/homebrew/bin/grok"),
            PathBuf::from("/usr/local/bin/grok"),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (index, (key, value)) in pairs.iter().enumerate() {
        if index > 0 {
            out.push('&');
        }
        out.push_str(&encode_form(key));
        out.push('=');
        out.push_str(&encode_form(value));
    }
    out
}

fn encode_form(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{from_billing, numeric};
    use serde_json::json;

    #[test]
    fn weekly_credits_percent_and_reset() {
        let config = json!({
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_WEEKLY",
                "start": "2026-08-15T00:00:00+00:00",
                "end": "2026-08-22T00:00:00+00:00"
            },
            "creditUsagePercent": 1.0,
            "onDemandCap": { "val": 0 },
            "onDemandUsed": { "val": 0 },
            "productUsage": [{ "product": "GrokBuild", "usagePercent": 1.0 }],
            "prepaidBalance": { "val": 0 },
            "billingPeriodEnd": "2026-08-22T00:00:00+00:00"
        });
        let provider = from_billing(&config, "user@example.com", "SuperGrok");
        assert_eq!(provider.plan, "SuperGrok");
        assert_eq!(provider.email, "user@example.com");
        assert_eq!(provider.meters.len(), 1);
        assert_eq!(provider.meters[0].label, "Weekly");
        assert_eq!(provider.meters[0].percent, Some(1.0));
        assert_eq!(
            provider.meters[0].reset_at.as_deref(),
            Some("2026-08-22T00:00:00+00:00")
        );
        assert!(provider.details.is_empty());
    }

    #[test]
    fn falls_back_to_product_usage_and_reports_on_demand() {
        let config = json!({
            "currentPeriod": { "type": "USAGE_PERIOD_TYPE_MONTHLY", "end": "2026-09-01T00:00:00+00:00" },
            "productUsage": [{ "product": "GrokBuild", "usagePercent": 42.5 }],
            "onDemandUsed": { "val": 3.5 },
            "onDemandCap": { "val": 10 },
            "prepaidBalance": { "val": 12 }
        });
        let provider = from_billing(&config, "", "SuperGrok Heavy");
        assert_eq!(provider.meters[0].label, "Monthly");
        assert_eq!(provider.meters[0].percent, Some(42.5));
        assert_eq!(provider.details, ["On demand: 3.5 / 10", "Prepaid: 12"]);
    }

    #[test]
    fn numeric_reads_wrapped_val() {
        assert_eq!(numeric(Some(&json!({ "val": 7 }))), 7.0);
        assert_eq!(numeric(Some(&json!(3.25))), 3.25);
        assert_eq!(numeric(None), 0.0);
    }
}
