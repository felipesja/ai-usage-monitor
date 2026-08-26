//! Location and I/O for the credential store, mirroring the Python collector's
//! layout: `~/.config/ai-usage-monitor/`. On Unix, writes keep Python's
//! 0600/0700 permissions (`ensure_private_dir`/`write_private_json`); on
//! Windows they are a no-op and we rely on the user profile's ACL.

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

pub fn grok_home() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".grok"))
}

pub fn grok_auth() -> PathBuf {
    grok_home().join("auth.json")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

/// Marks that the user removed Codex from the widget's Accounts view. Codex
/// owns no credential this app writes — `codex logout` clears its CLI auth,
/// but the local session cache can still make `collect()` succeed against
/// stale data. This marker suppresses the provider until a fresh `codex
/// login` produces live data again, at which point it is cleared automatically.
pub fn codex_removed_marker() -> PathBuf {
    config_dir().join("codex-removed")
}

/// Percentages at which a limit fires a notification. Mirrors
/// `DEFAULT_ALERT_THRESHOLDS` in `cli/usage_monitor.py`.
pub fn default_alert_thresholds() -> Vec<f64> {
    vec![80.0, 90.0, 95.0, 98.0, 100.0]
}

/// Ints in 1..=100, unique, ascending; the defaults when absent or unusable, so
/// a hand-edited config that goes wrong still notifies instead of going silent.
fn normalize_thresholds(value: Option<&Value>) -> Vec<f64> {
    let Some(items) = value.and_then(Value::as_array) else {
        return default_alert_thresholds();
    };
    let mut levels: Vec<i64> = items
        .iter()
        .filter(|v| !v.is_boolean())
        .filter_map(Value::as_f64)
        .map(|n| n.round() as i64)
        .filter(|n| (1..=100).contains(n))
        .collect();
    levels.sort_unstable();
    levels.dedup();
    if levels.is_empty() {
        default_alert_thresholds()
    } else {
        levels.into_iter().map(|n| n as f64).collect()
    }
}

/// Notification levels from config.json, or the defaults if absent/broken.
/// Writes the default file when missing so the user has one to edit — mirrors
/// `load_alert_thresholds` + `ensure_config_file` in the Python collector.
pub fn alert_thresholds() -> Vec<f64> {
    let path = config_file();
    if !path.exists() {
        let defaults = default_alert_thresholds();
        let json = serde_json::json!({ "alert_thresholds": defaults });
        let _ = write_json(&path, &json);
        return defaults;
    }
    match read_json(&path) {
        Ok(value) => normalize_thresholds(value.get("alert_thresholds")),
        Err(_) => default_alert_thresholds(),
    }
}

pub fn read_json(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    serde_json::from_str(&text).map_err(|err| err.to_string())
}

pub fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let text = serde_json::to_string_pretty(value).map_err(|err| err.to_string())?;
    #[cfg(unix)]
    {
        // 0600 from creation — no window where the credentials are readable.
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        file.write_all((text + "\n").as_bytes()).map_err(|err| err.to_string())?;
        // `mode` only applies on create; tighten files the Python CLI made too.
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, text + "\n").map_err(|err| err.to_string())
    }
}
