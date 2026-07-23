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
