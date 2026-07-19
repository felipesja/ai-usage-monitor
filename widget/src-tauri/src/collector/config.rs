//! Location and I/O for the credential store, mirroring the Python collector's
//! layout: `%USERPROFILE%\.config\ai-usage-monitor\`. Python's 0600/0700
//! permissions are a no-op on Windows; here we rely on the user profile's ACL.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

pub fn home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

pub fn config_dir() -> PathBuf {
    home().join(".config").join("ai-usage-monitor")
}

pub fn claude_dir() -> PathBuf {
    config_dir().join("claude")
}

pub fn cursor_config() -> PathBuf {
    config_dir().join("cursor.json")
}

/// The Claude Code CLI's `~/.claude.json` — used (temporarily) to know which
/// account is active. Goes away once dynamic standby lands (phase 4).
pub fn claude_active_file() -> PathBuf {
    home().join(".claude.json")
}

pub fn read_json(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    serde_json::from_str(&text).map_err(|err| err.to_string())
}

pub fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let text = serde_json::to_string_pretty(value).map_err(|err| err.to_string())?;
    fs::write(path, text + "\n").map_err(|err| err.to_string())
}
