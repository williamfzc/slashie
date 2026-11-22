//! Instance ownership via an exclusive OS lock on `app.lock` (DESIGN.md).
//!
//! The descriptor is held for the process lifetime; the kernel releases it
//! on exit, including crashes. Lock conflicts report an error and exit.
//! `File::try_lock` maps to `flock(LOCK_EX | LOCK_NB)` on Unix and
//! `LockFileEx` on Windows, so one implementation serves both.

use std::fs::OpenOptions;
use std::path::Path;

#[derive(Debug)]
pub struct AppLock {
    _file: std::fs::File,
}

#[derive(Debug)]
pub enum LockResult {
    Acquired(AppLock),
    Conflict,
    Failed(String),
}

pub fn acquire(home: &Path) -> LockResult {
    let path = home.join("app.lock");
    // No truncate: the lock file content is never read; keep it intact.
    let file = match OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => return LockResult::Failed(format!("cannot open {}: {}", path.display(), e)),
    };
    match file.try_lock() {
        Ok(()) => LockResult::Acquired(AppLock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => LockResult::Conflict,
        Err(e) => LockResult::Failed(format!("app.lock lock failed: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_conflicts() {
        let dir = std::env::temp_dir().join(format!("slashie-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = acquire(&dir);
        assert!(matches!(first, LockResult::Acquired(_)));
        let second = acquire(&dir);
        assert!(matches!(second, LockResult::Conflict));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
