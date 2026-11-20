//! macOS menu-bar shell (menu-bar.md): one tray item whose `/` template icon
//! frames express polling state, the native Polling check item, status copy,
//! the single Settings window over bundled local assets, and shared
//! shutdown/quit. Compiled only behind the `tray` feature; all runtime logic
//! lives in the shared runtime behind control handles.

use crate::config::Config;
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
    AppHandle, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder,
};

/// How long the dot stays bright after a complete scan, including an
/// empty one (menu-bar.md "Status item and menu").
const SCAN_FLASH_DURATION: Duration = Duration::from_millis(1500);

/// Sampling cadence of the tray thread while the icon is static.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// Frame cadence while a scan is in flight; eight spin frames at this rate
/// make the dot orbit about once a second.
const SPIN_INTERVAL: Duration = Duration::from_millis(100);

/// The menu-bar icon frames (menu-bar.md "Status item and menu").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Frame {
    /// Polling paused: the whole glyph dimmed.
    Dimmed,
    /// The status line speaks (startup, faults, retries): the plain glyph.
    Plain,
    /// Healthy polling between scans: the resting dim dot.
    DotIdle,
    /// Just completed a scan: the bright dot.
    DotActive,
    /// A scan in flight: the dot orbiting the slash, `SPIN_STEPS` per turn.
    Spin(usize),
}

/// The spinner orbit sits in the clear triangle beside the slash tail
/// (verified against the 32x32 template's alpha grid).
const SPIN_STEPS: usize = 8;
const SPIN_CENTER: (f64, f64) = (12.0, 25.0);
const SPIN_RADIUS: f64 = 4.0;

/// Classify the icon frame for the current runtime state. `tick` counts
/// loop iterations and advances the spinner while a scan is in flight.
fn tray_frame(status: &RuntimeStatus, polling: bool, now: Instant, tick: u64) -> Frame {
    if !polling {
        return Frame::Dimmed;
    }
    if status_line(status).is_some() {
        return Frame::Plain;
    }
    if status.scanning {
        return Frame::Spin((tick % SPIN_STEPS as u64) as usize);
    }
    if status
        .last_successful_scan
        .is_some_and(|scan| now.duration_since(scan) < SCAN_FLASH_DURATION)
    {
        return Frame::DotActive;
    }
    Frame::DotIdle
}

struct AppState {
    rt: Arc<Runtime>,
    runtime_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    shutting_down: AtomicBool,
    polling_item: Mutex<Option<CheckMenuItem<tauri::Wry>>>,
}

pub fn run(rt: Arc<Runtime>, shutdown: Arc<AtomicBool>) {
    // A menu-bar app with no visible window is eligible for App Nap, which
    // suspends the polling timers. Hold a user-initiated activity for the
    // life of the host; idle system sleep is still allowed.
    #[cfg(target_os = "macos")]
    let _activity = prevent_app_nap();
    // The shared runtime runs on its own thread so menu controls take
    // effect while a subprocess is waiting.
    rt.attach_arc();
    let rt_for_loop = rt.clone();
    let runtime_thread = std::thread::spawn(move || rt_for_loop.run());
    let state = AppState {
        rt,
        runtime_thread: Mutex::new(Some(runtime_thread)),
        shutting_down: AtomicBool::new(false),
        polling_item: Mutex::new(None),
    };

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings_cmd,
            open_log,
            get_status,
            report_settings
        ])
        .setup(|app| {
            // A menu-bar app owns no Dock icon. The bundled Info.plist covers
            // Finder launches; the accessory policy also covers unbundled dev
            // runs (menu-bar.md).
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            build_tray(app.handle().clone())?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(move |app, event| match event {
            // Cmd+Q carries no exit code. Closing Settings only hides the
            // window, so this path means the user requested a full quit.
            RunEvent::ExitRequested {
                code: None, api, ..
            } => {
                api.prevent_exit();
                request_quit(app);
            }
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => open_settings(app),
            RunEvent::Exit => {
                // The system can also terminate the application directly.
                shutdown.store(true, Ordering::SeqCst);
                if let Some(handle) = app
                    .state::<AppState>()
                    .runtime_thread
                    .lock()
                    .unwrap()
                    .take()
                {
                    let _ = handle.join();
                }
            }
            _ => {}
        });
}

fn build_tray(app: AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let state: tauri::State<AppState> = app.state();
    let polling_now = {
        let st = state.rt.state.lock().unwrap();
        st.polling
    };

    let initial_status = status_line(&state.rt.status.lock().unwrap());
    let status_item = MenuItem::with_id(
        &app,
        "status",
        initial_status.unwrap_or_default(),
        false,
        None::<&str>,
    )?;
    let polling =
        CheckMenuItem::with_id(&app, "polling", "Polling", true, polling_now, None::<&str>)?;
    let settings_item = MenuItem::with_id(&app, "settings", "Settings…", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(&app)?;
    let quit = MenuItem::with_id(&app, "quit", "Quit Slashie", true, None::<&str>)?;
    let menu = tauri::menu::Menu::with_items(&app, &[&polling, &settings_item, &sep, &quit])?;
    if initial_status.is_some() {
        menu.insert(&status_item, 0)?;
    }

    *state.polling_item.lock().unwrap() = Some(polling.clone());

    let icon = Image::from_bytes(include_bytes!("assets/tray-template.png"))?;
    let dimmed_icon = dim_icon(&icon);
    let dot_idle_icon = dot_icon_at(&icon, 6, 24, dot_alphas(false));
    let dot_active_icon = dot_icon_at(&icon, 6, 24, dot_alphas(true));
    let spin = spin_frames(&icon);
    let tray = TrayIconBuilder::with_id("slashie-tray")
        .icon(icon.clone())
        .icon_as_template(true)
        .tooltip("Slashie")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| handle_menu_event(app, event.id().as_ref()))
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
    // on changes so sampling the frame doesn't churn the native menu.
    std::thread::spawn(move || {
        let mut last_text = initial_status;
        let mut last_polling = polling_now;
        // The builder installed the plain glyph.
        let mut last_frame = Frame::Plain;
        let mut tick: u64 = 0;
        loop {
            let state: tauri::State<AppState> = app.state();
            if state.rt.shutdown.load(Ordering::SeqCst)
                && state
                    .shutting_down
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                // SIGINT/SIGTERM only sets the shared flag; finish the
                // accepted work, then exit the menu-bar host too.
                if let Some(handle) = state.runtime_thread.lock().unwrap().take() {
                    let _ = handle.join();
                }
                app.exit(0);
                return;
            }
            state.rt.refresh_status();
            let status = state.rt.status.lock().unwrap().clone();
            let polling_now = state.rt.state.lock().unwrap().polling;
            let frame = if state.shutting_down.load(Ordering::SeqCst) {
                Frame::Plain
            } else {
                tray_frame(&status, polling_now, Instant::now(), tick)
            };
            tick = tick.wrapping_add(1);
            if frame != last_frame {
                let next_icon = match frame {
                    Frame::Dimmed => &dimmed_icon,
                    Frame::Plain => &icon,
                    Frame::DotIdle => &dot_idle_icon,
                    Frame::DotActive => &dot_active_icon,
                    Frame::Spin(step) => &spin[step],
                };
                if tray
                    .set_icon_with_as_template(Some(next_icon.clone()), true)
                    .is_err()
                {
                    return;
                }
                last_frame = frame;
            }
            let text = status_line(&status);
            if text != last_text {
                let result = match (last_text, text) {
                    (None, Some(text)) => status_item
                        .set_text(text)
                        .and_then(|()| menu.insert(&status_item, 0)),
                    (Some(_), Some(text)) => status_item.set_text(text),
                    (Some(_), None) => menu.remove(&status_item),
                    (None, None) => Ok(()),
                };
                if result.is_err() {
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
            // Advance the spinner faster than the static sampling cadence.
            std::thread::sleep(if matches!(frame, Frame::Spin(_)) {
                SPIN_INTERVAL
            } else {
                SAMPLE_INTERVAL
            });
        }
    });
    Ok(())
}

#[cfg(target_os = "macos")]
fn prevent_app_nap() -> impl Sized {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
    let reason = NSString::from_str("Slashie polls Feishu for commands");
    NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
        &reason,
    )
}

fn dot_alphas(bright: bool) -> [[u8; 4]; 4] {
    if bright {
        [
            [32, 176, 176, 32],
            [176, 255, 255, 176],
            [176, 255, 255, 176],
            [32, 176, 176, 32],
        ]
    } else {
        [
            [12, 72, 72, 12],
            [72, 104, 104, 72],
            [72, 104, 104, 72],
            [12, 72, 72, 12],
        ]
    }
}

/// Stamp a 4x4 dot with its top-left corner at (ox, oy), clamped to the icon.
fn dot_icon_at(base: &Image<'_>, ox: isize, oy: isize, alphas: [[u8; 4]; 4]) -> Image<'static> {
    let mut rgba = base.rgba().to_vec();
    let width = base.width() as isize;
    for (y, row) in alphas.iter().enumerate() {
        for (x, opacity) in row.iter().enumerate() {
            let px = ox + x as isize;
            let py = oy + y as isize;
            if px < 0 || py < 0 || px >= width || py >= width {
                continue;
            }
            let pixel = ((py * width + px) * 4) as usize;
            rgba[pixel..pixel + 4].copy_from_slice(&[0, 0, 0, *opacity]);
        }
    }
    Image::new_owned(rgba, base.width(), base.height())
}

/// The spinner frames: the bright dot orbiting the clear space beside the
/// slash tail, one step per sample while a scan is in flight.
fn spin_frames(base: &Image<'_>) -> Vec<Image<'static>> {
    let alphas = dot_alphas(true);
    (0..SPIN_STEPS)
        .map(|step| {
            let angle = step as f64 * std::f64::consts::TAU / SPIN_STEPS as f64;
            let cx = SPIN_CENTER.0 + SPIN_RADIUS * angle.cos();
            let cy = SPIN_CENTER.1 + SPIN_RADIUS * angle.sin();
            dot_icon_at(
                base,
                cx.round() as isize - 1,
                cy.round() as isize - 1,
                alphas,
            )
        })
        .collect()
}

/// Template icons carry their shape in alpha; dimming it signals paused.
fn dim_icon(base: &Image<'_>) -> Image<'static> {
    let dim = |v: u8| ((v as u32 * 35) / 100) as u8;
    let rgba: Vec<u8> = base
        .rgba()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|px| [dim(px[0]), dim(px[1]), dim(px[2]), dim(px[3])])
        .collect();
    Image::new_owned(rgba, base.width(), base.height())
}

fn handle_menu_event(app: &AppHandle, id: &str) {
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
        "settings" => open_settings(app),
        "quit" => request_quit(app),
        _ => {}
    }
}

/// Shared shutdown: request it, wait for the runtime to drain accepted work,
/// then exit. Idempotent so menu quit, Cmd+Q, and system termination agree.
fn request_quit(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state
        .shutting_down
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    state.rt.status.lock().unwrap().phase = Some(crate::status::Phase::Quitting);
    state.rt.shutdown.store(true, Ordering::SeqCst);
    let app = app.clone();
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        if let Some(handle) = state.runtime_thread.lock().unwrap().take() {
            let _ = handle.join();
        }
        app.exit(0);
    });
}

fn open_settings(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.shutting_down.load(Ordering::SeqCst) {
        return;
    }
    let show = || {
        let window = match app.get_webview_window("settings") {
            Some(window) => window,
            None => {
                let window = WebviewWindowBuilder::new(
                    app,
                    "settings",
                    WebviewUrl::App("index.html".into()),
                )
                .title("Slashie Settings")
                .inner_size(600.0, 520.0)
                .center()
                .build()?;
                let hide_on_close = window.clone();
                window.on_window_event(move |event| {
                    // A menu-bar app keeps running when Settings closes.
                    // Reloading discards unsaved edits, as documented.
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = hide_on_close.hide();
                        let _ = hide_on_close.eval("window.location.reload()");
                    }
                });
                window
            }
        };
        window.unminimize()?;
        window.show()?;
        window.set_focus()
    };
    if let Err(error) = show() {
        state
            .rt
            .logger
            .log(&format!("cannot open Settings: {error}"));
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
    /// This binary, so the window can name the `route` subcommands' path.
    cli_path: String,
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> SettingsPayload {
    SettingsPayload::load(&state.rt.home)
}

impl SettingsPayload {
    fn load(home: &std::path::Path) -> Self {
        let path = home.join("config.json");
        let snapshot = std::fs::read_to_string(&path).unwrap_or_default();
        let config = Config::parse(&snapshot);
        Self {
            config_unparsable: config.is_err(),
            draft: SettingsDraft::from_config(&config.unwrap_or_default()),
            snapshot,
            config_path: path.display().to_string(),
            log_available: home.join("slashie.log").exists(),
            cli_path: std::env::current_exe()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        }
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
        crate::settings::SaveOutcome::Saved => SaveReply::Saved,
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

/// Bounded Settings-window diagnostics for the operational log. The window
/// has no console of its own; a load failure must remain observable.
#[tauri::command]
fn report_settings(state: State<'_, AppState>, message: String) {
    let message: String = message.chars().take(300).collect();
    state
        .rt
        .logger
        .log(&format!("settings window: {}", message));
}

#[derive(serde::Serialize)]
struct StatusPayload {
    polling: bool,
    status_text: Option<&'static str>,
    /// A scan is in flight right now.
    scanning: bool,
    /// Whole seconds since the last complete scan, when one exists.
    last_scan_age_secs: Option<u64>,
    poll_interval_secs: u64,
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
        scanning: status.scanning,
        last_scan_age_secs: status
            .last_successful_scan
            .map(|scan| scan.elapsed().as_secs()),
        poll_interval_secs: state.rt.config.lock().unwrap().poll_interval,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{OperatorFault, Phase, RetryKind};

    #[test]
    fn frames_distinguish_paused_fault_and_polling() {
        let now = Instant::now();
        // Paused dims the glyph regardless of other state.
        assert_eq!(
            tray_frame(&RuntimeStatus::default(), false, now, 0),
            Frame::Dimmed
        );
        // Healthy polling rests with the dim dot.
        assert_eq!(
            tray_frame(&RuntimeStatus::default(), true, now, 0),
            Frame::DotIdle
        );

        // A completed scan flashes, then rests again.
        let mut status = RuntimeStatus {
            last_successful_scan: Some(now),
            ..Default::default()
        };
        assert_eq!(tray_frame(&status, true, now, 0), Frame::DotActive);
        assert_eq!(
            tray_frame(&status, true, now + SCAN_FLASH_DURATION, 0),
            Frame::DotIdle
        );

        // A scan in flight spins the dot around the slash.
        status.scanning = true;
        assert_eq!(tray_frame(&status, true, now, 0), Frame::Spin(0));
        assert_eq!(tray_frame(&status, true, now, 1), Frame::Spin(1));
        assert_eq!(tray_frame(&status, true, now, 7), Frame::Spin(7));
        assert_eq!(tray_frame(&status, true, now, 8), Frame::Spin(0));
        assert_eq!(tray_frame(&status, false, now, 0), Frame::Dimmed);

        // The status line carries phases and faults; the dot drops out.
        status.scanning = false;
        status.last_successful_scan = Some(now);
        status.phase = Some(Phase::Starting);
        assert_eq!(tray_frame(&status, true, now, 0), Frame::Plain);
        status.phase = Some(Phase::Quitting);
        assert_eq!(tray_frame(&status, true, now, 0), Frame::Plain);
        status.phase = None;
        status.operator_action = Some(OperatorFault::SignInToFeishu);
        assert_eq!(tray_frame(&status, true, now, 0), Frame::Plain);
        status.operator_action = None;
        status.retrying = Some(RetryKind::RateLimited);
        assert_eq!(tray_frame(&status, true, now, 0), Frame::Plain);
    }
}
