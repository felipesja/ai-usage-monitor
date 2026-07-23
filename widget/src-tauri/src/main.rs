#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindow};
#[cfg(not(debug_assertions))]
use tauri_plugin_autostart::ManagerExt;

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

const MARGIN: i32 = 12;
const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Default)]
struct TrayState {
    pinned: bool,
    frontend_ready: bool,
    last_toggle: Option<Instant>,
}

/// Bottom-right corner of the work area. No position persistence: every show
/// repositions, even if the window was dragged elsewhere.
fn apply_position(window: &WebviewWindow) -> tauri::Result<()> {
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
            ..
        } => {
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
            frontend_ready
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
