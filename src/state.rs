//! Durable runtime state (DESIGN.md, "Polling and claims" and file table).
//!
//! `state.json` is the source of truth for the polling setting, the identity
//! binding, the last completed scan end, consumed (pending/settled) claims
//! keyed by message ID, and the chat-to-session mapping used by routes that
//! opted into `session: "chat"`. Updates are serialized by the runtime coordinator and persisted
//! atomically with a synced file and directory.

use crate::transport::ConversationKey;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    /// Claimed, worker has not yet settled the command+reply attempt.
    Pending,
    /// Command and reply attempt finished; never re-runs.
    Settled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Claim {
    pub message_id: String,
    pub status: ClaimStatus,
    pub claimed_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at_ms: Option<i64>,
    /// What happened: e.g. "ran:reply_sent", "interrupted_on_restart".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Reply message ID returned by a confirmed send. It must never be
    /// admitted as a new command, including in a topic/thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_id: Option<String>,
}

/// One chat route that already has a local agent session. The workspace is
/// derived from `chat_id`; this record only decides whether the next command
/// may resume the session (`SLASHIE_CONTINUE=1`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatSession {
    #[serde(default = "default_channel")]
    pub channel: String,
    pub chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic_id: Option<String>,
    pub prefix: String,
    pub last_success_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    /// Saved polling setting; shared by menu, headless `--poll`, and restarts.
    #[serde(default = "default_paused")]
    pub polling: bool,
    /// Verified identity binding (open_id). Intake blocks while absent/mismatched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// End of the latest completed scan, retained for diagnostics.
    #[serde(default, alias = "pruning_cutoff_ms")]
    pub last_scan_end_ms: i64,
    #[serde(default)]
    pub claims: Vec<Claim>,
    #[serde(default)]
    pub chat_sessions: Vec<ChatSession>,
}

fn default_channel() -> String {
    "feishu".into()
}

fn default_paused() -> bool {
    // A new installation starts paused.
    false
}

#[derive(Debug, PartialEq)]
pub struct StateError(pub String);

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid state: {}", self.0)
    }
}
impl std::error::Error for StateError {}

#[derive(Debug)]
pub enum StateLoad {
    /// Fresh state created (new installation, polling paused).
    Created(State),
    Loaded(State),
}

fn state_path(home: &Path) -> PathBuf {
    home.join("state.json")
}

pub fn load_or_init(home: &Path) -> Result<StateLoad, StateError> {
    let path = state_path(home);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            if text.trim().is_empty() {
                return Err(StateError("state.json is empty".into()));
            }
            let st: State = serde_json::from_str(&text)
                .map_err(|e| StateError(format!("cannot parse state.json: {}", e)))?;
            if st.last_scan_end_ms < 0 {
                return Err(StateError("last_scan_end_ms must be nonnegative".into()));
            }
            let mut seen: Vec<&str> = Vec::new();
            for c in &st.claims {
                if seen.contains(&c.message_id.as_str()) {
                    return Err(StateError(format!("duplicate claim for {}", c.message_id)));
                }
                seen.push(&c.message_id);
            }
            let mut seen_sessions: Vec<(&str, &str, Option<&str>, &str)> = Vec::new();
            for session in &st.chat_sessions {
                if session.last_success_ms < 0 {
                    return Err(StateError(
                        "chat session last_success_ms must be nonnegative".into(),
                    ));
                }
                let key = (
                    session.channel.as_str(),
                    session.chat_id.as_str(),
                    session.topic_id.as_deref(),
                    session.prefix.as_str(),
                );
                if seen_sessions.contains(&key) {
                    return Err(StateError(format!(
                        "duplicate chat session for {} {} {}",
                        session.channel, session.chat_id, session.prefix
                    )));
                }
                seen_sessions.push(key);
            }
            Ok(StateLoad::Loaded(st))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let st = State::default();
            save(home, &st).map_err(|e| StateError(format!("cannot create state.json: {}", e)))?;
            Ok(StateLoad::Created(st))
        }
        Err(e) => Err(StateError(format!("cannot read state.json: {}", e))),
    }
}

pub fn save(home: &Path, st: &State) -> Result<(), std::io::Error> {
    let text = serde_json::to_string_pretty(st).map_err(std::io::Error::other)?;
    crate::config::atomic_write(&state_path(home), &text)
}

impl State {
    /// True when this message ID already has a pending or settled claim.
    pub fn is_consumed(&self, message_id: &str) -> bool {
        self.claims.iter().any(|c| c.message_id == message_id)
    }

    #[allow(dead_code)] // part of the near-term surface for the real adapter
    pub fn pending_claims(&self) -> Vec<&Claim> {
        self.claims
            .iter()
            .filter(|c| c.status == ClaimStatus::Pending)
            .collect()
    }

    pub fn waiting_count(&self) -> usize {
        self.claims
            .iter()
            .filter(|c| c.status == ClaimStatus::Pending)
            .count()
    }

    /// Durably claim a message ID before handing it to the worker.
    pub fn add_claim(&mut self, message_id: &str, now_ms: i64) {
        self.claims.push(Claim {
            message_id: message_id.to_string(),
            status: ClaimStatus::Pending,
            claimed_at_ms: now_ms,
            settled_at_ms: None,
            outcome: None,
            reply_id: None,
        });
    }

    pub fn settle_claim(
        &mut self,
        message_id: &str,
        now_ms: i64,
        outcome: &str,
        reply_id: Option<&str>,
    ) {
        if let Some(c) = self
            .claims
            .iter_mut()
            .find(|c| c.message_id == message_id && c.status == ClaimStatus::Pending)
        {
            c.status = ClaimStatus::Settled;
            c.settled_at_ms = Some(now_ms);
            c.outcome = Some(outcome.to_string());
            c.reply_id = reply_id.map(|id| id.to_string());
        }
    }

    /// True when `message_id` is a reply Slashie already delivered. Such a
    /// message can appear in search results (especially inside a topic) and
    /// must never be treated as a new command.
    pub fn is_own_reply(&self, message_id: &str) -> bool {
        self.claims
            .iter()
            .any(|c| c.reply_id.as_deref() == Some(message_id))
    }

    /// Record the latest completed scan end; it must never decrease.
    pub fn record_scan_end(&mut self, now_ms: i64) {
        if now_ms > self.last_scan_end_ms {
            self.last_scan_end_ms = now_ms;
        }
    }

    fn session_matches(session: &ChatSession, key: &ConversationKey, prefix: &str) -> bool {
        session.channel == key.channel
            && session.chat_id == key.chat_id
            && session.topic_id == key.topic_id
            && session.prefix == prefix
    }

    /// True when this conversation route has completed a command successfully
    /// and may resume its agent session.
    pub fn has_chat_session(&self, key: &ConversationKey, prefix: &str) -> bool {
        self.chat_sessions
            .iter()
            .any(|s| Self::session_matches(s, key, prefix))
    }

    /// Remember that the conversation route now has a session. The timestamp
    /// only moves forward.
    pub fn record_chat_session(&mut self, key: &ConversationKey, prefix: &str, now_ms: i64) {
        if let Some(session) = self
            .chat_sessions
            .iter_mut()
            .find(|s| Self::session_matches(s, key, prefix))
        {
            if now_ms > session.last_success_ms {
                session.last_success_ms = now_ms;
            }
        } else {
            self.chat_sessions.push(ChatSession {
                channel: key.channel.clone(),
                chat_id: key.chat_id.clone(),
                topic_id: key.topic_id.clone(),
                prefix: prefix.to_string(),
                last_success_ms: now_ms,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_is_paused() {
        let st = State::default();
        assert!(!st.polling);
        assert_eq!(st.identity, None);
        assert_eq!(st.claims.len(), 0);
    }

    #[test]
    fn claim_settle_roundtrip_and_consumed() {
        let mut st = State::default();
        st.add_claim("om_1", 100);
        assert!(st.is_consumed("om_1"));
        assert_eq!(st.waiting_count(), 1);
        st.settle_claim("om_1", 200, "ran:reply_sent", Some("om_reply_1"));
        assert_eq!(st.claims[0].status, ClaimStatus::Settled);
        assert_eq!(st.claims[0].outcome.as_deref(), Some("ran:reply_sent"));
        assert_eq!(st.claims[0].reply_id.as_deref(), Some("om_reply_1"));
        assert!(st.is_own_reply("om_reply_1"));
        assert!(!st.is_own_reply("om_1"));
        st.settle_claim("om_1", 300, "again", None); // no-op, already settled
        assert_eq!(st.claims[0].settled_at_ms, Some(200));
        assert_eq!(st.claims[0].reply_id.as_deref(), Some("om_reply_1"));
    }

    #[test]
    fn legacy_scan_end_field_still_loads() {
        let st: State = serde_json::from_str(r#"{"polling":true,"pruning_cutoff_ms":42}"#).unwrap();
        assert_eq!(st.last_scan_end_ms, 42);
        let saved = serde_json::to_string(&st).unwrap();
        assert!(saved.contains("last_scan_end_ms"));
        assert!(!saved.contains("pruning_cutoff_ms"));
    }

    #[test]
    fn chat_session_mapping_roundtrip() {
        let mut st = State::default();
        let chat = ConversationKey {
            channel: "feishu".into(),
            chat_id: "oc_a".into(),
            topic_id: None,
        };
        let topic = ConversationKey {
            topic_id: Some("omt_topic".into()),
            ..chat.clone()
        };
        assert!(!st.has_chat_session(&chat, "/claude"));
        st.record_chat_session(&chat, "/claude", 100);
        st.record_chat_session(&chat, "/claude", 50); // never decreases
        st.record_chat_session(&chat, "/codex", 200);
        st.record_chat_session(&topic, "/claude", 300);
        assert!(st.has_chat_session(&chat, "/claude"));
        assert!(st.has_chat_session(&chat, "/codex"));
        assert!(st.has_chat_session(&topic, "/claude"));
        assert!(!st.has_chat_session(&topic, "/codex"));
        assert!(!st.has_chat_session(
            &ConversationKey {
                chat_id: "oc_b".into(),
                ..chat.clone()
            },
            "/claude"
        ));
        assert_eq!(st.chat_sessions.len(), 3);
        assert_eq!(
            st.chat_sessions
                .iter()
                .find(|s| s.prefix == "/claude" && s.topic_id.is_none())
                .unwrap()
                .last_success_ms,
            100
        );
        let text = serde_json::to_string(&st).unwrap();
        let back: State = serde_json::from_str(&text).unwrap();
        assert_eq!(back.chat_sessions, st.chat_sessions);
    }

    #[test]
    fn legacy_chat_session_defaults_to_feishu_without_topic() {
        let st: State = serde_json::from_str(
            r#"{"chat_sessions":[{"chat_id":"oc_a","prefix":"/claude","last_success_ms":1}]}"#,
        )
        .unwrap();
        assert_eq!(st.chat_sessions[0].channel, "feishu");
        assert_eq!(st.chat_sessions[0].topic_id, None);
    }

    #[test]
    fn scan_end_never_decreases() {
        let mut st = State::default();
        st.record_scan_end(500);
        st.record_scan_end(100);
        assert_eq!(st.last_scan_end_ms, 500);
    }

    #[test]
    fn load_or_init_creates_paused_then_persists_polling() {
        let dir = std::env::temp_dir().join(format!("slashie-state-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        match load_or_init(&dir).unwrap() {
            StateLoad::Created(st) => assert!(!st.polling),
            StateLoad::Loaded(_) => panic!("expected fresh state"),
        }
        let mut st = match load_or_init(&dir).unwrap() {
            StateLoad::Loaded(st) => st,
            StateLoad::Created(_) => panic!("expected loaded state"),
        };
        st.polling = true;
        save(&dir, &st).unwrap();
        match load_or_init(&dir).unwrap() {
            StateLoad::Loaded(st) => assert!(st.polling),
            StateLoad::Created(_) => panic!("expected loaded state"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_state_is_an_error() {
        let dir = std::env::temp_dir().join(format!("slashie-state-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.json"), "{not json").unwrap();
        assert!(load_or_init(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
