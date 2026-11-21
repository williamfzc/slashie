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

/// Sampling cadence of the tray thread while the icon is static.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// Frame cadence while a scan is in flight; eight spin frames at this rate
/// make the dot orbit about once a second.
const SPIN_INTERVAL: Duration = Duration::from_millis(100);

/// The menu-bar icon frames (menu-bar.md "Status item and menu").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Frame {
    /// Polling paused: the whole glyph dimmed, breathing slowly so the
    /// paused state is legible as alive-but-idle.
    Dimmed,
    DimmedBright,
    /// The status line speaks (startup, faults, retries): the plain glyph.
    Plain,
    /// Polling on: the dot orbiting the slash, `SPIN_STEPS` per turn.
    /// Bright while a scan is in flight or just completed, dim between
    /// scans — the orbit never stops while the switch is on.
    Spin {
        step: usize,
        bright: bool,
    },
}

/// The comet orbit circles the whole glyph: 16 steps per revolution, the
/// head bright and the tail fading behind it.
const SPIN_STEPS: usize = 16;
const SPIN_CENTER: (f64, f64) = (16.0, 16.0);
const SPIN_RADIUS: f64 = 13.0;
/// Trail dots behind the head, as alpha scale factors.
const TRAIL_SCALES: [f64; 3] = [0.55, 0.3, 0.12];

/// Classify the icon frame for the current runtime state. `tick` counts
/// loop iterations and advances the spinner while a scan is in flight.
fn tray_frame(status: &RuntimeStatus, polling: bool, now: Instant, tick: u64) -> Frame {
    if !polling {
        // Three samples (~750 ms) per breath step: clearly slower than the
        // orbit, so paused never reads as activity.
        return if (tick / 3).is_multiple_of(2) {
            Frame::Dimmed
        } else {
            Frame::DimmedBright
        };
    }
    if status_line(status).is_some() {
        return Frame::Plain;
    }
    let step = |samples: u64| ((tick / samples) % SPIN_STEPS as u64) as usize;
    if status.scanning {
        // Fast orbit: one revolution per second.
        return Frame::Spin {
            step: step(1),
            bright: true,
        };
    }
    // Between scans the orbit continues, slower and dim: the switch is on
    // and the poller is alive. A completed scan keeps it bright briefly.
    let bright = status
        .last_successful_scan
        .is_some_and(|scan| now.duration_since(scan) < SCAN_FLASH_DURATION);
    Frame::Spin {
        step: step(1),
        bright,
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
    let dimmed_icon = dim_icon(&icon, 30);
    let dimmed_bright_icon = dim_icon(&icon, 55);
    let (spin_bright, spin_dim) = spin_frames(&icon);
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
        // Exact first pass: the sampling loop below compares against this.
        let initial_toggleable = {
            let state: tauri::State<AppState> = app.state();
            !state.rt.is_intake_blocked()
        };
        let mut last_toggleable = initial_toggleable;
        let _ = polling.set_enabled(initial_toggleable);
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
                    Frame::DimmedBright => &dimmed_bright_icon,
                    Frame::Plain => &icon,
                    Frame::Spin { step, bright } => {
                        if bright {
                            &spin_bright[step]
                        } else {
                            &spin_dim[step]
                        }
                    }
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
            // An unavailable setting must not be toggleable (menu-bar.md):
            // a blocked intake means the state file cannot be trusted.
            let toggleable = !state.rt.is_intake_blocked();
            if toggleable != last_toggleable {
                if polling.set_enabled(toggleable).is_err() {
                    return;
                }
                last_toggleable = toggleable;
            }
            // Orbit cadence: fast and bright during a scan, slower and dim
            // between scans; static sampling everywhere else.
            std::thread::sleep(match frame {
                Frame::Spin { bright, .. } if bright => SPIN_INTERVAL / 2,
                Frame::Spin { .. } => SPIN_INTERVAL,
                _ => SAMPLE_INTERVAL,
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

/// Stamp a 4x4 dot with its top-left corner at (ox, oy), clamped to the
/// icon. The dot's alpha takes the maximum with what is underneath, so the
/// orbit passing over the glyph merges with it instead of erasing it.
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
            let under = rgba[pixel + 3];
            rgba[pixel + 3] = under.max(*opacity);
        }
    }
    Image::new_owned(rgba, base.width(), base.height())
}

/// The comet frames: a bright head with a fading tail circling the glyph.
/// Two brightness sets — bright (scan in flight or just completed) and dim
/// (between scans). The orbit never stops while the polling switch is on.
fn spin_frames(base: &Image<'_>) -> (Vec<Image<'static>>, Vec<Image<'static>>) {
    let orbit = |bright: bool| {
        (0..SPIN_STEPS)
            .map(|step| {
                // Stamp the head, then the tail fading behind it, each onto
                // the previous result: the lifetime stays 'static after the
                // first stamp because dot_icon_at owns its output.
                let mut frame: Option<Image<'static>> = None;
                for (offset, scale) in std::iter::once((0u64, 1.0)).chain(
                    TRAIL_SCALES
                        .iter()
                        .enumerate()
                        .map(|(i, s)| ((i + 1) as u64, *s)),
                ) {
                    let angle = ((step as f64 - offset as f64) * std::f64::consts::TAU
                        / SPIN_STEPS as f64)
                        .rem_euclid(std::f64::consts::TAU);
                    let cx = SPIN_CENTER.0 + SPIN_RADIUS * angle.cos();
                    let cy = SPIN_CENTER.1 + SPIN_RADIUS * angle.sin();
                    let alphas = dot_alphas(bright)
                        .map(|row| row.map(|v| ((v as f64 * scale) as u8).max(1)));
                    let source: &Image<'_> = frame.as_ref().unwrap_or(base);
                    frame = Some(dot_icon_at(
                        source,
                        cx.round() as isize - 1,
                        cy.round() as isize - 1,
                        alphas,
                    ));
                }
                frame.expect("the trail loop always stamps the head")
            })
            .collect()
    };
    (orbit(true), orbit(false))
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
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> SettingsPayload {
    SettingsPayload::load(&state.rt.home)
}

impl SettingsPayload {
    fn load(home: &std::path::Path) -> Self {
        let path = home.join("config.json");
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        Self {
            config_unparsable: Config::parse(&text).is_err(),
            config_text: text,
            log_available: home.join("slashie.log").exists(),
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
        // Paused dims the glyph and breathes, three samples per step.
        assert_eq!(
            tray_frame(&RuntimeStatus::default(), false, now, 0),
            Frame::Dimmed
        );
        assert_eq!(
            tray_frame(&RuntimeStatus::default(), false, now, 3),
            Frame::DimmedBright
        );
        assert_eq!(
            tray_frame(&RuntimeStatus::default(), false, now, 6),
            Frame::Dimmed
        );
        // Healthy polling always orbits: slow and dim between scans.
        assert_eq!(
            tray_frame(&RuntimeStatus::default(), true, now, 0),
            Frame::Spin {
                step: 0,
                bright: false
            }
        );
        assert_eq!(
            tray_frame(&RuntimeStatus::default(), true, now, 1),
            Frame::Spin {
                step: 1,
                bright: false
            }
        );
        assert_eq!(
            tray_frame(&RuntimeStatus::default(), true, now, 16),
            Frame::Spin {
                step: 0,
                bright: false
            }
        );

        // A completed scan keeps the orbit bright for a while.
        let mut status = RuntimeStatus {
            last_successful_scan: Some(now),
            ..Default::default()
        };
        assert_eq!(
            tray_frame(&status, true, now, 0),
            Frame::Spin {
                step: 0,
                bright: true
            }
        );
        assert_eq!(
            tray_frame(&status, true, now + SCAN_FLASH_DURATION, 0),
            Frame::Spin {
                step: 0,
                bright: false
            }
        );

        // A scan in flight orbits the same path, fast and bright.
        status.scanning = true;
        assert_eq!(
            tray_frame(&status, true, now, 0),
            Frame::Spin {
                step: 0,
                bright: true
            }
        );
        assert_eq!(
            tray_frame(&status, true, now, 1),
            Frame::Spin {
                step: 1,
                bright: true
            }
        );
        assert_eq!(
            tray_frame(&status, true, now, 16),
            Frame::Spin {
                step: 0,
                bright: true
            }
        );
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
