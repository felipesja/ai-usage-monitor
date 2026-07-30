//! Native collector — a port of `usage_monitor.py` into the Tauri app.
//! Each `collect_*` captures its own failure and returns a `Provider` with
//! `error` filled in; it never propagates. Serialization mirrors the Python
//! `asdict` so the frontend (`src/main.js`) consumes it unchanged.

pub mod config;
pub mod claude;
mod codex;
mod cursor;
mod date;

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::Serialize;

/// The 5h quota window, also the horizon for trusting a `.claude.json` mtime.
const CLAUDE_SESSION_SECONDS: u64 = 5 * 3600;
/// Environments whose configs are this close apart are both taken as in use.
const CLAUDE_CONFIG_TIE_SECONDS: u64 = 600;
/// An external activity hint remains relevant for the current quota window.
const CLAUDE_ACTIVITY_HINT_SECONDS: u64 = CLAUDE_SESSION_SECONDS;

#[derive(Serialize, Clone)]
pub struct Meter {
    pub label: String,
    pub percent: Option<f64>,
    pub reset_at: Option<String>,
    pub used: Option<String>,
    pub limit: Option<String>,
}

impl Meter {
    pub fn new(label: &str, percent: Option<f64>, reset_at: Option<String>) -> Self {
        Self {
            label: label.to_string(),
            percent,
            reset_at,
            used: None,
            limit: None,
        }
    }
}

#[derive(Serialize, Clone)]
pub struct Provider {
    pub name: String,
    pub account: String,
    pub plan: String,
    pub email: String,
    pub standby: bool,
    pub meters: Vec<Meter>,
    pub details: Vec<String>,
    pub error: Option<String>,
}

impl Provider {
    pub fn new(name: &str, account: &str, plan: &str, email: &str) -> Self {
        Self {
            name: name.to_string(),
            account: account.to_string(),
            plan: plan.to_string(),
            email: email.to_string(),
            standby: false,
            meters: Vec::new(),
            details: Vec::new(),
            error: None,
        }
    }

    pub fn with_error(name: &str, account: &str, error: String) -> Self {
        let mut provider = Self::new(name, account, "", "");
        provider.error = Some(error);
        provider
    }
}

/// HTTP client shared by the collectors (short timeout, TLS via rustls).
pub fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|err| err.to_string())
}

pub fn collect_all() -> Vec<Provider> {
    let profiles = claude_profiles();
    let mut providers: Vec<Provider> = Vec::new();

    std::thread::scope(|scope| {
        let claude_handles: Vec<_> = profiles
            .iter()
            .map(|dir| scope.spawn(move || claude::collect(dir)))
            .collect();
        let codex = scope.spawn(codex::collect);
        let cursor = scope.spawn(cursor::collect);

        for handle in claude_handles {
            providers.push(handle.join().expect("Claude collector thread panicked"));
        }
        providers.push(codex.join().expect("Codex collector thread panicked"));
        providers.push(cursor.join().expect("Cursor collector thread panicked"));
    });

    mark_standby(&mut providers);
    providers
}

fn claude_profiles() -> Vec<PathBuf> {
    let dir = config::claude_dir();
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            // Skip dotted dirs — the accounts UI stages credentials in a
            // transient `.staging-<pid>` that is not a profile.
            let hidden = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'));
            if !hidden && path.join(".credentials.json").exists() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Flag the Claude accounts that are not the one in use.
///
/// An optional external activity hint takes precedence when present. This lets
/// launchers or routers report the upstream account without making them a
/// dependency of the app.
///
/// Without a fresh hint, use the account the Claude Code CLI is logged into,
/// read from the most recently touched `.claude.json` across environments.
/// A single copy of that file is not enough — Windows and WSL keep separate
/// ones, and looking at only the local side leaves the account of the *other*
/// side wrongly unflagged.
/// When no `.claude.json` names a known account (usage driven from claude.ai or
/// the desktop app), fall back to the session window: an account whose 5h window
/// already rolled over cannot be the one burning quota. If nothing distinguishes
/// the accounts, nothing is flagged. Mirrors `mark_standby` in
/// `cli/usage_monitor.py`.
fn mark_standby(providers: &mut [Provider]) {
    let claude: Vec<&Provider> = providers
        .iter()
        .filter(|p| p.name == "Claude" && p.error.is_none())
        .collect();
    if claude.len() < 2 {
        return;
    }
    let mut emails = external_active_claude_emails();
    if emails.is_empty() {
        emails = active_claude_emails();
    }
    let mut in_use: HashSet<String> = claude
        .iter()
        .filter(|p| emails.contains(&p.email.to_lowercase()))
        .map(|p| p.account.clone())
        .collect();
    if in_use.is_empty() {
        in_use = claude
            .iter()
            .filter(|p| session_active(p))
            .map(|p| p.account.clone())
            .collect();
    }
    if in_use.is_empty() || in_use.len() == claude.len() {
        return;
    }
    for provider in providers
        .iter_mut()
        .filter(|p| p.name == "Claude" && p.error.is_none())
    {
        provider.standby = !in_use.contains(&provider.account);
    }
}

/// An optional active-account hint supplied by an external tool.
///
/// The app does not require or create this file. Standard Claude Code setups
/// continue to use `active_claude_emails`.
fn external_active_claude_emails() -> HashSet<String> {
    let path = config::config_dir().join("claude-active-account.json");
    let Ok(data) = config::read_json(&path) else {
        return HashSet::new();
    };
    let updated_at = data
        .get("updated_at")
        .and_then(|value| value.as_i64())
        .unwrap_or_default();
    let age = now_epoch().saturating_sub(updated_at);
    if updated_at <= 0 || age > CLAUDE_ACTIVITY_HINT_SECONDS as i64 {
        return HashSet::new();
    }
    let email = data
        .get("email")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if email.is_empty() {
        HashSet::new()
    } else {
        HashSet::from([email])
    }
}

/// Accounts the CLI is logged into right now, lowercased.
///
/// Each environment's freshness is the newest of its `.claude.json` (rewritten
/// on session events) and its `history.jsonl` (appended on every prompt) — the
/// config alone goes untouched during a long-running session, which would make
/// an actively working account look idle. The freshest environment names the
/// session in use; anything within `CLAUDE_CONFIG_TIE_SECONDS` of it counts
/// too: with a CLI working on each side, both accounts really are burning
/// quota, and without the tolerance the badge would bounce between them.
/// An environment untouched for longer than a session window says nothing
/// about what is running now — it is dropped, and the caller falls back to
/// the meters.
fn active_claude_emails() -> HashSet<String> {
    let mut found: Vec<(SystemTime, String)> = Vec::new();
    for (config, history) in claude_config_sources() {
        let Ok(mut stamp) = fs::metadata(&config).and_then(|meta| meta.modified()) else {
            continue;
        };
        if let Ok(activity) = fs::metadata(&history).and_then(|meta| meta.modified()) {
            stamp = stamp.max(activity);
        }
        // A clock skew that dates the file in the future counts as fresh.
        if SystemTime::now()
            .duration_since(stamp)
            .is_ok_and(|age| age > Duration::from_secs(CLAUDE_SESSION_SECONDS))
        {
            continue;
        }
        let Ok(data) = config::read_json(&config) else { continue };
        let email = data
            .get("oauthAccount")
            .and_then(|account| account.get("emailAddress"))
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        if !email.is_empty() {
            found.push((stamp, email.to_lowercase()));
        }
    }
    let Some(newest) = found.iter().map(|(stamp, _)| *stamp).max() else {
        return HashSet::new();
    };
    let cutoff = newest
        .checked_sub(Duration::from_secs(CLAUDE_CONFIG_TIE_SECONDS))
        .unwrap_or(newest);
    found
        .into_iter()
        .filter(|(stamp, _)| *stamp >= cutoff)
        .map(|(_, email)| email)
        .collect()
}

/// A (config, history) pair: `.claude.json` carries the account, the dir's
/// `history.jsonl` carries liveness.
type ClaudeSource = (PathBuf, PathBuf);

/// The default layout: `.claude.json` next to the `~/.claude` dir.
fn default_claude_source(home: &std::path::Path) -> ClaudeSource {
    (home.join(".claude.json"), home.join(".claude").join("history.jsonl"))
}

/// (config, history) inside each `.claude*` dir under `base`.
///
/// A CLI running with a custom `CLAUDE_CONFIG_DIR` keeps both files *inside*
/// that dir (the default layout keeps them next to `~/.claude` / in it) — the
/// `~/.claude*` naming is the discoverable convention for those setups.
/// Mirrors `custom_dir_claude_configs` in `usage_monitor.py`.
fn custom_dir_claude_configs(base: &std::path::Path) -> Vec<ClaudeSource> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            let path = entry.path();
            let named_claude = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".claude"));
            if named_claude && path.is_dir() {
                out.push((path.join(".claude.json"), path.join("history.jsonl")));
            }
        }
    }
    out.sort();
    out
}

/// Every (`.claude.json`, `history.jsonl`) pair the CLI may have written on
/// this machine, on both sides of WSL — the answer has to be the same from
/// Windows or Linux. macOS has no second side, so there the local sources are
/// the whole story.
fn claude_config_sources() -> Vec<ClaudeSource> {
    let mut sources = Vec::new();
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        let dir = PathBuf::from(dir);
        sources.push((dir.join(".claude.json"), dir.join("history.jsonl")));
    }
    let home = config::home();
    sources.push(default_claude_source(&home));
    sources.extend(custom_dir_claude_configs(&home));
    sources.extend(other_side_claude_configs());
    sources.dedup();
    sources
}

/// The CLI configs inside WSL, seen from Windows. Only *running* distros are
/// listed: reaching into `\\wsl.localhost\<name>` of a stopped one would boot
/// its VM on every refresh. `wsl.exe --list` reads the registry, boots nothing,
/// and answers in ~200 ms.
#[cfg(windows)]
fn other_side_claude_configs() -> Vec<ClaudeSource> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(output) = std::process::Command::new("wsl.exe")
        .args(["--list", "--running", "--quiet"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    else {
        return Vec::new();
    };
    // wsl.exe writes UTF-16LE.
    let units: Vec<u16> = output
        .stdout
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let listing = String::from_utf16_lossy(&units);

    let mut paths = Vec::new();
    for name in listing.lines().map(str::trim).filter(|name| !name.is_empty()) {
        let root = PathBuf::from(format!(r"\\wsl.localhost\{name}"));
        paths.push(default_claude_source(&root.join("root")));
        paths.extend(custom_dir_claude_configs(&root.join("root")));
        if let Ok(entries) = fs::read_dir(root.join("home")) {
            for entry in entries.flatten() {
                paths.push(default_claude_source(&entry.path()));
                paths.extend(custom_dir_claude_configs(&entry.path()));
            }
        }
    }
    paths
}

/// The CLI configs on the Windows profiles, seen from WSL through /mnt.
#[cfg(target_os = "linux")]
fn other_side_claude_configs() -> Vec<ClaudeSource> {
    let mut paths = Vec::new();
    let Ok(drives) = fs::read_dir("/mnt") else {
        return paths;
    };
    for drive in drives.flatten() {
        if let Ok(users) = fs::read_dir(drive.path().join("Users")) {
            for user in users.flatten() {
                paths.push(default_claude_source(&user.path()));
                paths.extend(custom_dir_claude_configs(&user.path()));
            }
        }
    }
    paths
}

/// macOS (and any other Unix): only the local sources exist.
#[cfg(not(any(windows, target_os = "linux")))]
fn other_side_claude_configs() -> Vec<ClaudeSource> {
    Vec::new()
}

/// Whether the account has a live 5h session window — i.e. it burned quota
/// recently enough that the window has not rolled over yet.
fn session_active(provider: &Provider) -> bool {
    let Some(meter) = provider.meters.iter().find(|m| m.label == "Session") else {
        return false;
    };
    if meter.percent.unwrap_or(0.0) <= 0.0 {
        return false;
    }
    match meter.reset_at.as_deref().and_then(date::iso_to_epoch) {
        Some(reset) => reset > now_epoch(),
        None => true,
    }
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}
