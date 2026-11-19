//! Durable runtime state (DESIGN.md, "Polling and claims" and file table).
//!
//! `state.json` is the source of truth for the polling setting, the identity
//! binding, the pruning cutoff, and consumed (pending/settled) claims keyed by
//! message ID. Updates are serialized by the runtime coordinator and persisted
//! atomically with a synced file and directory.

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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    /// Saved polling setting; shared by menu, headless `--poll`, and restarts.
    #[serde(default = "default_paused")]
    pub polling: bool,
    /// Verified identity binding (open_id). Intake blocks while absent/mismatched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Nondecreasing cutoff: never scan before this timestamp.
    #[serde(default)]
    pub pruning_cutoff_ms: i64,
    #[serde(default)]
    pub claims: Vec<Claim>,
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
            if st.pruning_cutoff_ms < 0 {
                return Err(StateError("pruning_cutoff_ms must be nonnegative".into()));
            }
            let mut seen: Vec<&str> = Vec::new();
            for c in &st.claims {
                if seen.contains(&c.message_id.as_str()) {
                    return Err(StateError(format!(
                        "duplicate claim for {}",
                        c.message_id
                    )));
                }
                seen.push(&c.message_id);
            }
            Ok(StateLoad::Loaded(st))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let st = State::default();
            save(home, &st)
                .map_err(|e| StateError(format!("cannot create state.json: {}", e)))?;
            Ok(StateLoad::Created(st))
        }
        Err(e) => Err(StateError(format!("cannot read state.json: {}", e))),
    }
}

pub fn save(home: &Path, st: &State) -> Result<(), std::io::Error> {
    let text = serde_json::to_string_pretty(st)
        .map_err(std::io::Error::other)?;
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
        });
    }

    pub fn settle_claim(&mut self, message_id: &str, now_ms: i64, outcome: &str) {
        if let Some(c) = self
            .claims
            .iter_mut()
            .find(|c| c.message_id == message_id && c.status == ClaimStatus::Pending)
        {
            c.status = ClaimStatus::Settled;
            c.settled_at_ms = Some(now_ms);
            c.outcome = Some(outcome.to_string());
        }
    }

    /// Advance the pruning cutoff; it must never decrease.
    pub fn advance_cutoff(&mut self, now_ms: i64) {
        if now_ms > self.pruning_cutoff_ms {
            self.pruning_cutoff_ms = now_ms;
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
        st.settle_claim("om_1", 200, "ran:reply_sent");
        assert_eq!(st.claims[0].status, ClaimStatus::Settled);
        assert_eq!(st.claims[0].outcome.as_deref(), Some("ran:reply_sent"));
        st.settle_claim("om_1", 300, "again"); // no-op, already settled
        assert_eq!(st.claims[0].settled_at_ms, Some(200));
    }

    #[test]
    fn cutoff_never_decreases() {
        let mut st = State::default();
        st.advance_cutoff(500);
        st.advance_cutoff(100);
        assert_eq!(st.pruning_cutoff_ms, 500);
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
