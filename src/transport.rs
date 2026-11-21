//! Channel boundary consumed by the runtime. The Feishu implementation lives
//! in `lark.rs`; `MockTransport` is the fixture-backed fake used by tests.
//! A channel owns its wire I/O, its conversation identity, and its workspace
//! layout, so the runtime never hard-codes Feishu message fields.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Message details as the runtime sees them after `mget`-style retrieval.
/// Timestamps are injectable fixture data (millis since the epoch) per the
/// slice's documented ambiguity note.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: String,
    /// Sender open_id.
    pub sender: String,
    pub created_at_ms: i64,
    /// "p2p" or "group" (mock-shaped; the real adapter decodes placement).
    pub chat_type: String,
    /// Chat ID (`oc_...` for Feishu). Fixtures may omit it.
    #[serde(default)]
    pub chat_id: String,
    /// Thread/topic ID (`omt_...`). Present for a topic root or a reply in a
    /// topic/thread; this is the conversation-isolation key within a chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic_id: Option<String>,
    /// Decoded text body for text messages; None for non-text messages.
    pub text: Option<String>,
    /// Validated placement: true when the message is top-level (no root/parent).
    pub top_level: bool,
    pub recalled: bool,
    /// Verified message link if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum TransportError {
    /// Transient; the scan retries with backoff.
    Transient(String),
    /// Operator action required (auth, local state).
    #[allow(dead_code)] // the real Lark CLI adapter classifies auth faults as Fatal
    Fatal(String),
    /// An incomplete result or missing page: block admission from this scan.
    Incomplete(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Transient(m) => write!(f, "transient transport failure: {}", m),
            TransportError::Fatal(m) => write!(f, "fatal transport failure: {}", m),
            TransportError::Incomplete(m) => write!(f, "incomplete search result: {}", m),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum ReplyError {
    /// Definitive rejection by the API.
    Rejected(String),
    /// Feishu may have accepted it; delivery is uncertain, never resend.
    Uncertain(String),
}

impl std::fmt::Display for ReplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplyError::Rejected(m) => write!(f, "reply rejected: {}", m),
            ReplyError::Uncertain(m) => write!(f, "reply delivery uncertain: {}", m),
        }
    }
}

/// Stable conversation identity used for session mapping, workspace
/// layout, and per-conversation mailboxes. A topic/thread is a separate
/// conversation from its parent chat.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationKey {
    pub channel: String,
    pub chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic_id: Option<String>,
}

/// One messaging channel. Feishu is the current implementation; a future
/// channel provides its own id, conversation key, workspace layout, and the
/// same search/detail/reaction/reply operations.
pub trait Channel: Send + Sync {
    /// Stable channel id stored with sessions, e.g. "feishu".
    fn id(&self) -> &'static str;

    /// Cached startup permission evidence. None means not verified, not usable.
    #[cfg_attr(not(feature = "tray"), allow(dead_code))]
    fn can_reply(&self) -> Option<bool> {
        None
    }

    /// Explicit user-requested, read-only connection check; never sends messages.
    #[cfg_attr(not(feature = "tray"), allow(dead_code))]
    fn check_connection(&self) -> Result<(), TransportError> {
        Err(TransportError::Incomplete(
            "Connection check is unavailable".into(),
        ))
    }

    /// Resolve the source navigation link only for an accepted handoff.
    fn message_link(&self, message: &Message) -> Result<Option<String>, TransportError> {
        Ok(message.link.clone())
    }

    /// Conversation identity for a message. Chat is the default scope; a
    /// topic/thread gets its own key. Channels with a different identity
    /// model override this.
    fn conversation_key(&self, msg: &Message) -> ConversationKey {
        ConversationKey {
            channel: self.id().to_string(),
            chat_id: msg.chat_id.clone(),
            topic_id: msg.topic_id.clone(),
        }
    }

    /// Stable workspace for the conversation. The channel decides the layout;
    /// Feishu keeps the existing `chats/<chat>[/<topic>]` paths so already
    /// running agent sessions keep their cwd.
    fn workspace_dir(&self, home: &Path, key: &ConversationKey) -> PathBuf;

    /// Search message IDs in [from_ms, to_ms]. Returns IDs only, in the order
    /// the backend provides; ordering and dedup happen in the runtime.
    fn search(&self, from_ms: i64, to_ms: i64) -> Result<Vec<String>, TransportError>;

    /// Fetch details for the given IDs. Missing IDs mean an incomplete result.
    fn mget(&self, ids: &[String]) -> Result<Vec<Message>, TransportError>;

    /// Software acknowledgment after durable acceptance, before queue delivery.
    /// Return only a confirmed reaction ID, so only our own status is removed.
    fn react_processing(&self, source_id: &str) -> Result<Option<String>, TransportError>;

    /// Software terminal state, independent of the target's text reply decision.
    fn react_finished(
        &self,
        source_id: &str,
        succeeded: bool,
        processing_id: Option<&str>,
    ) -> Result<(), TransportError>;

    /// One reply attempt targeting the source message. `in_thread` keeps the
    /// reply in the source topic/thread. Returns the reply ID on success.
    fn reply(&self, source_id: &str, text: &str, in_thread: bool) -> Result<String, ReplyError>;
}

/// Feishu-compatible workspace layout shared by the real adapter and the
/// mock: `~/.slashie/chats/<chat>/<topic>`.
pub fn chat_workspace_dir(home: &Path, key: &ConversationKey) -> PathBuf {
    let mut dir = home.join("chats").join(safe_component(&key.chat_id));
    if let Some(topic) = &key.topic_id {
        dir = dir.join(safe_component(topic));
    }
    dir
}

fn safe_component(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Records of replies made by the mock, for test assertions.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplyRecord {
    pub source_id: String,
    pub text: String,
    pub reply_id: String,
    pub in_thread: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockFixture {
    pub self_open_id: String,
    pub messages: Vec<Message>,
    /// Mock behavior switches.
    #[serde(default)]
    pub behavior: MockBehavior,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MockBehavior {
    /// Make search return a transient error this many times before succeeding.
    #[serde(default)]
    pub transient_search_failures: u32,
    /// Make mget drop these IDs (incomplete result -> scan blocked).
    #[serde(default)]
    pub mget_missing_ids: Vec<String>,
    #[serde(default)]
    pub reaction_rejected: bool,
    #[serde(default)]
    pub source_link_unavailable: bool,
    /// Make every reply fail definitively.
    #[serde(default)]
    pub reply_rejected: bool,
    /// Make every reply report uncertain delivery.
    #[serde(default)]
    pub reply_uncertain: bool,
}

pub struct MockTransport {
    fixture: MockFixture,
    search_failures_left: std::sync::atomic::AtomicU32,
    pub reaction_attempts: std::sync::Mutex<Vec<String>>,
    pub terminal_reactions: std::sync::Mutex<Vec<(String, bool, Option<String>)>>,
    pub replies: std::sync::Mutex<Vec<ReplyRecord>>,
    #[cfg(test)]
    pub terminal_failures_left: std::sync::Mutex<u32>,
    next_reply_id: std::sync::atomic::AtomicU32,
}

impl MockTransport {
    pub fn new(fixture: MockFixture) -> MockTransport {
        let left = fixture.behavior.transient_search_failures;
        MockTransport {
            fixture,
            search_failures_left: std::sync::atomic::AtomicU32::new(left),
            reaction_attempts: std::sync::Mutex::new(Vec::new()),
            terminal_reactions: std::sync::Mutex::new(Vec::new()),
            replies: std::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            terminal_failures_left: std::sync::Mutex::new(0),
            next_reply_id: std::sync::atomic::AtomicU32::new(1),
        }
    }

    pub fn from_json(text: &str) -> Result<MockTransport, String> {
        let fixture: MockFixture =
            serde_json::from_str(text).map_err(|e| format!("invalid mock fixture: {}", e))?;
        Ok(MockTransport::new(fixture))
    }

    pub fn self_open_id(&self) -> &str {
        &self.fixture.self_open_id
    }

    #[allow(dead_code)] // used by tests in the same module and by future hosts
    pub fn reply_records(&self) -> Vec<ReplyRecord> {
        self.replies.lock().unwrap().clone()
    }
}

impl Channel for MockTransport {
    fn message_link(&self, message: &Message) -> Result<Option<String>, TransportError> {
        if self.fixture.behavior.source_link_unavailable {
            return Err(TransportError::Incomplete(
                "Source message link unavailable".into(),
            ));
        }
        Ok(message.link.clone())
    }

    fn id(&self) -> &'static str {
        "feishu"
    }

    fn workspace_dir(&self, home: &Path, key: &ConversationKey) -> PathBuf {
        chat_workspace_dir(home, key)
    }

    fn search(&self, from_ms: i64, to_ms: i64) -> Result<Vec<String>, TransportError> {
        let left = self.search_failures_left.try_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |n| {
                if n > 0 {
                    Some(n - 1)
                } else {
                    None
                }
            },
        );
        if left.is_ok() {
            return Err(TransportError::Transient(
                "mock transient search failure".into(),
            ));
        }
        Ok(self
            .fixture
            .messages
            .iter()
            .filter(|m| m.created_at_ms >= from_ms && m.created_at_ms <= to_ms)
            .map(|m| m.id.clone())
            .collect())
    }

    fn mget(&self, ids: &[String]) -> Result<Vec<Message>, TransportError> {
        let missing = &self.fixture.behavior.mget_missing_ids;
        let mut out = Vec::new();
        for id in ids {
            if missing.contains(id) {
                return Err(TransportError::Incomplete(format!(
                    "missing detail for {}",
                    id
                )));
            }
            match self.fixture.messages.iter().find(|m| &m.id == id) {
                Some(m) => out.push(m.clone()),
                None => {
                    return Err(TransportError::Incomplete(format!("unknown id {}", id)));
                }
            }
        }
        Ok(out)
    }

    fn react_processing(&self, source_id: &str) -> Result<Option<String>, TransportError> {
        self.reaction_attempts
            .lock()
            .unwrap()
            .push(source_id.into());
        if self.fixture.behavior.reaction_rejected {
            return Err(TransportError::Fatal("mock reaction rejection".into()));
        }
        Ok(Some(format!("reaction-{}", source_id)))
    }

    fn react_finished(
        &self,
        source_id: &str,
        succeeded: bool,
        processing_id: Option<&str>,
    ) -> Result<(), TransportError> {
        self.terminal_reactions.lock().unwrap().push((
            source_id.into(),
            succeeded,
            processing_id.map(str::to_string),
        ));
        #[cfg(test)]
        {
            let mut remaining = self.terminal_failures_left.lock().unwrap();
            if *remaining > 0 {
                *remaining -= 1;
                return Err(TransportError::Transient(
                    "injected terminal failure".into(),
                ));
            }
        }
        if self.fixture.behavior.reaction_rejected {
            return Err(TransportError::Fatal("mock reaction rejection".into()));
        }
        Ok(())
    }

    fn reply(&self, source_id: &str, text: &str, in_thread: bool) -> Result<String, ReplyError> {
        if self.fixture.behavior.reply_rejected {
            return Err(ReplyError::Rejected("mock definitive rejection".into()));
        }
        if self.fixture.behavior.reply_uncertain {
            return Err(ReplyError::Uncertain("mock uncertain delivery".into()));
        }
        let n = self
            .next_reply_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let reply_id = format!("om_reply_{}", n);
        self.replies.lock().unwrap().push(ReplyRecord {
            source_id: source_id.to_string(),
            text: text.to_string(),
            reply_id: reply_id.clone(),
            in_thread,
        });
        Ok(reply_id)
    }
}

/// Placeholder channel for a host that started unconfigured: the runtime
/// blocks intake before any wire call, so these operations are never
/// reached; they fail closed if they ever are.
pub struct UnconfiguredChannel;

impl Channel for UnconfiguredChannel {
    fn id(&self) -> &'static str {
        "feishu"
    }

    fn workspace_dir(&self, home: &Path, key: &ConversationKey) -> PathBuf {
        chat_workspace_dir(home, key)
    }

    fn search(&self, _from_ms: i64, _to_ms: i64) -> Result<Vec<String>, TransportError> {
        Err(TransportError::Fatal("slashie is not configured".into()))
    }

    fn mget(&self, _ids: &[String]) -> Result<Vec<Message>, TransportError> {
        Err(TransportError::Fatal("slashie is not configured".into()))
    }

    fn react_processing(&self, _source_id: &str) -> Result<Option<String>, TransportError> {
        Err(TransportError::Fatal("slashie is not configured".into()))
    }

    fn react_finished(
        &self,
        _source_id: &str,
        _succeeded: bool,
        _processing_id: Option<&str>,
    ) -> Result<(), TransportError> {
        Err(TransportError::Fatal("slashie is not configured".into()))
    }

    fn reply(&self, _source_id: &str, _text: &str, _in_thread: bool) -> Result<String, ReplyError> {
        Err(ReplyError::Rejected("slashie is not configured".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, ts: i64) -> Message {
        Message {
            id: id.into(),
            sender: "ou_self".into(),
            created_at_ms: ts,
            chat_type: "p2p".into(),
            chat_id: "oc_test".into(),
            topic_id: None,
            text: Some("/ping".into()),
            top_level: true,
            recalled: false,
            link: None,
        }
    }

    #[test]
    fn search_filters_by_window() {
        let t = MockTransport::new(MockFixture {
            self_open_id: "ou_self".into(),
            messages: vec![msg("a", 100), msg("b", 200), msg("c", 300)],
            behavior: Default::default(),
        });
        let ids = t.search(150, 250).unwrap();
        assert_eq!(ids, vec!["b".to_string()]);
    }

    #[test]
    fn transient_search_failures_then_success() {
        let t = MockTransport::new(MockFixture {
            self_open_id: "ou_self".into(),
            messages: vec![],
            behavior: MockBehavior {
                transient_search_failures: 2,
                ..Default::default()
            },
        });
        assert_eq!(
            t.search(0, 1),
            Err(TransportError::Transient(
                "mock transient search failure".into()
            ))
        );
        assert_eq!(
            t.search(0, 1),
            Err(TransportError::Transient(
                "mock transient search failure".into()
            ))
        );
        assert_eq!(t.search(0, 1), Ok(vec![]));
    }

    #[test]
    fn mget_missing_blocks_scan() {
        let t = MockTransport::new(MockFixture {
            self_open_id: "ou_self".into(),
            messages: vec![msg("a", 100)],
            behavior: MockBehavior {
                mget_missing_ids: vec!["a".into()],
                ..Default::default()
            },
        });
        assert!(matches!(
            t.mget(&["a".to_string()]),
            Err(TransportError::Incomplete(_))
        ));
    }

    #[test]
    fn channel_key_and_workspace_isolate_topics() {
        let t = MockTransport::new(MockFixture {
            self_open_id: "ou_self".into(),
            messages: vec![],
            behavior: Default::default(),
        });
        let mut topic_msg = msg("om_topic", 100);
        topic_msg.topic_id = Some("omt_topic".into());
        let key = t.conversation_key(&topic_msg);
        assert_eq!(t.id(), "feishu");
        assert_eq!(key.channel, "feishu");
        assert_eq!(key.chat_id, "oc_test");
        assert_eq!(key.topic_id.as_deref(), Some("omt_topic"));
        assert_eq!(
            t.workspace_dir(std::path::Path::new("/tmp/home"), &key),
            std::path::Path::new("/tmp/home/chats/oc_test/omt_topic")
        );
        let plain = t.conversation_key(&msg("om_plain", 100));
        assert!(plain.topic_id.is_none());
        assert_eq!(
            t.workspace_dir(std::path::Path::new("/tmp/home"), &plain),
            std::path::Path::new("/tmp/home/chats/oc_test")
        );
    }

    #[test]
    fn reply_modes() {
        let mut t = MockTransport::new(MockFixture {
            self_open_id: "ou_self".into(),
            messages: vec![],
            behavior: Default::default(),
        });
        assert_eq!(t.reply("om_1", "pong", false).unwrap(), "om_reply_1");
        assert_eq!(t.reply_records().len(), 1);
        assert_eq!(t.reply_records()[0].source_id, "om_1");
        assert!(!t.reply_records()[0].in_thread);

        t.fixture.behavior.reply_rejected = true;
        assert!(matches!(
            t.reply("om_1", "x", false),
            Err(ReplyError::Rejected(_))
        ));
        t.fixture.behavior.reply_rejected = false;
        t.fixture.behavior.reply_uncertain = true;
        assert!(matches!(
            t.reply("om_1", "x", false),
            Err(ReplyError::Uncertain(_))
        ));
        // Uncertain attempts are not recorded as sent.
        assert_eq!(t.reply_records().len(), 1);
    }
}
