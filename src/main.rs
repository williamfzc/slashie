//! `slashie` entry point. The macOS bundle starts in menu-bar mode (tray
//! feature); `--headless` selects foreground mode, and a headless build
//! (no default features) is foreground-only (DESIGN.md "Launch and
//! shutdown", "Desktop build").

mod cli;
mod config;
mod lark;
mod lock;
mod logging;
mod mailbox;
mod runtime;
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
             slashie route <SUBCOMMAND>\n\
         \n\
         OPTIONS:\n\
             --headless        Run the shared runtime in the foreground even in\n\
                               the macOS bundle (default for headless builds).\n\
             --poll <on|off>   Update the persisted polling setting under the\n\
                               lock, then exit.\n\
             --mock <FILE>     Use the mock Feishu transport backed by a JSON\n\
                               fixture instead of the Lark CLI (verification).\n\
             --help            Print this help.\n\
             --version         Print the version.\n\
             --skill           Print the agent skill document for operating\n\
                               Slashie (routes, rules, and behavior).\n\
         \n\
         ROUTE SUBCOMMANDS:\n\
             Edit config.json routes offline; the running runtime applies\n\
             them at its next scan. Safe while Slashie is running.\n\
             route list        Print the route table as JSON.\n\
             route add <PREFIX> --cmd <COMMAND> [--dir <PATH>]\n\
                               [--timeout <SECS>] [--session chat]\n\
                               Add a route.\n\
             route remove <PREFIX>\n\
                               Remove a route.",
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
            // The agent-facing operating contract (cli.rs::skill); like the
            // route subcommands it needs no lock and no transport.
            "--skill" => {
                print!("{}", crate::cli::skill(&runtime_home()));
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
            // Route edits are offline config-file operations: like --help,
            // they finish before the instance lock, so they work while the
            // app is running and can never trigger the second-instance
            // alert (src/cli.rs, DESIGN.md "Configuration and files").
            "route" => {
                match crate::cli::route(&runtime_home(), args) {
                    Ok(text) => println!("{}", text),
                    Err(msg) => fail(&msg),
                }
                return;
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
    // instance shows the native alert and exits. A headless conflict explains
    // the way out in plain text (DESIGN.md "Launch and shutdown").
    let guard = match lock::acquire(&home) {
        lock::LockResult::Acquired(g) => g,
        lock::LockResult::Conflict => {
            if menu_bar_mode {
                alert_and_exit("Slashie is already running.");
            } else {
                fail(
                    "Slashie is already running; stop it first or use its \
                      menu to change polling.",
                );
            }
        }
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

    // `--poll on|off` is a control invocation before any transport exists:
    // it updates the persisted polling setting under the lock and exits,
    // with or without `--mock` (which would otherwise settle pending
    // claims as interrupted — a side effect the flag must not have).
    if let Some(on) = poll {
        let ok = runtime::apply_poll_control(&home, on).unwrap_or_else(|e| fail(&e));
        println!("polling set to {}", if ok { "on" } else { "off" });
        return;
    }

    // Startup faults must not kill the menu bar before it exists: in
    // menu-bar mode a fault yields a placeholder channel, an unbound
    // identity, and the matching operator-action row (menu-bar.md "Status
    // text"). Headless reports the fault and exits.
    enum Startup {
        Ready(Arc<dyn transport::Channel>, String),
        Unconfigured(String, status::OperatorFault),
    }
    let startup: Startup = match &mock_path {
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
            Startup::Ready(mock as Arc<dyn transport::Channel>, id)
        }
        None => {
            let die_or_wait = |msg: String, action: status::OperatorFault| -> Startup {
                if menu_bar_mode {
                    Startup::Unconfigured(msg, action)
                } else {
                    fail(&msg)
                }
            };
            let config_path = home.join("config.json").display().to_string();
            if let Some(fault) = &config_fault {
                die_or_wait(
                    format!(
                        "config.json at {} is invalid: {} (fix the file and restart)",
                        config_path, fault
                    ),
                    status::OperatorFault::CheckSettingsAndRestart,
                )
            } else if let Some(cli) = config.lark_cli.clone() {
                let lark = Arc::new(lark::LarkCliTransport::new(
                    &cli,
                    &config.process_path,
                    shutdown.clone(),
                ));
                match lark.verify_identity() {
                    Ok(id) => {
                        lark.set_self_id(&id);
                        Startup::Ready(lark as Arc<dyn transport::Channel>, id)
                    }
                    Err(e) => die_or_wait(
                        format!(
                            "identity verification failed: {}; authenticate the \
                             Lark CLI (lark-cli login) and restart",
                            e
                        ),
                        status::OperatorFault::SignInToFeishu,
                    ),
                }
            } else {
                die_or_wait(
                    format!(
                        "config.json at {} has no lark_cli section; set lark_cli.path \
                         and lark_cli.profile to your Lark CLI executable and profile",
                        config_path
                    ),
                    status::OperatorFault::CheckSettingsAndRestart,
                )
            }
        }
    };

    let (transport, identity, startup_fault) = match startup {
        Startup::Ready(t, id) => (t, id, None),
        Startup::Unconfigured(msg, action) => {
            let t: Arc<dyn transport::Channel> = Arc::new(transport::UnconfiguredChannel);
            (t, String::new(), Some((msg, action)))
        }
    };

    let logger = Arc::new(logging::Logger::new(&home, !menu_bar_mode));
    let startup_blocked = startup_fault
        .as_ref()
        .map(|(msg, _)| msg.clone())
        .or(config_fault);
    let rt = Arc::new(
        runtime::Runtime::init(
            home,
            runtime::LoadedConfig {
                config,
                fault: startup_blocked,
            },
            transport,
            identity,
            shutdown.clone(),
            logger.clone(),
            poll,
        )
        .unwrap_or_else(|e| fail(&e)),
    );
    // The mailbox runner reaches process_job through this handle.
    rt.attach_arc();
    if let Some((msg, action)) = &startup_fault {
        rt.status.lock().unwrap().operator_action = Some(action.clone());
        logger.log(&format!("ERROR {}", msg));
    }
    if rt.fresh_install {
        logger.log("first run: state created; polling starts paused");
    }
    // Surface any other blocked intake exactly once; startup faults were
    // already logged above and must not appear twice.
    if let Some(reason) = rt.intake_blocked.lock().unwrap().clone() {
        let already_logged = startup_fault
            .as_ref()
            .is_some_and(|(msg, _)| *msg == reason);
        if !already_logged {
            logger.log(&format!("ERROR intake blocked: {}", reason));
            eprintln!(
                "slashie: intake blocked: {} (operator action required)",
                reason
            );
        }
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
