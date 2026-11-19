//! macOS menu-bar shell (menu-bar.md): one tray item with a fixed `/`
//! template icon, the native Polling check item, status copy, the single
//! Settings window over bundled local assets, and shared shutdown/quit.
//! Compiled only behind the `tray` feature; all runtime logic lives in the
//! shared runtime behind control handles.

use crate::runtime::Runtime;
use crate::settings::{save_settings, SettingsDraft};
use crate::status::status_line;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{
    menu::{CheckMenuItem, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder,
};

pub struct AppState {
    pub rt: Arc<Runtime>,
    pub runtime_thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    pub settings_snapshot: Mutex<String>,
    pub shutting_down: AtomicBool,
    /// Tray item handles for status/checkmark updates from any thread.
    pub tray_items: Mutex<Option<TrayItems>>,
}

#[derive(Clone)]
pub struct TrayItems {
    pub status: tauri::menu::MenuItem<tauri::Wry>,
    pub polling: tauri::menu::CheckMenuItem<tauri::Wry>,
}

pub fn run(rt: Arc<Runtime>, shutdown: Arc<AtomicBool>) {
    let snapshot = std::fs::read_to_string(rt.home.join("config.json")).unwrap_or_default();
    let state = AppState {
        rt: rt.clone(),
        runtime_thread: Arc::new(Mutex::new(None)),
        settings_snapshot: Mutex::new(snapshot),
        shutting_down: AtomicBool::new(false),
        tray_items: Mutex::new(None),
    };

    // The shared runtime runs on its own thread so menu controls take
    // effect while a subprocess is waiting.
    let runtime_thread = state.runtime_thread.clone();
    let rt_for_loop = rt.clone();
    *runtime_thread.lock().unwrap() = Some(std::thread::spawn(move || {
        rt_for_loop.run();
    }));
    let thread_ref = state.runtime_thread.clone();

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings_cmd,
            open_log,
            get_status
        ])
        .setup(move |app| {
            build_tray(app.handle().clone(), shutdown.clone(), thread_ref.clone())?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn build_tray(
    app: AppHandle,
    shutdown: Arc<AtomicBool>,
    runtime_thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let state: tauri::State<AppState> = app.state();
    let polling_now = {
        let st = state.rt.state.lock().unwrap();
        st.polling
    };

    let status_item = MenuItem::with_id(&app, "status", "", false, None::<&str>)?;
    let polling = CheckMenuItem::with_id(
        &app,
        "polling",
        "Polling",
        true,
        polling_now,
        None::<&str>,
    )?;
    let settings_item = MenuItem::with_id(&app, "settings", "Settings…", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(&app)?;
    let quit = MenuItem::with_id(&app, "quit", "Quit Slashie", true, None::<&str>)?;
    let menu = tauri::menu::Menu::with_items(
        &app,
        &[&status_item, &polling, &settings_item, &sep, &quit],
    )?;

    {
        let state: tauri::State<AppState> = app.state();
        *state.tray_items.lock().unwrap() = Some(TrayItems {
            status: status_item.clone(),
            polling: polling.clone(),
        });
    }

    let icon = tauri::image::Image::from_bytes(include_bytes!("assets/tray-template.png"))?;
    TrayIconBuilder::with_id("slashie-tray")
        .icon(icon)
        .icon_as_template(true)
        .tooltip("Slashie")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| {
            handle_menu_event(app, event.id().as_ref(), shutdown.clone(), runtime_thread.clone());
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                // Primary click opens the menu (show_menu_on_left_click).
                let _ = tray.set_show_menu_on_left_click(true);
            }
        })
        .build(&app)?;

    // Reflect the shared status snapshot while the menu is open: the
    // read-only line follows the first applicable row; the checkmark
    // follows the saved setting.
    let refresh_app = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let state: tauri::State<AppState> = refresh_app.state();
        state.rt.refresh_status();
        let status = state.rt.status.lock().unwrap().clone();
        let polling = {
            let st = state.rt.state.lock().unwrap();
            st.polling
        };
        let text = status_line(&status).unwrap_or("").to_string();
        let app = refresh_app.clone();
        let items = state
            .tray_items
            .lock()
            .unwrap()
            .clone()
            .map(|i| (i.status, i.polling));
        if app
            .run_on_main_thread(move || {
                if let Some((status_item, polling_item)) = items {
                    let _ = status_item.set_text(&text);
                    let _ = polling_item.set_checked(polling);
                }
            })
            .is_err()
        {
            return; // app is exiting
        }
    });
    Ok(())
}

fn handle_menu_event(
    app: &AppHandle,
    id: &str,
    shutdown: Arc<AtomicBool>,
    runtime_thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
) {
    let state: tauri::State<AppState> = app.state();
    match id {
        "status" => {}
        "polling" => {
            // Disable the toggle while saving; confirm the change after the
            // runtime writes it; restore the previous checkmark on failure.
            let app = app.clone();
            let rt = state.rt.clone();
            let desired = {
                let st = state.rt.state.lock().unwrap();
                !st.polling
            };
            let polling_item = {
                let state: tauri::State<AppState> = app.state();
                let guard = state.tray_items.lock().unwrap();
                guard.as_ref().map(|i| i.polling.clone())
            };
            if let Some(item) = &polling_item {
                let _ = item.set_enabled(false);
            }
            let app_for_task = app.clone();
            let rt_for_finish = rt.clone();
            std::thread::spawn(move || {
                let result = rt.apply_poll_setting(desired);
                let saved_now = {
                    let st = rt_for_finish.state.lock().unwrap();
                    st.polling
                };
                let _ = app_for_task.run_on_main_thread(move || {
                    if let Some(item) = polling_item {
                        let _ = item.set_enabled(true);
                        // The checkmark reflects the saved setting even when
                        // an enabled poller is retrying.
                        let _ = item.set_checked(saved_now);
                        if result.is_err() {
                            // Restore the previous checkmark on write failure.
                            let _ = item.set_checked(!desired);
                        }
                    }
                    rt_for_finish.refresh_status();
                });
            });
        }
        "settings" => {
            if let Some(win) = app.get_webview_window("settings") {
                let _ = win.set_focus();
            } else {
                let _ = WebviewWindowBuilder::new(
                    app,
                    "settings",
                    WebviewUrl::App("index.html".into()),
                )
                .title("Slashie Settings")
                .build();
            }
        }
        "quit" if state
            .shutting_down
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok() =>
        {
            // Shared shutdown: request it, wait for the runtime to drain
            // accepted work, then exit.
            state.rt.status.lock().unwrap().phase = Some(crate::status::Phase::Quitting);
            shutdown.store(true, Ordering::SeqCst);
            let app = app.clone();
            std::thread::spawn(move || {
                let handle = runtime_thread.lock().unwrap().take();
                if let Some(handle) = handle {
                    let _ = handle.join();
                }
                app.cleanup_before_exit();
                app.exit(0);
            });
        }
        _ => {}
    }
}

// ---- Settings window commands (DESIGN.md: same schema and validator) ----

#[derive(serde::Serialize)]
struct SettingsPayload {
    draft: SettingsDraft,
    snapshot: String,
    config_unparsable: bool,
    config_path: String,
    log_available: bool,
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> SettingsPayload {
    let cfg = state.rt.config.clone();
    let snapshot = state.settings_snapshot.lock().unwrap().clone();
    let log_available = state.rt.home.join("slashie.log").exists();
    SettingsPayload {
        draft: SettingsDraft::from_config(&cfg),
        snapshot,
        config_unparsable: state.rt.is_intake_blocked()
            && state.settings_snapshot.lock().unwrap().is_empty(),
        config_path: state.rt.home.join("config.json").display().to_string(),
        log_available,
    }
}

#[derive(serde::Serialize)]
enum SaveReply {
    Saved,
    Invalid(Vec<crate::settings::FieldError>),
    FileChanged,
    Unparsable(String),
    Io(String),
}

#[tauri::command]
fn save_settings_cmd(
    state: State<'_, AppState>,
    snapshot: String,
    draft: SettingsDraft,
) -> SaveReply {
    // If the file changed since the window loaded it, reject the save.
    match save_settings(&state.rt.home, &snapshot, &draft) {
        crate::settings::SaveOutcome::Saved => {
            *state.settings_snapshot.lock().unwrap() =
                std::fs::read_to_string(state.rt.home.join("config.json")).unwrap_or_default();
            SaveReply::Saved
        }
        crate::settings::SaveOutcome::Invalid(e) => SaveReply::Invalid(e),
        crate::settings::SaveOutcome::FileChanged => SaveReply::FileChanged,
        crate::settings::SaveOutcome::Unparsable(p) => SaveReply::Unparsable(p),
        crate::settings::SaveOutcome::Io(m) => SaveReply::Io(m),
    }
}

#[tauri::command]
fn open_log(state: State<'_, AppState>) -> Result<(), String> {
    let path = state.rt.home.join("slashie.log");
    if !path.exists() {
        return Err(format!("log not found at {}", path.display()));
    }
    // Reveal the fixed log path with the platform opener.
    std::process::Command::new("/usr/bin/open")
        .arg(&path)
        .spawn()
        .map_err(|e| format!("cannot open log: {}", e))?;
    Ok(())
}

#[derive(serde::Serialize)]
struct StatusPayload {
    polling: bool,
    status_text: Option<&'static str>,
}

#[tauri::command]
fn get_status(state: State<'_, AppState>) -> StatusPayload {
    state.rt.refresh_status();
    let status = state.rt.status.lock().unwrap().clone();
    let polling = {
        let st = state.rt.state.lock().unwrap();
        st.polling
    };
    StatusPayload {
        polling,
        status_text: status_line(&status),
    }
}
