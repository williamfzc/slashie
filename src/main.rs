//! `slashie` entry point. The macOS bundle starts in menu-bar mode (tray
//! feature); `--headless` selects foreground mode, and a headless build
//! (no default features) is foreground-only (DESIGN.md "Launch and
//! shutdown", "Desktop build").

mod config;
mod lark;
mod lock;
mod logging;
mod runtime;
// Settings draft handling is exercised by the Settings window (tray build);
// the headless artifact compiles it so both builds share one validator.
#[allow(dead_code)]
mod settings;
mod state;
// The status copy table is consumed by the menu bar (tray build); the
// headless artifact compiles it so both builds share one copy mapping.
#[allow(dead_code)]
mod status;
mod transport;
mod worker;

#[cfg(feature = "tray")]
mod app;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

const VERSION: &str = env!("CARGO_PKG_VERSION");

static SHUTDOWN_PTR: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_signal(_sig: libc::c_int) {
    let p = SHUTDOWN_PTR.load(Ordering::SeqCst) as *const AtomicBool;
    if !p.is_null() {
        unsafe { (*p).store(true, Ordering::SeqCst) };
    }
}

fn print_help() {
    println!(
        "slashie {}\n\
         Send a slash command in Feishu, run it locally, reply with the outcome.\n\
         \n\
         USAGE:\n\
             slashie [OPTIONS]\n\
         \n\
         OPTIONS:\n\
             --headless        Run the shared runtime in the foreground even in\n\
                               the macOS bundle (default for headless builds).\n\
             --poll <on|off>   Update the persisted polling setting under the\n\
                               lock, then exit.\n\
             --mock <FILE>     Use the mock Feishu transport backed by a JSON\n\
                               fixture instead of the Lark CLI (verification).\n\
             --help            Print this help.\n\
             --version         Print the version.",
        VERSION
    );
}

fn fail(msg: &str) -> ! {
    eprintln!("slashie: {}", msg);
    std::process::exit(2);
}

/// Native alert for a Finder-launched second instance (menu-bar.md).
#[cfg(feature = "tray")]
fn alert_and_exit(msg: &str) -> ! {
    use rfd::MessageDialog;
    MessageDialog::new()
        .set_title("Slashie")
        .set_description(msg)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
    std::process::exit(2);
}

#[cfg(not(feature = "tray"))]
fn alert_and_exit(msg: &str) -> ! {
    fail(msg)
}

fn runtime_home() -> PathBuf {
    if let Ok(dir) = std::env::var("SLASHIE_HOME") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| fail("HOME is not set"));
    PathBuf::from(home).join(".slashie")
}

fn main() {
    let mut headless = false;
    let mut poll: Option<bool> = None;
    let mut mock_path: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            // Help and version exit before opening runtime files (DESIGN.md).
            "--help" | "-h" => {
                print_help();
                return;
            }
            "--version" | "-V" => {
                println!("slashie {}", VERSION);
                return;
            }
            "--headless" => headless = true,
            "--poll" => {
                let v = args
                    .next()
                    .unwrap_or_else(|| fail("--poll requires on|off"));
                poll = Some(match v.as_str() {
                    "on" => true,
                    "off" => false,
                    other => fail(&format!("--poll expects on|off, got {:?}", other)),
                });
            }
            "--mock" => {
                let v = args
                    .next()
                    .unwrap_or_else(|| fail("--mock requires a fixture path"));
                mock_path = Some(PathBuf::from(v));
            }
            other => fail(&format!("unknown argument {:?}; try --help", other)),
        }
    }
    // The macOS bundle starts in menu-bar mode; --headless forces foreground.
    // A headless build is foreground-only.
    #[cfg(not(feature = "tray"))]
    let menu_bar_mode = {
        let _ = headless;
        false
    };
    #[cfg(feature = "tray")]
    let menu_bar_mode = !headless && std::env::var("SLASHIE_HEADLESS").is_err();

    let home = runtime_home();
    std::fs::create_dir_all(&home)
        .unwrap_or_else(|e| fail(&format!("cannot create {}: {}", home.display(), e)));

    // Exclusive instance lock, held until shutdown; a Finder-launched second
    // instance shows the native alert and exits.
    let guard = match lock::acquire(&home) {
        lock::LockResult::Acquired(g) => g,
        lock::LockResult::Conflict => alert_and_exit(&format!(
            "Slashie is already running. (lock held at {})",
            home.join("app.lock").display()
        )),
        lock::LockResult::Failed(m) => fail(&m),
    };
    // The AppLock is held for the process lifetime; nothing reads the file.
    std::mem::forget(guard);

    let shutdown = Arc::new(AtomicBool::new(false));
    SHUTDOWN_PTR.store(Arc::as_ptr(&shutdown) as usize, Ordering::SeqCst);
    unsafe {
        let handler = on_signal as extern "C" fn(libc::c_int) as usize;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }

    // Load (or create) configuration before choosing the transport so both
    // launch paths share the same config file and validator.
    let (config, config_fault) = config::Config::load_or_create(&home)
        .unwrap_or_else(|e| fail(&format!("cannot read config.json: {}", e)));

    let (transport, identity): (Arc<dyn transport::Transport>, String) = match &mock_path {
        Some(path) => {
            let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
                fail(&format!(
                    "cannot read mock fixture {}: {}",
                    path.display(),
                    e
                ))
            });
            let mock =
                Arc::new(transport::MockTransport::from_json(&text).unwrap_or_else(|e| fail(&e)));
            let id = mock.self_open_id().to_string();
            let t: Arc<dyn transport::Transport> = mock;
            (t, id)
        }
        None if poll.is_some() => {
            // Control invocation: no transport is needed to update the
            // persisted polling setting under the lock.
            let ok = runtime::apply_poll_control(&home, poll.unwrap()).unwrap_or_else(|e| fail(&e));
            println!("polling set to {}", if ok { "on" } else { "off" });
            return;
        }
        None => {
            if let Some(fault) = &config_fault {
                fail(&format!(
                    "config.json is invalid: {} (fix the file and restart)",
                    fault
                ));
            }
            let cli = config.lark_cli.clone().unwrap_or_else(|| {
                fail(
                    "config.json has no lark_cli section; set lark_cli.path and \
                     lark_cli.profile to your Lark CLI executable and profile",
                )
            });
            let lark = Arc::new(lark::LarkCliTransport::new(
                &cli,
                &config.process_path,
                shutdown.clone(),
            ));
            let id = lark.verify_identity().unwrap_or_else(|e| {
                fail(&format!(
                    "identity verification failed: {} (operator action required)",
                    e
                ))
            });
            lark.set_self_id(&id);
            let t: Arc<dyn transport::Transport> = lark;
            (t, id)
        }
    };

    let logger = Arc::new(logging::Logger::new(&home, !menu_bar_mode));
    let rt = Arc::new(
        runtime::Runtime::init(
            home,
            runtime::LoadedConfig {
                config,
                fault: config_fault,
            },
            transport,
            identity,
            shutdown.clone(),
            logger.clone(),
            poll,
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    if let Some(reason) = rt.intake_blocked.lock().unwrap().clone() {
        logger.log(&format!("ERROR intake blocked: {}", reason));
        eprintln!(
            "slashie: intake blocked: {} (operator action required)",
            reason
        );
        // Keep the fault observable; a control run still exits nonzero.
        if poll.is_some() {
            std::process::exit(3);
        }
    }

    if poll.is_some() {
        // `--poll on|off` is a control invocation: apply the persisted
        // setting under the lock, then exit (see the slice handoff notes).
        return;
    }

    logger.log(if menu_bar_mode {
        "slashie starting (menu bar)"
    } else {
        "slashie starting (headless)"
    });

    if menu_bar_mode {
        #[cfg(feature = "tray")]
        {
            app::run(rt, shutdown);
        }
    } else {
        rt.run();
    }
}
