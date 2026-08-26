# AI Usage Widget — Tauri app

Native desktop widget (Tauri v2; WebView2 on Windows, WKWebView on macOS) carrying the UI of the TUI's compact mode: same per-provider colors, bars, `%`, renewal time, and `◉ STANDBY` marker.

## How it works

- **Data**: collection runs natively in Rust inside the app (`src-tauri/src/collector/`), talking to the Claude, Codex, Cursor, and Grok APIs directly. There is no WSL bridge and no external process — the frontend requests each reading through the `fetch_usage` command and renders the JSON.
- **Window**: borderless, always on top, out of the taskbar (`alwaysOnTop`, `decorations: false`, `skipTaskbar`; on macOS `ActivationPolicy::Accessory` hides the Dock icon and the panel is visible on every Space), pinned to the bottom-right of the work area (computed in Rust's `setup`). It starts hidden and can only be shown once the frontend is ready, avoiding the WebView's initial white flash.
- **Start on login**: `tauri-plugin-autostart` (registry Run entry on Windows, LaunchAgent on macOS; enabled on the first run of a release build).
- **Notifications**: `tauri-plugin-notification` when a limit crosses 80% — once on crossing, re-arming when usage drops back below the threshold (hysteresis). On macOS the system asks for permission on the first alert, and notifications only work from the bundled `.app` (not the bare dev binary).
- **Tray**: the app starts hidden, with only the tray/menu-bar icon (a monochrome template icon on macOS, tinted by the system). Left click opens the window with focus (click again to hide); right click → "Quit" exits the app.
- **Per-platform config**: `tauri.conf.json` holds the shared config; Tauri merges `tauri.windows.conf.json` (NSIS target) or `tauri.macos.conf.json` (app/dmg targets, `macOSPrivateApi` for the transparent window) over it at build time.
- **Interaction**: `r` or the `↻` button refresh; `a` or `⚙` open the accounts view (`Esc` goes back); `q`/`Esc`, `✕`, or Alt+F4 hide to the tray; dragging any empty area moves the window (it returns to the corner on reopen).
- **Accounts**: the accounts view registers Claude profiles from the credentials already on the machine — macOS Keychain entries (one per Claude Code config dir) and `~/.claude*/.credentials.json` files, deduplicated against what is registered — reports Codex and Grok from their CLIs, and configures Cursor (admin key or dashboard cookie). Detection never reads a secret; that happens on Add, behind the macOS permission prompt. Providers with nothing configured stay hidden in the main panel.
- **Optional activity hint**: an external launcher or router can write `~/.config/ai-usage-monitor/claude-active-account.json` with `email` and Unix `updated_at`. A fresh hint identifies the active Claude account for one 5h window. The app never requires or creates this file; standard Windows and macOS setups keep using Claude Code login detection.

## Building

### Windows

Requirements (installable via winget): Rust (rustup, MSVC toolchain), Visual Studio Build Tools 2022 with C++, WebView2 Runtime, Node.js.

The build must run on the Windows filesystem — cargo does not work well under `\\wsl.localhost`, so it cannot be compiled from WSL.

```powershell
npm install
npm run dev      # dev mode
npm run build    # release: src-tauri\target\release\ai-usage-widget.exe + NSIS installer in ...\release\bundle\nsis\
```

### macOS

Requirements: Xcode Command Line Tools (`xcode-select --install`), Rust (rustup), Node.js.

```bash
npm install
npm run dev      # dev mode
npm run build    # release: .app in src-tauri/target/release/bundle/macos/ + installer in .../bundle/dmg/
```

Move the `.app` to `/Applications` **before** the first launch: the autostart LaunchAgent records the binary's path, so launching from the DMG mount or another folder and moving it later breaks login start. The bundle is ad-hoc signed — fine locally; distributing it requires a Developer ID certificate and notarization.

The icons in `src-tauri/icons/` are already generated; to change them, use `npx tauri icon path/to/icon.png` (`tray-macos.png`, the menu-bar template icon, is maintained by hand).

## Diagnostics

```powershell
ai-usage-widget.exe --probe   # Windows: prints the collected JSON and exits, without the GUI
```

```bash
"/Applications/AI Usage Widget.app/Contents/MacOS/ai-usage-widget" --probe   # macOS
```
