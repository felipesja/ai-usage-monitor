//! Codex limits. Tries the `app-server` via JSON-RPC over stdio and, on
//! failure, falls back to the local session cache — flagging the downgrade in
//! `details`, like the Python collector.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{json, Value};

use super::config::{
    default_codex_home, home, read_json, write_json, CODEX_LIVE_ACCOUNT,
};
use super::date::epoch_to_iso;
use super::{Meter, Provider};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const REFRESH_BUFFER_SECS: f64 = 120.0;

/// One Codex account from a given `CODEX_HOME` (live CLI or a stored profile).
pub fn collect_home(home: &std::path::Path, account: &str) -> Provider {
    let is_live = same_home(home, &default_codex_home());
    let fetch_result = if is_live { live(home) } else { http(home) };
    let marker = super::config::codex_removed_marker();
    if is_live && marker.exists() {
        match &fetch_result {
            // A fresh `codex login` produced live data again — welcome back.
            Ok(_) => {
                let _ = std::fs::remove_file(&marker);
            }
            // Still logged out: ignore any stale session cache and stay hidden,
            // like a provider that was never set up.
            Err(_) => {
                return collect_error(home, account, "no local session found".into());
            }
        }
    }

    let mut downgrade = None;
    let raw = match fetch_result {
        Ok(value) => value,
        Err(err) => match cached(home) {
            Ok(value) => {
                downgrade = Some(err);
                value
            }
            Err(_) => {
                return collect_error(home, account, describe_live_error(&err));
            }
        },
    };

    let limits = raw.get("rateLimits").cloned().unwrap_or(Value::Null);
    let plan = limits
        .get("planType")
        .or_else(|| limits.get("plan_type"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .replace('_', " ");
    let mut provider = Provider::new("Codex", account, &title_case(&plan), &email_from(home));
    if let Some(err) = downgrade {
        provider.details.push(format!("⚠ local cache · {}", describe_live_error(&err)));
    }

    for key in ["primary", "secondary"] {
        let Some(block) = limits.get(key) else { continue };
        if block.is_null() {
            continue;
        }
        let minutes = block
            .get("windowDurationMins")
            .or_else(|| block.get("window_minutes"))
            .and_then(Value::as_f64);
        let label = match minutes {
            Some(m) if m == 300.0 => "Session".to_string(),
            Some(m) if m == 10_080.0 => "Weekly".to_string(),
            Some(m) => format!("{m:.0} min"),
            None => key.to_string(),
        };
        let percent = block
            .get("usedPercent")
            .or_else(|| block.get("used_percent"))
            .and_then(Value::as_f64);
        let reset = block.get("resetsAt").or_else(|| block.get("resets_at"));
        let reset_at = match reset {
            // Epoch seconds become ISO-8601 UTC (the frontend does `new Date`).
            Some(Value::Number(number)) => number.as_f64().map(epoch_to_iso),
            Some(Value::String(text)) => Some(text.clone()),
            _ => None,
        };
        provider.meters.push(Meter::new(&label, percent, reset_at));
    }

    if let Some(credits) = limits.get("credits") {
        let has = credits
            .get("hasCredits")
            .or_else(|| credits.get("has_credits"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if has {
            let balance = credits
                .get("balance")
                .map(|value| value.to_string())
                .unwrap_or_else(|| "?".into());
            provider.details.push(format!("Credits: {balance}"));
        }
    }
    provider
}

fn collect_error(home: &std::path::Path, account: &str, error: String) -> Provider {
    let mut provider = Provider::with_error("Codex", account, error);
    provider.email = email_from(home);
    provider
}

fn describe_live_error(err: &str) -> String {
    let lower = err.to_lowercase();
    if ["authentication required", "unauthorized", "invalid_grant", "401", "403"]
        .iter()
        .any(|part| lower.contains(part))
    {
        "session expired — log into this account in the Codex CLI, then add it again".into()
    } else {
        err.to_string()
    }
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_secs_f64())
        .unwrap_or(0.0)
}

fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn jwt_exp(token: &str) -> f64 {
    jwt_claims(token)
        .and_then(|claims| claims.get("exp").and_then(Value::as_f64))
        .unwrap_or(0.0)
}

fn refresh_auth(codex_home: &std::path::Path, force: bool) -> Result<Value, String> {
    let path = codex_home.join("auth.json");
    let mut data = read_json(&path)?;
    let tokens = data.get("tokens").cloned().unwrap_or(Value::Null);
    let access = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let refresh = tokens
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if !force && jwt_exp(&access) > now_epoch() + REFRESH_BUFFER_SECS {
        return Ok(data);
    }
    if refresh.is_empty() {
        return Err("session expired — log into this account in the Codex CLI, then add it again".into());
    }
    let client = super::http_client()?;
    let response: Value = client
        .post(TOKEN_URL)
        .header("Accept", "application/json")
        .header("User-Agent", "codex-cli")
        .json(&json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh,
            "scope": "openid profile email",
        }))
        .send()
        .map_err(|err| err.to_string())?
        .error_for_status()
        .map_err(|err| err.to_string())?
        .json()
        .map_err(|err| err.to_string())?;
    let access = response
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or("ChatGPT token refresh did not return an access token")?;
    let tokens = data
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .ok_or("auth.json has no tokens")?;
    tokens.insert("access_token".into(), json!(access));
    if let Some(rotated) = response.get("refresh_token").and_then(Value::as_str) {
        tokens.insert("refresh_token".into(), json!(rotated));
    }
    if let Some(id_token) = response.get("id_token").and_then(Value::as_str) {
        tokens.insert("id_token".into(), json!(id_token));
    }
    write_json(&path, &data)?;
    Ok(data)
}

fn usage_from_wham(usage: &Value) -> Value {
    let rate = usage
        .get("rate_limit")
        .or_else(|| usage.get("rateLimit"))
        .cloned()
        .unwrap_or(Value::Null);
    let plan = usage
        .get("plan_type")
        .or_else(|| usage.get("planType"))
        .and_then(Value::as_str)
        .unwrap_or("");
    json!({
        "rateLimits": {
            "planType": plan,
            "primary": window_block(rate.get("primary_window").or_else(|| rate.get("primary"))),
            "secondary": window_block(rate.get("secondary_window").or_else(|| rate.get("secondary"))),
            "credits": usage.get("credits").or_else(|| rate.get("credits")).cloned().unwrap_or(json!({})),
        }
    })
}

fn window_block(block: Option<&Value>) -> Value {
    let Some(block) = block.filter(|value| value.is_object()) else {
        return Value::Null;
    };
    let minutes = block
        .get("limit_window_seconds")
        .or_else(|| block.get("window_seconds"))
        .and_then(Value::as_f64)
        .map(|seconds| seconds / 60.0)
        .or_else(|| block.get("windowDurationMins").and_then(Value::as_f64));
    json!({
        "usedPercent": block.get("used_percent").or_else(|| block.get("usedPercent")).cloned(),
        "windowDurationMins": minutes,
        "resetsAt": block.get("reset_at").or_else(|| block.get("resets_at")).or_else(|| block.get("resetsAt")).cloned(),
    })
}

fn get_usage(client: &reqwest::blocking::Client, data: &Value) -> Result<Value, String> {
    let tokens = data.get("tokens").cloned().unwrap_or(Value::Null);
    let access = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or("Codex auth.json has no access token")?;
    let mut request = client
        .get(USAGE_URL)
        .header("Authorization", format!("Bearer {access}"))
        .header("Accept", "application/json")
        .header("User-Agent", "codex-cli");
    if let Some(account_id) = tokens.get("account_id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
        request = request.header("ChatGPT-Account-Id", account_id);
    }
    let usage: Value = request
        .send()
        .map_err(|err| err.to_string())?
        .error_for_status()
        .map_err(|err| err.to_string())?
        .json()
        .map_err(|err| err.to_string())?;
    Ok(usage_from_wham(&usage))
}

fn http(codex_home: &std::path::Path) -> Result<Value, String> {
    let mut data = refresh_auth(codex_home, false)?;
    let client = super::http_client()?;
    match get_usage(&client, &data) {
        Ok(value) => Ok(value),
        Err(err) if err.contains("401") || err.contains("403") => {
            data = refresh_auth(codex_home, true)?;
            get_usage(&client, &data)
        }
        Err(err) => Err(err),
    }
}

/// Runs `codex logout`, the same as a user typing it in a terminal — this
/// touches the Codex CLI's own auth (`~/.codex/auth.json`), which the widget
/// does not own or write, unlike the Claude/Cursor stores.
pub fn logout() -> Result<(), String> {
    let mut command = codex_command()?;
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
        .map_err(|err| format!("codex logout did not start: {err}"))?;
    if !output.status.success() {
        let text = String::from_utf8_lossy(&output.stderr);
        let text = if text.trim().is_empty() {
            String::from_utf8_lossy(&output.stdout)
        } else {
            text
        };
        return Err(format!("codex logout failed: {}", text.trim()));
    }
    // Suppresses the provider even if a stale session cache would otherwise
    // let `collect()` keep showing old numbers; cleared on the next login.
    let _ = std::fs::write(super::config::codex_removed_marker(), "");
    Ok(())
}

/// The npm shim on Windows (`codex.cmd`) or the native binary installed inside
/// the npm package. Recent npm versions can leave only `codex`/`codex.ps1` in
/// the global bin directory, neither of which a GUI process can execute
/// reliably with its reduced PATH.
pub fn codex_command() -> Result<Command, String> {
    #[cfg(windows)]
    {
        let npm_dir = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData").join("Roaming"))
            .join("npm");
        let shim = npm_dir.join("codex.cmd");
        if shim.exists() {
            let mut command = Command::new("cmd.exe");
            command.arg("/C").arg(shim);
            return Ok(command);
        }
        let binary = find_windows_codex(&npm_dir).unwrap_or_else(|| PathBuf::from("codex.exe"));
        Ok(Command::new(binary))
    }
    #[cfg(not(windows))]
    {
        let binary = find_codex();
        let mut command = Command::new(&binary);
        // GUI apps launched by launchd/Finder get a minimal PATH (no Homebrew,
        // no nvm). Prepend the binary's own directory so the npm shim finds its
        // sibling `node`, mirroring the Python collector's `codex_live`.
        if let Some(parent) = binary.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            let path = std::env::var("PATH").unwrap_or_default();
            command.env("PATH", format!("{}:{path}", parent.display()));
        }
        Ok(command)
    }
}

/// Find the native executable shipped by `@openai/codex`. npm may install the
/// platform package either beside `codex` or nested below it.
#[cfg(windows)]
fn find_windows_codex(npm_dir: &std::path::Path) -> Option<PathBuf> {
    if let Some(found) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("codex.exe"))
            .find(|candidate| candidate.is_file())
    }) {
        return Some(found);
    }

    // The standalone Windows installer puts the binary outside npm's global
    // directory. A GUI launched before the installer ran can also have a
    // stale PATH, so check the install locations directly.
    for candidate in native_windows_codex_candidates() {
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    let openai = npm_dir.join("node_modules").join("@openai");
    let package_roots = [openai.clone(), openai.join("codex").join("node_modules").join("@openai")];
    for root in package_roots {
        let Ok(entries) = std::fs::read_dir(root) else { continue };
        let mut packages: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("codex-win32-")
            })
            .map(|entry| entry.path())
            .collect();
        packages.sort();
        packages.reverse();
        for package in packages {
            if let Some(found) = find_codex_exe(&package) {
                return Some(found);
            }
        }
    }
    None
}

#[cfg(windows)]
fn native_windows_codex_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local_app_data)
                .join("Programs")
                .join("OpenAI")
                .join("Codex")
                .join("bin")
                .join("codex.exe"),
        );
    }
    for variable in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(program_files) = std::env::var_os(variable) {
            candidates.push(
                PathBuf::from(program_files)
                    .join("OpenAI")
                    .join("Codex")
                    .join("bin")
                    .join("codex.exe"),
            );
        }
    }
    candidates
}

#[cfg(windows)]
fn find_codex_exe(root: &std::path::Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if entry.file_name().to_string_lossy().eq_ignore_ascii_case("codex.exe") {
                return Some(path);
            }
        }
    }
    None
}

/// `codex` from PATH, then the install locations a GUI app's minimal launchd
/// PATH misses (nvm, ~/.local/bin, Homebrew) — mirroring Python's `codex_bin`.
#[cfg(not(windows))]
fn find_codex() -> PathBuf {
    if let Some(found) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("codex"))
            .find(|candidate| candidate.is_file())
    }) {
        return found;
    }
    let home = home();
    let mut candidates = Vec::new();
    // Highest nvm version first, like Python's `sorted(...)[-1]`.
    if let Ok(entries) = std::fs::read_dir(home.join(".nvm").join("versions").join("node")) {
        let mut versions: Vec<PathBuf> =
            entries.flatten().map(|entry| entry.path().join("bin").join("codex")).collect();
        versions.sort();
        versions.reverse();
        candidates.extend(versions);
    }
    candidates.push(home.join(".local").join("bin").join("codex"));
    candidates.push(home.join("bin").join("codex"));
    candidates.push(PathBuf::from("/opt/homebrew/bin/codex"));
    candidates.push(PathBuf::from("/usr/local/bin/codex"));
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from("codex"))
}

fn live(codex_home: &std::path::Path) -> Result<Value, String> {
    let _ = std::fs::create_dir_all(codex_home);
    let mut command = codex_command()?;
    command
        .env("CODEX_HOME", codex_home)
        .arg("app-server")
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|err| format!("app-server did not start: {err}"))?;

    let stdout = child.stdout.take().ok_or("app-server has no stdout")?;
    let (sender, receiver) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if sender.send(value).is_err() {
                    return;
                }
            }
        }
    });

    let result = (|| -> Result<Value, String> {
        let stdin = child.stdin.as_mut().ok_or("app-server has no stdin")?;
        let init = serde_json::json!({
            "method": "initialize",
            "id": 1,
            "params": {
                "clientInfo": {"name": "ai-usage-monitor", "title": "AI Usage Monitor", "version": "0.1.0"},
                "capabilities": null,
            },
        });
        writeln!(stdin, "{init}").map_err(|err| err.to_string())?;
        stdin.flush().map_err(|err| err.to_string())?;
        wait_for(&receiver, 1, Duration::from_secs(5)).map_err(|err| format!("{err} (init phase)"))?;

        writeln!(stdin, "{}", r#"{"method":"initialized"}"#).map_err(|err| err.to_string())?;
        writeln!(stdin, "{}", r#"{"method":"account/rateLimits/read","id":2}"#)
            .map_err(|err| err.to_string())?;
        stdin.flush().map_err(|err| err.to_string())?;
        wait_for(&receiver, 2, Duration::from_secs(15)).map_err(|err| format!("{err} (rateLimits phase)"))
    })();

    terminate(&mut child);
    result
}

fn wait_for(receiver: &mpsc::Receiver<Value>, id: i64, timeout: Duration) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("Codex did not respond in time".into());
        }
        match receiver.recv_timeout(left) {
            Ok(message) => {
                if message.get("id").and_then(Value::as_i64) == Some(id) {
                    if let Some(error) = message.get("error") {
                        return Err(error.to_string());
                    }
                    return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            Err(_) => return Err("Codex did not respond in time".into()),
        }
    }
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Last `token_count` carrying `rate_limits` in the most recent session.
fn cached(codex_home: &std::path::Path) -> Result<Value, String> {
    let dir = codex_home.join("sessions");
    let latest = newest_jsonl(&dir).ok_or("no local session found")?;
    let file = std::fs::File::open(&latest).map_err(|err| err.to_string())?;
    let mut found = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
        let payload = value.get("payload").cloned().unwrap_or(Value::Null);
        if payload.get("type").and_then(Value::as_str) == Some("token_count") {
            if let Some(limits) = payload.get("rate_limits") {
                if !limits.is_null() {
                    found = Some(limits.clone());
                }
            }
        }
    }
    let limits = found.ok_or("no limits were found in the local sessions")?;
    Ok(serde_json::json!({ "rateLimits": limits }))
}

fn newest_jsonl(dir: &std::path::Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                if let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) {
                    if best.as_ref().is_none_or(|(best_time, _)| modified > *best_time) {
                        best = Some((modified, path));
                    }
                }
            }
        }
    }
    best.map(|(_, path)| path)
}

/// Email from the `id_token` claims in `$CODEX_HOME/auth.json`.
pub fn email_from(codex_home: &std::path::Path) -> String {
    let Ok(auth) = read_json(&codex_home.join("auth.json")) else {
        return String::new();
    };
    let Some(token) = auth
        .get("tokens")
        .and_then(|tokens| tokens.get("id_token"))
        .and_then(Value::as_str)
    else {
        return String::new();
    };
    let Some(payload) = token.split('.').nth(1) else {
        return String::new();
    };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
        return String::new();
    };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|claims| claims.get("email").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

fn same_home(left: &std::path::Path, right: &std::path::Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(a), Ok(b)) => a == b,
        _ => left == right,
    }
}

pub fn profile_dirs() -> Vec<PathBuf> {
    let dir = super::config::codex_dir();
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let hidden = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'));
            if !hidden && path.join("auth.json").is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn emails_match(left: &str, right: &str) -> bool {
    !left.is_empty() && left.eq_ignore_ascii_case(right)
}

/// Overwrite stored copies of the live CLI account with its current `auth.json`.
/// Codex refresh tokens are single-use — a forked copy would invalidate the CLI
/// (or go stale) unless it stays on the same lineage while that account is live.
pub fn sync_live_profiles() {
    let live = default_codex_home();
    let live_email = email_from(&live);
    if live_email.is_empty() {
        return;
    }
    for path in profile_dirs() {
        if emails_match(&email_from(&path), &live_email) {
            if let Ok(auth) = read_json(&live.join("auth.json")) {
                let _ = write_json(&path.join("auth.json"), &auth);
            }
        }
    }
}

fn live_present() -> bool {
    let live = default_codex_home();
    live.join("auth.json").is_file() || live.join("sessions").is_dir()
}

/// `(account name, CODEX_HOME)` pairs to collect. A registered profile whose
/// email matches the live CLI is collected from the live home so token refresh
/// hits the CLI's `auth.json`.
pub fn targets() -> Vec<(String, PathBuf)> {
    sync_live_profiles();
    let live = default_codex_home();
    let live_email = email_from(&live);
    let mut jobs = Vec::new();
    let mut live_used = false;
    for path in profile_dirs() {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("codex")
            .to_string();
        if emails_match(&email_from(&path), &live_email) {
            if !live_used {
                jobs.push((name, live.clone()));
                live_used = true;
            }
        } else {
            jobs.push((name, path));
        }
    }
    if !live_used && (jobs.is_empty() || live_present()) {
        jobs.push((CODEX_LIVE_ACCOUNT.to_string(), live));
    }
    jobs
}

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
