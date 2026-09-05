# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Language

Comments, user-visible strings, error messages, and docs are in **English**. Keep that standard when editing.

## Commands

The collector is a single Python script with no external dependencies (stdlib only), at `cli/usage_monitor.py`. Installed as `ai-usage` (symlink `~/.local/bin/ai-usage` → `cli/usage_monitor.py`).

```bash
python3 cli/usage_monitor.py doctor    # check the setup (Claude profiles, Codex CLI, Cursor, Grok)
python3 cli/usage_monitor.py once      # one reading as text; --json for JSON
python3 cli/usage_monitor.py watch     # open the curses TUI (--interval N, --alert N)
```

There is no test suite, linter, or build for the Python script — it runs directly.

### Tauri widget (`widget/`)

On **Windows**, the build must run on the Windows filesystem (cargo fails under `\\wsl.localhost`) — it does not compile from WSL. Flow (PowerShell, in the synced Windows workspace):

```powershell
npm install
npm run dev      # dev
npm run build    # release: .exe + NSIS installer in src-tauri\target\release\bundle\nsis\
```

On **macOS**, the same `npm` flow builds natively (requires Xcode CLT, Rust, Node); `npm run build` produces the `.app` in `src-tauri/target/release/bundle/macos/` and a `.dmg` in `.../bundle/dmg/`.

Icons: `npx tauri icon path/to/icon.png` (`icons/tray-macos.png`, the menu-bar template icon, is maintained by hand).

## Architecture

### Python collector (`cli/usage_monitor.py`) — credential setup + terminal dashboard

Registers the credentials that both collectors read, and renders the terminal dashboard. Flow:

- **Data model:** dataclasses `Provider` (name, account, plan, email, `standby`, list of `Meter`, `error`) and `Meter` (label, percent, reset_at, used, limit). Serialized via `asdict` for `--json`.
- **`collect_all()`** fans out with a `ThreadPoolExecutor` over: each Claude profile in `~/.config/ai-usage-monitor/claude/*/`, each Codex target (stored profiles in `~/.config/ai-usage-monitor/codex/*/` plus the live CLI when it is a different account), plus Cursor and Grok. Each `collect_*` **captures its own exceptions** and returns a `Provider` with `.error` filled in — it never propagates. Then `mark_standby()` flags `◉ STANDBY` on the Claude accounts that are not the one in use. The signal is the CLI's `.claude.json` (`oauthAccount.emailAddress`), read from **every environment on the machine, not just the local one** — Windows and WSL keep separate copies (`claude_config_sources()` reaches into the running WSL distros from Windows via `\\wsl.localhost`, and into the Windows profiles from WSL via `/mnt`; macOS has no second side). Custom `CLAUDE_CONFIG_DIR` setups keep their copy *inside* the config dir, so every `~/.claude*` dir is scanned too (`custom_dir_claude_configs()`), locally and across environments. An environment's freshness is the newest of its `.claude.json` (rewritten on session events) **and its `history.jsonl`** (appended on every prompt) — a long-running session keeps the latter fresh while the config goes untouched; the freshest environment names the session in use. Copies within `CLAUDE_CONFIG_TIE_SECONDS` of the newest count as in use too — a CLI open on each side means both accounts are burning quota — and anything older than `CLAUDE_SESSION_SECONDS` is dropped as stale. When no `.claude.json` names a known account (usage driven from claude.ai or the desktop app), it falls back to `session_active()`: an account whose 5h window already rolled over cannot be the one burning quota. When every account looks active — or none does — nothing is flagged.
- **Credential setup:** `claude-login`, `claude-add`, `claude-list`, `codex-login`, `codex-add`, `codex-list`, `cursor-cookie`, `cursor-admin`. The widget's accounts view (`src-tauri/src/accounts.rs`) registers credentials too — both write the same store, so keep the file formats in sync; the Python commands remain the headless path.
- **Claude:** OAuth with automatic token refresh (`refresh_claude` renews when under 2 min remain), reads `oauth/usage` and `oauth/profile`. Multi-account: profiles isolated in subdirectories with `.credentials.json`.
- **Codex:** tries the `app-server` via JSON-RPC over stdio (`codex_live`); on failure falls back to the local session cache (`codex_cached` reads the last `token_count` in `$CODEX_HOME/sessions/*.jsonl`) and flags the downgrade in `details`. `codex_bin()` works around PATH: it ignores `/mnt/` shims (Windows) and looks up the Linux binary via nvm. Multi-account: extra sessions are stored as `codex/<name>/auth.json` with `cli_auth_credentials_store = "file"`. Each collect sets `CODEX_HOME` to that home. Refresh tokens are single-use, so a registered profile whose email matches the live CLI is collected from the live `~/.codex` (and the stored copy is overwritten from it) rather than from the fork. `mark_codex_standby()` flags the accounts that are not the live CLI login. Unregistered live sessions keep `account = "ChatGPT"`.
- **Cursor:** two methods in `cursor.json` — `admin_key` (team admin API, preferred) or `dashboard_cookie` (internal dashboard endpoint, may break if Cursor changes it). The dashboard-cookie path reads included usage in USD cents and displays it as dollars on the `Extra usage:` detail (the `Usage`/`Weekly` meters themselves show only percent + reset). Its `Usage` meter is **not** `plan.used`/`plan.limit` — those saturate at the included allowance, pinning an account with bonus credits at a permanent 100%; the real balance is `breakdown` (included + bonus) scaled by `totalPercentUsed` (`cursor_plan_usage` in Python, `plan_usage` in `cursor.rs`, same fallbacks on both sides), and whatever is spent past `breakdown.included` is reported as an `Extra usage:` detail under the bar. The cookie path also fetches `dashboard/get-filtered-usage-events` for the current billing cycle and derives a `Weekly` pace meter: cycle-aligned week (`week_index = floor((now - cycle_start) / 7d)`), budget = the limit's share of that window (`plan_total * window_days / cycle_days`), used = the window's raw `tokenUsage.totalCents` — before the plan overflows the limit is charged in those same raw model-cost units, so no quota-dollar conversion. The percent is **not** clamped: above 100% means burning faster than the cycle sustains. The endpoint requires an `Origin: https://cursor.com` header (403 otherwise) and is undocumented, so a failure silently drops the meter and leaves the monthly bar intact. `admin_key` accounts get no `Weekly` meter. The `admin_key` path keeps its plain `Spend: $x / $y` — its `monthlyLimitDollars` is a spend cap, not an included allowance.
- **Grok:** SuperGrok / Grok Build weekly credits from a `grok login` session in `~/.grok/auth.json` (or `$GROK_HOME`). Refreshes the OIDC token when under 2 min remain and writes the rotated pair back (refresh tokens are single-use). Reads `cli-chat-proxy.grok.com/v1/billing?format=credits` and the settings `subscription_tier_display` for the plan label. Hidden when there is no local session, like Codex. This is the consumer subscription, not xAI Console prepaid API credits.

### Rendering

- `plain()` — text output for `once`.
- `tui()` — curses TUI with a **background fetch thread** (the draw loop never blocks; spinner while fetching). Two layouts: cards (2 columns if width ≥96) and `draw_compact_tui` (aligned list) for small windows. Uses synchronized output DEC 2026 (`\x1b[?2026h/l`) to compose without tearing, and a `redrawwin` on resize to clear artifacts.
- **Alerts:** `alert_meters` notifies once as a limit rises through each level in `load_alert_thresholds()` (config.json `alert_thresholds`, default `[80, 90, 95, 98, 100]`; `--alert N` overrides with a single level, `--alert 0` disables). It keeps a per-meter high-water mark and re-arms a level only after usage falls `ALERT_REARM_MARGIN` points below it, so a meter parked on a boundary is not re-announced every refresh. `notify_windows` fires a toast via `powershell.exe` (WinRT); no-op outside WSL. The widget mirrors this in `checkAlerts` (`widget/src/main.js`), reading the same thresholds through the Rust `alert_thresholds` command.

### Tauri widget (`widget/`)

Native Tauri v2 app (Windows: WebView2; macOS: WKWebView) mirroring the TUI's compact mode.

- **Native collector (`src-tauri/src/collector/`):** a Rust port of the Python collector — `claude.rs` (OAuth refresh + usage/profile), `codex.rs` (JSON-RPC against the `app-server`; on Windows via `codex.cmd` or the native executable inside the npm package, with `CREATE_NO_WINDOW`; on Unix via `find_codex()` mirroring Python's `codex_bin` because GUI apps get a minimal launchd PATH; falling back to the session cache), `cursor.rs` (admin_key/dashboard_cookie; cookie path also builds the cycle-aligned `Weekly` pace meter from `get-filtered-usage-events`, same silent-failure contract as Python), `grok.rs` (OIDC refresh of `~/.grok/auth.json` + CLI-proxy billing), `date.rs` (ISO-8601 without `chrono`), `config.rs` (store in `~/.config/ai-usage-monitor/`; 0600/0700 on Unix writes). It serializes exactly the same JSON as Python, so the frontend serves both. **There is no WSL bridge** — the app is self-contained. `--probe` prints the collection and exits, for diagnosis without the GUI.
- **Per-platform config:** `tauri.conf.json` is shared; Tauri merges `tauri.windows.conf.json` (NSIS) or `tauri.macos.conf.json` (app/dmg, `macOSPrivateApi` for the transparent window) over it. macOS-specific behavior in `main.rs` is `cfg(target_os = "macos")`-gated: `ActivationPolicy::Accessory` (no Dock icon), visible-on-all-workspaces, template tray icon.
- **Accounts view (`src-tauri/src/accounts.rs`):** `a`/`⚙` opens in-widget account management. Detection is metadata-only: macOS Keychain services `Claude Code-credentials[-<hash>]` (`hash` = first 8 hex of sha256 of the config dir path; entries are reverse-mapped to `~/.claude*` dirs and labeled with the path) plus `~/.claude*/.credentials.json` files; a file whose dir also has a Keychain entry is hidden (Keychain wins), as are sources whose logged-in account (from the dir's `.claude.json`) is already registered. The secret is only read on Add (macOS permission prompt), then the account is identified via the OAuth profile API, deduplicated by email, and registered under the email's local part. Codex `+ add` copies the live CLI `auth.json` into the store the same way. Commands: `detect_accounts`, `add_claude_account`, `remove_claude_account`, `add_codex_account`, `remove_codex_profile`, `save_cursor_config`, `remove_cursor_config`, `remove_codex_account`, `remove_grok_account`. Unconfigured providers (Codex/Grok without a local session, Cursor without a key) are hidden from the main panel by the frontend.
- **Two implementations of the same contract:** when changing the shape of `Provider`/`Meter`, update the Python (`cli/usage_monitor.py`) and the Rust (`collector/mod.rs`) together. The same goes for user-visible meter labels and `details` strings, so both surfaces read alike.
- **Frontend (`src/main.js`, `src/index.html`):** vanilla JS, no framework, `withGlobalTauri`. Replicates the TUI's logic (per-provider colors, per-level alert latch). Tauri commands: `fetch_usage` (async, otherwise it freezes the UI), `hide_to_tray`, `frontend_ready`, `alert_thresholds` (the notification levels from config.json).
- **Window:** starts hidden (avoids the white flash), borderless, always-on-top, out of the taskbar, repositioned to the bottom-right corner on every show (no position persistence). Tray: left click toggles, right → Quit. Autostart and notifications only in release builds.

## Security

Tokens/cookies/keys never appear in the dashboard or in process arguments. Config directories `0700`, credential files `0600` (see `ensure_private_dir`/`write_private_json`). Never copy credentials into chats, issues, or commits.
