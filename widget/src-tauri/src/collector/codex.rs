//! Codex limits. Tries the `app-server` via JSON-RPC over stdio and, on
//! failure, falls back to the local session cache — flagging the downgrade in
//! `details`, like the Python collector.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::Value;

use super::config::{home, read_json};
use super::date::epoch_to_iso;
use super::{Meter, Provider};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn collect() -> Provider {
    let mut downgrade = None;
    let raw = match live() {
        Ok(value) => value,
        Err(err) => match cached() {
            Ok(value) => {
                downgrade = Some(err);
                value
            }
            Err(cache_err) => {
                return Provider::with_error("Codex", "ChatGPT", format!("{err}; cache: {cache_err}"))
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
    let mut provider = Provider::new("Codex", "ChatGPT", &title_case(&plan), &email());
    if let Some(err) = downgrade {
        provider.details.push(format!("⚠ local cache · app-server: {err}"));
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

/// The npm shim on Windows (`codex.cmd`) or the native binary installed inside
/// the npm package. Recent npm versions can leave only `codex`/`codex.ps1` in
/// the global bin directory, neither of which a GUI process can execute
/// reliably with its reduced PATH.
fn codex_command() -> Result<Command, String> {
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

fn live() -> Result<Value, String> {
    let mut command = codex_command()?;
    command
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
fn cached() -> Result<Value, String> {
    let dir = home().join(".codex").join("sessions");
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

/// Email from the `id_token` claims in `~/.codex/auth.json`.
fn email() -> String {
    let Ok(auth) = read_json(&home().join(".codex").join("auth.json")) else {
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
