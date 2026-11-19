//! Settings draft handling shared by the Settings window and validators
//! (menu-bar.md "Settings", DESIGN.md "Configuration and files").
//!
//! The window edits only the polling interval and the route prefix/command
//! pairs. Saving validates the whole draft, preserves per-route working
//! directories and timeouts, preserves fields the window does not edit
//! (lark_cli, process_path), and rejects the save when the file changed
//! since the window loaded it. Saved edits take effect after restart.

use crate::config::{atomic_write, validate, Config, Route, ALLOWED_POLL_INTERVALS};
use std::path::Path;

/// One editable row in the Settings window.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftRoute {
    pub prefix: String,
    pub cmd: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsDraft {
    pub poll_interval: u64,
    pub routes: Vec<DraftRoute>,
}

impl SettingsDraft {
    pub fn from_config(cfg: &Config) -> SettingsDraft {
        SettingsDraft {
            poll_interval: cfg.poll_interval,
            routes: cfg
                .routes
                .iter()
                .map(|r| DraftRoute {
                    prefix: r.prefix.clone(),
                    cmd: r.cmd.clone(),
                })
                .collect(),
        }
    }

    /// Field-level validation for in-place marks: which row and field is
    /// invalid, plus the interval check.
    pub fn validate(&self) -> Vec<FieldError> {
        let mut errors = Vec::new();
        if !ALLOWED_POLL_INTERVALS.contains(&self.poll_interval) {
            errors.push(FieldError {
                row: None,
                field: "poll_interval".into(),
                message: format!(
                    "must be one of 10, 20, 30, 60 seconds; got {}",
                    self.poll_interval
                ),
            });
        }
        let mut seen: Vec<&str> = Vec::new();
        for (i, r) in self.routes.iter().enumerate() {
            if r.prefix.is_empty() || !r.prefix.starts_with('/') {
                errors.push(FieldError {
                    row: Some(i),
                    field: "prefix".into(),
                    message: "must start with '/'".into(),
                });
            } else if r.prefix.split_whitespace().count() != 1 {
                errors.push(FieldError {
                    row: Some(i),
                    field: "prefix".into(),
                    message: "must be a single token".into(),
                });
            } else if seen.contains(&r.prefix.as_str()) {
                errors.push(FieldError {
                    row: Some(i),
                    field: "prefix".into(),
                    message: format!("duplicate prefix {}", r.prefix),
                });
            }
            seen.push(r.prefix.as_str());
            if r.cmd.trim().is_empty() {
                errors.push(FieldError {
                    row: Some(i),
                    field: "cmd".into(),
                    message: "command must not be blank".into(),
                });
            }
        }
        errors
    }

    /// Apply the draft onto the loaded config, preserving per-route dir and
    /// timeout by row position and everything the window does not edit.
    /// Fails if the draft is invalid or rows were removed while options were
    /// tracked (position count changed), in which case the caller keeps the
    /// last known options for the surviving rows by prefix where possible.
    pub fn apply_to(&self, base: &Config) -> Result<Config, String> {
        if !self.validate().is_empty() {
            return Err("draft has validation errors".into());
        }
        let mut out = base.clone();
        out.poll_interval = self.poll_interval;
        out.routes = self
            .routes
            .iter()
            .enumerate()
            .map(|(i, d)| {
                // Preserve dir/timeout: same position first (unchanged list),
                // then same prefix (edited rows keep their options).
                let preserved = base
                    .routes
                    .get(i)
                    .filter(|r| r.prefix == d.prefix)
                    .or_else(|| {
                        base.routes
                            .iter()
                            .find(|r| r.prefix == d.prefix && (r.dir.is_some() || r.timeout.is_some()))
                    });
                Route {
                    prefix: d.prefix.clone(),
                    cmd: d.cmd.clone(),
                    dir: preserved.and_then(|r| r.dir.clone()),
                    timeout: preserved.and_then(|r| r.timeout),
                }
            })
            .collect();
        validate(&out).map_err(|e| e.to_string())?;
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FieldError {
    pub row: Option<usize>,
    pub field: String,
    pub message: String,
}

#[derive(Debug, PartialEq)]
pub enum SaveOutcome {
    Saved,
    Invalid(Vec<FieldError>),
    /// The configuration file changed since the window loaded it.
    FileChanged,
    /// The configuration file cannot be parsed; Save stays disabled until
    /// the operator repairs it.
    Unparsable(String),
    Io(String),
}

/// Persist a draft. `loaded_snapshot` is the config.json text the window
/// loaded; if the file's current text differs, the save is rejected.
pub fn save_settings(
    home: &Path,
    loaded_snapshot: &str,
    draft: &SettingsDraft,
) -> SaveOutcome {
    let path = home.join("config.json");
    let current = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return SaveOutcome::Io(format!("cannot read {}: {}", path.display(), e)),
    };
    if current != loaded_snapshot {
        return SaveOutcome::FileChanged;
    }
    let (base, fault) = match Config::load_or_create(home) {
        Ok(v) => v,
        Err(e) => return SaveOutcome::Io(format!("cannot load config: {}", e)),
    };
    if fault.is_some() {
        return SaveOutcome::Unparsable(path.display().to_string());
    }
    let merged = match draft.apply_to(&base) {
        Ok(c) => c,
        Err(_) => return SaveOutcome::Invalid(draft.validate()),
    };
    let text = match serde_json::to_string_pretty(&merged) {
        Ok(t) => t,
        Err(e) => return SaveOutcome::Io(format!("serialize failed: {}", e)),
    };
    match atomic_write(&path, &text) {
        Ok(()) => SaveOutcome::Saved,
        Err(e) => SaveOutcome::Io(format!("write failed: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(interval: u64, routes: &[(&str, &str)]) -> SettingsDraft {
        SettingsDraft {
            poll_interval: interval,
            routes: routes
                .iter()
                .map(|(p, c)| DraftRoute {
                    prefix: p.to_string(),
                    cmd: c.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn valid_draft_has_no_errors() {
        assert!(draft(20, &[("/ping", "echo pong")]).validate().is_empty());
        assert!(draft(60, &[("/ping", "echo pong"), ("/go", "echo go")])
            .validate()
            .is_empty());
    }

    #[test]
    fn blank_command_and_duplicate_prefix_marked_in_place() {
        let d = draft(20, &[("/ping", ""), ("/ping", "echo x")]);
        let errs = d.validate();
        assert_eq!(errs.len(), 2);
        assert!(errs.iter().any(|e| e.row == Some(0) && e.field == "cmd"));
        assert!(errs.iter().any(|e| e.row == Some(1) && e.field == "prefix"));
        // Bad interval is its own field error.
        let d = draft(15, &[("/ping", "echo pong")]);
        let errs = d.validate();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "poll_interval");
    }

    #[test]
    fn apply_preserves_route_dir_and_timeout() {
        let mut base = Config::default();
        base.routes[0].dir = Some("~/work".into());
        base.routes[0].timeout = Some(45);
        base.routes.push(Route {
            prefix: "/go".into(),
            cmd: "echo go".into(),
            dir: Some("/tmp/other".into()),
            timeout: Some(5),
        });
        let d = draft(30, &[("/ping", "echo new-ping"), ("/go", "echo new-go")]);
        let out = d.apply_to(&base).unwrap();
        assert_eq!(out.poll_interval, 30);
        assert_eq!(out.routes[0].cmd, "echo new-ping");
        assert_eq!(out.routes[0].dir.as_deref(), Some("~/work"));
        assert_eq!(out.routes[0].timeout, Some(45));
        assert_eq!(out.routes[1].dir.as_deref(), Some("/tmp/other"));
        assert_eq!(out.routes[1].timeout, Some(5));
        // Fields the window does not edit survive.
        assert_eq!(out.process_path, base.process_path);
    }

    #[test]
    fn apply_rejects_invalid_draft() {
        let base = Config::default();
        assert!(draft(15, &[("/ping", "echo pong")])
            .apply_to(&base)
            .is_err());
        assert!(draft(20, &[("/ping", ""), ("/ping", "x")])
            .apply_to(&base)
            .is_err());
    }

    #[test]
    fn save_roundtrip_and_restart_semantics() {
        let dir = std::env::temp_dir().join(format!("slashie-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (base, _) = Config::load_or_create(&dir).unwrap();
        let snapshot = std::fs::read_to_string(dir.join("config.json")).unwrap();
        assert_eq!(base.routes[0].prefix, "/ping");

        // Valid save succeeds.
        let d = draft(10, &[("/ping", "echo pong")]);
        assert_eq!(save_settings(&dir, &snapshot, &d), SaveOutcome::Saved);
        let (reloaded, _) = Config::load_or_create(&dir).unwrap();
        assert_eq!(reloaded.poll_interval, 10, "takes effect after restart");

        // Saving against a stale snapshot is rejected.
        let d2 = draft(60, &[("/ping", "echo pong")]);
        assert_eq!(save_settings(&dir, &snapshot, &d2), SaveOutcome::FileChanged);

        // A changed snapshot validates against the CURRENT file, not the
        // stale one; matching snapshot saves fine.
        let fresh = std::fs::read_to_string(dir.join("config.json")).unwrap();
        assert_eq!(save_settings(&dir, &fresh, &d2), SaveOutcome::Saved);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unparsable_file_rejects_save_with_path() {
        let dir = std::env::temp_dir().join(format!("slashie-settings-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), "{not json").unwrap();
        let d = draft(20, &[("/ping", "echo pong")]);
        match save_settings(&dir, "{not json", &d) {
            SaveOutcome::Unparsable(path) => {
                assert!(path.ends_with("config.json"));
            }
            other => panic!("expected Unparsable, got {:?}", other),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
