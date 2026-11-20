//! Settings draft handling shared by the Settings window and validators
//! (menu-bar.md "Settings", DESIGN.md "Configuration and files").
//!
//! The window edits only the polling interval and the route prefix/command
//! pairs. Saving validates the whole draft, preserves per-route working
//! directories and timeouts, preserves fields the window does not edit
//! (lark_cli, process_path), and rejects the save when the file changed
//! since the window loaded it. The runtime picks up the saved file at its
//! next scan; only the channel binding needs a restart.

use crate::config::{
    atomic_write, route_prefix_problem, validate, Config, Route, ALLOWED_POLL_INTERVALS,
};
use std::path::Path;

/// One editable row in the Settings window.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftRoute {
    pub prefix: String,
    pub cmd: String,
    /// Client-side row identity: the base-route index for rows the window
    /// loaded, so an edited row keeps its dir/timeout/session even when its
    /// prefix is renamed. Absent for rows added during this session, which
    /// are new routes and never inherit a dead row's options. The file
    /// itself never stores this; serde defaults it for other producers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_id: Option<u64>,
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
                .enumerate()
                .map(|(i, r)| DraftRoute {
                    prefix: r.prefix.clone(),
                    cmd: r.cmd.clone(),
                    row_id: Some(i as u64),
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
            if let Some(problem) = route_prefix_problem(&r.prefix) {
                errors.push(FieldError {
                    row: Some(i),
                    field: "prefix".into(),
                    message: problem.into(),
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
            .map(|d| {
                // Preserve dir/timeout/session only for rows the window
                // loaded, matched by their stable row identity: a renamed
                // row keeps its options, and a row added this session never
                // inherits a dead row's options by reusing its prefix.
                let preserved = d.row_id.and_then(|id| base.routes.get(id as usize));
                Route {
                    prefix: d.prefix.clone(),
                    cmd: d.cmd.clone(),
                    dir: preserved.and_then(|r| r.dir.clone()),
                    timeout: preserved.and_then(|r| r.timeout),
                    session: preserved.and_then(|r| r.session.clone()),
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
pub fn save_settings(home: &Path, loaded_snapshot: &str, draft: &SettingsDraft) -> SaveOutcome {
    let path = home.join("config.json");
    let current = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return SaveOutcome::Io(format!("cannot read {}: {}", path.display(), e)),
    };
    if current != loaded_snapshot {
        return SaveOutcome::FileChanged;
    }
    let base = match Config::parse(&current) {
        Ok(config) => config,
        Err(_) => return SaveOutcome::Unparsable(path.display().to_string()),
    };
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
                    row_id: None,
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

        let errs = draft(20, &[("/", "echo pong")]).validate();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "prefix");
    }

    #[test]
    fn apply_preserves_route_dir_and_timeout() {
        let mut base = Config::default();
        base.routes[0].dir = Some("~/work".into());
        base.routes[0].timeout = Some(45);
        base.routes.push(Route {
            prefix: "/go".into(),
            cmd: "echo go".into(),
            dir: None,
            timeout: Some(5),
            session: Some("chat".into()),
        });
        let mut d = draft(30, &[("/ping", "echo new-ping"), ("/go", "echo new-go")]);
        // Rows the window loaded carry their base index; /go sits after
        // the four stock presets.
        d.routes[0].row_id = Some(0);
        d.routes[1].row_id = Some(4);
        let out = d.apply_to(&base).unwrap();
        assert_eq!(out.poll_interval, 30);
        assert_eq!(out.routes[0].cmd, "echo new-ping");
        assert_eq!(out.routes[0].dir.as_deref(), Some("~/work"));
        assert_eq!(out.routes[0].timeout, Some(45));
        assert_eq!(out.routes[1].dir, None);
        assert_eq!(out.routes[1].timeout, Some(5));
        assert_eq!(out.routes[1].session.as_deref(), Some("chat"));
        // Fields the window does not edit survive.
        assert_eq!(out.process_path, base.process_path);
    }

    #[test]
    fn renamed_row_keeps_options_new_row_never_inherits() {
        let mut base = Config::default();
        base.routes[0].session = Some("chat".into());
        base.routes[0].timeout = Some(120);
        // Rename row 0: same identity, different prefix.
        let mut d = draft(20, &[("/claude2", "claude -p")]);
        d.routes[0].row_id = Some(0);
        let out = d.apply_to(&base).unwrap();
        assert_eq!(out.routes[0].prefix, "/claude2");
        assert_eq!(out.routes[0].session.as_deref(), Some("chat"));
        assert_eq!(out.routes[0].timeout, Some(120));

        // A NEW row reusing the old prefix inherits nothing.
        let mut d2 = draft(20, &[("/ping", "echo pong"), ("/claude", "echo replaced")]);
        d2.routes[1].row_id = None;
        let out = d2.apply_to(&base).unwrap();
        assert_eq!(out.routes[1].session, None, "new rows never inherit");
        assert_eq!(out.routes[1].timeout, None);
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
    fn save_roundtrip_and_next_scan_semantics() {
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
        assert_eq!(
            reloaded.poll_interval, 10,
            "the saved file is what the next scan loads"
        );

        // Saving against a stale snapshot is rejected.
        let d2 = draft(60, &[("/ping", "echo pong")]);
        assert_eq!(
            save_settings(&dir, &snapshot, &d2),
            SaveOutcome::FileChanged
        );

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
