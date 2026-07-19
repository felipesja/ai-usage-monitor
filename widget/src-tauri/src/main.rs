#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindow};
#[cfg(not(debug_assertions))]
use tauri_plugin_autostart::ManagerExt;

mod collector;

/// Coleta os limites nativamente (sem ponte WSL) e devolve o JSON que o
/// frontend renderiza. Async + spawn_blocking: a coleta faz I/O de rede e
/// congelaria a UI (e as animações) se rodasse na thread principal do Tauri.
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

/// Canto inferior direito da área de trabalho. Sem persistência de posição:
/// todo show reposiciona, mesmo que a janela tenha sido arrastada.
fn apply_position(window: &WebviewWindow) -> tauri::Result<()> {
    if let Some(monitor) = window.current_monitor()? {
        let area = monitor.work_area();
        let size = window.outer_size()?;
        let x = area.position.x + area.size.width as i32 - size.width as i32 - MARGIN;
        let y = area.position.y + area.size.height as i32 - size.height as i32 - MARGIN;
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
        let mut st = state.lock().expect("estado do tray envenenado");
        st.pinned = false;
    }
    hide_widget(&app);
}

#[tauri::command]
fn frontend_ready(app: AppHandle, state: tauri::State<'_, Mutex<TrayState>>) {
    let show = {
        let mut st = state.lock().expect("estado do tray envenenado");
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
                let mut st = state.lock().expect("estado do tray envenenado");
                let now = Instant::now();
                // Double-click gera dois Clicks; sem debounce o widget "pisca".
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
    // `ai-usage-widget --probe` imprime a coleta em JSON e sai, sem subir a
    // janela. Serve para diagnosticar o coletor sem depender da GUI.
    if std::env::args().any(|arg| arg == "--probe") {
        let providers = collector::collect_all();
        match serde_json::to_string_pretty(&providers) {
            Ok(json) => println!("{json}"),
            Err(err) => eprintln!("erro ao serializar: {err}"),
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
            // Alt+F4 (e qualquer close) vira esconder para a tray.
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
            // Iniciar com o Windows (entrada em Run do registro).
            // Só no release: em dev o exe é temporário e não deve ficar no registro.
            #[cfg(not(debug_assertions))]
            let _ = app.autolaunch().enable();

            app.manage(Mutex::new(TrayState::default()));

            // Posiciona já no setup para o primeiro show (janela nasce oculta).
            let window = app.get_webview_window("main").expect("janela main ausente");
            apply_position(&window)?;

            let quit = MenuItemBuilder::with_id("quit", "Sair").build(app)?;
            let menu = MenuBuilder::new(app).item(&quit).build()?;
            TrayIconBuilder::with_id("tray")
                .icon(
                    app.default_window_icon()
                        .expect("ícone padrão ausente")
                        .clone(),
                )
                .tooltip("AI Usage")
                .menu(&menu)
                // Sem isso, click esquerdo abre o menu em vez de togglar.
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
        .expect("erro ao iniciar o AI Usage Widget");
}
