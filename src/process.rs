//! Process-tree isolation and termination shared by the command worker and
//! the Feishu adapter (DESIGN.md "Command execution"): isolate the child at
//! spawn, then apply the graceful-then-force contract — SIGTERM, wait up to
//! two seconds, SIGKILL on Unix; `taskkill /PID <pid> /T` and then
//! `taskkill /PID <pid> /T /F` on Windows.

use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// How long the graceful step gets before survivors are force-killed.
const KILL_WAIT_MS: u64 = 2000;

/// Put the child in its own process group at spawn (Unix), so the whole
/// group can be signaled without a setpgid race. A no-op on Windows, where
/// tree termination walks the process tree instead.
pub fn isolate(_cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        _cmd.process_group(0);
    }
}

/// Graceful termination of the child's whole tree, then wait up to two
/// seconds, then force-kill survivors. SIGTERM/SIGKILL against the process
/// group on Unix; `taskkill /T` without and with `/F` on Windows. The
/// caller still reaps the direct child.
pub fn terminate_tree(child: &mut Child) {
    let pid = child.id();
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .status();
    }
    let wait_until = Instant::now() + Duration::from_millis(KILL_WAIT_MS);
    while Instant::now() < wait_until {
        if tree_exit_observed(child) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !tree_exit_observed(child) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .status();
        }
    }
}

/// Whether the tree has been observed to exit: on Unix the errno probe
/// `kill(-pgid, 0)` covers the whole group; on Windows only the direct
/// child can be watched (`try_wait` needs a mutable handle), and
/// `taskkill /T /F` removes any pipe-holding survivor it might have left
/// behind.
fn tree_exit_observed(child: &mut Child) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(-(child.id() as i32), 0) != 0 }
    }
    #[cfg(windows)]
    {
        child.try_wait().ok().flatten().is_some()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn shutdown_terminates_a_sleeping_tree() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        isolate(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        terminate_tree(&mut child);
        // terminate_tree returned once the group could no longer be
        // signalled. On macOS a terminated process stays that way for a
        // moment before it is reapable: `kill(-pgid, 0)` fails with EPERM
        // before the child turns into a zombie. Let the status settle
        // instead of demanding it in the same instant.
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut status = child.try_wait().unwrap();
        while status.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
            status = child.try_wait().unwrap();
        }
        assert!(status.is_some(), "tree did not terminate");
    }
}
