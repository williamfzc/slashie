//! Configuration schema and validation (DESIGN.md, "Configuration and files").
//!
//! Settings and headless mode share this schema. Validation happens once at
//! launch; an invalid configuration blocks intake and requires operator action.

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const DEFAULT_POLL_INTERVAL: u64 = 5;
/// Inclusive bounds on the scan interval; the rate between them is an
/// operator choice.
pub const MIN_POLL_INTERVAL: u64 = 1;
pub const MAX_POLL_INTERVAL: u64 = 60;
/// Default delivery rule added to every prompt. The agent still decides;
/// Slashie never forces a reply.
pub const DEFAULT_DELIVERY_HINT: &str = "先读上下文，再决定是否回复原消息。";
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// The only command eligible for managed Codex follow-up steering.
pub const STOCK_CODEX_COMMAND: &str = r#"if [ "$SLASHIE_CONTINUE" = 1 ]; then codex exec resume --last --skip-git-repo-check -- "$SLASHIE_PROMPT"; else codex exec --skip-git-repo-check -- "$SLASHIE_PROMPT"; fi"#;
/// System dirs plus the common Homebrew prefixes, so the stock agent
/// routes (`claude`, `codex`, `pi`) resolve on a normal machine without
/// hand-editing config.json (DESIGN.md "Configuration and files").
#[cfg(unix)]
pub const DEFAULT_PROCESS_PATH: &[&str] = &[
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
    "/usr/local/bin",
    "/opt/homebrew/bin",
];
/// Windows has no canonical tool directories: an empty default leaves the
/// route command inheriting the ambient PATH. Route resolution itself
/// still needs `sh` (Git for Windows) on the binary's own PATH (DESIGN.md).
#[cfg(windows)]
pub const DEFAULT_PROCESS_PATH: &[&str] = &[];

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
    /// Include messages that explicitly mention the bound Feishu user.
    #[serde(default)]
    pub allow_mentions: bool,
    /// Follow-up handling for a managed agent session.
    #[serde(default, skip_serializing_if = "FollowUp::is_queue")]
    pub follow_up: FollowUp,
    /// Opt into one persistent agent session per Feishu chat. The only
    /// supported value is "chat"; the route then runs in a Slashie-owned
    /// workspace and receives SLASHIE_CONTINUE.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FollowUp {
    #[default]
    Queue,
    Steer,
}

impl FollowUp {
    fn is_queue(value: &Self) -> bool {
        matches!(value, Self::Queue)
    }
}

/// Agent owns delivery by default; legacy stdout forwarding is opt-in.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReplyMode {
    #[default]
    Agent,
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelConfig>,
    #[serde(default)]
    pub process_path: Vec<String>,
    #[serde(default = "default_poll_interval")]
    pub poll_interval: u64,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub reply_mode: ReplyMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChannelConfig {
    Feishu { lark_cli: LarkCli },
}

impl Default for Config {
    fn default() -> Self {
        Config {
            channel: None,
            process_path: DEFAULT_PROCESS_PATH.iter().map(|s| s.to_string()).collect(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            reply_mode: ReplyMode::Agent,
            routes: [
                ("/ping", "echo pong", None, None),
                // Pi treats positional inputs starting with `@` as attachments.
                (
                    "/pi",
                    r#"if command -v pi-real >/dev/null 2>&1; then tool=pi-real; else tool=pi; fi
if [ "$SLASHIE_CONTINUE" = 1 ]; then printf '%s' "$SLASHIE_PROMPT" | "$tool" -p --continue; else printf '%s' "$SLASHIE_PROMPT" | "$tool" -p; fi"#,
                    Some(120),
                    Some("chat"),
                ),
                (
                    "/claude",
                    r#"if [ "$SLASHIE_CONTINUE" = 1 ]; then claude -p --continue -- "$SLASHIE_PROMPT"; else claude -p -- "$SLASHIE_PROMPT"; fi"#,
                    Some(120),
                    Some("chat"),
                ),
                (
                    "/codex",
                    STOCK_CODEX_COMMAND,
                    Some(120),
                    Some("chat"),
                ),
            ]
            .into_iter()
            .map(|(prefix, cmd, timeout, session)| Route {
                prefix: prefix.into(),
                cmd: cmd.into(),
                dir: None,
                timeout,
                allow_mentions: false,
                follow_up: FollowUp::Queue,
                session: session.map(str::to_string),
            })
            .collect(),
        }
    }
}

/// An omitted `poll_interval` falls back to the documented default; the
/// type's own zero would fail validation.
fn default_poll_interval() -> u64 {
    DEFAULT_POLL_INTERVAL
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
    if cfg.poll_interval < MIN_POLL_INTERVAL {
        return err(format!(
            "poll_interval must be at least {} seconds; got {}",
            MIN_POLL_INTERVAL, cfg.poll_interval
        ));
    }
    if cfg.poll_interval > MAX_POLL_INTERVAL {
        return err(format!(
            "poll_interval must be at most {} seconds; got {}",
            MAX_POLL_INTERVAL, cfg.poll_interval
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
        if let Some(session) = &route.session {
            if session != "chat" {
                return err(format!(
                    "route {:?} session must be \"chat\" (got {:?})",
                    p, session
                ));
            }
            if route.dir.is_some() {
                return err(format!(
                    "route {:?} with session chat uses its own workspace; remove dir",
                    p
                ));
            }
        }
        if route.follow_up == FollowUp::Steer
            && (p != "/codex"
                || route.session.as_deref() != Some("chat")
                || route.cmd != STOCK_CODEX_COMMAND)
        {
            return err(format!(
                "route {:?} follow_up steer requires session chat and the stock /codex command",
                p
            ));
        }
    }
    for dir in &cfg.process_path {
        if !Path::new(dir).is_absolute() {
            return err(format!("process_path entries must be absolute: {:?}", dir));
        }
    }
    if let Some(ChannelConfig::Feishu { lark_cli: cli }) = &cfg.channel {
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
        let mut value: serde_json::Value = serde_json::from_str(text)
            .map_err(|e| ConfigError(format!("cannot parse config.json: {}", e)))?;
        if let Some(object) = value.as_object_mut() {
            if !object.contains_key("channel") {
                if let Some(lark_cli) = object.remove("lark_cli") {
                    if !lark_cli.is_null() {
                        object.insert(
                            "channel".into(),
                            serde_json::json!({"type": "feishu", "lark_cli": lark_cli}),
                        );
                    }
                }
            }
        }
        let cfg: Config = serde_json::from_value(value)
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
/// The file is user-only (0600): route commands and state may hold
/// operator-sensitive material (DESIGN.md "Configuration and files").
pub fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
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
    fn config_without_default_prompts_still_parses_and_ignores_legacy_key() {
        let cfg = Config::default();
        let text = serde_json::to_string(&cfg).unwrap();
        assert_eq!(Config::parse(&text).unwrap(), cfg);
        // A configuration saved by an older build may still carry the key;
        // it is an unknown property now and must not block intake.
        let legacy = text.replace(
            "}]",
            "}], \"default_prompts\": {\"/ping\": \"Keep it concise\"}",
        );
        assert_eq!(Config::parse(&legacy).unwrap(), cfg);
    }

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
        assert_eq!(DEFAULT_POLL_INTERVAL, 5);
        assert_eq!(cfg.poll_interval, DEFAULT_POLL_INTERVAL);
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
    fn accepts_operator_poll_interval() {
        let cfg = Config {
            poll_interval: 5,
            ..Config::default()
        };
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn accepts_one_second_poll_interval() {
        let cfg = Config {
            poll_interval: 1,
            ..Config::default()
        };
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn omitted_poll_interval_defaults_to_five() {
        let cfg = Config::parse(r#"{"routes":[]}"#).unwrap();
        assert_eq!(cfg.poll_interval, DEFAULT_POLL_INTERVAL);
    }

    #[test]
    fn accepts_sixty_second_poll_interval() {
        let cfg = Config {
            poll_interval: MAX_POLL_INTERVAL,
            ..Config::default()
        };
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn rejects_zero_poll_interval() {
        let cfg = Config {
            poll_interval: 0,
            ..Config::default()
        };
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn rejects_poll_interval_above_the_maximum() {
        let cfg = Config {
            poll_interval: MAX_POLL_INTERVAL + 1,
            ..Config::default()
        };
        assert_eq!(
            validate(&cfg),
            Err(ConfigError(
                "poll_interval must be at most 60 seconds; got 61".into()
            ))
        );
    }

    #[test]
    fn default_agent_routes_use_chat_sessions() {
        let cfg = Config::default();
        let by_prefix = |prefix: &str| cfg.routes.iter().find(|r| r.prefix == prefix).unwrap();
        assert_eq!(by_prefix("/ping").session, None);
        for prefix in ["/pi", "/claude", "/codex"] {
            assert_eq!(
                by_prefix(prefix).session.as_deref(),
                Some("chat"),
                "{prefix} keeps one session per chat"
            );
            assert!(by_prefix(prefix).cmd.contains("SLASHIE_CONTINUE"));
        }
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn rejects_unknown_session_mode_and_session_with_dir() {
        let mut cfg = Config::default();
        cfg.routes[1].session = Some("route".into());
        assert!(validate(&cfg).is_err(), "only chat sessions exist");

        let mut cfg = Config::default();
        cfg.routes[1].dir = Some("~/work".into());
        assert!(
            validate(&cfg).is_err(),
            "a chat workspace cannot also set dir"
        );
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
                    allow_mentions: false,
                    follow_up: FollowUp::Queue,
                    session: None,
                },
                Route {
                    prefix: "/ping".into(),
                    cmd: "echo again".into(),
                    dir: None,
                    timeout: None,
                    allow_mentions: false,
                    follow_up: FollowUp::Queue,
                    session: None,
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
        // Each platform's own notion of an absolute path.
        #[cfg(unix)]
        let cli_path = "/usr/local/bin/lark-cli";
        #[cfg(windows)]
        let cli_path = "C:\\Program Files\\lark-cli\\lark-cli.cmd";
        let mut cfg = Config {
            channel: Some(ChannelConfig::Feishu {
                lark_cli: LarkCli {
                    path: cli_path.into(),
                    profile: "".into(),
                },
            }),
            ..Config::default()
        };
        validate(&cfg).unwrap();
        let Some(ChannelConfig::Feishu { lark_cli }) = cfg.channel.as_mut() else {
            panic!("expected Feishu channel")
        };
        lark_cli.profile = "  ".into();
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn agent_delivery_is_default_and_auto_is_explicit() {
        let cfg = Config::parse(r#"{"poll_interval":10,"routes":[]}"#).unwrap();
        assert_eq!(cfg.reply_mode, ReplyMode::Agent);
        let cfg = Config::parse(r#"{"poll_interval":10,"routes":[],"reply_mode":"auto"}"#).unwrap();
        assert_eq!(cfg.reply_mode, ReplyMode::Auto);
        assert!(
            Config::parse(r#"{"poll_interval":10,"routes":[],"reply_mode":"unknown"}"#).is_err()
        );
    }

    #[test]
    fn roundtrip_serialization() {
        let cfg = Config::default();
        let text = serde_json::to_string_pretty(&cfg).unwrap();
        assert_eq!(Config::parse(&text).unwrap(), cfg);
    }

    #[test]
    fn legacy_lark_cli_loads_and_serializes_as_channel() {
        let text = serde_json::json!({
            "lark_cli": {"path": std::env::current_exe().unwrap(), "profile": "work"},
            "poll_interval": 10,
            "routes": [],
        });
        let cfg = Config::parse(&text.to_string()).unwrap();
        let output = serde_json::to_string(&cfg).unwrap();
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["channel"]["type"], "feishu");
        assert!(output.get("lark_cli").is_none());
    }

    #[test]
    fn route_mentions_default_to_disabled() {
        let cfg =
            Config::parse(r#"{"poll_interval":10,"routes":[{"prefix":"/x","cmd":"echo hi"}]}"#)
                .unwrap();
        assert!(!cfg.routes[0].allow_mentions);
        assert!(!serde_json::to_string(&cfg).unwrap().contains("follow_up"));
    }

    #[test]
    fn steer_requires_the_stock_codex_managed_route() {
        let mut cfg = Config::default();
        let codex = cfg
            .routes
            .iter_mut()
            .find(|r| r.prefix == "/codex")
            .unwrap();
        codex.follow_up = FollowUp::Steer;
        validate(&cfg).unwrap();

        let mut invalid = cfg.clone();
        invalid.routes[3].cmd = "codex exec --skip-git-repo-check".into();
        assert!(validate(&invalid).is_err());
        let mut invalid = cfg;
        invalid.routes[3].session = None;
        assert!(validate(&invalid).is_err());
    }

    #[test]
    fn legacy_null_lark_cli_remains_unconfigured() {
        let cfg = Config::parse(r#"{"lark_cli":null,"poll_interval":10,"routes":[]}"#).unwrap();
        assert!(cfg.channel.is_none());
    }
}
