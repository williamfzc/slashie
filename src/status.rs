//! Runtime status shared between hosts, and the menu status copy table
//! (menu-bar.md "Status text"): the first applicable row wins; normal
//! enabled/paused states show the checkmark alone (no read-only line).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeStatus {
    /// Operator action needed; the copy names the specific fault.
    pub operator_action: Option<OperatorFault>,
    /// Latest reply failed or its outcome is uncertain.
    pub reply_failed: bool,
    pub reply_unconfirmed: bool,
    /// Polling is paused while accepted work remains.
    pub finishing_accepted: bool,
    /// Polling is retrying a transient failure.
    pub retrying: Option<RetryKind>,
    /// Starting or shutting down.
    pub phase: Option<Phase>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OperatorFault {
    CheckSettingsAndRestart,
    SignInToFeishu,
    LocalStateUnavailable,
    CouldNotSavePollingSetting,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RetryKind {
    FeishuUnavailable,
    RateLimited,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Phase {
    Starting,
    Quitting,
}

/// The read-only status line for the menu, or None when the checkmark alone
/// covers the current state.
pub fn status_line(s: &RuntimeStatus) -> Option<&'static str> {
    // Starting or shutting down
    if s.phase == Some(Phase::Quitting) {
        return Some("Quitting…");
    }
    if s.phase == Some(Phase::Starting) {
        return Some("Starting…");
    }
    // Operator action needed
    match &s.operator_action {
        Some(OperatorFault::CheckSettingsAndRestart) => return Some("Check Settings and restart"),
        Some(OperatorFault::SignInToFeishu) => return Some("Sign in to Feishu and restart"),
        Some(OperatorFault::LocalStateUnavailable) => return Some("Local state unavailable"),
        Some(OperatorFault::CouldNotSavePollingSetting) => {
            return Some("Could not save polling setting")
        }
        None => {}
    }
    // Latest reply failed or uncertain
    if s.reply_failed {
        return Some("Reply failed");
    }
    if s.reply_unconfirmed {
        return Some("Reply unconfirmed · Check Feishu");
    }
    // Paused while accepted work remains
    if s.finishing_accepted {
        return Some("Finishing accepted requests");
    }
    // Retrying a transient failure
    match &s.retrying {
        Some(RetryKind::FeishuUnavailable) => return Some("Retrying · Feishu unavailable"),
        Some(RetryKind::RateLimited) => return Some("Retrying · Rate limited"),
        None => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_states_show_checkmark_alone() {
        assert_eq!(status_line(&RuntimeStatus::default()), None);
    }

    #[test]
    fn first_applicable_row_wins() {
        let mut s = RuntimeStatus {
            phase: Some(Phase::Quitting),
            operator_action: Some(OperatorFault::LocalStateUnavailable),
            reply_failed: true,
            reply_unconfirmed: true,
            finishing_accepted: true,
            retrying: Some(RetryKind::RateLimited),
        };
        assert_eq!(status_line(&s), Some("Quitting…"));
        s.phase = Some(Phase::Starting);
        assert_eq!(status_line(&s), Some("Starting…"));
        s.phase = None;
        s.operator_action = Some(OperatorFault::CheckSettingsAndRestart);
        assert_eq!(status_line(&s), Some("Check Settings and restart"));
        s.operator_action = Some(OperatorFault::SignInToFeishu);
        assert_eq!(status_line(&s), Some("Sign in to Feishu and restart"));
        s.operator_action = Some(OperatorFault::CouldNotSavePollingSetting);
        assert_eq!(status_line(&s), Some("Could not save polling setting"));
        s.operator_action = Some(OperatorFault::LocalStateUnavailable);
        assert_eq!(status_line(&s), Some("Local state unavailable"));
        s.operator_action = None;
        s.finishing_accepted = false;
        assert_eq!(status_line(&s), Some("Reply failed"));
        s.reply_failed = false;
        s.retrying = None;
        assert_eq!(status_line(&s), Some("Reply unconfirmed · Check Feishu"));
        s.reply_unconfirmed = false;
        s.finishing_accepted = true;
        assert_eq!(status_line(&s), Some("Finishing accepted requests"));
        s.finishing_accepted = false;
        s.retrying = Some(RetryKind::RateLimited);
        s.retrying = Some(RetryKind::FeishuUnavailable);
        assert_eq!(status_line(&s), Some("Retrying · Feishu unavailable"));
        s.retrying = None;
        assert_eq!(status_line(&s), None);
    }
}
