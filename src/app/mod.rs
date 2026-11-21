//! macOS menu-bar shell (menu-bar.md): one tray item whose `/` template icon
//! frames express polling state, the native Polling check item, status copy,
//! the single Settings window over bundled local assets, and shared
//! shutdown/quit. Compiled only behind the `tray` feature; all runtime logic
//! lives in the shared runtime behind control handles.

use crate::config::Config;
use crate::runtime::Runtime;
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

/// Sampling cadence of the tray thread; eight levels at this rate give the
/// glyph a one-second breath cycle, twice as fast during a scan.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(125);

/// The menu-bar icon frames (menu-bar.md "Status item and menu").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Frame {
    /// Polling paused: the whole glyph dimmed, completely still — motion
    /// belongs to the switch being on, stillness to it being off.
    Dimmed,
    /// The status line speaks (startup, faults, retries): the plain glyph.
    Plain,
    /// Polling on: the whole glyph breathing, one level per sample of the
    /// sine table — quicker while a scan is in flight, full brightness
    /// held briefly after a complete scan.
    Breath { level: usize },
}

/// Whole-glyph breathing: eight sine-sampled alpha levels from 35% to
/// full, one level per sample (125 ms) — a one-second breath cycle.
const BREATH_LEVELS: usize = 8;
const BREATH_MIN: u32 = 35;

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
        // Quicker breath: the scan is felt.
        return Frame::Breath {
            level: (tick % (BREATH_LEVELS as u64 / 2)) as usize,
        };
    }
    // A completed scan holds full brightness briefly, then the calm breath
    // resumes — the switch is on and the poller is alive.
    if status
        .last_successful_scan
        .is_some_and(|scan| now.duration_since(scan) < SCAN_FLASH_DURATION)
    {
        return Frame::Breath {
            level: BREATH_LEVELS / 2,
        };
    }
    Frame::Breath {
        level: (tick % BREATH_LEVELS as u64) as usize,
    }
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
            edit_config,
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
            // A first run has no way to discover the menu; open Settings so
            // the setup path (lark_cli, polling) starts visibly.
            if app.state::<AppState>().rt.fresh_install {
                open_settings(app.handle());
            }
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
    let polling = CheckMenuItem::with_id(
        &app,
        "polling",
        polling_title(polling_now),
        true,
        polling_now,
        None::<&str>,
    )?;
    let settings_item = MenuItem::with_id(&app, "settings", "Settings…", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(&app)?;
    let quit = MenuItem::with_id(&app, "quit", "Quit Slashie", true, None::<&str>)?;
    let menu = tauri::menu::Menu::with_items(&app, &[&polling, &settings_item, &sep, &quit])?;
    if initial_status.is_some() {
        menu.insert(&status_item, 0)?;
    }

    *state.polling_item.lock().unwrap() = Some(polling.clone());

    let icon = Image::from_bytes(include_bytes!("assets/tray-template.png"))?;
    let dimmed_icon = dim_icon(&icon, 40);
    let breath = breath_icons(&icon);
    // Classify before building so a paused launch never shows the
    // full-brightness glyph for one sampling interval.
    let initial_frame = {
        let status = state.rt.status.lock().unwrap().clone();
        tray_frame(&status, polling_now, Instant::now(), 0)
    };
    let initial_icon = match initial_frame {
        Frame::Dimmed => &dimmed_icon,
        Frame::Plain => &icon,
        Frame::Breath { level } => &breath[level],
    };
    let tray = TrayIconBuilder::with_id("slashie-tray")
        .icon(initial_icon.clone())
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
        let mut last_frame = initial_frame;
        let mut last_polling = polling_now;
        // Exact first pass: the sampling loop below compares against this.
        let initial_toggleable = {
            let state: tauri::State<AppState> = app.state();
            !state.rt.is_intake_blocked()
        };
        let mut last_toggleable = initial_toggleable;
        let _ = polling.set_enabled(initial_toggleable);
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
                    Frame::Breath { level } => &breath[level],
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
                if polling
                    .set_checked(polling_now)
                    .and_then(|()| polling.set_text(polling_title(polling_now)))
                    .is_err()
                {
                    return;
                }
                last_polling = polling_now;
            }
            // An unavailable setting must not be toggleable (menu-bar.md):
            // a blocked intake means the state file cannot be trusted, and
            // a shutdown in progress must not persist new settings.
            let toggleable =
                !state.rt.is_intake_blocked() && !state.shutting_down.load(Ordering::SeqCst);
            if toggleable != last_toggleable {
                if polling.set_enabled(toggleable).is_err() {
                    return;
                }
                last_toggleable = toggleable;
            }
            std::thread::sleep(SAMPLE_INTERVAL);
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

/// One pre-rendered glyph per breath level: the whole `//` alpha scaled
/// along a sine from BREATH_MIN to full brightness.
fn breath_icons(base: &Image<'_>) -> Vec<Image<'static>> {
    (0..BREATH_LEVELS)
        .map(|i| {
            let phase = i as f64 / BREATH_LEVELS as f64;
            let level = BREATH_MIN
                + ((100 - BREATH_MIN) as f64 * (0.5 - 0.5 * (phase * std::f64::consts::TAU).cos()))
                    .round() as u32;
            dim_icon(base, level)
        })
        .collect()
}

/// Template icons carry their shape in alpha; dimming it signals paused.
fn dim_icon(base: &Image<'_>, level: u32) -> Image<'static> {
    let dim = |v: u8| ((v as u32 * level) / 100) as u8;
    let rgba: Vec<u8> = base
        .rgba()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|px| [dim(px[0]), dim(px[1]), dim(px[2]), dim(px[3])])
        .collect();
    Image::new_owned(rgba, base.width(), base.height())
}

/// The menu item names its state in the title; the native checkmark rides
/// along when it renders.
fn polling_title(on: bool) -> &'static str {
    if on {
        "Polling: On"
    } else {
        "Polling: Off"
    }
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
                        let _ = item.set_enabled(!rt_for_finish.is_intake_blocked());
                        // The title and checkmark reflect the saved setting
                        // even when an enabled poller is retrying.
                        let _ = item.set_checked(saved_now);
                        let _ = item.set_text(polling_title(saved_now));
                        if result.is_err() {
                            // Restore the previous state on write failure.
                            let _ = item.set_checked(!desired);
                            let _ = item.set_text(polling_title(!desired));
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
                .inner_size(560.0, 430.0)
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

/// The read-only configuration mirror (menu-bar.md "Settings"): the raw
/// config.json, verbatim. The window never edits; every change goes through
/// the route subcommands or the file itself.
#[derive(serde::Serialize)]
struct SettingsPayload {
    config_text: String,
    config_unparsable: bool,
    log_available: bool,
    /// The Feishu identity verified at startup, when one was.
    identity: String,
    /// One word for how the identity binding stands.
    identity_status: &'static str,
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> SettingsPayload {
    let identity = state.rt.identity.clone();
    let blocked = state.rt.is_intake_blocked();
    SettingsPayload::load(&state.rt.home, &identity, blocked)
}

impl SettingsPayload {
    fn load(home: &std::path::Path, identity: &str, blocked: bool) -> Self {
        let path = home.join("config.json");
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let identity_status = if identity.is_empty() {
            "not verified"
        } else if blocked {
            "unavailable"
        } else {
            "connected"
        };
        Self {
            config_unparsable: Config::parse(&text).is_err(),
            config_text: text,
            log_available: home.join("slashie.log").exists(),
            identity: identity.to_string(),
            identity_status,
        }
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

/// Open config.json in the user's default text editor (menu-bar.md
/// "Settings"): the mirror is read-only, so hand edits happen in the file
/// itself and apply at the next scan.
#[tauri::command]
fn edit_config(state: State<'_, AppState>) -> Result<(), String> {
    let path = state.rt.home.join("config.json");
    if !path.exists() {
        return Err(format!("config not found at {}", path.display()));
    }
    std::process::Command::new("/usr/bin/open")
        .arg("-t")
        .arg(&path)
        .spawn()
        .map_err(|e| format!("cannot open an editor: {}", e))?;
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
    /// Whole seconds until the run loop's next scan, when scheduled.
    next_scan_in_secs: Option<u64>,
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
    let next_scan_in_secs = state
        .rt
        .next_scan_at
        .lock()
        .unwrap()
        .and_then(|wake| wake.checked_duration_since(Instant::now()))
        .map(|d| d.as_secs());
    StatusPayload {
        polling,
        status_text: status_line(&status),
        scanning: status.scanning,
        next_scan_in_secs,
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
        // Paused dims the glyph, completely still at any tick.
        for tick in [0, 3, 6, 9] {
            assert_eq!(
                tray_frame(&RuntimeStatus::default(), false, now, tick),
                Frame::Dimmed
            );
        }

        // Polling on: the whole glyph breathes on a one-second cycle.
        let mut status = RuntimeStatus::default();
        assert_eq!(
            tray_frame(&status, true, now, 0),
            Frame::Breath { level: 0 }
        );
        assert_eq!(
            tray_frame(&status, true, now, 4),
            Frame::Breath { level: 4 }
        );
        assert_eq!(
            tray_frame(&status, true, now, 8),
            Frame::Breath { level: 0 }
        );

        // A scan in flight quickens the breath (half cycle).
        status.scanning = true;
        assert_eq!(
            tray_frame(&status, true, now, 0),
            Frame::Breath { level: 0 }
        );
        assert_eq!(
            tray_frame(&status, true, now, 2),
            Frame::Breath { level: 2 }
        );
        assert_eq!(tray_frame(&status, false, now, 0), Frame::Dimmed);

        // After a complete scan the glyph holds full brightness, then the
        // calm breath resumes.
        status.scanning = false;
        status.last_successful_scan = Some(now);
        assert_eq!(
            tray_frame(&status, true, now, 0),
            Frame::Breath { level: 4 }
        );
        // At now + FLASH the hold has elapsed: the calm breath resumes.
        assert_eq!(
            tray_frame(&status, true, now + SCAN_FLASH_DURATION, 0),
            Frame::Breath { level: 0 }
        );
        assert_eq!(
            tray_frame(&status, true, now + SCAN_FLASH_DURATION, 1),
            Frame::Breath { level: 1 }
        );

        // The status line carries faults; the breath drops out.
        status.last_successful_scan = None;
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
