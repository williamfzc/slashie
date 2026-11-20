//! macOS menu-bar shell (menu-bar.md): one tray item with a `/` template
//! icon and scan pulse, the native Polling check item, status copy, the single
//! Settings window over bundled local assets, and shared shutdown/quit.
//! Compiled only behind the `tray` feature; all runtime logic lives in the
//! shared runtime behind control handles.

use crate::runtime::Runtime;
use crate::settings::{save_settings, SettingsDraft};
use crate::status::{status_line, RuntimeStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{
    image::Image,
    menu::{CheckMenuItem, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder,
};

const SCAN_PULSE_DURATION: Duration = Duration::from_millis(500);

pub struct AppState {
    pub rt: Arc<Runtime>,
    pub runtime_thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    pub settings_snapshot: Mutex<String>,
    pub shutting_down: AtomicBool,
    polling_item: Mutex<Option<CheckMenuItem<tauri::Wry>>>,
}

pub fn run(rt: Arc<Runtime>, shutdown: Arc<AtomicBool>) {
    let snapshot = std::fs::read_to_string(rt.home.join("config.json")).unwrap_or_default();
    let state = AppState {
        rt: rt.clone(),
        runtime_thread: Arc::new(Mutex::new(None)),
        settings_snapshot: Mutex::new(snapshot),
        shutting_down: AtomicBool::new(false),
        polling_item: Mutex::new(None),
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
    let polling =
        CheckMenuItem::with_id(&app, "polling", "Polling", true, polling_now, None::<&str>)?;
    let settings_item = MenuItem::with_id(&app, "settings", "Settings…", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(&app)?;
    let quit = MenuItem::with_id(&app, "quit", "Quit Slashie", true, None::<&str>)?;
    let menu = tauri::menu::Menu::with_items(
        &app,
        &[&status_item, &polling, &settings_item, &sep, &quit],
    )?;

    *state.polling_item.lock().unwrap() = Some(polling.clone());

    let icon = Image::from_bytes(include_bytes!("assets/tray-template.png"))?;
    let pulse_icon = pulse_icon(&icon);
    let tray = TrayIconBuilder::with_id("slashie-tray")
        .icon(icon.clone())
        .icon_as_template(true)
        .tooltip("Slashie")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| {
            handle_menu_event(
                app,
                event.id().as_ref(),
                shutdown.clone(),
                runtime_thread.clone(),
            );
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

    // Tauri dispatches the native setters on the main thread. Update only
    // on changes so sampling the pulse doesn't churn the native menu.
    std::thread::spawn(move || {
        let mut last_text = "";
        let mut last_polling = polling_now;
        let mut showing_pulse = false;
        loop {
            let state: tauri::State<AppState> = app.state();
            state.rt.refresh_status();
            let status = state.rt.status.lock().unwrap().clone();
            let polling_now = state.rt.state.lock().unwrap().polling;
            let pulse = pulse_visible(&status, polling_now, Instant::now())
                && !state.shutting_down.load(Ordering::SeqCst);
            if pulse != showing_pulse {
                let next_icon = if pulse { &pulse_icon } else { &icon };
                if tray
                    .set_icon_with_as_template(Some(next_icon.clone()), true)
                    .is_err()
                {
                    return;
                }
                showing_pulse = pulse;
            }
            let text = status_line(&status).unwrap_or("");
            if text != last_text {
                if status_item.set_text(text).is_err() {
                    return;
                }
                last_text = text;
            }
            if polling_now != last_polling {
                if polling.set_checked(polling_now).is_err() {
                    return;
                }
                last_polling = polling_now;
            }
            std::thread::sleep(SCAN_PULSE_DURATION / 2);
        }
    });
    Ok(())
}

fn pulse_visible(status: &RuntimeStatus, polling: bool, now: Instant) -> bool {
    polling
        && status.phase.is_none()
        && status.operator_action.is_none()
        && status.retrying.is_none()
        && status
            .last_successful_scan
            .is_some_and(|scan| now.duration_since(scan) < SCAN_PULSE_DURATION)
}

fn pulse_icon(base: &Image<'_>) -> Image<'static> {
    let mut rgba = base.rgba().to_vec();
    // Add a small round dot in the transparent space beside the slash tail.
    let alpha = [
        [32, 176, 176, 32],
        [176, 255, 255, 176],
        [176, 255, 255, 176],
        [32, 176, 176, 32],
    ];
    for (y, row) in alpha.into_iter().enumerate() {
        for (x, opacity) in row.into_iter().enumerate() {
            let pixel = ((24 + y) * base.width() as usize + 6 + x) * 4;
            rgba[pixel..pixel + 4].copy_from_slice(&[0, 0, 0, opacity]);
        }
    }
    Image::new_owned(rgba, base.width(), base.height())
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
            let polling_item = state.polling_item.lock().unwrap().clone();
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
        "quit"
            if state
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{OperatorFault, Phase, RetryKind};

    #[test]
    fn only_a_recent_successful_scan_pulses_while_polling() {
        let now = Instant::now();
        let mut status = RuntimeStatus::default();
        assert!(!pulse_visible(&status, true, now));
        status.last_successful_scan = Some(now);
        assert!(pulse_visible(&status, true, now));
        assert!(!pulse_visible(&status, true, now + SCAN_PULSE_DURATION));
        assert!(!pulse_visible(&status, false, now));

        for phase in [Phase::Starting, Phase::Quitting] {
            status.phase = Some(phase);
            assert!(!pulse_visible(&status, true, now));
        }
        status.phase = None;
        status.operator_action = Some(OperatorFault::SignInToFeishu);
        assert!(!pulse_visible(&status, true, now));
        status.operator_action = None;
        status.retrying = Some(RetryKind::RateLimited);
        assert!(!pulse_visible(&status, true, now));
        status.retrying = None;
        status.reply_unconfirmed = true;
        assert!(pulse_visible(&status, true, now));
    }
}
