//! `slashie route` subcommands: list, add, and remove route entries in
//! config.json (DESIGN.md, "Configuration and files"). This is the
//! interface agents use to configure routes without hand-editing JSON.
//! The subcommand parses before the instance lock: the runtime reads the
//! file once at startup, so an offline edit never races a running app, and
//! a second process must not trip the second-instance alert.

use crate::config::{atomic_write, route_prefix_problem, Config, Route};
use std::path::Path;

const ROUTE_USAGE: &str = "usage:\n  \
     slashie route list\n  \
     slashie route add <PREFIX> --cmd <COMMAND> [--dir <PATH>] [--timeout <SECS>] [--session chat]\n  \
     slashie route remove <PREFIX>";

/// Run one route subcommand and return its success output or a
/// user-facing error. Exits the process from `main` either way.
pub fn route(home: &Path, args: impl Iterator<Item = String>) -> Result<String, String> {
    std::fs::create_dir_all(home)
        .map_err(|e| format!("cannot create {}: {}", home.display(), e))?;
    let mut args = args;
    match args.next().as_deref() {
        Some("list") => list(home),
        Some("add") => add(home, &mut args),
        Some("remove") => remove(home, &mut args),
        _ => Err(ROUTE_USAGE.into()),
    }
}

/// The route table as JSON, exactly as stored in config.json.
fn list(home: &Path) -> Result<String, String> {
    let cfg = load_or_default(home)?;
    serde_json::to_string_pretty(&cfg.routes).map_err(|e| format!("cannot encode routes: {}", e))
}

fn add(home: &Path, args: &mut impl Iterator<Item = String>) -> Result<String, String> {
    let prefix = args
        .next()
        .ok_or_else(|| format!("route add needs a prefix\n{}", ROUTE_USAGE))?;
    let mut cmd: Option<String> = None;
    let mut dir: Option<String> = None;
    let mut timeout: Option<u64> = None;
    let mut session: Option<String> = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--cmd" => cmd = Some(next_value(args, "--cmd")?),
            "--dir" => dir = Some(next_value(args, "--dir")?),
            "--timeout" => {
                let raw = next_value(args, "--timeout")?;
                timeout = Some(raw.parse().map_err(|_| {
                    format!(
                        "--timeout expects a positive number of seconds, got {:?}",
                        raw
                    )
                })?);
            }
            "--session" => session = Some(next_value(args, "--session")?),
            other => {
                return Err(format!(
                    "unknown route add flag {:?}\n{}",
                    other, ROUTE_USAGE
                ))
            }
        }
    }
    let cmd = cmd.ok_or_else(|| format!("route add needs --cmd\n{}", ROUTE_USAGE))?;
    if let Some(problem) = route_prefix_problem(&prefix) {
        return Err(format!(
            "prefix {:?} {}; prefixes look like /deploy",
            prefix, problem
        ));
    }
    let mut cfg = load_or_default(home)?;
    if cfg.routes.iter().any(|r| r.prefix == prefix) {
        return Err(format!(
            "route {:?} already exists; remove it first with `slashie route remove {}`",
            prefix, prefix
        ));
    }
    // Field rules (dir shape, session value, dir+session conflict, blank
    // command, zero timeout) stay in the shared validator.
    cfg.routes.push(Route {
        prefix: prefix.clone(),
        cmd,
        dir,
        timeout,
        session,
    });
    save_config(home, &cfg)?;
    Ok(format!("route {} added; restart Slashie to apply.", prefix))
}

fn remove(home: &Path, args: &mut impl Iterator<Item = String>) -> Result<String, String> {
    let prefix = args
        .next()
        .ok_or_else(|| format!("route remove needs a prefix\n{}", ROUTE_USAGE))?;
    let mut cfg = load_or_default(home)?;
    let before = cfg.routes.len();
    cfg.routes.retain(|r| r.prefix != prefix);
    if cfg.routes.len() == before {
        return Err(format!("no route {:?} to remove", prefix));
    }
    save_config(home, &cfg)?;
    Ok(format!(
        "route {} removed; restart Slashie to apply.",
        prefix
    ))
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{} needs a value\n{}", flag, ROUTE_USAGE))
}

fn load_or_default(home: &Path) -> Result<Config, String> {
    let (cfg, fault) =
        Config::load_or_create(home).map_err(|e| format!("cannot read config.json: {}", e))?;
    if let Some(fault) = fault {
        return Err(format!(
            "config.json is invalid: {}; repair it before editing routes",
            fault
        ));
    }
    Ok(cfg)
}

/// Validate the whole file with the shared validator, then write atomically.
fn save_config(home: &Path, cfg: &Config) -> Result<(), String> {
    crate::config::validate(cfg).map_err(|e| e.to_string())?;
    atomic_write(
        &home.join("config.json"),
        &serde_json::to_string_pretty(cfg).unwrap(),
    )
    .map_err(|e| format!("cannot write config.json: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("slashie-cli-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run(home: &Path, args: &[&str]) -> Result<String, String> {
        route(home, args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn add_list_remove_round_trip() {
        let home = temp_home("roundtrip");
        run(
            &home,
            &[
                "add",
                "/deploy",
                "--cmd",
                "cd ~/app && ./deploy.sh",
                "--timeout",
                "120",
            ],
        )
        .unwrap();
        run(
            &home,
            &[
                "add",
                "/ask",
                "--cmd",
                "claude -p \"$SLASHIE_PROMPT\"",
                "--session",
                "chat",
            ],
        )
        .unwrap();

        let listed = run(&home, &["list"]).unwrap();
        assert!(listed.contains("\"/deploy\""));
        assert!(listed.contains("cd ~/app && ./deploy.sh"));
        assert!(listed.contains("120"));

        let cfg = load_or_default(&home).unwrap();
        crate::config::validate(&cfg).unwrap();
        let find = |prefix: &str| {
            cfg.routes
                .iter()
                .find(|r| r.prefix == prefix)
                .unwrap_or_else(|| panic!("route {} missing", prefix))
        };
        assert_eq!(find("/deploy").timeout, Some(120));
        assert_eq!(find("/deploy").cmd, "cd ~/app && ./deploy.sh");
        assert_eq!(find("/ask").session.as_deref(), Some("chat"));

        run(&home, &["remove", "/deploy"]).unwrap();
        let cfg = load_or_default(&home).unwrap();
        assert!(cfg.routes.iter().all(|r| r.prefix != "/deploy"));
        assert!(run(&home, &["remove", "/deploy"]).is_err());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn rejects_duplicates_and_bad_prefixes() {
        let home = temp_home("reject");
        run(&home, &["add", "/deploy", "--cmd", "echo hi"]).unwrap();
        let duplicate = run(&home, &["add", "/deploy", "--cmd", "echo again"]).unwrap_err();
        assert!(duplicate.contains("already exists"), "{}", duplicate);
        let bad = run(&home, &["add", "deploy", "--cmd", "echo hi"]).unwrap_err();
        assert!(bad.contains("must start with"), "{}", bad);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn defers_field_rules_to_the_shared_validator() {
        let home = temp_home("validator");
        // Session chat conflicts with dir.
        let err = run(
            &home,
            &[
                "add",
                "/x",
                "--cmd",
                "echo hi",
                "--dir",
                "~/w",
                "--session",
                "chat",
            ],
        )
        .unwrap_err();
        assert!(err.contains("session chat"), "{}", err);
        // Unknown session value.
        let err = run(
            &home,
            &["add", "/y", "--cmd", "echo hi", "--session", "route"],
        )
        .unwrap_err();
        assert!(err.contains("session"), "{}", err);
        // Blank command.
        let err = run(&home, &["add", "/z", "--cmd", "   "]).unwrap_err();
        assert!(err.contains("blank command"), "{}", err);
        // Non-numeric timeout.
        let err = run(
            &home,
            &["add", "/t", "--cmd", "echo hi", "--timeout", "soon"],
        )
        .unwrap_err();
        assert!(err.contains("--timeout"), "{}", err);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn refuses_to_edit_an_unparsable_config() {
        let home = temp_home("unparsable");
        std::fs::write(home.join("config.json"), "{not json").unwrap();
        let err = run(&home, &["list"]).unwrap_err();
        assert!(err.contains("repair it"), "{}", err);
        let err = run(&home, &["add", "/x", "--cmd", "echo hi"]).unwrap_err();
        assert!(err.contains("repair it"), "{}", err);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn usage_errors_name_the_usage_block() {
        let home = temp_home("usage");
        assert!(run(&home, &[]).unwrap_err().contains("usage:"));
        assert!(run(&home, &["frobnicate"]).unwrap_err().contains("usage:"));
        assert!(run(&home, &["add", "/x"]).unwrap_err().contains("--cmd"));
        assert!(run(&home, &["add", "/x", "--cmd"])
            .unwrap_err()
            .contains("value"));
        assert!(run(&home, &["add", "/x", "--cmd", "echo", "--wat"])
            .unwrap_err()
            .contains("--wat"));
        std::fs::remove_dir_all(home).unwrap();
    }
}
