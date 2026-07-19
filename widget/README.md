# AI Usage Widget — Tauri app

Native desktop widget (Tauri v2 + WebView2) carrying the UI of the TUI's compact mode: same per-provider colors, bars, `%`, renewal time, and `◉ STANDBY` marker.

## How it works

- **Data**: collection runs natively in Rust inside the app (`src-tauri/src/collector/`), talking to the Claude, Codex, and Cursor APIs directly. There is no WSL bridge and no external process — the frontend requests each reading through the `fetch_usage` command and renders the JSON.
- **Window**: borderless, always on top, out of the taskbar (`alwaysOnTop`, `decorations: false`, `skipTaskbar`), pinned to the bottom-right of the work area (computed in Rust's `setup`). It starts hidden and can only be shown once the frontend is ready, avoiding the WebView's initial white flash.
- **Start with Windows**: `tauri-plugin-autostart` (registry entry, enabled on the first run of a release build).
- **Notifications**: `tauri-plugin-notification` when a limit crosses 80% — once on crossing, re-arming when usage drops back below the threshold (hysteresis).
- **Tray**: the app starts hidden, with only the tray icon. Left click opens the window with focus (click again to hide); right click → "Quit" exits the app.
- **Interaction**: `r` or the `↻` button refresh; `q`/`Esc`, `✕`, or Alt+F4 hide to the tray; dragging any empty area moves the window (it returns to the corner on reopen).

## Building

Requirements on Windows (installable via winget): Rust (rustup, MSVC toolchain), Visual Studio Build Tools 2022 with C++, WebView2 Runtime, Node.js.

The build must run on the Windows filesystem — cargo does not work well under `\\wsl.localhost`, so it cannot be compiled from WSL.

```powershell
npm install
npm run dev      # dev mode
npm run build    # release: src-tauri\target\release\ai-usage-widget.exe + NSIS installer in ...\release\bundle\nsis\
```

The icons in `src-tauri/icons/` are already generated; to change them, use `npx tauri icon path\to\icon.png`.

## Diagnostics

```powershell
ai-usage-widget.exe --probe   # prints the collected JSON and exits, without the GUI
```
