//! Native collector — a port of `usage_monitor.py` into the Tauri app.
//! Each `collect_*` captures its own failure and returns a `Provider` with
//! `error` filled in; it never propagates. Serialization mirrors the Python
//! `asdict` so the frontend (`src/main.js`) consumes it unchanged.

pub mod config;
mod claude;
mod codex;
mod cursor;
mod date;

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

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
            if path.join(".credentials.json").exists() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Provisional standby (phase 1): reads the active email from the Claude Code
/// CLI, like Python does. Phase 4 replaces this with detection via usage delta
/// between reads.
fn mark_standby(providers: &mut [Provider]) {
    if providers.iter().filter(|p| p.name == "Claude").count() < 2 {
        return;
    }
    let active = active_claude_email();
    if active.is_empty() {
        return;
    }
    for provider in providers.iter_mut().filter(|p| p.name == "Claude") {
        provider.standby = provider.email.to_lowercase() != active;
    }
}

fn active_claude_email() -> String {
    config::read_json(&config::claude_active_file())
        .ok()
        .and_then(|data| {
            data.get("oauthAccount")
                .and_then(|account| account.get("emailAddress"))
                .and_then(|value| value.as_str())
                .map(|email| email.to_lowercase())
        })
        .unwrap_or_default()
}
