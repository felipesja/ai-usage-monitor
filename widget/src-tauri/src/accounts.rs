//! Account registration from the widget UI — detects Claude credentials on the
//! machine (macOS Keychain entries, `~/.claude*` credential files), registers
//! them as profiles in the shared store, and manages the Cursor config. The
//! Python CLI (`claude-add`, `cursor-admin`, `cursor-cookie`) remains the
//! headless path; both write the same store.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::collector::claude;
use crate::collector::config::{claude_dir, cursor_config, home, read_json, write_json};

#[derive(Serialize)]
pub struct Candidate {
    pub id: String,
    pub label: String,
}

#[derive(Serialize)]
pub struct Detection {
    pub claude: Vec<Candidate>,
    pub cursor_configured: bool,
}

/// Credential sources found on the machine. Listing is metadata-only — secrets
/// are read on registration, which is when macOS asks the user for permission.
pub fn detect() -> Detection {
    let mut claude_candidates = Vec::new();
    #[cfg(target_os = "macos")]
    claude_candidates.extend(keychain_candidates());
    claude_candidates.extend(file_candidates());
    Detection {
        claude: claude_candidates,
        cursor_configured: cursor_config().exists(),
    }
}

/// Keychain services named `Claude Code-credentials[-<hash>]` — one per Claude
/// Code login: the default config dir, plus one per custom CLAUDE_CONFIG_DIR
/// (suffixed with the first 8 hex chars of the dir path's sha256).
#[cfg(target_os = "macos")]
fn keychain_candidates() -> Vec<Candidate> {
    let Ok(output) = std::process::Command::new("security").arg("dump-keychain").output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut services: Vec<String> = text
        .lines()
        .filter_map(|line| {
            let value = line.trim().strip_prefix("\"svce\"<blob>=\"")?.strip_suffix('"')?;
            value.starts_with("Claude Code-credentials").then(|| value.to_string())
        })
        .collect();
    services.sort();
    services.dedup();
    services
        .into_iter()
        .map(|service| {
            let label = match service.strip_prefix("Claude Code-credentials-") {
                Some(suffix) => format!("Keychain · {suffix}"),
                None => "Keychain · default".to_string(),
            };
            Candidate { id: format!("keychain:{service}"), label }
        })
        .collect()
}

/// `~/.claude*/.credentials.json` files — the storage on Linux/Windows; on
/// macOS usually stale copies, hence listed after the Keychain entries.
fn file_candidates() -> Vec<Candidate> {
    let home = home();
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&home) else { return out };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".claude"))
                && path.join(".credentials.json").is_file()
        })
        .collect();
    dirs.sort();
    for dir in dirs {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or(".claude").to_string();
        out.push(Candidate {
            id: format!("file:{}", dir.join(".credentials.json").display()),
            label: format!("File · ~/{name}"),
        });
    }
    out
}

#[derive(Serialize)]
pub struct Registered {
    pub profile: String,
    pub email: String,
    pub plan: String,
    /// The account was already registered; `profile` names the existing one.
    pub already: bool,
}

pub fn add_claude(id: &str) -> Result<Registered, String> {
    let data = read_source(id)?;
    let oauth = data
        .get("claudeAiOauth")
        .ok_or("the source does not contain a Claude OAuth session")?;
    if oauth.get("accessToken").and_then(Value::as_str).is_none()
        || oauth.get("refreshToken").and_then(Value::as_str).is_none()
    {
        return Err("the source does not contain a complete Claude OAuth session".into());
    }

    // Stage the credential so identify() can persist a rotated token, then
    // name the profile after the account it turns out to be.
    let staging = claude_dir().join(format!(".staging-{}", std::process::id()));
    let staged = staging.join(".credentials.json");
    write_json(&staged, &data)?;
    let (email, plan) = match claude::identify(&staged) {
        Ok(identity) => identity,
        Err(err) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(err);
        }
    };

    // One profile per account: an existing profile with the same email wins —
    // its refresh-token lineage keeps working; the staged copy is dropped.
    for dir in profiles() {
        if let Ok((existing, _)) = claude::identify(&dir.join(".credentials.json")) {
            if existing.eq_ignore_ascii_case(&email) {
                let _ = fs::remove_dir_all(&staging);
                let profile =
                    dir.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
                return Ok(Registered { profile, email, plan, already: true });
            }
        }
    }

    let name = profile_name(&email);
    let target = claude_dir().join(&name);
    fs::rename(&staging, &target).map_err(|err| err.to_string())?;
    Ok(Registered { profile: name, email, plan, already: false })
}

pub fn remove_claude(profile: &str) -> Result<(), String> {
    if profile.is_empty()
        || profile.starts_with('.')
        || profile.contains('/')
        || profile.contains('\\')
        || profile.contains("..")
    {
        return Err("invalid profile name".into());
    }
    let dir = claude_dir().join(profile);
    if !dir.join(".credentials.json").is_file() {
        return Err(format!("profile not found: {profile}"));
    }
    fs::remove_dir_all(&dir).map_err(|err| err.to_string())
}

/// Mirrors the Python CLI's cursor-admin / cursor-cookie validations and file
/// format (`cursor.json`).
pub fn save_cursor(method: &str, secret: &str, email: &str) -> Result<(), String> {
    let secret = secret.trim();
    match method {
        "admin_key" => {
            if !secret.starts_with("key_") {
                return Err("the key must start with key_".into());
            }
            let email = email.trim();
            if email.is_empty() {
                return Err("email is required with the admin key".into());
            }
            write_json(
                &cursor_config(),
                &serde_json::json!({"method": "admin_key", "admin_key": secret, "email": email}),
            )
        }
        "dashboard_cookie" => {
            let value = secret.strip_prefix("WorkosCursorSessionToken=").unwrap_or(secret);
            if value.len() < 100 || !(value.contains("%3A%3A") || value.contains("::")) {
                return Err(
                    "the cookie does not look like a complete WorkosCursorSessionToken".into()
                );
            }
            write_json(
                &cursor_config(),
                &serde_json::json!({"method": "dashboard_cookie", "session_cookie": value}),
            )
        }
        _ => Err(format!("unknown method: {method}")),
    }
}

pub fn remove_cursor() -> Result<(), String> {
    fs::remove_file(cursor_config()).map_err(|err| err.to_string())
}

fn read_source(id: &str) -> Result<Value, String> {
    if let Some(path) = id.strip_prefix("file:") {
        return read_json(Path::new(path));
    }
    #[cfg(target_os = "macos")]
    if let Some(service) = id.strip_prefix("keychain:") {
        if !service.starts_with("Claude Code-credentials") {
            return Err("unexpected keychain service".into());
        }
        let output = std::process::Command::new("security")
            .args(["find-generic-password", "-s", service, "-w"])
            .output()
            .map_err(|err| err.to_string())?;
        if !output.status.success() {
            return Err("Keychain read failed or was denied".into());
        }
        return serde_json::from_slice(&output.stdout).map_err(|err| err.to_string());
    }
    Err(format!("unknown source: {id}"))
}

fn profiles() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(claude_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join(".credentials.json").is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Sanitized email local part, suffixed on collision (same email never
/// collides — that is deduplicated before naming).
fn profile_name(email: &str) -> String {
    let local = email.split('@').next().unwrap_or("claude").to_lowercase();
    let base: String = local
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    let base = base.trim_matches(|c| c == '-' || c == '.').to_string();
    let base = if base.is_empty() { "claude".to_string() } else { base };
    let mut name = base.clone();
    let mut counter = 2;
    while claude_dir().join(&name).exists() {
        name = format!("{base}-{counter}");
        counter += 1;
    }
    name
}
