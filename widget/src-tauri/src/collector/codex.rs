//! Limites do Codex. Tenta o `app-server` via JSON-RPC sobre stdio e, em caso
//! de falha, cai para o cache local das sessões — sinalizando o downgrade em
//! `details`, como o coletor Python.

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
        provider.details.push(format!("⚠ cache local · app-server: {err}"));
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
            // Epoch em segundos vira ISO-8601 UTC (o frontend faz `new Date`).
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
            provider.details.push(format!("Créditos: {balance}"));
        }
    }
    provider
}

/// Shim do npm no Windows (`codex.cmd`) ou binário no PATH. O `.cmd` não pode
/// ser executado direto pelo `Command`; precisa passar pelo `cmd.exe /C`.
fn codex_command() -> Result<Command, String> {
    #[cfg(windows)]
    {
        let shim = home().join("AppData").join("Roaming").join("npm").join("codex.cmd");
        if shim.exists() {
            let mut command = Command::new("cmd.exe");
            command.arg("/C").arg(shim);
            return Ok(command);
        }
        Ok(Command::new("codex.exe"))
    }
    #[cfg(not(windows))]
    {
        Ok(Command::new("codex"))
    }
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
    let mut child = command.spawn().map_err(|err| format!("app-server não iniciou: {err}"))?;

    let stdout = child.stdout.take().ok_or("app-server sem stdout")?;
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
        let stdin = child.stdin.as_mut().ok_or("app-server sem stdin")?;
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
        wait_for(&receiver, 1, Duration::from_secs(5)).map_err(|err| format!("{err} (fase init)"))?;

        writeln!(stdin, "{}", r#"{"method":"initialized"}"#).map_err(|err| err.to_string())?;
        writeln!(stdin, "{}", r#"{"method":"account/rateLimits/read","id":2}"#)
            .map_err(|err| err.to_string())?;
        stdin.flush().map_err(|err| err.to_string())?;
        wait_for(&receiver, 2, Duration::from_secs(15)).map_err(|err| format!("{err} (fase rateLimits)"))
    })();

    terminate(&mut child);
    result
}

fn wait_for(receiver: &mpsc::Receiver<Value>, id: i64, timeout: Duration) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("Codex não respondeu a tempo".into());
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
            Err(_) => return Err("Codex não respondeu a tempo".into()),
        }
    }
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Último `token_count` com `rate_limits` na sessão mais recente.
fn cached() -> Result<Value, String> {
    let dir = home().join(".codex").join("sessions");
    let latest = newest_jsonl(&dir).ok_or("nenhuma sessão local encontrada")?;
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
    let limits = found.ok_or("nenhum limite foi encontrado nas sessões locais")?;
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

/// E-mail a partir das claims do `id_token` em `~/.codex/auth.json`.
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
