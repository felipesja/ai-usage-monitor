#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindow};
#[cfg(not(debug_assertions))]
use tauri_plugin_autostart::ManagerExt;

#[cfg(windows)]
mod bridge {
    //! Ponte WSL persistente criada diretamente com CREATE_NO_WINDOW. O uso de
    //! WslLaunch parece mais nativo, mas as versões atuais do WSL implementam
    //! essa API iniciando um wsl.exe com console próprio; quando a ponte fica
    //! viva, o Windows Terminal também fica aberto. Criar o processo como filho
    //! GUI, com stdio redirecionado e sem console, evita a janela por completo.
    use std::io::{self, BufRead, BufReader, Write};
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DISTRO: &str = "Ubuntu";
    const BRIDGE_CMD: &str = "exec \"$HOME/.local/bin/ai-usage\" bridge";

    fn log_bridge(message: &str) {
        let path = std::env::temp_dir().join("ai-usage-widget.log");
        let line = format!("{message}\n");
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
    }

    struct Bridge {
        child: Child,
        stdin: Box<dyn Write + Send>,
        reader: Box<dyn BufRead + Send>,
    }

    impl Bridge {
        fn kill(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    static BRIDGE: Mutex<Option<Bridge>> = Mutex::new(None);

    fn spawn_bridge() -> io::Result<Bridge> {
        log_bridge("criando ponte wsl.exe oculta com CREATE_NO_WINDOW");
        let mut child = Command::new("wsl.exe")
            .args(["-d", DISTRO, "--", "bash", "-lc", BRIDGE_CMD])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin da ponte ausente");
        let stdout = child.stdout.take().expect("stdout da ponte ausente");
        Ok(Bridge {
            child,
            stdin: Box::new(stdin),
            reader: Box::new(BufReader::new(stdout)),
        })
    }

    pub fn fetch() -> Result<String, String> {
        let mut guard = BRIDGE.lock().map_err(|_| "ponte ocupada")?;
        for _ in 0..2 {
            if guard.is_none() {
                *guard = Some(spawn_bridge().map_err(|err| err.to_string())?);
            }
            let bridge = guard.as_mut().expect("ponte recém-criada");
            let sent = bridge.stdin.write_all(b"\n").is_ok() && bridge.stdin.flush().is_ok();
            if sent {
                // Shells de login podem ecoar lixo antes do JSON; pula até 5
                // linhas procurando a resposta (que sempre começa com '[').
                let mut skipped = 0;
                loop {
                    let mut line = String::new();
                    match bridge.reader.read_line(&mut line) {
                        Ok(n) if n > 0 => {
                            if line.trim_start().starts_with('[') {
                                return Ok(line);
                            }
                            skipped += 1;
                            if skipped >= 5 {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
            }
            // Ponte morta ou resposta inválida: derruba e tenta com uma nova.
            log_bridge("ponte WSL respondeu inválido — recriando");
            bridge.kill();
            *guard = None;
        }
        Err("ponte WSL indisponível".into())
    }

    /// O wsl.exe filho não morre quando o app sai; precisa de kill explícito.
    pub fn shutdown() {
        if let Ok(mut guard) = BRIDGE.lock() {
            if let Some(mut bridge) = guard.take() {
                bridge.kill();
            }
        }
    }
}

/// Pede uma leitura de `ai-usage once --json` à ponte e devolve o JSON cru.
/// Async: comandos síncronos rodam na thread principal do Tauri e
/// congelariam a UI (e as animações) durante o fetch.
#[tauri::command]
async fn fetch_usage() -> Result<String, String> {
    #[cfg(windows)]
    {
        tauri::async_runtime::spawn_blocking(bridge::fetch)
            .await
            .map_err(|err| err.to_string())?
    }
    #[cfg(not(windows))]
    {
        Err("disponível apenas no Windows".into())
    }
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
                        #[cfg(windows)]
                        bridge::shutdown();
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
