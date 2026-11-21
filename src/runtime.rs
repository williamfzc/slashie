//! Runtime coordinator (DESIGN.md, "Polling and claims"): scans, admission,
//! durable claims, per-conversation mailboxes (src/mailbox.rs) whose workers
//! run jobs serially within a conversation and in parallel across them,
//! pause/resume, and restart recovery. All state transitions are serialized
//! on one state mutex; claim updates and pause are persisted before work is
//! handed off.

use crate::config::{Config, Route, DEFAULT_TIMEOUT_SECS};
use crate::logging::{now_ms, Logger};
use crate::state::{load_or_init, save, ClaimStatus, State, StateLoad};
use crate::transport::{Channel, TransportError};
use crate::worker::{build_inputs, run_command_with, SessionEnv};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const LOOKBACK_MS: i64 = 5 * 60 * 1000;
pub const MAX_WAITING_JOBS: usize = 10;

#[derive(Debug)]
pub struct Job {
    /// Conversation identity, computed once at admission and carried
    /// through delivery, execution, and settlement.
    pub key: crate::transport::ConversationKey,
    pub message: crate::transport::Message,
    pub route: Route,
}

pub struct Runtime {
    pub home: PathBuf,
    /// Live configuration. Swapped when config.json changes on disk (see
    /// `maybe_reload_config`); routes and the poll interval apply from the
    /// next scan on, while the channel binding is still startup-only.
    pub config: Mutex<Config>,
    /// mtime of config.json as currently loaded, for change detection.
    config_mtime: Mutex<Option<std::time::SystemTime>>,
    pub state: Arc<Mutex<State>>,
    pub transport: Arc<dyn Channel>,
    pub identity: String,
    pub logger: Arc<Logger>,
    pub shutdown: Arc<AtomicBool>,
    /// Messages created strictly after this point are eligible (start/resume).
    pub resume_boundary_ms: Mutex<i64>,
    /// Set when a fault requires operator action; blocks all intake.
    pub intake_blocked: Mutex<Option<String>>,
    /// Shared status snapshot reflected in the menu bar.
    pub status: Arc<Mutex<crate::status::RuntimeStatus>>,
    /// Per-conversation job mailboxes; the run loop delivers what scans
    /// claim, and shutdown drains them.
    mailboxes: crate::mailbox::Mailboxes,
    /// A view of this runtime as an `Arc`, wired after construction so the
    /// mailbox runner can call back into `process_job` without an `Arc`
    /// cycle (runtime -> mailboxes -> runner -> runtime).
    self_arc: Mutex<Option<std::sync::Weak<Runtime>>>,
    /// True when this run created state.json: hosts use it to offer
    /// first-run guidance (menu-bar.md).
    pub fresh_install: bool,
    /// When the run loop will next scan, as hosts show a countdown.
    pub next_scan_at: Mutex<Option<std::time::Instant>>,
}

pub fn error(msg: impl Into<String>) -> String {
    msg.into()
}

/// The validated configuration plus any intake-blocking fault, as returned
/// by `Config::load_or_create`.
pub struct LoadedConfig {
    pub config: Config,
    pub fault: Option<String>,
}

/// `--poll on|off` control invocation: update the persisted polling setting
/// under the lock, then exit. Identity binding and intake recovery still
/// happen on the next full startup.
pub fn apply_poll_control(home: &PathBuf, on: bool) -> Result<bool, String> {
    std::fs::create_dir_all(home)
        .map_err(|e| error(format!("cannot create runtime dir: {}", e)))?;
    let mut st = match load_or_init(home) {
        Ok(StateLoad::Created(s)) => s,
        Ok(StateLoad::Loaded(s)) => s,
        Err(e) => return Err(error(format!("intake blocked (invalid state): {}", e))),
    };
    st.polling = on;
    save(home, &st).map_err(|e| error(format!("cannot save polling setting: {}", e)))?;
    Ok(on)
}

impl Runtime {
    /// Full startup: state, identity binding, pending-claim recovery, poll
    /// override. The caller loads or creates config.json first so the chosen
    /// transport can be built from it. A missing or invalid state after
    /// initialization blocks intake (DESIGN.md).
    pub fn init(
        home: PathBuf,
        loaded: LoadedConfig,
        transport: Arc<dyn Channel>,
        identity: String,
        shutdown: Arc<AtomicBool>,
        logger: Arc<Logger>,
        poll_override: Option<bool>,
    ) -> Result<Runtime, String> {
        let LoadedConfig {
            config,
            fault: config_fault,
        } = loaded;
        std::fs::create_dir_all(&home).map_err(|e| {
            error(format!(
                "cannot create runtime dir {}: {}",
                home.display(),
                e
            ))
        })?;

        let (state, state_fault, state_created) = match load_or_init(&home) {
            Ok(StateLoad::Created(s)) => (s, None, true),
            Ok(StateLoad::Loaded(s)) => (s, None, false),
            Err(e) => (State::default(), Some(e.to_string()), false),
        };

        let state_corrupt = state_fault.is_some();
        let blocked = config_fault.or(state_fault);

        let status = Arc::new(Mutex::new(crate::status::RuntimeStatus {
            phase: Some(crate::status::Phase::Starting),
            ..Default::default()
        }));

        let fresh_install = state_created;
        let identity_verified = !identity.is_empty();
        let runtime = Runtime {
            config_mtime: Mutex::new(config_mtime(&home)),
            home,
            config: Mutex::new(config),
            state: Arc::new(Mutex::new(state)),
            transport,
            identity,
            logger,
            mailboxes: crate::mailbox::Mailboxes::new(Arc::clone(&shutdown)),
            shutdown,
            resume_boundary_ms: Mutex::new(now_ms()),
            intake_blocked: Mutex::new(blocked),
            status,
            self_arc: Mutex::new(None),
            fresh_install,
            next_scan_at: Mutex::new(None),
        };

        // A failed state load blocks intake; writing defaults over the
        // corrupt file would destroy the evidence and silently reset the
        // operator's polling choice.
        if !state_corrupt {
            if identity_verified {
                runtime.bind_identity();
            }
            runtime.settle_pending_as_interrupted();
        } else {
            runtime.status.lock().unwrap().operator_action =
                Some(crate::status::OperatorFault::LocalStateUnavailable);
        }

        if let Some(on) = poll_override {
            let _ = runtime.apply_poll_setting(on);
        }
        runtime.refresh_status();
        runtime.status.lock().unwrap().phase = None;
        Ok(runtime)
    }

    /// Record the `Arc` that will own this runtime, so the mailbox runner
    /// can reach `process_job`. Callers must hold the runtime in an `Arc`;
    /// `run` upgrades it per job and is a no-op runner after the owner
    /// drops it (shutdown has already joined everything by then).
    pub fn attach_arc(self: &Arc<Self>) {
        *self.self_arc.lock().unwrap() = Some(Arc::downgrade(self));
    }

    /// Verify and persist the identity binding; a changed identity blocks
    /// intake until the operator restores it.
    pub fn bind_identity(&self) {
        let mut st = self.state.lock().unwrap();
        match &st.identity {
            None => {
                st.identity = Some(self.identity.clone());
                if let Err(e) = save(&self.home, &st) {
                    self.logger
                        .log(&format!("ERROR cannot persist identity binding: {}", e));
                } else {
                    self.logger
                        .log(&format!("identity bound: {}", self.identity));
                }
            }
            Some(id) if id == &self.identity => {}
            Some(id) => {
                let msg = format!(
                    "identity changed: expected {}, transport reports {}",
                    id, self.identity
                );
                self.logger.log(&format!("ERROR {}", msg));
                *self.intake_blocked.lock().unwrap() = Some(msg);
                self.status.lock().unwrap().operator_action =
                    Some(crate::status::OperatorFault::SignInToFeishu);
            }
        }
    }

    /// On restart, pending claims are reported as interrupted and settled
    /// before pruning or polling (DESIGN.md). They never re-run.
    pub fn settle_pending_as_interrupted(&self) {
        let mut st = self.state.lock().unwrap();
        let now = now_ms();
        let pending: Vec<String> = st
            .claims
            .iter()
            .filter(|c| c.status == ClaimStatus::Pending)
            .map(|c| c.message_id.clone())
            .collect();
        for id in &pending {
            st.settle_claim(id, now, "interrupted_on_restart", None);
            self.logger
                .log(&format!("claim {} interrupted on restart", id));
        }
        if !pending.is_empty() {
            if let Err(e) = save(&self.home, &st) {
                self.logger
                    .log(&format!("ERROR cannot persist interrupted claims: {}", e));
            }
        }
    }

    /// Update the persisted polling setting under the state lock (shared by
    /// the menu toggle, headless `--poll on|off`, and restarts). Returns
    /// Err when the write failed or intake is blocked, so the menu can
    /// restore its checkmark; a blocked intake means the state file is
    /// untrustworthy and must never be overwritten with defaults.
    pub fn apply_poll_setting(&self, on: bool) -> Result<(), ()> {
        if self.is_intake_blocked() {
            return Err(());
        }
        {
            let mut status = self.status.lock().unwrap();
            let mut st = self.state.lock().unwrap();
            if st.polling != on {
                st.polling = on;
                if let Err(e) = save(&self.home, &st) {
                    st.polling = !on;
                    self.logger
                        .log(&format!("ERROR cannot save polling setting: {}", e));
                    status.operator_action =
                        Some(crate::status::OperatorFault::CouldNotSavePollingSetting);
                    return Err(());
                }
                status.last_successful_scan = None;
                if status.operator_action
                    == Some(crate::status::OperatorFault::CouldNotSavePollingSetting)
                {
                    status.operator_action = None;
                }
                self.logger
                    .log(&format!("polling set to {}", if on { "on" } else { "off" }));
            }
            if on {
                // Resume sets a new time boundary.
                *self.resume_boundary_ms.lock().unwrap() = now_ms();
            }
        }
        self.refresh_status();
        Ok(())
    }

    /// Recompute the derived status fields from current state.
    pub fn refresh_status(&self) {
        let mut status = self.status.lock().unwrap();
        // A blocked intake needs the operator even when nothing else set a
        // fault row; a more specific fault set earlier wins.
        if status.operator_action.is_none() && self.is_intake_blocked() {
            status.operator_action = Some(crate::status::OperatorFault::CheckSettingsAndRestart);
        }
        status.finishing_accepted = {
            let st = self.state.lock().unwrap();
            !st.polling && st.waiting_count() > 0
        };
        if status.finishing_accepted {
            // Accepted work finishing is more specific than a stale retry.
            status.retrying = None;
        }
    }

    pub fn is_intake_blocked(&self) -> bool {
        self.intake_blocked.lock().unwrap().is_some()
    }

    /// Swap in config.json when it changed on disk (DESIGN.md
    /// "Configuration and files"): routes and the poll interval apply from
    /// the next scan, no restart. A file that fails to read or validate
    /// keeps the previous configuration and logs once per changed version.
    /// The channel binding (`lark_cli`) is startup-only, so swapping it here
    /// has no effect until a restart.
    fn maybe_reload_config(&self) {
        let mtime = config_mtime(&self.home);
        {
            let mut seen = self.config_mtime.lock().unwrap();
            if mtime == *seen {
                return;
            }
            *seen = mtime;
        }
        let text = match std::fs::read_to_string(self.home.join("config.json")) {
            Ok(text) => text,
            Err(e) => {
                self.logger.log(&format!(
                    "ERROR cannot read config.json: {} (keeping previous configuration)",
                    e
                ));
                return;
            }
        };
        match Config::parse(&text) {
            Ok(config) => {
                *self.config.lock().unwrap() = config;
                self.logger.log("configuration reloaded from config.json");
                let mut status = self.status.lock().unwrap();
                if status.operator_action == Some(crate::status::OperatorFault::ConfigRejected) {
                    status.operator_action = None;
                }
            }
            Err(e) => {
                self.logger.log(&format!(
                    "ERROR config.json is invalid: {} (keeping previous configuration)",
                    e
                ));
                let mut status = self.status.lock().unwrap();
                if status.operator_action.is_none() {
                    status.operator_action = Some(crate::status::OperatorFault::ConfigRejected);
                }
            }
        }
    }

    /// One scan with a fixed end time. Admits eligible messages in creation
    /// order (message ID tiebreaker), durably claiming each before returning
    /// it for delivery to its conversation mailbox. `status.scanning` spans
    /// the scan so the menu bar can show it in flight.
    pub fn scan_once(&self) -> Result<Vec<Job>, String> {
        self.status.lock().unwrap().scanning = true;
        let result = self.scan_inner();
        self.status.lock().unwrap().scanning = false;
        result
    }

    fn scan_inner(&self) -> Result<Vec<Job>, String> {
        self.maybe_reload_config();
        self.status.lock().unwrap().last_successful_scan = None;
        if self.shutdown.load(Ordering::SeqCst) || self.is_intake_blocked() {
            return Ok(Vec::new());
        }
        let boundary = {
            let st = self.state.lock().unwrap();
            if !st.polling {
                return Ok(Vec::new());
            }
            *self.resume_boundary_ms.lock().unwrap()
        };

        let end = now_ms();
        let horizon = end - LOOKBACK_MS;
        // Search can index eligible messages after a scan has completed.
        let start = horizon.max(boundary);
        if start > end {
            return Ok(Vec::new());
        }

        let ids = match self.transport.search(start, end) {
            Ok(ids) => ids,
            Err(TransportError::Transient(m)) => {
                // Slice note: full exponential backoff comes with the real adapter.
                self.logger.log(&format!("scan transient failure: {}", m));
                let kind = if m.to_lowercase().contains("rate") {
                    crate::status::RetryKind::RateLimited
                } else {
                    crate::status::RetryKind::FeishuUnavailable
                };
                self.status.lock().unwrap().retrying = Some(kind);
                return Err(error(format!("transient scan failure: {}", m)));
            }
            Err(TransportError::Fatal(m)) => {
                self.logger.log(&format!("ERROR scan fatal failure: {}", m));
                *self.intake_blocked.lock().unwrap() = Some(format!("transport fatal: {}", m));
                return Err(error(m));
            }
            Err(TransportError::Incomplete(m)) => {
                // An incomplete result blocks admission from this scan.
                self.logger
                    .log(&format!("scan incomplete, admission blocked: {}", m));
                return Ok(Vec::new());
            }
        };

        // Overlapping scans can rediscover consumed IDs. Fetch only new details.
        let unique = {
            let st = self.state.lock().unwrap();
            let mut unique = Vec::new();
            for id in ids {
                if !st.is_consumed(&id) && !st.is_own_reply(&id) && !unique.contains(&id) {
                    unique.push(id);
                }
            }
            unique
        };
        let details = match self.transport.mget(&unique) {
            Ok(d) => d,
            Err(TransportError::Incomplete(m)) => {
                self.logger
                    .log(&format!("scan incomplete, admission blocked: {}", m));
                return Ok(Vec::new());
            }
            Err(e) => {
                self.logger.log(&format!("scan detail failure: {}", e));
                return Err(error(e.to_string()));
            }
        };

        // One consistent snapshot per scan; a mid-scan config swap only
        // affects the next scan.
        let routes = self.config.lock().unwrap().routes.clone();
        // The search API can round the requested window to whole seconds.
        let mut eligible: Vec<_> = details
            .into_iter()
            .filter(|m| {
                m.sender == self.identity
                    && !m.recalled
                    // A topic root or a reply in a topic is allowed; an
                    // ordinary reply thread is still excluded.
                    && (m.top_level || m.topic_id.is_some())
                    && m.text.is_some()
                    && m.created_at_ms > boundary
                    && m.created_at_ms >= start
                    && m.created_at_ms <= end
            })
            .filter(|m| {
                let text = m.text.as_ref().unwrap();
                let first = text.split_whitespace().next().unwrap_or("");
                routes.iter().any(|r| r.prefix == first)
            })
            .collect();
        // Creation order, message ID as tiebreaker.
        eligible.sort_by(|a, b| (a.created_at_ms, &a.id).cmp(&(b.created_at_ms, &b.id)));

        let eligible_count = eligible.len();
        let mut jobs = Vec::new();
        // Claims this scan has already made per conversation, and new
        // mailboxes it has promised: mailbox occupancy only moves at
        // delivery, so admission counts its own in-flight claims against
        // both caps.
        let mut claimed_per_key: std::collections::HashMap<
            crate::transport::ConversationKey,
            usize,
        > = std::collections::HashMap::new();
        let mut promised_new = 0usize;
        for m in eligible {
            let route = {
                let first = m.text.as_ref().unwrap().split_whitespace().next().unwrap();
                routes.iter().find(|r| r.prefix == first).cloned()
            };
            let route = match route {
                Some(r) => r,
                None => continue,
            };
            // Conversation identity is fixed at admission and carried with
            // the job; the mailbox it names must have room before claiming.
            let key = self.transport.conversation_key(&m);
            let extra = claimed_per_key.get(&key).copied().unwrap_or(0);
            match self.mailboxes.room(&key, extra, promised_new) {
                crate::mailbox::Room::Available => {}
                crate::mailbox::Room::MailboxFull => {
                    self.logger.log(&format!(
                        "mailbox full for {}; leaving {} for a later scan",
                        key.chat_id, m.id
                    ));
                    continue;
                }
                crate::mailbox::Room::MailboxesExhausted => {
                    self.logger
                        .log("conversation slots full; leaving candidates for a later scan");
                    break;
                }
            }
            {
                let mut st = self.state.lock().unwrap();
                // Pause invalidates results from a scan already in flight.
                if !st.polling
                    || self.shutdown.load(Ordering::SeqCst)
                    || *self.resume_boundary_ms.lock().unwrap() != boundary
                {
                    self.logger
                        .log("scan results invalidated by pause or shutdown");
                    break;
                }
                if st.is_consumed(&m.id) {
                    continue;
                }
                // Reserve space, then durably claim before handoff.
                if st.waiting_count() >= MAX_WAITING_JOBS {
                    self.logger
                        .log("queue full; leaving candidates for a later scan");
                    break;
                }
                st.add_claim(&m.id, now_ms());
                if let Err(e) = save(&self.home, &st) {
                    self.logger
                        .log(&format!("ERROR cannot persist claim for {}: {}", m.id, e));
                    return Err(error(format!("claim persist failed: {}", e)));
                }
            }
            self.logger
                .log(&format!("claimed {} for route {}", m.id, route.prefix));
            if extra == 0 && !self.mailboxes.is_tracked(&key) {
                promised_new += 1;
            }
            *claimed_per_key.entry(key.clone()).or_insert(0) += 1;
            jobs.push(Job {
                key,
                message: m,
                route,
            });
        }

        // Complete scan: record its end and prune settled claims that have
        // left the scan horizon.
        self.logger.log(&format!(
            "scan ok: {} eligible, {} admitted",
            eligible_count,
            jobs.len()
        ));
        {
            let mut status = self.status.lock().unwrap();
            let mut st = self.state.lock().unwrap();
            st.record_scan_end(end);
            st.claims.retain(|c| {
                !(c.status == ClaimStatus::Settled
                    && c.settled_at_ms.map(|t| t < horizon).unwrap_or(false))
            });
            if let Err(e) = save(&self.home, &st) {
                self.logger
                    .log(&format!("ERROR cannot persist scan end: {}", e));
            } else if st.polling
                && !self.shutdown.load(Ordering::SeqCst)
                && *self.resume_boundary_ms.lock().unwrap() == boundary
            {
                status.retrying = None;
                status.last_successful_scan = Some(std::time::Instant::now());
            }
        }
        Ok(jobs)
    }

    /// Run one job to completion: processing reaction, command, reply, settle.
    pub fn process_job(&self, job: Job) {
        if !self.shutdown.load(Ordering::SeqCst) {
            if let Err(e) = self.transport.react_processing(&job.message.id) {
                self.logger.log(&format!(
                    "processing reaction failed for {}: {}",
                    job.message.id, e
                ));
            }
        }
        // Fixed at admission; a config swap mid-queue never reroutes a job.
        let key = job.key.clone();
        let mut session = SessionEnv {
            channel: key.channel.clone(),
            chat_id: key.chat_id.clone(),
            topic_id: key.topic_id.clone().unwrap_or_default(),
            ..SessionEnv::default()
        };
        if job.route.session.as_deref() == Some("chat") {
            if key.chat_id.is_empty() {
                self.logger.log(&format!(
                    "session chat requested for {} but the message has no chat id",
                    job.message.id
                ));
            } else {
                let dir = self.transport.workspace_dir(&self.home, &key);
                match ensure_chat_workspace(&dir) {
                    Ok(()) => {
                        session.continuing = self
                            .state
                            .lock()
                            .unwrap()
                            .has_chat_session(&key, &job.route.prefix);
                        session.dir = Some(dir);
                    }
                    Err(e) => self.logger.log(&format!(
                        "cannot create chat workspace for {}: {}",
                        job.message.id, e
                    )),
                }
            }
        }
        let inputs = build_inputs(&job.route.prefix, &job.message, &key);
        let timeout_secs = job.route.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS);
        // Commands started after a config swap use the new subprocess path.
        let process_path = self.config.lock().unwrap().process_path.clone();
        let outcome = run_command_with(
            &job.route,
            &inputs,
            &session,
            &process_path,
            &home_dir(),
            self.shutdown.clone(),
        );
        self.logger.log(&format!(
            "command for {} {} after {}ms (exit {:?})",
            job.message.id, outcome.termination, outcome.duration_ms, outcome.exit_code
        ));

        // Exactly one reply attempt targeting the original message.
        let reply_result =
            self.transport
                .reply(&job.message.id, &outcome.reply_text, key.topic_id.is_some());
        let mut delivered_reply_id: Option<String> = None;
        let reply_state = match reply_result {
            Ok(reply_id) => {
                self.logger.log(&format!(
                    "reply sent to {} (reply {})",
                    job.message.id, reply_id
                ));
                // A confirmed later reply clears the reply status.
                let mut status = self.status.lock().unwrap();
                status.reply_failed = false;
                status.reply_unconfirmed = false;
                delivered_reply_id = Some(reply_id);
                "reply_sent".to_string()
            }
            Err(crate::transport::ReplyError::Rejected(m)) => {
                self.logger
                    .log(&format!("reply rejected for {}: {}", job.message.id, m));
                self.status.lock().unwrap().reply_failed = true;
                "reply_rejected".to_string()
            }
            Err(crate::transport::ReplyError::Uncertain(m)) => {
                self.logger.log(&format!(
                    "reply delivery uncertain for {}: {} (no resend)",
                    job.message.id, m
                ));
                let mut status = self.status.lock().unwrap();
                status.reply_unconfirmed = true;
                status.reply_failed = false;
                "reply_uncertain".to_string()
            }
        };

        let outcome_code = format!("{}:{}", outcome.termination, reply_state);
        {
            let mut st = self.state.lock().unwrap();
            // A successful run leaves a resumable agent session in the
            // conversation workspace; remember it before the durable settle.
            if session.dir.is_some() && outcome.exit_code == Some(0) {
                st.record_chat_session(&key, &job.route.prefix, now_ms());
            }
            st.settle_claim(
                &job.message.id,
                now_ms(),
                &outcome_code,
                delivered_reply_id.as_deref(),
            );
            if let Err(e) = save(&self.home, &st) {
                self.logger.log(&format!(
                    "ERROR cannot persist settle for {}: {}",
                    job.message.id, e
                ));
            }
        }
        self.logger
            .log(&format!("settled {} ({})", job.message.id, outcome_code));
        let _ = timeout_secs;
    }

    /// Foreground loop: scan at the configured interval until shutdown.
    /// Claimed jobs go to their conversation mailboxes, where each
    /// conversation's worker runs them serially while conversations
    /// progress in parallel.
    pub fn run(self: &Arc<Self>) {
        // The runner hands each mailbox job back into the runtime; the
        // Weak inside keeps the Arc cycle broken (attach_arc).
        let weak = self
            .self_arc
            .lock()
            .unwrap()
            .clone()
            .expect("run before attach_arc");
        self.mailboxes.set_runner(Arc::new(move |job| {
            if let Some(runtime) = weak.upgrade() {
                runtime.process_job(job);
            }
        }));

        while !self.shutdown.load(Ordering::SeqCst) {
            // Re-read each cycle so a saved interval change applies without
            // a restart (one scan of latency).
            let interval =
                std::time::Duration::from_secs(self.config.lock().unwrap().poll_interval.max(1));
            match self.scan_once() {
                Ok(jobs) if !jobs.is_empty() => self.mailboxes.deliver(jobs),
                Ok(_) => {}
                // Slice note: exponential backoff up to five minutes arrives
                // with the real adapter; sleep one interval for now.
                Err(e) => self.logger.log(&format!("scan failed, will retry: {}", e)),
            }
            let wake = std::time::Instant::now() + interval;
            *self.next_scan_at.lock().unwrap() = Some(wake);
            while std::time::Instant::now() < wake && !self.shutdown.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }

        // Shared shutdown: stop intake, drain accepted work; the active
        // command group terminates via the shutdown flag in run_command.
        self.status.lock().unwrap().phase = Some(crate::status::Phase::Quitting);
        self.logger.log("shutdown: draining accepted work");
        self.mailboxes.drain_and_join();
        self.logger.log("shutdown complete");
    }
}

fn config_mtime(home: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(home.join("config.json"))
        .and_then(|m| m.modified())
        .ok()
}

fn ensure_chat_workspace(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::Logger;
    use crate::transport::{
        ConversationKey, Message, MockBehavior, MockFixture, MockTransport, ReplyError,
    };
    use std::sync::atomic::AtomicBool;

    fn mkmsg(
        id: &str,
        ts: i64,
        sender: &str,
        text: Option<&str>,
        top_level: bool,
        recalled: bool,
    ) -> Message {
        Message {
            id: id.into(),
            sender: sender.into(),
            created_at_ms: ts,
            chat_type: "p2p".into(),
            chat_id: "oc_test".into(),
            topic_id: None,
            text: text.map(|s| s.into()),
            top_level,
            recalled,
            link: None,
        }
    }

    fn test_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("slashie-rt-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fixture_with(msgs: Vec<Message>) -> MockFixture {
        MockFixture {
            self_open_id: "ou_self".into(),
            messages: msgs,
            behavior: MockBehavior::default(),
        }
    }

    /// The conversation key for test messages (matches mkmsg's chat ids).
    fn test_key(chat: &str, topic: Option<&str>) -> ConversationKey {
        ConversationKey {
            channel: "feishu".into(),
            chat_id: chat.into(),
            topic_id: topic.map(str::to_string),
        }
    }

    fn init_rt(
        name: &str,
        fixture: MockFixture,
        boundary_ms: i64,
    ) -> (Runtime, Arc<MockTransport>, PathBuf) {
        init_rt_with_config(name, fixture, boundary_ms, Config::default())
    }

    fn init_rt_with_config(
        name: &str,
        fixture: MockFixture,
        boundary_ms: i64,
        config: Config,
    ) -> (Runtime, Arc<MockTransport>, PathBuf) {
        let home = test_home(name);
        let mock = Arc::new(MockTransport::new(fixture));
        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = Arc::new(Logger::new(&home, false));
        let rt = Runtime::init(
            home.clone(),
            LoadedConfig {
                config,
                fault: None,
            },
            mock.clone(),
            "ou_self".into(),
            shutdown,
            logger,
            Some(true),
        )
        .unwrap();
        *rt.resume_boundary_ms.lock().unwrap() = boundary_ms;
        (rt, mock, home)
    }

    #[test]
    fn fresh_install_creates_ping_config_and_paused_state() {
        let home = test_home("fresh");
        let mock = Arc::new(MockTransport::new(MockFixture {
            self_open_id: "ou_self".into(),
            messages: vec![],
            behavior: Default::default(),
        }));
        let logger = Arc::new(Logger::new(&home, false));
        let (cfg, fault) = Config::load_or_create(&home).unwrap();
        assert_eq!(fault, None, "fresh install has no config fault");
        let rt = Runtime::init(
            home.clone(),
            LoadedConfig {
                config: cfg,
                fault: None,
            },
            mock.clone(),
            "ou_self".into(),
            Arc::new(AtomicBool::new(false)),
            logger,
            None,
        )
        .unwrap();
        // Config created with fixed /ping route.
        let cfg =
            Config::parse(&std::fs::read_to_string(home.join("config.json")).unwrap()).unwrap();
        assert_eq!(cfg.routes[0].prefix, "/ping");
        assert_eq!(cfg.routes[0].cmd, "echo pong");
        // State created paused.
        let st = load_or_init(&home).unwrap();
        match st {
            StateLoad::Loaded(s) => assert!(!s.polling, "new install starts paused"),
            StateLoad::Created(_) => panic!("state should have been created by init"),
        }
        // Polling off => scan admits nothing.
        *rt.resume_boundary_ms.lock().unwrap() = 0;
        assert_eq!(rt.scan_once().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn admission_rules_exact_prefix_only() {
        let now = now_ms();
        let boundary = now - 60_000;
        let msgs = vec![
            mkmsg(
                "om_ping",
                boundary + 1_000,
                "ou_self",
                Some("/ping"),
                true,
                false,
            ),
            mkmsg(
                "om_near",
                boundary + 2_000,
                "ou_self",
                Some("/pingpong"),
                true,
                false,
            ),
            mkmsg(
                "om_other",
                boundary + 3_000,
                "ou_friend",
                Some("/ping hi"),
                true,
                false,
            ),
            mkmsg(
                "om_reply",
                boundary + 4_000,
                "ou_self",
                Some("/ping in a reply"),
                false,
                false,
            ),
            mkmsg(
                "om_recalled",
                boundary + 5_000,
                "ou_self",
                Some("/ping"),
                true,
                true,
            ),
            mkmsg("om_nontext", boundary + 6_000, "ou_self", None, true, false),
            mkmsg(
                "om_before",
                boundary - 1_000,
                "ou_self",
                Some("/ping"),
                true,
                false,
            ),
        ];
        let (rt, _mock, home) = init_rt("admission", fixture_with(msgs), boundary);
        let claimed = rt.scan_once().unwrap();
        assert_eq!(claimed.len(), 1, "only the exact /ping is eligible");
        let st = load_or_init(&home).unwrap();
        match st {
            StateLoad::Loaded(s) => {
                assert_eq!(s.claims.len(), 1);
                assert_eq!(s.claims[0].message_id, "om_ping");
            }
            _ => panic!(),
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn duplicate_delivery_claims_once() {
        let now = now_ms();
        let boundary = now - 60_000;
        let msgs = vec![mkmsg(
            "om_dup",
            boundary + 1_000,
            "ou_self",
            Some("/ping"),
            true,
            false,
        )];
        let (rt, _mock, home) = init_rt("duplicate", fixture_with(msgs), boundary);
        assert_eq!(rt.scan_once().unwrap().len(), 1);
        // Same message delivered again in a later scan (duplicate delivery).
        assert_eq!(rt.scan_once().unwrap().len(), 0);
        let st = match load_or_init(&home).unwrap() {
            StateLoad::Loaded(s) => s,
            _ => panic!(),
        };
        assert_eq!(st.claims.len(), 1);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn late_search_result_is_admitted_after_a_completed_scan() {
        let boundary = now_ms() - 60_000;
        let message = mkmsg(
            "om_late",
            boundary + 1_000,
            "ou_self",
            Some("/ping"),
            true,
            false,
        );
        let (mut rt, _, home) = init_rt("late_index", fixture_with(vec![]), boundary);
        assert_eq!(rt.scan_once().unwrap().len(), 0);
        let st = match load_or_init(&home).unwrap() {
            StateLoad::Loaded(s) => s,
            _ => panic!(),
        };
        assert!(st.last_scan_end_ms > message.created_at_ms);

        // The same message becomes visible to search only after that scan.
        rt.transport = Arc::new(MockTransport::new(fixture_with(vec![message])));
        assert_eq!(rt.scan_once().unwrap().len(), 1);
        assert_eq!(rt.scan_once().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn queue_bound_leaves_candidates_unclaimed() {
        let now = now_ms();
        let boundary = now - 60_000;
        let msgs: Vec<Message> = (0..12)
            .map(|i| {
                mkmsg(
                    &format!("om_q{}", i),
                    boundary + 1_000 + i,
                    "ou_self",
                    Some("/ping"),
                    true,
                    false,
                )
            })
            .collect();
        let (rt, _mock, home) = init_rt("queue", fixture_with(msgs), boundary);
        let claimed = rt.scan_once().unwrap();
        assert_eq!(
            claimed.len(),
            crate::mailbox::MAX_WAITING_PER_CONVERSATION,
            "the conversation mailbox caps one scan; the rest wait"
        );
        let st = match load_or_init(&home).unwrap() {
            StateLoad::Loaded(s) => s,
            _ => panic!(),
        };
        assert_eq!(
            st.claims.len(),
            crate::mailbox::MAX_WAITING_PER_CONVERSATION
        );
        for job in claimed {
            rt.process_job(job);
        }
        // Each later scan admits another mailbox's worth; none are lost.
        let more = rt.scan_once().unwrap();
        assert_eq!(more.len(), crate::mailbox::MAX_WAITING_PER_CONVERSATION);
        for job in more {
            rt.process_job(job);
        }
        let rest = rt.scan_once().unwrap();
        assert_eq!(
            rest.len(),
            12 - 2 * crate::mailbox::MAX_WAITING_PER_CONVERSATION
        );
        let st = match load_or_init(&home).unwrap() {
            StateLoad::Loaded(s) => s,
            _ => panic!(),
        };
        assert_eq!(st.claims.len(), 12);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn job_runs_once_with_mock_reply_then_never_reruns() {
        let now = now_ms();
        let boundary = now - 60_000;
        let msgs = vec![mkmsg(
            "om_once",
            boundary + 1_000,
            "ou_self",
            Some("/ping"),
            true,
            false,
        )];
        let (rt, mock, home) = init_rt("once", fixture_with(msgs), boundary);
        assert_eq!(rt.scan_once().unwrap().len(), 1);
        let job = Job {
            key: test_key("oc_test", None),
            message: mkmsg(
                "om_once",
                boundary + 1_000,
                "ou_self",
                Some("/ping"),
                true,
                false,
            ),
            route: rt.config.lock().unwrap().routes[0].clone(),
        };
        rt.process_job(job);
        // Mock reply targeted the original message with stdout.
        let recs = mock.reply_records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].source_id, "om_once");
        assert_eq!(recs[0].text, "pong");
        assert_eq!(*mock.reaction_attempts.lock().unwrap(), ["om_once"]);
        // Claim settled; a restart-like rescan never re-runs it.
        let st = match load_or_init(&home).unwrap() {
            StateLoad::Loaded(s) => s,
            _ => panic!(),
        };
        assert_eq!(st.claims[0].status, ClaimStatus::Settled);
        assert_eq!(st.claims[0].outcome.as_deref(), Some("exit:reply_sent"));
        assert_eq!(rt.scan_once().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn rejected_processing_reaction_still_runs_and_replies() {
        let boundary = now_ms() - 60_000;
        let m = mkmsg(
            "om_reaction",
            boundary + 1000,
            "ou_self",
            Some("/ping"),
            true,
            false,
        );
        let mut fixture = fixture_with(vec![m.clone()]);
        fixture.behavior.reaction_rejected = true;
        let (rt, mock, home) = init_rt("rejected_reaction", fixture, boundary);
        assert_eq!(rt.scan_once().unwrap().len(), 1);
        assert!(rt.status.lock().unwrap().last_successful_scan.is_some());
        // Bind the route first: a lock guard inside the call argument would
        // live across process_job and deadlock it.
        let route = rt.config.lock().unwrap().routes[0].clone();
        rt.process_job(Job {
            key: test_key("oc_test", None),
            message: m,
            route,
        });
        assert_eq!(*mock.reaction_attempts.lock().unwrap(), ["om_reaction"]);
        assert_eq!(mock.reply_records()[0].text, "pong");
        rt.apply_poll_setting(false).unwrap();
        assert!(rt.status.lock().unwrap().last_successful_scan.is_none());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn chat_session_continues_only_within_the_same_chat() {
        let now = now_ms();
        let boundary = now - 60_000;
        let config = Config {
            routes: vec![Route {
                prefix: "/claude".into(),
                cmd: "printf '%s|%s' \"$SLASHIE_CONTINUE\" \"$SLASHIE_CHAT\" > \"$SLASHIE_SESSION_DIR/state\"".into(),
                dir: None,
                timeout: Some(10),
                session: Some("chat".into()),
            }],
            ..Config::default()
        };
        let (rt, _, home) =
            init_rt_with_config("chat_session", fixture_with(vec![]), boundary, config);
        let route = rt.config.lock().unwrap().routes[0].clone();
        let message = |id: &str, chat: &str| {
            let mut m = mkmsg(
                id,
                now - 1000,
                "ou_self",
                Some("/claude hello"),
                true,
                false,
            );
            m.chat_id = chat.into();
            m
        };
        let read = |chat: &str| {
            std::fs::read_to_string(
                crate::transport::chat_workspace_dir(
                    &home,
                    &ConversationKey {
                        channel: "feishu".into(),
                        chat_id: chat.into(),
                        topic_id: None,
                    },
                )
                .join("state"),
            )
            .unwrap()
        };

        rt.process_job(Job {
            key: test_key("oc_one", None),
            message: message("om_one_a", "oc_one"),
            route: route.clone(),
        });
        assert_eq!(read("oc_one"), "0|oc_one");
        rt.process_job(Job {
            key: test_key("oc_one", None),
            message: message("om_one_b", "oc_one"),
            route: route.clone(),
        });
        assert_eq!(read("oc_one"), "1|oc_one");
        rt.process_job(Job {
            key: test_key("oc_two", None),
            message: message("om_two_a", "oc_two"),
            route: route.clone(),
        });
        assert_eq!(read("oc_two"), "0|oc_two");

        let st = rt.state.lock().unwrap();
        let key = |chat: &str| ConversationKey {
            channel: "feishu".into(),
            chat_id: chat.into(),
            topic_id: None,
        };
        assert!(st.has_chat_session(&key("oc_one"), "/claude"));
        assert!(st.has_chat_session(&key("oc_two"), "/claude"));
        let on_disk = match load_or_init(&home).unwrap() {
            StateLoad::Loaded(s) => s,
            StateLoad::Created(_) => panic!("state should exist"),
        };
        assert_eq!(on_disk.chat_sessions.len(), 2);
        drop(st);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn topic_reply_maps_to_its_own_session_and_thread_reply() {
        let now = now_ms();
        let boundary = now - 60_000;
        let config = Config {
            routes: vec![Route {
                prefix: "/topictest".into(),
                cmd: "printf '%s|%s|%s|%s|%s' \"$SLASHIE_CHANNEL\" \"$SLASHIE_CHAT\" \"$SLASHIE_TOPIC\" \"$SLASHIE_CONTINUE\" \"$(basename \"$PWD\")\" > \"$SLASHIE_SESSION_DIR/state\"".into(),
                dir: None,
                timeout: Some(10),
                session: Some("chat".into()),
            }],
            ..Config::default()
        };
        let (rt, mock, home) =
            init_rt_with_config("topic_session", fixture_with(vec![]), boundary, config);
        let route = rt.config.lock().unwrap().routes[0].clone();
        let message = |id: &str, topic: Option<&str>| {
            let mut m = mkmsg(
                id,
                now - 1000,
                "ou_self",
                Some("/topictest hi"),
                false,
                false,
            );
            m.chat_id = "oc_chat".into();
            m.topic_id = topic.map(|t| t.to_string());
            m
        };
        let read = |topic: &str| {
            std::fs::read_to_string(
                crate::transport::chat_workspace_dir(
                    &home,
                    &ConversationKey {
                        channel: "feishu".into(),
                        chat_id: "oc_chat".into(),
                        topic_id: Some(topic.into()),
                    },
                )
                .join("state"),
            )
            .unwrap()
        };

        rt.process_job(Job {
            key: test_key("oc_chat", Some("omt_a")),
            message: message("om_topic_a1", Some("omt_a")),
            route: route.clone(),
        });
        assert_eq!(read("omt_a"), "feishu|oc_chat|omt_a|0|omt_a");
        rt.process_job(Job {
            key: test_key("oc_chat", Some("omt_a")),
            message: message("om_topic_a2", Some("omt_a")),
            route: route.clone(),
        });
        assert_eq!(read("omt_a"), "feishu|oc_chat|omt_a|1|omt_a");
        rt.process_job(Job {
            key: test_key("oc_chat", Some("omt_b")),
            message: message("om_topic_b1", Some("omt_b")),
            route: route.clone(),
        });
        assert_eq!(read("omt_b"), "feishu|oc_chat|omt_b|0|omt_b");

        // A topic reply is posted into the same thread.
        assert!(mock.reply_records().iter().all(|r| r.in_thread));
        let st = rt.state.lock().unwrap();
        assert!(st.has_chat_session(
            &ConversationKey {
                channel: "feishu".into(),
                chat_id: "oc_chat".into(),
                topic_id: Some("omt_a".into()),
            },
            "/topictest"
        ));
        assert_ne!(
            crate::transport::chat_workspace_dir(
                &home,
                &ConversationKey {
                    channel: "feishu".into(),
                    chat_id: "oc_chat".into(),
                    topic_id: Some("omt_a".into()),
                }
            ),
            crate::transport::chat_workspace_dir(
                &home,
                &ConversationKey {
                    channel: "feishu".into(),
                    chat_id: "oc_chat".into(),
                    topic_id: Some("omt_b".into()),
                }
            )
        );
        drop(st);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn topic_replies_are_admitted_but_plain_replies_are_not() {
        let now = now_ms();
        let boundary = now - 60_000;
        let mut root = mkmsg(
            "om_topic_root",
            boundary + 1000,
            "ou_self",
            Some("/ping"),
            true,
            false,
        );
        root.topic_id = Some("omt_a".into());
        let mut reply = mkmsg(
            "om_topic_reply",
            boundary + 2000,
            "ou_self",
            Some("/ping"),
            false,
            false,
        );
        reply.topic_id = Some("omt_a".into());
        let plain = mkmsg(
            "om_plain_reply",
            boundary + 3000,
            "ou_self",
            Some("/ping"),
            false,
            false,
        );
        let (rt, _, home) = init_rt(
            "topic_admission",
            fixture_with(vec![root, reply, plain]),
            boundary,
        );
        assert_eq!(rt.scan_once().unwrap().len(), 2);
        let st = rt.state.lock().unwrap();
        assert!(st.is_consumed("om_topic_root"));
        assert!(st.is_consumed("om_topic_reply"));
        assert!(!st.is_consumed("om_plain_reply"));
        drop(st);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn own_reply_id_is_never_admitted_again() {
        let now = now_ms();
        let boundary = now - 60_000;
        let msg = mkmsg(
            "om_own_reply",
            boundary + 1000,
            "ou_self",
            Some("/ping"),
            true,
            false,
        );
        let (rt, _, home) = init_rt("own_reply", fixture_with(vec![msg]), boundary);
        {
            let mut st = rt.state.lock().unwrap();
            st.add_claim("om_source", now);
            st.settle_claim("om_source", now, "exit:reply_sent", Some("om_own_reply"));
        }
        assert_eq!(rt.scan_once().unwrap().len(), 0);
        assert!(!rt.state.lock().unwrap().is_consumed("om_own_reply"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn pending_claim_interrupted_on_restart_never_reruns() {
        let now = now_ms();
        let boundary = now - 60_000;
        let msgs = vec![mkmsg(
            "om_crash",
            boundary + 1_000,
            "ou_self",
            Some("/ping"),
            true,
            false,
        )];
        let home = test_home("crash");
        // Simulate a crash after claim: write state with a pending claim.
        let mut st = State {
            polling: true,
            identity: Some("ou_self".into()),
            ..State::default()
        };
        st.add_claim("om_crash", boundary + 2_000);
        save(&home, &st).unwrap();
        let mock = Arc::new(MockTransport::new(fixture_with(msgs)));
        let logger = Arc::new(Logger::new(&home, false));
        let rt = Runtime::init(
            home.clone(),
            LoadedConfig {
                config: Config::default(),
                fault: None,
            },
            mock.clone(),
            "ou_self".into(),
            Arc::new(AtomicBool::new(false)),
            logger,
            None,
        )
        .unwrap();
        *rt.resume_boundary_ms.lock().unwrap() = boundary;
        // Startup settled it as interrupted.
        let st2 = match load_or_init(&home).unwrap() {
            StateLoad::Loaded(s) => s,
            _ => panic!(),
        };
        assert_eq!(st2.claims[0].status, ClaimStatus::Settled);
        assert_eq!(
            st2.claims[0].outcome.as_deref(),
            Some("interrupted_on_restart")
        );
        // It is consumed: no re-run, no reply.
        assert_eq!(rt.scan_once().unwrap().len(), 0);
        assert_eq!(mock.reply_records().len(), 0);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn pause_stops_admissions_accepted_work_can_finish() {
        let now = now_ms();
        let boundary = now - 60_000;
        let msgs = vec![
            mkmsg(
                "om_before_pause",
                boundary + 1_000,
                "ou_self",
                Some("/ping"),
                true,
                false,
            ),
            mkmsg(
                "om_after_pause",
                boundary + 2_000,
                "ou_self",
                Some("/ping"),
                true,
                false,
            ),
        ];
        let (rt, mock, home) = init_rt("pause", fixture_with(msgs), boundary);
        assert_eq!(rt.scan_once().unwrap().len(), 2);
        // Pause: stops new admissions.
        let _ = rt.apply_poll_setting(false);
        assert_eq!(rt.scan_once().unwrap().len(), 0);
        // Accepted work can still finish: process all pending jobs directly.
        let st = rt.state.lock().unwrap();
        let ids: Vec<String> = st
            .claims
            .iter()
            .filter(|c| c.status == ClaimStatus::Pending)
            .map(|c| c.message_id.clone())
            .collect();
        drop(st);
        for id in ids {
            // Bind the route first: a lock guard inside the call argument
            // would live across process_job and deadlock it.
            let route = rt.config.lock().unwrap().routes[0].clone();
            rt.process_job(Job {
                key: test_key("oc_test", None),
                message: mkmsg(&id, boundary, "ou_self", Some("/ping"), true, false),
                route,
            });
        }
        assert_eq!(mock.reply_records().len(), 2);
        // Resume admits only messages after the new resume boundary: both
        // fixture messages are older, so nothing new is admitted.
        let now2 = now_ms();
        let _ = rt.apply_poll_setting(true);
        *rt.resume_boundary_ms.lock().unwrap() = now2;
        assert_eq!(rt.scan_once().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn incomplete_result_blocks_admission_for_that_scan() {
        let now = now_ms();
        let boundary = now - 60_000;
        let mut f = fixture_with(vec![mkmsg(
            "om_blk",
            boundary + 1_000,
            "ou_self",
            Some("/ping"),
            true,
            false,
        )]);
        f.behavior.mget_missing_ids = vec!["om_blk".into()];
        let (rt, mock, home) = init_rt("incomplete", f, boundary);
        assert_eq!(rt.scan_once().unwrap().len(), 0);
        assert!(rt.status.lock().unwrap().last_successful_scan.is_none());
        assert!(mock.reaction_attempts.lock().unwrap().is_empty());
        let st = match load_or_init(&home).unwrap() {
            StateLoad::Loaded(s) => s,
            _ => panic!(),
        };
        assert_eq!(st.claims.len(), 0);
        assert_eq!(mock.reply_records().len(), 0);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn identity_change_blocks_intake() {
        let home = test_home("identity");
        let mock = Arc::new(MockTransport::new(MockFixture {
            self_open_id: "ou_self".into(),
            messages: vec![],
            behavior: Default::default(),
        }));
        // First bind ou_self.
        let logger = Arc::new(Logger::new(&home, false));
        Runtime::init(
            home.clone(),
            LoadedConfig {
                config: Config::default(),
                fault: None,
            },
            mock.clone(),
            "ou_self".into(),
            Arc::new(AtomicBool::new(false)),
            logger,
            None,
        )
        .unwrap();
        // Next start reports a different identity -> intake blocked.
        let logger2 = Arc::new(Logger::new(&home, false));
        let rt2 = Runtime::init(
            home.clone(),
            LoadedConfig {
                config: Config::default(),
                fault: None,
            },
            mock.clone(),
            "ou_someone_else".into(),
            Arc::new(AtomicBool::new(false)),
            logger2,
            None,
        )
        .unwrap();
        assert!(rt2.is_intake_blocked());
        assert!(rt2.scan_once().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn invalid_state_blocks_intake() {
        let home = test_home("badstate");
        std::fs::write(home.join("state.json"), "{corrupt").unwrap();
        let mock = Arc::new(MockTransport::new(MockFixture {
            self_open_id: "ou_self".into(),
            messages: vec![],
            behavior: Default::default(),
        }));
        let logger = Arc::new(Logger::new(&home, false));
        let rt = Runtime::init(
            home.clone(),
            LoadedConfig {
                config: Config::default(),
                fault: None,
            },
            mock.clone(),
            "ou_self".into(),
            Arc::new(AtomicBool::new(false)),
            logger,
            None,
        )
        .unwrap();
        assert!(rt.is_intake_blocked());
        assert_eq!(rt.scan_once().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn invalid_config_blocks_intake() {
        let home = test_home("badconfig");
        std::fs::write(home.join("config.json"), "{\"poll_interval\": 15}").unwrap();
        let (_, fault) = Config::load_or_create(&home).unwrap();
        assert!(fault.is_some(), "invalid config yields a fault");
        let mock = Arc::new(MockTransport::new(MockFixture {
            self_open_id: "ou_self".into(),
            messages: vec![],
            behavior: Default::default(),
        }));
        let logger = Arc::new(Logger::new(&home, false));
        let rt = Runtime::init(
            home.clone(),
            LoadedConfig {
                config: Config::default(),
                fault,
            },
            mock.clone(),
            "ou_self".into(),
            Arc::new(AtomicBool::new(false)),
            logger,
            None,
        )
        .unwrap();
        assert!(rt.is_intake_blocked());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn reply_rejected_and_uncertain_settle_without_resend() {
        for behavior in [
            MockBehavior {
                reply_rejected: true,
                ..Default::default()
            },
            MockBehavior {
                reply_uncertain: true,
                ..Default::default()
            },
        ] {
            let rejected = behavior.reply_rejected;
            let now = now_ms();
            let boundary = now - 60_000;
            let mut f = fixture_with(vec![mkmsg(
                "om_r",
                boundary + 1_000,
                "ou_self",
                Some("/ping"),
                true,
                false,
            )]);
            f.behavior = behavior;
            let (rt, mock, home) = init_rt("replyfail", f, boundary);
            assert_eq!(rt.scan_once().unwrap().len(), 1);
            let route = rt.config.lock().unwrap().routes[0].clone();
            rt.process_job(Job {
                key: test_key("oc_test", None),
                message: mkmsg(
                    "om_r",
                    boundary + 1_000,
                    "ou_self",
                    Some("/ping"),
                    true,
                    false,
                ),
                route,
            });
            assert_eq!(mock.reply_records().len(), 0);
            let st = match load_or_init(&home).unwrap() {
                StateLoad::Loaded(s) => s,
                _ => panic!(),
            };
            let want = if rejected {
                "exit:reply_rejected"
            } else {
                "exit:reply_uncertain"
            };
            assert_eq!(st.claims[0].outcome.as_deref(), Some(want));
            // Failure must be perceivable: the menu-bar status line speaks.
            let status = rt.status.lock().unwrap();
            if rejected {
                assert!(status.reply_failed, "definitive rejection surfaces");
            } else {
                assert!(status.reply_unconfirmed, "uncertain delivery surfaces");
            }
            drop(status);
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn multiline_and_metacharacters_reach_worker_as_data() {
        let now = now_ms();
        let boundary = now - 60_000;
        let msgs = vec![mkmsg(
            "om_ml",
            boundary + 1_000,
            "ou_self",
            Some("/ping keep\nthese\nlines"),
            true,
            false,
        )];
        let (rt, mock, home) = init_rt("multiline", fixture_with(msgs), boundary);
        assert_eq!(rt.scan_once().unwrap().len(), 1);
        // Route echoes its args back with markers.
        rt.config.lock().unwrap().routes[0].cmd = "printf '[%s]' \"$SLASHIE_ARGS\"".into();
        let route = rt.config.lock().unwrap().routes[0].clone();
        rt.process_job(Job {
            key: test_key("oc_test", None),
            message: mkmsg(
                "om_ml",
                boundary + 1_000,
                "ou_self",
                Some("/ping keep\nthese\nlines"),
                true,
                false,
            ),
            route,
        });
        let recs = mock.reply_records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].text, "[ keep\nthese\nlines]");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn poll_override_updates_persisted_state() {
        let home = test_home("polloverride");
        let mock = Arc::new(MockTransport::new(MockFixture {
            self_open_id: "ou_self".into(),
            messages: vec![],
            behavior: Default::default(),
        }));
        let logger = Arc::new(Logger::new(&home, false));
        Runtime::init(
            home.clone(),
            LoadedConfig {
                config: Config::default(),
                fault: None,
            },
            mock.clone(),
            "ou_self".into(),
            Arc::new(AtomicBool::new(false)),
            logger,
            Some(true),
        )
        .unwrap();
        let st = match load_or_init(&home).unwrap() {
            StateLoad::Loaded(s) => s,
            _ => panic!(),
        };
        assert!(st.polling, "--poll on persisted");
        let logger2 = Arc::new(Logger::new(&home, false));
        Runtime::init(
            home.clone(),
            LoadedConfig {
                config: Config::default(),
                fault: None,
            },
            mock.clone(),
            "ou_self".into(),
            Arc::new(AtomicBool::new(false)),
            logger2,
            Some(false),
        )
        .unwrap();
        let st = match load_or_init(&home).unwrap() {
            StateLoad::Loaded(s) => s,
            _ => panic!(),
        };
        assert!(!st.polling, "--poll off persisted");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn group_shaped_messages_are_eligible_like_p2p() {
        let now = now_ms();
        let boundary = now - 60_000;
        let mut m = mkmsg(
            "om_grp",
            boundary + 1_000,
            "ou_self",
            Some("/ping"),
            true,
            false,
        );
        m.chat_type = "group".into();
        let (rt, _mock, _home) = init_rt("group", fixture_with(vec![m]), boundary);
        assert_eq!(rt.scan_once().unwrap().len(), 1);
    }

    #[test]
    fn reply_error_display_distinguishes_uncertain() {
        assert!(ReplyError::Uncertain("x".into())
            .to_string()
            .contains("uncertain"));
        assert!(ReplyError::Rejected("x".into())
            .to_string()
            .contains("rejected"));
    }

    #[test]
    fn same_conversation_burst_is_capped_at_the_mailbox_not_silently_lost() {
        let now = now_ms();
        let boundary = now - 60_000;
        let msgs: Vec<_> = (0..10)
            .map(|n| {
                mkmsg(
                    &format!("om_h{n}"),
                    boundary + 1_000 + n,
                    "ou_self",
                    Some("/ping"),
                    true,
                    false,
                )
            })
            .collect();
        let (rt, _mock, home) = init_rt("burst", fixture_with(msgs), boundary);
        let jobs = rt.scan_once().unwrap();
        assert_eq!(
            jobs.len(),
            crate::mailbox::MAX_WAITING_PER_CONVERSATION,
            "only mailbox-capacity jobs are claimed in one scan"
        );
        rt.mailboxes.set_runner(Arc::new(|_job: Job| {}));
        rt.mailboxes.deliver(jobs);
        let st = match load_or_init(&home).unwrap() {
            StateLoad::Loaded(s) => s,
            _ => panic!(),
        };
        assert_eq!(
            st.claims.len(),
            crate::mailbox::MAX_WAITING_PER_CONVERSATION
        );
        assert!(
            st.claims.iter().all(|c| c.status == ClaimStatus::Pending),
            "delivered claims stay pending, unclaimed candidates stay unclaimed"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn conversation_slots_cap_claims_without_stranding_them() {
        let now = now_ms();
        let boundary = now - 60_000;
        let msgs: Vec<_> = (0..20)
            .map(|n| {
                mkmsg(
                    &format!("om_c{n}"),
                    boundary + 1_000 + n,
                    "ou_self",
                    Some("/ping"),
                    true,
                    false,
                )
            })
            .collect();
        // Give every message its own conversation, like 20 different chats.
        let mut fixture = fixture_with(msgs);
        for n in 0..20 {
            fixture.messages[n].chat_id = format!("oc_conv{n}");
        }
        let (rt, mock, home) = init_rt("convslots", fixture, boundary);
        // Pre-fill 12 live mailboxes with blocked workers, so the
        // conversation cap binds before the global claims cap.
        rt.mailboxes.set_runner(Arc::new(|_job: Job| {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }));
        let prefill: Vec<_> = (0..12)
            .map(|n| {
                let mut m = mkmsg(
                    &format!("om_pre{n}"),
                    boundary + 100,
                    "ou_self",
                    Some("/ping"),
                    true,
                    false,
                );
                m.chat_id = format!("oc_prefill{n}");
                Job {
                    key: crate::transport::ConversationKey {
                        channel: "feishu".into(),
                        chat_id: m.chat_id.clone(),
                        topic_id: None,
                    },
                    message: m,
                    route: rt.config.lock().unwrap().routes[0].clone(),
                }
            })
            .collect();
        rt.mailboxes.deliver(prefill);
        std::thread::sleep(std::time::Duration::from_millis(50));

        let jobs = rt.scan_once().unwrap();
        assert_eq!(
            jobs.len(),
            crate::mailbox::MAX_ACTIVE_CONVERSATIONS - 12,
            "the conversation cap binds: only the free slots are claimed"
        );
        rt.mailboxes.deliver(jobs);
        let st = match load_or_init(&home).unwrap() {
            StateLoad::Loaded(s) => s,
            _ => panic!(),
        };
        assert_eq!(
            st.claims.len(),
            crate::mailbox::MAX_ACTIVE_CONVERSATIONS - 12,
            "claimed work is never stranded beyond any cap"
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = mock;
    }

    #[test]
    fn blocked_intake_never_overwrites_the_state_file() {
        let home = test_home("blockedpoll");
        std::fs::write(home.join("state.json"), "{not json").unwrap();
        let corrupt = "{not json".to_string();
        let mock = Arc::new(MockTransport::new(fixture_with(vec![])));
        let logger = Arc::new(Logger::new(&home, false));
        let rt = Runtime::init(
            home.clone(),
            LoadedConfig {
                config: Config::default(),
                fault: None,
            },
            mock,
            String::new(),
            Arc::new(AtomicBool::new(false)),
            logger,
            None,
        )
        .unwrap();
        assert!(rt.is_intake_blocked());
        assert_eq!(
            rt.apply_poll_setting(true),
            Err(()),
            "the toggle refuses while the state file is untrustworthy"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("state.json")).unwrap(),
            corrupt,
            "the corrupt file is left untouched for the operator to repair"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn fresh_install_flag_is_true_only_when_state_was_created() {
        let home = test_home("freshflag");
        let mock = Arc::new(MockTransport::new(fixture_with(vec![])));
        let logger = Arc::new(Logger::new(&home, false));
        let rt = Runtime::init(
            home.clone(),
            LoadedConfig {
                config: Config::default(),
                fault: None,
            },
            mock,
            "ou_self".into(),
            Arc::new(AtomicBool::new(false)),
            logger,
            None,
        )
        .unwrap();
        assert!(
            rt.fresh_install,
            "first init on an empty home is a fresh install"
        );

        // A second run on the same home is not.
        let mock = Arc::new(MockTransport::new(fixture_with(vec![])));
        let logger = Arc::new(Logger::new(&home, false));
        let rt = Runtime::init(
            home.clone(),
            LoadedConfig {
                config: Config::default(),
                fault: None,
            },
            mock,
            "ou_self".into(),
            Arc::new(AtomicBool::new(false)),
            logger,
            None,
        )
        .unwrap();
        assert!(!rt.fresh_install);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn config_edits_apply_at_the_next_scan_without_restart() {
        let now = now_ms();
        let boundary = now - 60_000;
        let m = mkmsg(
            "om_late",
            boundary + 1_000,
            "ou_self",
            Some("/late hello"),
            true,
            false,
        );
        let (rt, _mock, home) = init_rt("hotreload", fixture_with(vec![m]), boundary);
        // A baseline file, as a real installation has.
        std::fs::write(
            home.join("config.json"),
            serde_json::to_string_pretty(&Config::default()).unwrap(),
        )
        .unwrap();

        // /late is not configured yet.
        assert_eq!(rt.scan_once().unwrap().len(), 0);

        // An agent (or the CLI) edits the file directly while we run.
        let mut cfg = Config::default();
        cfg.routes.push(Route {
            prefix: "/late".into(),
            cmd: "echo late".into(),
            dir: None,
            timeout: None,
            session: None,
        });
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(
            home.join("config.json"),
            serde_json::to_string_pretty(&cfg).unwrap(),
        )
        .unwrap();

        // The next scan picks it up without any restart.
        assert_eq!(rt.scan_once().unwrap().len(), 1);
        assert!(rt
            .config
            .lock()
            .unwrap()
            .routes
            .iter()
            .any(|r| r.prefix == "/late"));

        // An invalid edit keeps the previous configuration.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(home.join("config.json"), "{oops").unwrap();
        assert_eq!(rt.scan_once().unwrap().len(), 0);
        assert!(rt
            .config
            .lock()
            .unwrap()
            .routes
            .iter()
            .any(|r| r.prefix == "/late"));
        let _ = std::fs::remove_dir_all(&home);
    }
}
