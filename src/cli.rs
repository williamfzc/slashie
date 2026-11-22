//! Offline subcommands that parse before the instance lock (DESIGN.md,
//! "Configuration and files"). `slashie route` lists, adds, and removes
//! route entries in config.json — the interface agents use to configure
//! routes without hand-editing JSON. `slashie log` prints recent
//! operational log lines so a target agent can diagnose its own handoff.
//! Both work while the app runs: route edits hot-reload at the next scan,
//! and the log read never touches runtime state.

use crate::config::{atomic_write, route_prefix_problem, Config, Route};
use std::path::Path;

const ROUTE_USAGE: &str = "usage:\n  \
     slashie route list\n  \
     slashie route add <PREFIX> --cmd <COMMAND> [--dir <PATH>] [--timeout <SECS>] [--session chat]\n  \
     slashie route remove <PREFIX>";

const LOG_USAGE: &str = "usage:\n  \
     slashie log [--last N] [--id MESSAGE_ID]";

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
    Ok(format!("route {} added; applies at the next scan.", prefix))
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
        "route {} removed; applies at the next scan.",
        prefix
    ))
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{} needs a value\n{}", flag, ROUTE_USAGE))
}

/// Recent operational log lines (DESIGN.md file table) for agent
/// self-diagnosis: a target that received `$SLASHIE_ID` can look up what
/// happened to its own handoff. Strictly read-only — the current and
/// rotated log are the only files opened, so this cannot admit, claim, or
/// re-run anything, and it generates no new log lines.
pub fn log(home: &Path, args: impl Iterator<Item = String>) -> Result<String, String> {
    let mut last: usize = 100;
    let mut id: Option<String> = None;
    let mut args = args;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--last" => {
                let raw = next_value(&mut args, "--last")?;
                last = raw
                    .parse()
                    .map_err(|_| format!("--last expects a positive number, got {:?}", raw))?;
                if last == 0 {
                    return Err(format!("--last expects a positive number, got {:?}", raw));
                }
            }
            "--id" => id = Some(next_value(&mut args, "--id")?),
            other => return Err(format!("unknown log flag {:?}\n{}", other, LOG_USAGE)),
        }
    }
    let mut lines: Vec<String> = Vec::new();
    let mut present = false;
    // Rotated first: slashie.log.1 holds the older half of the history.
    for name in ["slashie.log.1", "slashie.log"] {
        if let Ok(text) = std::fs::read_to_string(home.join(name)) {
            present = true;
            lines.extend(text.lines().map(str::to_string));
        }
    }
    if !present {
        return Ok("no log entries yet.".into());
    }
    if let Some(id) = &id {
        lines.retain(|line| line.contains(id.as_str()));
    }
    let start = lines.len().saturating_sub(last);
    let tail = &lines[start..];
    if tail.is_empty() {
        return Ok(format!(
            "no log entries match {:?}.",
            id.as_deref().unwrap_or("")
        ));
    }
    Ok(tail.join("\n"))
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

/// The agent-facing operating contract (DESIGN.md "Configuration and
/// files"), SKILL.md-style so an agent that discovers the binary can teach
/// itself how to operate Slashie.
pub fn skill(home: &Path) -> String {
    format!(
        "---\n\
         name: slashie\n\
         description: Operate Slashie - inspect and edit the Feishu slash-command routes it runs locally.\n\
         ---\n\
         \n\
         Slashie watches the user's Feishu chats for messages that start with a route\n\
         prefix (like `/deploy`) and runs that route's shell command on this machine,\n\
         then hands the request and source message link to that agent or script.\n\
         Slashie acknowledges accepted work and updates its completion reaction.\n\
         The agent decides whether to send text through its own authorized tools;\n\
         not replying is allowed. Only explicit reply_mode=auto forwards stdout.\n\
         \n\
         # Files\n\
         \n\
         - Runtime directory: {}\n\
         - `config.json` in it holds all settings: routes, the poll interval, and\n\
         the Lark CLI binding. `state.json` records the polling on/off setting,\n\
         durable per-message claims, and conversation session mappings.\n\
         `slashie.log` is the operational log; read it with `slashie log`.\n\
         \n\
         # Commands\n\
         \n\
         - `slashie route list` - print the route table as JSON.\n\
         - `slashie route add <PREFIX> --cmd <COMMAND> [--dir <PATH>] [--timeout <SECS>] [--session chat]`\n\
         - `slashie route remove <PREFIX>`\n\
         - `slashie log [--last N] [--id MESSAGE_ID]` - print recent operational log\n\
         entries (timestamps, IDs, outcomes); works while Slashie is running.\n\
         \n\
         # Rules\n\
         \n\
         - PREFIX is unique across routes, starts with `/`, and is a single token.\n\
         - COMMAND is a shell program run with `/bin/sh -c`. It receives the message\n\
         text in `$SLASHIE_ARGS` (text after the prefix) and `$SLASHIE_PROMPT` (that\n\
         text plus the message source). Quote variables where the command expects\n\
         them, e.g. `claude -p -- \"$SLASHIE_PROMPT\"`.\n\
         - `--dir` sets the working directory (absolute or `~/...`, default home).\n\
         - `--timeout` bounds the command in seconds (default 30).\n\
         - `--session chat` keeps one agent session per Feishu conversation; the\n\
         command then runs in a Slashie-owned workspace with `$SLASHIE_CONTINUE`\n\
         set to `1` when it may resume. It cannot be combined with `--dir`.\n\
         \n\
         # Behavior\n\
         \n\
         - Route and interval edits apply at the next scan without restarting\n\
         Slashie; the Lark CLI binding applies only at startup.\n\
         - Use the route subcommands instead of editing config.json by hand so the\n\
         shared validation runs; an unparsable config blocks intake until repaired.\n\
         - Ask the user what they want to automate before changing routes, and show\n\
         the final `slashie route list` afterwards.\n\
         - Log entries describe past work; never re-run work because of one. A\n\
         request repeats only when the user sends a fresh Feishu message.\n\
         - Polling starts paused on a fresh install; route edits do nothing for\n\
         intake until the operator enables `Polling` from the menu-bar icon's\n\
         menu. `slashie --poll on|off` edits the same saved setting but only\n\
         runs while no other Slashie instance holds the lock.\n",
        home.display()
    )
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

    #[test]
    fn log_prints_tail_filters_by_id_and_spans_rotation() {
        let home = temp_home("logtail");
        assert!(log(&home, std::iter::empty())
            .unwrap()
            .contains("no log entries yet."));
        std::fs::write(
            home.join("slashie.log.1"),
            "older om_x line\nolder plain line\n",
        )
        .unwrap();
        std::fs::write(
            home.join("slashie.log"),
            "newer om_y line\nnewer plain line\n",
        )
        .unwrap();
        let out = log(&home, std::iter::empty()).unwrap();
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            [
                "older om_x line",
                "older plain line",
                "newer om_y line",
                "newer plain line"
            ]
        );
        let args = ["--id", "om_x"].iter().map(|s| s.to_string());
        assert_eq!(
            log(&home, args).unwrap().lines().collect::<Vec<_>>(),
            ["older om_x line"]
        );
        let args = ["--last", "2"].iter().map(|s| s.to_string());
        assert_eq!(
            log(&home, args).unwrap().lines().collect::<Vec<_>>(),
            ["newer om_y line", "newer plain line"]
        );
        let args = ["--id", "om_missing"].iter().map(|s| s.to_string());
        assert!(log(&home, args).unwrap().contains("no log entries match"));
        // Usage errors.
        let args = ["--last", "0"].iter().map(|s| s.to_string());
        assert!(log(&home, args).is_err());
        let args = ["--last", "soon"].iter().map(|s| s.to_string());
        assert!(log(&home, args).is_err());
        let args = ["--wat"].iter().map(|s| s.to_string());
        assert!(log(&home, args).is_err());
        let args = ["--id"].iter().map(|s| s.to_string());
        assert!(log(&home, args).is_err());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn skill_document_names_the_runtime_and_contract() {
        let home = temp_home("skill");
        let doc = skill(&home);
        assert!(doc.starts_with("---\nname: slashie"), "{}", doc);
        assert!(doc.contains(&home.display().to_string()));
        for needle in [
            "route list",
            "route add <PREFIX> --cmd <COMMAND>",
            "route remove <PREFIX>",
            "slashie log",
            "SLASHIE_ARGS",
            "SLASHIE_PROMPT",
            "--session chat",
            "next scan",
            "fresh Feishu message",
        ] {
            assert!(doc.contains(needle), "missing {needle} in skill doc");
        }
        std::fs::remove_dir_all(home).unwrap();
    }
}
