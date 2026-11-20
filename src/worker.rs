//! Command execution (DESIGN.md, "Command execution").
//!
//! Runs the trusted route program with /bin/sh -c, passing message data only
//! through quoted SLASHIE_ARGS / SLASHIE_PROMPT environment values so message
//! text can never change the shell program. Each subprocess gets its own Unix
//! process group; timeouts and shutdown terminate then kill the group.

use crate::config::Route;
use crate::transport::Message;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const MAX_CAPTURE_BYTES: usize = 64 * 1024;
pub const MAX_REPLY_CHARS: usize = 4000;
const GROUP_KILL_WAIT_MS: u64 = 2000;

pub struct Outcome {
    pub reply_text: String,
    /// "exit", "timeout", "shutdown"
    pub termination: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
}

/// Message data passed to the shell. `args` is the text after the slash
/// command with internal line breaks preserved. `prompt` is that text, a
/// blank line, and the source context.
pub fn build_inputs(route_prefix: &str, msg: &Message) -> (String, String) {
    let text = msg.text.clone().unwrap_or_default();
    let args = text
        .split_once(route_prefix)
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_default();
    let mut source = format!("message: {}", msg.id);
    if let Some(link) = &msg.link {
        source.push_str(&format!("\nlink: {}", link));
    }
    let prompt = format!("{}\n\n{}", args, source);
    (args, prompt)
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

fn kill_process_group(pgid: i32, sig: i32) {
    unsafe {
        libc::kill(-pgid, sig);
    }
}

/// Run the route command. `shutdown` is polled so shutdown can stop intake,
/// terminate the active group, and reap it.
pub fn run_command(
    route: &Route,
    args: &str,
    prompt: &str,
    process_path: &[String],
    home_dir: &str,
    shutdown: Arc<AtomicBool>,
) -> Outcome {
    let started = std::time::Instant::now();
    let timeout_ms = route.timeout.unwrap_or(crate::config::DEFAULT_TIMEOUT_SECS) * 1000;

    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(&route.cmd);
    cmd.env("SLASHIE_ARGS", args);
    cmd.env("SLASHIE_PROMPT", prompt);
    if !process_path.is_empty() {
        if let Ok(paths) = std::env::join_paths(process_path) {
            cmd.env("PATH", paths);
        }
    }
    let cwd = match &route.dir {
        Some(d) if d.starts_with("~/") => {
            let expanded = format!("{}/{}", home_dir.trim_end_matches('/'), &d[2..]);
            expanded
        }
        Some(d) => d.clone(),
        None => home_dir.to_string(),
    };
    if std::path::Path::new(&cwd).is_dir() {
        cmd.current_dir(&cwd);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Own process group from spawn (std CommandExt; no setpgid race).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

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
    let pgid = child.id() as i32;

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
            if stdout_buf.len() >= MAX_CAPTURE_BYTES {
                break;
            }
            match stdout_pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let take = n.min(MAX_CAPTURE_BYTES - stdout_buf.len());
                    stdout_buf.extend_from_slice(&chunk[..take]);
                }
            }
        }
        stdout_buf
    });
    let t2 = std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            if stderr_buf.len() >= MAX_CAPTURE_BYTES {
                break;
            }
            match stderr_pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let take = n.min(MAX_CAPTURE_BYTES - stderr_buf.len());
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
            kill_process_group(pgid, libc::SIGTERM);
            termination = "shutdown".into();
            break;
        }
        if std::time::Instant::now() >= deadline {
            kill_process_group(pgid, libc::SIGTERM);
            termination = "timeout".into();
            timed_out = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    if timed_out || termination == "shutdown" {
        // Wait up to 2s for the group to die, then SIGKILL survivors.
        let wait_until =
            std::time::Instant::now() + std::time::Duration::from_millis(GROUP_KILL_WAIT_MS);
        while std::time::Instant::now() < wait_until {
            // group_alive: kill(pgid, 0)
            let alive = unsafe { libc::kill(-pgid, 0) } == 0;
            if !alive {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let still = unsafe { libc::kill(-pgid, 0) } == 0;
        if still {
            kill_process_group(pgid, libc::SIGKILL);
        }
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

    Outcome {
        reply_text: compose_reply(success, &stdout, &stderr, timed_out),
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
        }
    }

    #[test]
    fn inputs_preserve_multiline_and_source() {
        let msg = Message {
            id: "om_1".into(),
            sender: "ou_self".into(),
            created_at_ms: 0,
            chat_type: "p2p".into(),
            text: Some("/t line1\nline2\nline3".into()),
            top_level: true,
            recalled: false,
            link: Some("https://example.example/msg".into()),
        };
        let (args, prompt) = build_inputs("/t", &msg);
        assert_eq!(args, " line1\nline2\nline3");
        assert!(prompt.starts_with(" line1\nline2\nline3\n\n"));
        assert!(prompt.contains("message: om_1"));
        assert!(prompt.contains("link: https://example.example/msg"));
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
        assert_eq!(out.reply_text, "Command timed out.");
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
