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
/// Primary signal: the account the Claude Code CLI is logged into, read from the
/// most recently touched `.claude.json` across environments. A single copy of
/// that file is not enough — Windows and WSL keep separate ones, and looking at
/// only the local side leaves the account of the *other* side wrongly unflagged.
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
    let emails = active_claude_emails();
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

/// Accounts the CLI is logged into right now, lowercased.
///
/// Each environment keeps its own `.claude.json`, so the file touched last is
/// the one describing the session in use. Anything touched within
/// `CLAUDE_CONFIG_TIE_SECONDS` of it counts too: with a CLI open on each side,
/// both accounts really are burning quota, and without the tolerance the badge
/// would bounce between them as one file or the other gets rewritten.
/// The file is rewritten on every CLI run, so one untouched for longer than a
/// session window says nothing about what is running now — it is dropped, and
/// the caller falls back to the meters.
fn active_claude_emails() -> HashSet<String> {
    let mut found: Vec<(SystemTime, String)> = Vec::new();
    for path in claude_config_paths() {
        let Ok(stamp) = fs::metadata(&path).and_then(|meta| meta.modified()) else {
            continue;
        };
        // A clock skew that dates the file in the future counts as fresh.
        if SystemTime::now()
            .duration_since(stamp)
            .is_ok_and(|age| age > Duration::from_secs(CLAUDE_SESSION_SECONDS))
        {
            continue;
        }
        let Ok(data) = config::read_json(&path) else { continue };
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

/// `.claude.json` inside each `.claude*` dir under `base`.
///
/// A CLI running with a custom `CLAUDE_CONFIG_DIR` keeps its copy *inside*
/// that dir (the default lives next to `~/.claude`, not in it) — the
/// `~/.claude*` naming is the discoverable convention for those setups.
/// Mirrors `custom_dir_claude_configs` in `usage_monitor.py`.
fn custom_dir_claude_configs(base: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            let path = entry.path();
            let named_claude = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".claude"));
            if named_claude && path.is_dir() {
                out.push(path.join(".claude.json"));
            }
        }
    }
    out.sort();
    out
}

/// Every `.claude.json` the CLI may have written on this machine, on both sides
/// of WSL — the answer has to be the same from Windows or Linux. macOS has no
/// second side, so there the local files are the whole story.
fn claude_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        paths.push(PathBuf::from(dir).join(".claude.json"));
    }
    let home = config::home();
    paths.push(home.join(".claude.json"));
    paths.extend(custom_dir_claude_configs(&home));
    paths.extend(other_side_claude_configs());
    paths.dedup();
    paths
}

/// The CLI configs inside WSL, seen from Windows. Only *running* distros are
/// listed: reaching into `\\wsl.localhost\<name>` of a stopped one would boot
/// its VM on every refresh. `wsl.exe --list` reads the registry, boots nothing,
/// and answers in ~200 ms.
#[cfg(windows)]
fn other_side_claude_configs() -> Vec<PathBuf> {
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
        paths.push(root.join("root").join(".claude.json"));
        paths.extend(custom_dir_claude_configs(&root.join("root")));
        if let Ok(entries) = fs::read_dir(root.join("home")) {
            for entry in entries.flatten() {
                paths.push(entry.path().join(".claude.json"));
                paths.extend(custom_dir_claude_configs(&entry.path()));
            }
        }
    }
    paths
}

/// The CLI configs on the Windows profiles, seen from WSL through /mnt.
#[cfg(target_os = "linux")]
fn other_side_claude_configs() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let Ok(drives) = fs::read_dir("/mnt") else {
        return paths;
    };
    for drive in drives.flatten() {
        if let Ok(users) = fs::read_dir(drive.path().join("Users")) {
            for user in users.flatten() {
                paths.push(user.path().join(".claude.json"));
                paths.extend(custom_dir_claude_configs(&user.path()));
            }
        }
    }
    paths
}

/// macOS (and any other Unix): only the local config exists.
#[cfg(not(any(windows, target_os = "linux")))]
fn other_side_claude_configs() -> Vec<PathBuf> {
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
