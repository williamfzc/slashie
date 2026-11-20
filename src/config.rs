//! Configuration schema and validation (DESIGN.md, "Configuration and files").
//!
//! Settings and headless mode share this schema. Validation happens once at
//! launch; an invalid configuration blocks intake and requires operator action.

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const DEFAULT_POLL_INTERVAL: u64 = 20;
pub const ALLOWED_POLL_INTERVALS: [u64; 4] = [10, 20, 30, 60];
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_PROCESS_PATH: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LarkCli {
    pub path: String,
    pub profile: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Route {
    /// Unique slash-prefixed token, e.g. "/ping".
    pub prefix: String,
    /// Nonempty shell program run via /bin/sh -c.
    pub cmd: String,
    /// Optional absolute path or a path starting with "~/". Defaults to home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Optional positive integer in seconds. Defaults to 30.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lark_cli: Option<LarkCli>,
    #[serde(default)]
    pub process_path: Vec<String>,
    #[serde(default)]
    pub poll_interval: u64,
    #[serde(default)]
    pub routes: Vec<Route>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            lark_cli: None,
            process_path: DEFAULT_PROCESS_PATH.iter().map(|s| s.to_string()).collect(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            routes: [
                ("/ping", "echo pong", None),
                // Pi treats positional inputs starting with `@` as attachments.
                (
                    "/pi",
                    r#"printf '%s' "$SLASHIE_PROMPT" | if command -v pi-real >/dev/null 2>&1; then pi-real -p; else pi -p; fi"#,
                    Some(120),
                ),
                ("/claude", r#"claude -p -- "$SLASHIE_PROMPT""#, Some(120)),
                (
                    "/codex",
                    r#"codex exec --skip-git-repo-check -- "$SLASHIE_PROMPT""#,
                    Some(120),
                ),
            ]
            .into_iter()
            .map(|(prefix, cmd, timeout)| Route {
                prefix: prefix.into(),
                cmd: cmd.into(),
                dir: None,
                timeout,
            })
            .collect(),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid configuration: {}", self.0)
    }
}
impl std::error::Error for ConfigError {}

fn err<T>(msg: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError(msg.into()))
}

fn is_absolute_or_home(dir: &str) -> bool {
    Path::new(dir).is_absolute() || dir == "~" || dir.starts_with("~/")
}

pub(crate) fn route_prefix_problem(prefix: &str) -> Option<&'static str> {
    if !prefix.starts_with('/') {
        Some("must start with '/'")
    } else if prefix.len() < 2 {
        Some("must include a command name after '/'")
    } else if prefix.split_whitespace().count() != 1 || prefix.contains(['\n', '\t']) {
        Some("must be a single token")
    } else {
        None
    }
}

pub fn validate(cfg: &Config) -> Result<(), ConfigError> {
    if !ALLOWED_POLL_INTERVALS.contains(&cfg.poll_interval) {
        return err(format!(
            "poll_interval must be one of 10, 20, 30, 60; got {}",
            cfg.poll_interval
        ));
    }
    let mut seen: Vec<&str> = Vec::new();
    for route in &cfg.routes {
        let p = route.prefix.as_str();
        if let Some(problem) = route_prefix_problem(p) {
            return err(format!("route prefix {:?} {}", p, problem));
        }
        if seen.contains(&p) {
            return err(format!("duplicate route prefix {:?}", p));
        }
        seen.push(p);
        if route.cmd.trim().is_empty() {
            return err(format!("route {:?} has a blank command", p));
        }
        if let Some(dir) = &route.dir {
            if !is_absolute_or_home(dir) {
                return err(format!(
                    "route {:?} dir {:?} must be absolute or start with '~/",
                    p, dir
                ));
            }
        }
        if let Some(t) = route.timeout {
            if t == 0 {
                return err(format!("route {:?} timeout must be a positive integer", p));
            }
        }
    }
    for dir in &cfg.process_path {
        if !Path::new(dir).is_absolute() {
            return err(format!("process_path entries must be absolute: {:?}", dir));
        }
    }
    if let Some(cli) = &cfg.lark_cli {
        if !Path::new(&cli.path).is_absolute() {
            return err("lark_cli.path must be an absolute path");
        }
        // An empty profile means "use the CLI's default profile"; any other
        // whitespace-only value is a mistake.
        if !cli.profile.is_empty() && cli.profile.trim().is_empty() {
            return err("lark_cli.profile must be nonempty or empty for the CLI default");
        }
    }
    Ok(())
}

impl Config {
    /// Parse and validate configuration text.
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let cfg: Config = serde_json::from_str(text)
            .map_err(|e| ConfigError(format!("cannot parse config.json: {}", e)))?;
        validate(&cfg)?;
        Ok(cfg)
    }

    /// Load config.json, creating the default route presets on a new
    /// installation. Returns the config plus any fault for intake blocking.
    pub fn load_or_create(
        home: &std::path::Path,
    ) -> Result<(Config, Option<String>), std::io::Error> {
        match std::fs::read_to_string(home.join("config.json")) {
            Ok(text) => match Config::parse(&text) {
                Ok(c) => Ok((c, None)),
                Err(e) => Ok((Config::default(), Some(e.to_string()))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let c = Config::default();
                atomic_write(
                    &home.join("config.json"),
                    &serde_json::to_string_pretty(&c).unwrap(),
                )?;
                Ok((c, None))
            }
            Err(e) => Err(e),
        }
    }
}

/// Atomically persist JSON text: write temp file, sync, rename, sync dir.
pub fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        if let Ok(d) = std::fs::File::open(parent) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid_and_has_route_presets() {
        let cfg = Config::default();
        validate(&cfg).unwrap();
        let prefixes: Vec<_> = cfg
            .routes
            .iter()
            .map(|route| route.prefix.as_str())
            .collect();
        assert_eq!(prefixes, ["/ping", "/pi", "/claude", "/codex"]);
        assert_eq!(cfg.routes[0].cmd, "echo pong");
        assert_eq!(cfg.poll_interval, 20);
    }

    #[test]
    fn creates_presets_only_when_config_is_missing() {
        let home = std::env::temp_dir().join(format!("slashie-config-{}", std::process::id()));
        std::fs::create_dir(&home).unwrap();
        let path = home.join("config.json");

        let (created, fault) = Config::load_or_create(&home).unwrap();
        assert_eq!(fault, None);
        assert_eq!(created, Config::default());
        assert_eq!(
            Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap(),
            created
        );

        let original = r#"{"poll_interval":20,"routes":[{"prefix":"/pi","cmd":"echo custom"}]}"#;
        std::fs::write(&path, original).unwrap();
        let (cfg, fault) = Config::load_or_create(&home).unwrap();
        assert_eq!(fault, None);
        assert_eq!(cfg.routes.len(), 1);
        assert_eq!(cfg.routes[0].prefix, "/pi");
        assert_eq!(cfg.routes[0].cmd, "echo custom");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        let empty = r#"{"poll_interval":20,"routes":[]}"#;
        std::fs::write(&path, empty).unwrap();
        let (cfg, fault) = Config::load_or_create(&home).unwrap();
        assert_eq!(fault, None);
        assert!(cfg.routes.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), empty);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn rejects_bad_poll_interval() {
        let cfg = Config {
            poll_interval: 15,
            ..Config::default()
        };
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn rejects_duplicate_prefix() {
        let cfg = Config {
            routes: vec![
                Route {
                    prefix: "/ping".into(),
                    cmd: "echo pong".into(),
                    dir: None,
                    timeout: None,
                },
                Route {
                    prefix: "/ping".into(),
                    cmd: "echo again".into(),
                    dir: None,
                    timeout: None,
                },
            ],
            ..Config::default()
        };
        assert_eq!(
            validate(&cfg),
            Err(ConfigError("duplicate route prefix \"/ping\"".into()))
        );
    }

    #[test]
    fn rejects_blank_command_and_bad_prefix() {
        let mut cfg = Config::default();
        cfg.routes[0].cmd = "  ".into();
        assert!(validate(&cfg).is_err());
        cfg.routes[0].cmd = "echo pong".into();
        cfg.routes[0].prefix = "ping".into();
        assert!(validate(&cfg).is_err());
        cfg.routes[0].prefix = "/ping pong".into();
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn rejects_relative_dir_and_zero_timeout() {
        let mut cfg = Config::default();
        cfg.routes[0].dir = Some("relative/path".into());
        assert!(validate(&cfg).is_err());
        cfg.routes[0].dir = Some("~/work".into());
        validate(&cfg).unwrap();
        cfg.routes[0].timeout = Some(0);
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn empty_lark_profile_means_cli_default() {
        let mut cfg = Config {
            lark_cli: Some(LarkCli {
                path: "/usr/local/bin/lark-cli".into(),
                profile: "".into(),
            }),
            ..Config::default()
        };
        validate(&cfg).unwrap();
        cfg.lark_cli.as_mut().unwrap().profile = "  ".into();
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn roundtrip_serialization() {
        let cfg = Config::default();
        let text = serde_json::to_string_pretty(&cfg).unwrap();
        assert_eq!(Config::parse(&text).unwrap(), cfg);
    }
}
