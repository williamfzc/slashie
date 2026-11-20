//! Runtime coordinator (DESIGN.md, "Polling and claims"): scans, admission,
//! durable claims, a serial worker with a bounded queue, pause/resume, and
//! restart recovery. All state transitions are serialized on one state mutex;
//! claim updates and pause are persisted before work is handed off.

use crate::config::{Config, Route, DEFAULT_TIMEOUT_SECS};
use crate::logging::{now_ms, Logger};
use crate::state::{load_or_init, save, ClaimStatus, State, StateLoad};
use crate::transport::{Transport, TransportError};
use crate::worker::{build_inputs, run_command};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

pub const LOOKBACK_MS: i64 = 5 * 60 * 1000;
pub const MAX_WAITING_JOBS: usize = 10;

#[derive(Debug)]
pub struct Job {
    pub message: crate::transport::Message,
    pub route: Route,
}

pub struct Runtime {
    pub home: PathBuf,
    pub config: Config,
    pub state: Arc<Mutex<State>>,
    pub transport: Arc<dyn Transport>,
    pub identity: String,
    pub logger: Arc<Logger>,
    pub shutdown: Arc<AtomicBool>,
    /// Messages created strictly after this point are eligible (start/resume).
    pub resume_boundary_ms: Mutex<i64>,
    /// Set when a fault requires operator action; blocks all intake.
    pub intake_blocked: Mutex<Option<String>>,
    /// Shared status snapshot reflected in the menu bar.
    pub status: Arc<Mutex<crate::status::RuntimeStatus>>,
    job_tx: mpsc::Sender<Option<Job>>,
    job_rx: Mutex<Option<mpsc::Receiver<Option<Job>>>>,
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
        transport: Arc<dyn Transport>,
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

        let (state, state_fault) = match load_or_init(&home) {
            Ok(StateLoad::Created(s)) => (s, None),
            Ok(StateLoad::Loaded(s)) => (s, None),
            Err(e) => (State::default(), Some(e.to_string())),
        };

        let blocked = config_fault.or(state_fault);

        let status = Arc::new(Mutex::new(crate::status::RuntimeStatus {
            phase: Some(crate::status::Phase::Starting),
            ..Default::default()
        }));

        let (job_tx, job_rx) = mpsc::channel::<Option<Job>>();

        let runtime = Runtime {
            home,
            config,
            state: Arc::new(Mutex::new(state)),
            transport,
            identity,
            logger,
            shutdown,
            resume_boundary_ms: Mutex::new(now_ms()),
            intake_blocked: Mutex::new(blocked),
            status,
            job_tx,
            job_rx: Mutex::new(Some(job_rx)),
        };

        runtime.bind_identity();
        runtime.settle_pending_as_interrupted();
        if let Some(on) = poll_override {
            let _ = runtime.apply_poll_setting(on);
        }
        runtime.refresh_status();
        runtime.status.lock().unwrap().phase = None;
        Ok(runtime)
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
            st.settle_claim(id, now, "interrupted_on_restart");
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
    /// Err when the write failed so the menu can restore its checkmark.
    pub fn apply_poll_setting(&self, on: bool) -> Result<(), ()> {
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

    /// One scan with a fixed end time. Admits eligible messages in creation
    /// order (message ID tiebreaker), durably claiming before handoff.
    /// Returns the number of newly claimed messages.
    pub fn scan_once(&self) -> Result<usize, String> {
        self.status.lock().unwrap().last_successful_scan = None;
        if self.shutdown.load(Ordering::SeqCst) || self.is_intake_blocked() {
            return Ok(0);
        }
        let boundary = {
            let st = self.state.lock().unwrap();
            if !st.polling {
                return Ok(0);
            }
            *self.resume_boundary_ms.lock().unwrap()
        };

        let end = now_ms();
        let start = {
            let st = self.state.lock().unwrap();
            std::cmp::max(end - LOOKBACK_MS, st.pruning_cutoff_ms)
        };

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
                return Ok(0);
            }
        };

        // Merge duplicate IDs.
        let mut unique = Vec::new();
        for id in ids {
            if !unique.contains(&id) {
                unique.push(id);
            }
        }
        let details = match self.transport.mget(&unique) {
            Ok(d) => d,
            Err(TransportError::Incomplete(m)) => {
                self.logger
                    .log(&format!("scan incomplete, admission blocked: {}", m));
                return Ok(0);
            }
            Err(e) => {
                self.logger.log(&format!("scan detail failure: {}", e));
                return Err(error(e.to_string()));
            }
        };

        // Eligibility: self-authored, top-level, text, not recalled, exact
        // first-token prefix, created strictly after the boundary.
        let mut eligible: Vec<_> = details
            .into_iter()
            .filter(|m| {
                m.sender == self.identity
                    && !m.recalled
                    && m.top_level
                    && m.text.is_some()
                    && m.created_at_ms > boundary
            })
            .filter(|m| {
                let text = m.text.as_ref().unwrap();
                let first = text.split_whitespace().next().unwrap_or("");
                self.config.routes.iter().any(|r| r.prefix == first)
            })
            .collect();
        // Creation order, message ID as tiebreaker.
        eligible.sort_by(|a, b| (a.created_at_ms, &a.id).cmp(&(b.created_at_ms, &b.id)));

        let mut claimed = 0;
        for m in eligible {
            let route = {
                let first = m.text.as_ref().unwrap().split_whitespace().next().unwrap();
                self.config
                    .routes
                    .iter()
                    .find(|r| r.prefix == first)
                    .cloned()
            };
            let route = match route {
                Some(r) => r,
                None => continue,
            };
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
            self.job_tx.send(Some(Job { message: m, route })).unwrap();
            claimed += 1;
        }

        // Complete scan: advance the pruning cutoff and prune settled claims
        // that have left the scan horizon.
        {
            let mut status = self.status.lock().unwrap();
            let mut st = self.state.lock().unwrap();
            st.advance_cutoff(end);
            let horizon = end - LOOKBACK_MS;
            st.claims.retain(|c| {
                !(c.status == ClaimStatus::Settled
                    && c.settled_at_ms.map(|t| t < horizon).unwrap_or(false))
            });
            if let Err(e) = save(&self.home, &st) {
                self.logger
                    .log(&format!("ERROR cannot persist pruning cutoff: {}", e));
            } else if st.polling
                && !self.shutdown.load(Ordering::SeqCst)
                && *self.resume_boundary_ms.lock().unwrap() == boundary
            {
                status.retrying = None;
                status.last_successful_scan = Some(std::time::Instant::now());
            }
        }
        Ok(claimed)
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
        let (args, prompt) = build_inputs(&job.route.prefix, &job.message);
        let timeout_secs = job.route.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS);
        let outcome = run_command(
            &job.route,
            &args,
            &prompt,
            &self.config.process_path,
            &home_dir(),
            self.shutdown.clone(),
        );
        self.logger.log(&format!(
            "command for {} {} after {}ms (exit {:?})",
            job.message.id, outcome.termination, outcome.duration_ms, outcome.exit_code
        ));

        // Exactly one reply attempt targeting the original message.
        let reply_result = self.transport.reply(&job.message.id, &outcome.reply_text);
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
            st.settle_claim(&job.message.id, now_ms(), &outcome_code);
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

    /// Foreground loop: scan at the configured interval until shutdown. The
    /// worker runs on its own thread so controls take effect while a command
    /// is waiting.
    pub fn run(&self) {
        let rx = self
            .job_rx
            .lock()
            .unwrap()
            .take()
            .expect("run() called twice");
        let tx = self.job_tx.clone();
        std::thread::scope(|s| {
            let worker = s.spawn(move || {
                while let Ok(Some(job)) = rx.recv() {
                    self.process_job(job);
                }
            });

            let interval = std::time::Duration::from_secs(self.config.poll_interval.max(1));
            while !self.shutdown.load(Ordering::SeqCst) {
                if let Err(e) = self.scan_once() {
                    // Slice note: exponential backoff up to five minutes arrives
                    // with the real adapter; sleep one interval for now.
                    self.logger.log(&format!("scan failed, will retry: {}", e));
                }
                let wake = std::time::Instant::now() + interval;
                while std::time::Instant::now() < wake && !self.shutdown.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }

            // Shared shutdown: stop intake, drain accepted work; the active
            // command group terminates via the shutdown flag in run_command.
            self.status.lock().unwrap().phase = Some(crate::status::Phase::Quitting);
            self.logger.log("shutdown: draining accepted work");
            let _ = tx.send(None);
            drop(tx);
            let _ = worker.join();
        });
        self.logger.log("shutdown complete");
    }
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::Logger;
    use crate::transport::{Message, MockBehavior, MockFixture, MockTransport, ReplyError};
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

    fn init_rt(
        name: &str,
        fixture: MockFixture,
        boundary_ms: i64,
    ) -> (Runtime, Arc<MockTransport>, PathBuf) {
        let home = test_home(name);
        let mock = Arc::new(MockTransport::new(fixture));
        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = Arc::new(Logger::new(&home, false));
        let rt = Runtime::init(
            home.clone(),
            LoadedConfig {
                config: Config::default(),
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
        assert_eq!(rt.scan_once().unwrap(), 0);
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
        assert_eq!(claimed, 1, "only the exact /ping is eligible");
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
        assert_eq!(rt.scan_once().unwrap(), 1);
        // Same message delivered again in a later scan (duplicate delivery).
        assert_eq!(rt.scan_once().unwrap(), 0);
        let st = match load_or_init(&home).unwrap() {
            StateLoad::Loaded(s) => s,
            _ => panic!(),
        };
        assert_eq!(st.claims.len(), 1);
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
            claimed, MAX_WAITING_JOBS,
            "only 10 reserved; rest wait for a later scan"
        );
        let st = match load_or_init(&home).unwrap() {
            StateLoad::Loaded(s) => s,
            _ => panic!(),
        };
        assert_eq!(st.claims.len(), MAX_WAITING_JOBS);
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
        assert_eq!(rt.scan_once().unwrap(), 1);
        let job = Job {
            message: mkmsg(
                "om_once",
                boundary + 1_000,
                "ou_self",
                Some("/ping"),
                true,
                false,
            ),
            route: rt.config.routes[0].clone(),
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
        assert_eq!(rt.scan_once().unwrap(), 0);
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
        assert_eq!(rt.scan_once().unwrap(), 1);
        assert!(rt.status.lock().unwrap().last_successful_scan.is_some());
        rt.process_job(Job {
            message: m,
            route: rt.config.routes[0].clone(),
        });
        assert_eq!(*mock.reaction_attempts.lock().unwrap(), ["om_reaction"]);
        assert_eq!(mock.reply_records()[0].text, "pong");
        rt.apply_poll_setting(false).unwrap();
        assert!(rt.status.lock().unwrap().last_successful_scan.is_none());
        let _ = std::fs::remove_dir_all(home);
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
        assert_eq!(rt.scan_once().unwrap(), 0);
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
        assert_eq!(rt.scan_once().unwrap(), 2);
        // Pause: stops new admissions.
        let _ = rt.apply_poll_setting(false);
        assert_eq!(rt.scan_once().unwrap(), 0);
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
            rt.process_job(Job {
                message: mkmsg(&id, boundary, "ou_self", Some("/ping"), true, false),
                route: rt.config.routes[0].clone(),
            });
        }
        assert_eq!(mock.reply_records().len(), 2);
        // Resume admits only messages after the new resume boundary: both
        // fixture messages are older, so nothing new is admitted.
        let now2 = now_ms();
        let _ = rt.apply_poll_setting(true);
        *rt.resume_boundary_ms.lock().unwrap() = now2;
        assert_eq!(rt.scan_once().unwrap(), 0);
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
        assert_eq!(rt.scan_once().unwrap(), 0);
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
        assert_eq!(rt2.scan_once().unwrap(), 0);
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
        assert_eq!(rt.scan_once().unwrap(), 0);
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
            assert_eq!(rt.scan_once().unwrap(), 1);
            rt.process_job(Job {
                message: mkmsg(
                    "om_r",
                    boundary + 1_000,
                    "ou_self",
                    Some("/ping"),
                    true,
                    false,
                ),
                route: rt.config.routes[0].clone(),
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
        assert_eq!(rt.scan_once().unwrap(), 1);
        // Route echoes its args back with markers.
        let mut rt = rt;
        rt.config.routes[0].cmd = "printf '[%s]' \"$SLASHIE_ARGS\"".into();
        rt.process_job(Job {
            message: mkmsg(
                "om_ml",
                boundary + 1_000,
                "ou_self",
                Some("/ping keep\nthese\nlines"),
                true,
                false,
            ),
            route: rt.config.routes[0].clone(),
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
        assert_eq!(rt.scan_once().unwrap(), 1);
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
}
