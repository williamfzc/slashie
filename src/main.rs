//! `slashie` entry point. This slice ships headless mode over the shared
//! runtime with a mock Feishu transport; the macOS tray and Settings surface
//! land after the slice passes review (DESIGN.md "Desktop build").

mod config;
mod lock;
mod logging;
mod runtime;
mod state;
mod transport;
mod worker;

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
             --headless        Run the shared runtime in the foreground (the only\n\
                               mode in this slice; the macOS tray comes later).\n\
             --poll <on|off>   Update the persisted polling setting under the\n\
                               lock, then exit.\n\
             --mock <FILE>     Use the mock Feishu transport backed by a JSON\n\
                               fixture instead of the Lark CLI (slice scope).\n\
             --help            Print this help.\n\
             --version         Print the version.",
        VERSION
    );
}

fn fail(msg: &str) -> ! {
    eprintln!("slashie: {}", msg);
    std::process::exit(2);
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
                let v = args.next().unwrap_or_else(|| fail("--mock requires a fixture path"));
                mock_path = Some(PathBuf::from(v));
            }
            other => fail(&format!("unknown argument {:?}; try --help", other)),
        }
    }
    // Headless is the default for this slice build; the flag is accepted so
    // the documented launch line already works.
    let _ = headless;

    let home = runtime_home();
    std::fs::create_dir_all(&home).unwrap_or_else(|e| fail(&format!("cannot create {}: {}", home.display(), e)));

    // Exclusive instance lock, held until shutdown; conflict reports and exits.
    let _guard = match lock::acquire(&home) {
        lock::LockResult::Acquired(g) => g,
        lock::LockResult::Conflict => fail(&format!(
            "Slashie is already running (lock held at {}).",
            home.join("app.lock").display()
        )),
        lock::LockResult::Failed(m) => fail(&m),
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    SHUTDOWN_PTR.store(Arc::as_ptr(&shutdown) as usize, Ordering::SeqCst);
    unsafe {
        let handler = on_signal as extern "C" fn(libc::c_int) as usize;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }

    let (transport, identity): (Arc<dyn transport::Transport>, String) = match &mock_path {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .unwrap_or_else(|e| fail(&format!("cannot read mock fixture {}: {}", path.display(), e)));
            let mock = Arc::new(
                transport::MockTransport::from_json(&text).unwrap_or_else(|e| fail(&e)),
            );
            let id = mock.self_open_id().to_string();
            let t: Arc<dyn transport::Transport> = mock;
            (t, id)
        }
        None if poll.is_some() => {
            // Control invocation: no transport is needed to update the
            // persisted polling setting under the lock.
            let ok = runtime::apply_poll_control(&home, poll.unwrap())
                .unwrap_or_else(|e| fail(&e));
            println!("polling set to {}", if ok { "on" } else { "off" });
            return;
        }
        None => fail(
            "the real Lark CLI adapter is follow-up scope; this slice requires --mock <fixture>",
        ),
    };

    let logger = Arc::new(logging::Logger::new(&home, true));
    let rt = runtime::Runtime::init(
        home,
        transport,
        identity,
        shutdown.clone(),
        logger.clone(),
        poll,
    )
    .unwrap_or_else(|e| fail(&e));

    if let Some(reason) = rt.intake_blocked.lock().unwrap().clone() {
        logger.log(&format!("ERROR intake blocked: {}", reason));
        eprintln!("slashie: intake blocked: {} (operator action required)", reason);
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

    logger.log("slashie starting (headless, mock transport)");
    rt.run();
}
