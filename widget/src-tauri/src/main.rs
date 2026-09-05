#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindow};
#[cfg(not(debug_assertions))]
use tauri_plugin_autostart::ManagerExt;

mod accounts;
mod collector;

/// Collects the limits natively (no WSL bridge) and returns the JSON the
/// frontend renders. Async + spawn_blocking: collection does network I/O and
/// would freeze the UI (and its animations) on Tauri's main thread.
#[tauri::command]
async fn fetch_usage() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let providers = collector::collect_all();
        serde_json::to_string(&providers).map_err(|err| err.to_string())
    })
    .await
    .map_err(|err| err.to_string())?
}

/// The notification thresholds (percent) the frontend alerts on, read from
/// config.json. Creates the file with defaults on first read so it is editable.
#[tauri::command]
fn alert_thresholds() -> Vec<f64> {
    collector::config::alert_thresholds()
}

/// Credential sources present on the machine, for the Accounts view.
/// spawn_blocking: `security dump-keychain` is a subprocess.
#[tauri::command]
async fn detect_accounts() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(|| {
        serde_json::to_string(&accounts::detect()).map_err(|err| err.to_string())
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Registers the Claude credential behind a detected source (may prompt for
/// Keychain access and does network I/O to identify the account).
#[tauri::command]
async fn add_claude_account(source: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        accounts::add_claude(&source)
            .and_then(|registered| serde_json::to_string(&registered).map_err(|err| err.to_string()))
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
fn remove_claude_account(profile: String) -> Result<(), String> {
    accounts::remove_claude(&profile)
}

#[tauri::command]
fn save_cursor_config(method: String, secret: String, email: String) -> Result<(), String> {
    accounts::save_cursor(&method, &secret, &email)
}

#[tauri::command]
fn remove_cursor_config() -> Result<(), String> {
    accounts::remove_cursor()
}

/// Copies the live Codex CLI session into the credential store, named after
/// the account's email — same idea as `add_claude_account`.
#[tauri::command]
async fn add_codex_account() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(|| {
        accounts::add_codex()
            .and_then(|registered| serde_json::to_string(&registered).map_err(|err| err.to_string()))
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Drops a stored Codex profile. Does not log the CLI out.
#[tauri::command]
fn remove_codex_profile(profile: String) -> Result<(), String> {
    accounts::remove_codex_profile(&profile)
}

/// Logs the Codex CLI out (`codex logout`) — a subprocess call, unlike the
/// Claude/Cursor removals which just delete a file this app owns.
#[tauri::command]
async fn remove_codex_account() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(collector::codex::logout).await.map_err(|err| err.to_string())?
}

/// Logs the Grok CLI out (`grok logout`). Falls back to deleting `auth.json`
/// when the binary is not on PATH.
#[tauri::command]
async fn remove_grok_account() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(collector::grok::logout).await.map_err(|err| err.to_string())?
}

const MARGIN: i32 = 12;
const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Default)]
struct TrayState {
    pinned: bool,
    frontend_ready: bool,
    last_toggle: Option<Instant>,
    /// Where the tray icon was last clicked (physical px). macOS anchors the
    /// panel below it, like other menu bar widgets.
    #[cfg(target_os = "macos")]
    tray_click: Option<tauri::PhysicalPosition<f64>>,
}

/// macOS: just below the menu bar, horizontally centered on the tray icon —
/// the menu-bar-widget convention. Everywhere else (or before any tray click):
/// bottom-right corner of the work area. No position persistence: every show
/// repositions, even if the window was dragged elsewhere.
fn apply_position(window: &WebviewWindow) -> tauri::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let click = {
            let state = window.app_handle().state::<Mutex<TrayState>>();
            let st = state.lock().expect("tray state poisoned");
            st.tray_click
        };
        if let Some(click) = click {
            // The monitor holding the clicked menu bar, not the hidden
            // window's notion of "current". Containment test in physical px:
            // `monitor_from_point` takes logical points on macOS, which would
            // pick the wrong display for a physical click position.
            let monitor = window
                .app_handle()
                .available_monitors()?
                .into_iter()
                .find(|m| {
                    let pos = m.position();
                    let size = m.size();
                    click.x >= pos.x as f64
                        && click.x < pos.x as f64 + size.width as f64
                        && click.y >= pos.y as f64
                        && click.y < pos.y as f64 + size.height as f64
                })
                .or(window.current_monitor()?);
            if let Some(monitor) = monitor {
                let area = monitor.work_area();
                let size = window.outer_size()?;
                let margin = (MARGIN as f64 * monitor.scale_factor()).round() as i32;
                let gap = (6.0 * monitor.scale_factor()).round() as i32;
                let x = (click.x as i32 - size.width as i32 / 2).clamp(
                    area.position.x + margin,
                    area.position.x + area.size.width as i32 - size.width as i32 - margin,
                );
                // The work area already excludes the menu bar.
                let y = area.position.y + gap;
                window.set_position(tauri::PhysicalPosition::new(x, y))?;
                return Ok(());
            }
        }
    }
    if let Some(monitor) = window.current_monitor()? {
        let area = monitor.work_area();
        let size = window.outer_size()?;
        // The work area is in physical pixels; scale the margin so it reads as
        // 12 logical px regardless of DPI (e.g. Retina's 2x).
        let margin = (MARGIN as f64 * monitor.scale_factor()).round() as i32;
        let x = area.position.x + area.size.width as i32 - size.width as i32 - margin;
        let y = area.position.y + area.size.height as i32 - size.height as i32 - margin;
        window.set_position(tauri::PhysicalPosition::new(x, y))?;
    }
    Ok(())
}

fn show_pinned(window: &WebviewWindow) {
    let _ = apply_position(window);
    let _ = window.show();
    let _ = window.set_focus();
}

fn hide_widget(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
}

#[tauri::command]
fn hide_to_tray(app: AppHandle, state: tauri::State<'_, Mutex<TrayState>>) {
    {
        let mut st = state.lock().expect("tray state poisoned");
        st.pinned = false;
    }
    hide_widget(&app);
}

/// Grows/shrinks the window to fit its content, keeping the bottom edge where
/// it is so the panel does not appear to jump. Deliberately not `apply_position`:
/// a window the user dragged elsewhere must not snap back to the corner on
/// every refresh.
#[tauri::command]
fn resize_to_content(app: AppHandle, height: f64) -> Result<(), String> {
    let window = app.get_webview_window("main").ok_or("no main window")?;
    let scale = window.scale_factor().map_err(|err| err.to_string())?;
    // Matches `minHeight` in tauri.conf.json — change both together.
    let min = 320.0;
    let max = match window.current_monitor().map_err(|err| err.to_string())? {
        Some(monitor) => {
            (monitor.work_area().size.height as f64 / monitor.scale_factor()) - 2.0 * MARGIN as f64
        }
        None => height,
    };
    let target = height.clamp(min, max.max(min));

    let before = window.outer_size().map_err(|err| err.to_string())?;
    let position = window.outer_position().map_err(|err| err.to_string())?;
    window
        .set_size(tauri::LogicalSize::new(before.width as f64 / scale, target))
        .map_err(|err| err.to_string())?;
    let after = window.outer_size().map_err(|err| err.to_string())?;
    // Keep the bottom edge fixed (physical px; x untouched).
    let y = position.y + before.height as i32 - after.height as i32;
    window
        .set_position(tauri::PhysicalPosition::new(position.x, y))
        .map_err(|err| err.to_string())?;
    Ok(())
}

#[tauri::command]
fn frontend_ready(app: AppHandle, state: tauri::State<'_, Mutex<TrayState>>) {
    let show = {
        let mut st = state.lock().expect("tray state poisoned");
        st.frontend_ready = true;
        st.pinned
    };
    if show {
        if let Some(window) = app.get_webview_window("main") {
            show_pinned(&window);
        }
    }
}

fn on_tray_event(app: &AppHandle, event: TrayIconEvent) {
    match event {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            position,
            ..
        } => {
            #[cfg(not(target_os = "macos"))]
            let _ = position;
            let Some(window) = app.get_webview_window("main") else {
                return;
            };
            let visible = window.is_visible().unwrap_or(false);
            enum Action {
                Hide,
                Show,
                FocusOnly,
                WaitForFrontend,
            }
            let action = {
                let state = app.state::<Mutex<TrayState>>();
                let mut st = state.lock().expect("tray state poisoned");
                #[cfg(target_os = "macos")]
                {
                    st.tray_click = Some(position);
                }
                let now = Instant::now();
                // A double-click emits two Clicks; without debounce it flickers.
                if st
                    .last_toggle
                    .is_some_and(|last| now.duration_since(last) < TOGGLE_DEBOUNCE)
                {
                    return;
                }
                st.last_toggle = Some(now);
                if st.pinned {
                    st.pinned = false;
                    Action::Hide
                } else {
                    st.pinned = true;
                    if !st.frontend_ready {
                        Action::WaitForFrontend
                    } else if visible {
                        Action::FocusOnly
                    } else {
                        Action::Show
                    }
                }
            };
            match action {
                Action::Hide => hide_widget(app),
                Action::Show => show_pinned(&window),
                Action::FocusOnly => {
                    let _ = window.set_focus();
                }
                Action::WaitForFrontend => {}
            }
        }
        _ => {}
    }
}

fn main() {
    // `ai-usage-widget --probe` prints the collection as JSON and exits without
    // bringing up the window. Useful to diagnose the collector without the GUI.
    if std::env::args().any(|arg| arg == "--probe") {
        let providers = collector::collect_all();
        match serde_json::to_string_pretty(&providers) {
            Ok(json) => println!("{json}"),
            Err(err) => eprintln!("serialization error: {err}"),
        }
        return;
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            fetch_usage,
            hide_to_tray,
            resize_to_content,
            frontend_ready,
            alert_thresholds,
            detect_accounts,
            add_claude_account,
            remove_claude_account,
            add_codex_account,
            remove_codex_profile,
            save_cursor_config,
            remove_cursor_config,
            remove_codex_account,
            remove_grok_account
        ])
        .on_window_event(|window, event| {
            // Alt+F4 (and any close) becomes hide-to-tray.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let app = window.app_handle();
                if let Ok(mut st) = app.state::<Mutex<TrayState>>().lock() {
                    st.pinned = false;
                }
                hide_widget(app);
            }
        })
        .setup(|app| {
            // Autostart on login (registry Run entry on Windows, LaunchAgent on
            // macOS). Release only: in dev the exe is temporary and should not
            // linger in the registry.
            #[cfg(not(debug_assertions))]
            let _ = app.autolaunch().enable();

            // macOS: menu-bar utility without a Dock icon (`skipTaskbar` is a
            // Windows/Linux concept).
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            app.manage(Mutex::new(TrayState::default()));

            // Position during setup for the first show (window starts hidden).
            let window = app.get_webview_window("main").expect("main window missing");
            // macOS: widget convention — the panel follows the user to every
            // Space instead of staying behind on the one where it was opened.
            #[cfg(target_os = "macos")]
            let _ = window.set_visible_on_all_workspaces(true);
            apply_position(&window)?;

            let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
            let menu = MenuBuilder::new(app).item(&quit).build()?;
            let tray = TrayIconBuilder::with_id("tray").icon(
                app.default_window_icon()
                    .expect("default icon missing")
                    .clone(),
            );
            // macOS: a template icon (monochrome + alpha) so the menu bar tints
            // it to match light/dark appearance.
            #[cfg(target_os = "macos")]
            let tray = tray
                .icon(tauri::image::Image::from_bytes(include_bytes!(
                    "../icons/tray-macos.png"
                ))?)
                .icon_as_template(true);
            tray.tooltip("AI Usage")
                .menu(&menu)
                // Without this, a left click opens the menu instead of toggling.
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    if event.id().as_ref() == "quit" {
                        app.exit(0);
                    }
                })
                .on_tray_icon_event(|tray, event| on_tray_event(tray.app_handle(), event))
                .build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to start AI Usage Widget");
}
