//! Operational log (DESIGN.md file table): timestamps, IDs, durations,
//! outcomes. Rotate at 2 MiB keeping one previous log. Headless mode also
//! mirrors lines to stderr.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

pub struct Logger {
    path: PathBuf,
    mirror_stderr: bool,
    inner: Mutex<()>,
}

impl Logger {
    pub fn new(home: &Path, mirror_stderr: bool) -> Logger {
        Logger {
            path: home.join("slashie.log"),
            mirror_stderr,
            inner: Mutex::new(()),
        }
    }

    pub fn log(&self, line: &str) {
        let _guard = self.inner.lock();
        self.rotate_if_needed();
        let stamp = chrono_millis_now();
        let full = format!("{} {}\n", rfc3339_millis(stamp), line);
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = f.write_all(full.as_bytes());
        }
        if self.mirror_stderr {
            let _ = std::io::stderr().write_all(full.as_bytes());
        }
    }

    fn rotate_if_needed(&self) {
        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => return,
        };
        if meta.len() < MAX_LOG_BYTES {
            return;
        }
        let prev = self.path.with_extension("log.1");
        let _ = std::fs::rename(&self.path, &prev);
    }
}

pub fn now_ms() -> i64 {
    chrono_millis_now()
}

fn chrono_millis_now() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(_) => 0,
    }
}

/// Minimal RFC 3339 UTC formatting for log lines (no external time crate).
pub fn rfc3339_millis(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (h, m, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    // Civil-from-days algorithm (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mth, d, h, m, s, millis
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339_millis(0), "1970-01-01T00:00:00.000Z");
        // 2026-11-19T14:00:00Z
        assert_eq!(rfc3339_millis(1_795_096_800_000), "2026-11-19T14:00:00.000Z");
    }

    #[test]
    fn log_appends_and_rotates() {
        let dir = std::env::temp_dir().join(format!("slashie-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = Logger::new(&dir, false);
        log.log("hello");
        log.log("world /ping om_1");
        let text = std::fs::read_to_string(dir.join("slashie.log")).unwrap();
        assert!(text.contains("hello") && text.contains("world /ping om_1"));
        // Force rotation.
        std::fs::write(dir.join("slashie.log"), vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();
        log.log("after rotation");
        assert!(dir.join("slashie.log.1").exists());
        let cur = std::fs::read_to_string(dir.join("slashie.log")).unwrap();
        assert!(cur.contains("after rotation"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
