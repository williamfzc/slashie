//! Command execution (DESIGN.md, "Command execution").
//!
//! Runs the trusted route program with `sh -c` (/bin/sh on Unix; sh from
//! PATH on Windows, supplied by Git for Windows), passing message data only
//! through quoted SLASHIE_ARGS / SLASHIE_PROMPT environment values so
//! message text can never change the shell program. Each subprocess is
//! isolated at spawn; timeouts and shutdown apply the graceful-then-force
//! termination contract (src/process.rs).

use crate::config::Route;
use crate::transport::{ConversationKey, Message};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const MAX_CAPTURE_BYTES: usize = 64 * 1024;
pub const MAX_REPLY_CHARS: usize = 4000;

pub struct Outcome {
    pub reply_text: String,
    /// "exit", "timeout", "shutdown"
    pub termination: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
}

/// Message data passed to the shell. `args` is the text after the slash
/// command with internal line breaks preserved. `prompt` is that text, a
/// blank line, and the source context. `source` is also exposed as
/// `SLASHIE_CONTEXT`.
pub struct CommandInputs {
    pub args: String,
    pub prompt: String,
    pub source: String,
    pub id: String,
    pub link: String,
}

pub fn build_inputs(route_prefix: &str, msg: &Message, key: &ConversationKey) -> CommandInputs {
    let text = msg.text.clone().unwrap_or_default();
    let args = text
        .split_once(route_prefix)
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_default();
    let mut source = format!("message: {}", msg.id);
    source.push_str(&format!("\nchannel: {}", key.channel));
    if !msg.chat_id.is_empty() {
        source.push_str(&format!("\nchat: {} ({})", msg.chat_id, msg.chat_type));
    }
    if let Some(topic) = &key.topic_id {
        source.push_str(&format!("\ntopic: {}", topic));
    }
    if let Some(link) = &msg.link {
        source.push_str(&format!("\nlink: {}", link));
    }
    source.push_str(&format!("\nsender: {}", msg.sender));
    let prompt = format!("{}\n\n{}", args, source);
    CommandInputs {
        args,
        prompt,
        source,
        id: msg.id.clone(),
        link: msg.link.clone().unwrap_or_default(),
    }
}

/// Session routing for one command. `dir` is the per-chat workspace when the
/// route opted into `session: "chat"`; `continuing` says that this chat route
/// already completed a command and may resume its agent session.
#[derive(Debug, Clone, Default)]
pub struct SessionEnv {
    pub channel: String,
    pub chat_id: String,
    pub topic_id: String,
    pub dir: Option<std::path::PathBuf>,
    pub continuing: bool,
}

/// Build the reply text per the product contract: stdout on success, a short
/// acknowledgment for empty output, reason plus diagnostics (stderr first)
/// on failure or timeout, truncation notice beyond the character limit.
pub fn compose_reply(outcome_success: bool, stdout: &[u8], stderr: &[u8], timeout: bool) -> String {
    let limit = MAX_REPLY_CHARS;
    let mut text = if outcome_success {
        let s = String::from_utf8_lossy(stdout);
        let s = s.trim();
        if s.is_empty() {
            "Done. (no output)".to_string()
        } else {
            s.to_string()
        }
    } else {
        let diag_src = if stderr.is_empty() { stdout } else { stderr };
        let reason = if timeout {
            "Command timed out.".to_string()
        } else {
            "Command failed.".to_string()
        };
        let diag = String::from_utf8_lossy(diag_src);
        let diag = diag.trim();
        if diag.is_empty() {
            reason
        } else {
            format!("{}\n{}", reason, diag)
        }
    };
    if text.chars().count() > limit {
        let notice = format!("[truncated: output exceeded {} characters]", limit);
        // Keep the complete reply (body, newline, notice) within the limit.
        let keep = limit.saturating_sub(notice.chars().count() + 1);
        let body: String = text.chars().take(keep).collect();
        text = format!("{}\n{}", body, notice);
    }
    text
}

/// Compatibility wrapper used by command-level tests and callers without a
/// chat session.
#[cfg(test)]
pub fn run_command(
    route: &Route,
    args: &str,
    prompt: &str,
    process_path: &[String],
    home_dir: &str,
    shutdown: Arc<AtomicBool>,
) -> Outcome {
    let inputs = CommandInputs {
        args: args.to_string(),
        prompt: prompt.to_string(),
        source: String::new(),
        id: String::new(),
        link: String::new(),
    };
    run_command_with(
        route,
        &inputs,
        &SessionEnv::default(),
        process_path,
        home_dir,
        shutdown,
    )
}

/// Run the route command with the message and session context. `shutdown` is
/// polled so shutdown can stop intake, terminate the active group, and reap it.
pub fn run_command_with(
    route: &Route,
    inputs: &CommandInputs,
    session: &SessionEnv,
    process_path: &[String],
    home_dir: &str,
    shutdown: Arc<AtomicBool>,
) -> Outcome {
    let started = std::time::Instant::now();
    let timeout_ms = route.timeout.unwrap_or(crate::config::DEFAULT_TIMEOUT_SECS) * 1000;

    // The route contract is a POSIX shell program. Unix keeps the absolute
    // /bin/sh; Windows resolves `sh` from PATH (Git for Windows supplies it).
    #[cfg(windows)]
    let mut cmd = Command::new("sh");
    #[cfg(not(windows))]
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(&route.cmd);
    cmd.env("SLASHIE_ARGS", &inputs.args);
    cmd.env("SLASHIE_PROMPT", &inputs.prompt);
    cmd.env("SLASHIE_ID", &inputs.id);
    cmd.env("SLASHIE_LINK", &inputs.link);
    cmd.env("SLASHIE_CONTEXT", &inputs.source);
    cmd.env("SLASHIE_CHANNEL", &session.channel);
    cmd.env("SLASHIE_CHAT", &session.chat_id);
    cmd.env("SLASHIE_TOPIC", &session.topic_id);
    cmd.env(
        "SLASHIE_CONTINUE",
        if session.continuing { "1" } else { "0" },
    );
    cmd.env(
        "SLASHIE_SESSION_DIR",
        session
            .dir
            .as_ref()
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_default(),
    );
    if !process_path.is_empty() {
        if let Ok(paths) = std::env::join_paths(process_path) {
            cmd.env("PATH", paths);
        }
    }
    let cwd = if let Some(dir) = session.dir.as_ref().filter(|d| d.is_dir()) {
        dir.to_string_lossy().to_string()
    } else {
        match &route.dir {
            Some(d) if d.starts_with("~/") => {
                let expanded = format!("{}/{}", home_dir.trim_end_matches('/'), &d[2..]);
                expanded
            }
            Some(d) => d.clone(),
            None => home_dir.to_string(),
        }
    };
    if std::path::Path::new(&cwd).is_dir() {
        cmd.current_dir(&cwd);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Isolated at spawn: own process group on Unix, plain child on Windows
    // (tree termination walks the tree instead).
    crate::process::isolate(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Outcome {
                reply_text: compose_reply(
                    false,
                    &[],
                    format!("spawn failed: {}", e).as_bytes(),
                    false,
                ),
                termination: "exit".into(),
                exit_code: None,
                duration_ms: started.elapsed().as_millis() as u64,
            };
        }
    };
    // Close stdin to signal EOF (no stdin input is provided).
    drop(child.stdin.take());

    let mut stdout_pipe = child.stdout.take().unwrap();
    let mut stderr_pipe = child.stderr.take().unwrap();
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();

    // Drain both pipes concurrently, bounded in size.
    let t1 = std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match stdout_pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // Drain to EOF even past the capture cap: closing the
                    // pipe early would kill a productive child with SIGPIPE
                    // and turn a successful command into "Command failed."
                    let take = n.min(MAX_CAPTURE_BYTES.saturating_sub(stdout_buf.len()));
                    stdout_buf.extend_from_slice(&chunk[..take]);
                }
            }
        }
        stdout_buf
    });
    let t2 = std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match stderr_pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // Same drain-past-the-cap contract as stdout.
                    let take = n.min(MAX_CAPTURE_BYTES.saturating_sub(stderr_buf.len()));
                    stderr_buf.extend_from_slice(&chunk[..take]);
                }
            }
        }
        stderr_buf
    });

    let deadline = started + std::time::Duration::from_millis(timeout_ms);
    let termination;
    let mut timed_out = false;
    let mut status = None;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                status = Some(st);
                termination = "exit".to_string();
                break;
            }
            Ok(None) => {}
            Err(_) => {
                termination = "exit".to_string();
                break;
            }
        }
        if shutdown.load(Ordering::SeqCst) {
            termination = "shutdown".into();
            break;
        }
        if std::time::Instant::now() >= deadline {
            termination = "timeout".into();
            timed_out = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    if timed_out || termination == "shutdown" {
        // SIGTERM the group, wait up to 2s, SIGKILL survivors (taskkill
        // graceful-then-force on Windows).
        crate::process::terminate_tree(&mut child);
    }

    let stdout = t1.join().unwrap_or_default();
    let stderr = t2.join().unwrap_or_default();
    // Reap the direct child regardless of path.
    let exit_code = match status {
        Some(st) => st.code(),
        None => {
            let _ = child.wait();
            None
        }
    };
    let success = !timed_out && termination != "shutdown" && exit_code == Some(0);

    let reply_text = if termination == "shutdown" {
        // A quit killed this command (running or queued); that is not an
        // ordinary command failure and must read as one to the user.
        "Canceled: Slashie is shutting down.".to_string()
    } else {
        compose_reply(success, &stdout, &stderr, timed_out)
    };
    Outcome {
        reply_text,
        termination,
        exit_code,
        duration_ms: started.elapsed().as_millis() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn route(cmd: &str) -> Route {
        Route {
            prefix: "/t".into(),
            cmd: cmd.into(),
            dir: None,
            timeout: None,
            session: None,
        }
    }

    #[test]
    fn inputs_preserve_multiline_and_source() {
        let msg = Message {
            id: "om_1".into(),
            sender: "ou_self".into(),
            created_at_ms: 0,
            chat_type: "p2p".into(),
            chat_id: "oc_test".into(),
            topic_id: None,
            text: Some("/t line1\nline2\nline3".into()),
            top_level: true,
            recalled: false,
            link: Some("https://example.example/msg".into()),
        };
        let key = ConversationKey {
            channel: "feishu".into(),
            chat_id: "oc_test".into(),
            topic_id: Some("omt_test".into()),
        };
        let inputs = build_inputs("/t", &msg, &key);
        assert_eq!(inputs.args, " line1\nline2\nline3");
        assert!(inputs.prompt.starts_with(" line1\nline2\nline3\n\n"));
        assert!(inputs.source.contains("message: om_1"));
        assert!(inputs.source.contains("channel: feishu"));
        assert!(inputs.source.contains("chat: oc_test (p2p)"));
        assert!(inputs.source.contains("topic: omt_test"));
        assert!(inputs.source.contains("link: https://example.example/msg"));
        assert!(inputs.source.contains("sender: ou_self"));
        assert_eq!(inputs.id, "om_1");
        assert_eq!(inputs.link, "https://example.example/msg");
    }

    #[test]
    fn default_agent_routes_start_and_continue_in_the_right_order() {
        let dir = std::env::temp_dir().join(format!("slashie-agent-flags-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for tool in ["claude", "codex", "pi-real"] {
            let path = dir.join(tool);
            std::fs::write(
                &path,
                "#!/bin/sh\nprintf '%s|%s|' \"$0\" \"$*\"\nexec cat\n",
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let cfg = crate::config::Config::default();
        let inputs = CommandInputs {
            args: "".into(),
            prompt: "hello".into(),
            source: "message: om_1".into(),
            id: "om_1".into(),
            link: String::new(),
        };
        let path = vec![dir.to_string_lossy().to_string(), "/bin".to_string()];
        let run = |prefix: &str, continuing: bool| {
            let route = cfg.routes.iter().find(|r| r.prefix == prefix).unwrap();
            let session = SessionEnv {
                channel: "feishu".into(),
                chat_id: "oc_chat".into(),
                topic_id: String::new(),
                dir: None,
                continuing,
            };
            run_command_with(
                route,
                &inputs,
                &session,
                &path,
                "/tmp",
                Arc::new(AtomicBool::new(false)),
            )
            .reply_text
        };
        let claude = dir.join("claude");
        assert_eq!(
            run("/claude", false),
            format!("{}|-p -- hello|", claude.display())
        );
        assert_eq!(
            run("/claude", true),
            format!("{}|-p --continue -- hello|", claude.display())
        );
        let codex = dir.join("codex");
        assert_eq!(
            run("/codex", false),
            format!("{}|exec --skip-git-repo-check -- hello|", codex.display())
        );
        assert_eq!(
            run("/codex", true),
            format!(
                "{}|exec resume --last --skip-git-repo-check -- hello|",
                codex.display()
            )
        );
        let pi = dir.join("pi-real");
        assert_eq!(run("/pi", false), format!("{}|-p|hello", pi.display()));
        assert_eq!(
            run("/pi", true),
            format!("{}|-p --continue|hello", pi.display())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn session_env_and_workspace_are_passed() {
        let dir = std::env::temp_dir().join(format!("slashie-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = SessionEnv {
            channel: "feishu".into(),
            chat_id: "oc_chat".into(),
            topic_id: "omt_topic".into(),
            dir: Some(dir.clone()),
            continuing: true,
        };
        let inputs = CommandInputs {
            args: "a".into(),
            prompt: "p".into(),
            source: "message: om_1".into(),
            id: "om_1".into(),
            link: "https://example.example/msg".into(),
        };
        let shutdown = Arc::new(AtomicBool::new(false));
        let out = run_command_with(
            &route(
                "printf '%s|%s|%s|%s|%s|%s|%s|%s|%s' \"$SLASHIE_CHANNEL\" \"$SLASHIE_CHAT\" \"$SLASHIE_TOPIC\" \"$SLASHIE_CONTINUE\" \"$SLASHIE_CONTEXT\" \"$SLASHIE_ID\" \"$SLASHIE_LINK\" \"$SLASHIE_SESSION_DIR\" \"$(basename \"$PWD\")\"",
            ),
            &inputs,
            &session,
            &[],
            "/tmp",
            shutdown,
        );
        assert_eq!(
            out.reply_text,
            format!(
                "feishu|oc_chat|omt_topic|1|message: om_1|om_1|https://example.example/msg|{}|{}",
                dir.display(),
                dir.file_name().unwrap().to_string_lossy()
            )
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compose_reply_contracts() {
        assert_eq!(compose_reply(true, b"pong\n", b"", false), "pong");
        assert_eq!(
            compose_reply(true, b"   \n", b"", false),
            "Done. (no output)"
        );
        assert_eq!(
            compose_reply(false, b"", b"boom", false),
            "Command failed.\nboom"
        );
        assert_eq!(
            compose_reply(false, b"out only", b"", false),
            "Command failed.\nout only"
        );
        assert_eq!(compose_reply(false, b"", b"", true), "Command timed out.");
        let long = "x".repeat(4500);
        let r = compose_reply(true, long.as_bytes(), b"", false);
        let notice = "[truncated: output exceeded 4000 characters]";
        assert_eq!(
            r.chars().count(),
            4000,
            "complete reply stays within the limit"
        );
        assert!(r.ends_with(notice));
    }

    #[test]
    fn runs_simple_command() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let out = run_command(&route("echo pong"), "", "", &[], "/tmp", shutdown);
        assert_eq!(out.reply_text, "pong");
        assert_eq!(out.termination, "exit");
        assert_eq!(out.exit_code, Some(0));
    }

    #[test]
    fn multiline_args_arrive_as_one_value() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let args = " alpha\nbeta\ngamma";
        // Print the arg with delimiters; single shell word via quotes.
        let cmd = route("printf '[%s]' \"$SLASHIE_ARGS\"");
        let out = run_command(&cmd, args, "", &[], "/tmp", shutdown);
        assert_eq!(out.reply_text, "[ alpha\nbeta\ngamma]");
    }

    #[test]
    fn metacharacters_cannot_change_program() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let args = " rm -rf /; echo pwned";
        let out = run_command(
            &route("echo got:$SLASHIE_ARGS"),
            args,
            "",
            &[],
            "/tmp",
            shutdown,
        );
        assert!(out.reply_text.starts_with("got: rm -rf /; echo pwned"));
        assert!(!out.reply_text.contains("pwned") || out.reply_text.starts_with("got:"));
    }

    #[test]
    fn timeout_kills_group() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut r = route("sleep 30 & sleep 30");
        r.timeout = Some(1);
        let started = std::time::Instant::now();
        let out = run_command(&r, "", "", &[], "/tmp", shutdown);
        assert_eq!(out.termination, "timeout");
        assert!(out.reply_text.starts_with("Command timed out."));
        // The shell may append platform-specific child termination diagnostics.
        // 1s timeout + up to 2s SIGTERM wait, well under the 30s child runtime.
        assert!(started.elapsed().as_secs() < 10);
    }

    #[test]
    fn shutdown_terminates_active_group() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let r = route("sleep 30");
        let sd = shutdown.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            sd.store(true, Ordering::SeqCst);
        });
        let started = std::time::Instant::now();
        let out = run_command(&r, "", "", &[], "/tmp", shutdown);
        h.join().unwrap();
        assert_eq!(out.termination, "shutdown");
        assert!(started.elapsed().as_secs() < 10);
    }

    #[test]
    fn failing_command_reports_diagnostics() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let out = run_command(
            &route("echo out; echo err >&2; exit 3"),
            "",
            "",
            &[],
            "/tmp",
            shutdown,
        );
        assert_eq!(out.exit_code, Some(3));
        assert_eq!(out.reply_text, "Command failed.\nerr");
    }
}
