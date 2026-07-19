# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Language

Comments, user-visible strings, error messages, and docs are in **English**. Keep that standard when editing.

## Commands

The collector is a single Python script with no external dependencies (stdlib only), at `cli/usage_monitor.py`. Installed as `ai-usage` (symlink `~/.local/bin/ai-usage` → `cli/usage_monitor.py`).

```bash
python3 cli/usage_monitor.py doctor    # check the setup (Claude profiles, Codex CLI, Cursor)
python3 cli/usage_monitor.py once      # one reading as text; --json for JSON
python3 cli/usage_monitor.py watch     # open the curses TUI (--interval N, --alert N)
```

There is no test suite, linter, or build for the Python script — it runs directly.

### Tauri widget (`widget/`)

The build **must run on the Windows filesystem** (cargo fails under `\\wsl.localhost`). It does not compile from WSL. Flow (PowerShell, in the synced Windows workspace):

```powershell
npm install
npm run dev      # dev
npm run build    # release: .exe + NSIS installer in src-tauri\target\release\bundle\nsis\
```

Icons: `npx tauri icon path\to\icon.png`.

## Architecture

### Python collector (`cli/usage_monitor.py`) — credential setup + terminal dashboard

Registers the credentials that both collectors read, and renders the terminal dashboard. Flow:

- **Data model:** dataclasses `Provider` (name, account, plan, email, `standby`, list of `Meter`, `error`) and `Meter` (label, percent, reset_at, used, limit). Serialized via `asdict` for `--json`.
- **`collect_all()`** fans out with a `ThreadPoolExecutor` over: each Claude profile in `~/.config/ai-usage-monitor/claude/*/`, plus Codex and Cursor. Each `collect_*` **captures its own exceptions** and returns a `Provider` with `.error` filled in — it never propagates. Then `mark_standby()` compares the Claude emails against the CLI's active account (`~/.claude.json`) and flags `◉ STANDBY` on the one not logged in.
- **Credential setup — Python only:** `claude-login`, `claude-add`, `claude-list`, `cursor-cookie`, `cursor-admin`. The Rust collector only *reads* the store and refreshes existing tokens; it cannot create a credential. So the Python script stays required for onboarding.
- **Claude:** OAuth with automatic token refresh (`refresh_claude` renews when under 2 min remain), reads `oauth/usage` and `oauth/profile`. Multi-account: profiles isolated in subdirectories with `.credentials.json`.
- **Codex:** tries the `app-server` via JSON-RPC over stdio (`codex_live`); on failure falls back to the local session cache (`codex_cached` reads the last `token_count` in `~/.codex/sessions/*.jsonl`) and flags the downgrade in `details`. `codex_bin()` works around PATH: it ignores `/mnt/` shims (Windows) and looks up the Linux binary via nvm.
- **Cursor:** two methods in `cursor.json` — `admin_key` (team admin API, preferred) or `dashboard_cookie` (internal dashboard endpoint, may break if Cursor changes it). The team dashboard divides request units by 4 (`request_scale`).

### Rendering

- `plain()` — text output for `once`.
- `tui()` — curses TUI with a **background fetch thread** (the draw loop never blocks; spinner while fetching). Two layouts: cards (2 columns if width ≥96) and `draw_compact_tui` (aligned list) for small windows. Uses synchronized output DEC 2026 (`\x1b[?2026h/l`) to compose without tearing, and a `redrawwin` on resize to clear artifacts.
- **Alerts:** `alert_meters` with hysteresis — notifies once on crossing the threshold, re-arms only when usage drops back below. `notify_windows` fires a toast via `powershell.exe` (WinRT); no-op outside WSL.

### Tauri widget (`widget/`)

Native Tauri v2 + WebView2 app (Windows) mirroring the TUI's compact mode.

- **Native collector (`src-tauri/src/collector/`):** a Rust port of the Python collector — `claude.rs` (OAuth refresh + usage/profile), `codex.rs` (JSON-RPC against the `app-server`, via `codex.cmd` with `CREATE_NO_WINDOW`, falling back to the session cache), `cursor.rs` (admin_key/dashboard_cookie), `date.rs` (ISO-8601 without `chrono`), `config.rs` (store in `%USERPROFILE%\.config\ai-usage-monitor\`). It serializes exactly the same JSON as Python, so the frontend serves both. **There is no WSL bridge** — the app is self-contained. `--probe` prints the collection and exits, for diagnosis without the GUI.
- **Two implementations of the same contract:** when changing the shape of `Provider`/`Meter`, update the Python (`cli/usage_monitor.py`) and the Rust (`collector/mod.rs`) together. The same goes for user-visible meter labels and `details` strings, so both surfaces read alike.
- **Frontend (`src/main.js`, `src/index.html`):** vanilla JS, no framework, `withGlobalTauri`. Replicates the TUI's logic (per-provider colors, alert hysteresis). Tauri commands: `fetch_usage` (async, otherwise it freezes the UI), `hide_to_tray`, `frontend_ready`.
- **Window:** starts hidden (avoids the white flash), borderless, always-on-top, out of the taskbar, repositioned to the bottom-right corner on every show (no position persistence). Tray: left click toggles, right → Quit. Autostart and notifications only in release builds.

## Security

Tokens/cookies/keys never appear in the dashboard or in process arguments. Config directories `0700`, credential files `0600` (see `ensure_private_dir`/`write_private_json`). Never copy credentials into chats, issues, or commits.
