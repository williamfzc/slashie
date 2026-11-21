//! Per-conversation mailboxes (DESIGN.md "Polling and claims"): one FIFO
//! queue and one worker thread per conversation key. Commands within a
//! conversation run strictly in admission order — a structural property of
//! the single-consumer channel, not an enforced lock — while different
//! conversations progress in parallel, so one slow chat never blocks
//! another.
//!
//! Workers are spawned on first delivery and retire after idling:
//! retirement deregisters the mailbox first (deliveries arriving afterwards
//! open a fresh mailbox) and only then drains whatever was queued, which
//! makes the retire-versus-deliver race loss-free by construction. Capacity
//! is the runtime's admission concern; this module only reports room.

use crate::runtime::Job;
use crate::transport::ConversationKey;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a worker waits for its conversation's next job before retiring.
const IDLE_RETIRE: Duration = Duration::from_secs(60);

/// Jobs that may sit unstarted in one conversation's mailbox; further
/// candidates for that conversation are left for a later scan.
pub const MAX_WAITING_PER_CONVERSATION: usize = 4;

/// Conversations that may hold a mailbox at once; candidates for further
/// conversations are left for a later scan.
pub const MAX_ACTIVE_CONVERSATIONS: usize = 16;

/// Room for a candidate in its conversation's mailbox, as seen at
/// admission time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Room {
    /// Claiming is fine: the mailbox has queue space, or a new one fits.
    Available,
    /// This conversation's mailbox is full; other conversations unaffected.
    MailboxFull,
    /// No conversation slot is left; stop admitting this scan.
    MailboxesExhausted,
}

struct Mailbox {
    /// Distinguishes a mailbox from any successor on the same key.
    id: u64,
    tx: mpsc::Sender<Option<Job>>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Jobs enqueued but not yet finished, including the one in flight;
    /// this is what admission counts when it decides to claim more.
    waiting: usize,
}

#[derive(Default)]
struct Registry {
    map: HashMap<ConversationKey, Mailbox>,
    next_id: u64,
}

type Runner = Arc<dyn Fn(Job) + Send + Sync>;

pub struct Mailboxes {
    registry: Arc<Mutex<Registry>>,
    /// Set once by `set_runner` before the first delivery; owning the job
    /// runner keeps this module independent of the runtime.
    runner: Mutex<Option<Runner>>,
    shutdown: Arc<AtomicBool>,
}

impl Mailboxes {
    pub fn new(shutdown: Arc<AtomicBool>) -> Self {
        Mailboxes {
            registry: Arc::new(Mutex::new(Registry::default())),
            runner: Mutex::new(None),
            shutdown,
        }
    }

    /// Attach the job runner, once the runtime lives in an `Arc`. The
    /// worker bodies hold a `Weak` back, so runtime and mailboxes free
    /// each other normally.
    pub fn set_runner(&self, runner: Runner) {
        *self.runner.lock().unwrap() = Some(runner);
    }

    /// Whether a candidate for `key` can be claimed right now. `extra`
    /// counts candidates already claimed for this key earlier in the same
    /// scan, and `promised_new` counts new mailboxes earlier claims in this
    /// scan have already promised: mailbox occupancy only moves at delivery
    /// and completion, so the scan must account for its own in-flight
    /// claims to keep both caps exact.
    pub fn room(&self, key: &ConversationKey, extra: usize, promised_new: usize) -> Room {
        let registry = self.registry.lock().unwrap();
        match registry.map.get(key) {
            Some(mailbox) => {
                if mailbox.waiting + extra < MAX_WAITING_PER_CONVERSATION {
                    Room::Available
                } else {
                    Room::MailboxFull
                }
            }
            None => {
                if extra >= MAX_WAITING_PER_CONVERSATION {
                    Room::MailboxFull
                } else if extra > 0 || registry.map.len() + promised_new < MAX_ACTIVE_CONVERSATIONS
                {
                    // An extra > 0 means an earlier claim in this scan
                    // already promised this conversation a mailbox.
                    Room::Available
                } else {
                    Room::MailboxesExhausted
                }
            }
        }
    }

    /// Whether a worker currently serves this conversation.
    pub fn is_tracked(&self, key: &ConversationKey) -> bool {
        self.registry.lock().unwrap().map.contains_key(key)
    }

    /// Enqueue jobs into their conversation mailboxes in the given order,
    /// spawning a worker for each new conversation. Callers claim durably
    /// and check `room` before claiming, so every job here has a mailbox.
    pub fn deliver(&self, jobs: Vec<Job>) {
        let runner = self
            .runner
            .lock()
            .unwrap()
            .clone()
            .expect("deliver before set_runner");
        let mut registry = self.registry.lock().unwrap();
        for job in jobs {
            let key = job.key.clone();
            // Resolve the destination on an owned snapshot of the decision,
            // then act on the registry: the immutable borrow would collide
            // with the insert on the create path.
            let destination = match registry.map.get(&key) {
                Some(mailbox) if mailbox.waiting < MAX_WAITING_PER_CONVERSATION => {
                    Destination::Reuse(mailbox.tx.clone())
                }
                // Backstop only: with room() checked against in-scan claim
                // and mailbox counts, an admitted batch always fits. A skip
                // here strands its claim until restart recovery.
                Some(_) => Destination::Skip,
                None => Destination::Create,
            };
            match destination {
                Destination::Reuse(tx) => {
                    if tx.send(Some(job)).is_ok() {
                        if let Some(mailbox) = registry.map.get_mut(&key) {
                            mailbox.waiting += 1;
                        }
                    }
                }
                Destination::Create => {
                    if registry.map.len() >= MAX_ACTIVE_CONVERSATIONS {
                        // Unreachable with in-scan accounting: room() counted
                        // this scan's promised mailboxes. Kept as a backstop;
                        // a skip here strands its claim until restart.
                        continue;
                    }
                    registry.next_id += 1;
                    let id = registry.next_id;
                    let (tx, rx) = mpsc::channel();
                    let worker = Worker {
                        id,
                        key: key.clone(),
                        rx,
                        registry: Arc::clone(&self.registry),
                        runner: Arc::clone(&runner),
                        shutdown: Arc::clone(&self.shutdown),
                    };
                    let spawned = std::thread::Builder::new()
                        .name(format!("mailbox {}/{}", key.channel, key.chat_id))
                        .spawn(move || worker.serve());
                    let handle = match spawned {
                        Ok(handle) => handle,
                        // Without a thread the mailbox cannot serve; leave
                        // the candidate for a later scan. Its durable claim
                        // follows the interrupted-on-restart recovery if the
                        // scan never retries it.
                        Err(_) => continue,
                    };
                    registry.map.insert(
                        key.clone(),
                        Mailbox {
                            id,
                            tx: tx.clone(),
                            handle: Some(handle),
                            waiting: 0,
                        },
                    );
                    if tx.send(Some(job)).is_ok() {
                        registry.map.get_mut(&key).expect("inserted above").waiting += 1;
                    }
                }
                Destination::Skip => {}
            }
        }
    }

    /// Shutdown: stop intake everywhere and hand every queued job to its
    /// runner before joining the workers. The command runner itself aborts
    /// an in-flight or never-started command on the shutdown flag and
    /// replies a cancellation notice, so queued work finishes as a reply,
    /// not silence. A kill before that leaves pending claims, which
    /// recover as interrupted on restart.
    pub fn drain_and_join(&self) {
        let mut registry = self.registry.lock().unwrap();
        let taken: Vec<Mailbox> = registry.map.drain().map(|(_, mailbox)| mailbox).collect();
        drop(registry);
        let handles: Vec<_> = taken
            .into_iter()
            .filter_map(|mailbox| {
                // An Err means the worker had already retired after draining
                // its queue; there is nothing left to wait for.
                let _ = mailbox.tx.send(None);
                mailbox.handle
            })
            .collect();
        for handle in handles {
            let _ = handle.join();
        }
    }
}

enum Destination {
    Reuse(mpsc::Sender<Option<Job>>),
    Create,
    Skip,
}

/// One conversation's serving loop. Owns the receiving end; the registry
/// entry is removed before the final drain so a re-delivery can only ever
/// reach a fresh mailbox.
struct Worker {
    id: u64,
    key: ConversationKey,
    rx: mpsc::Receiver<Option<Job>>,
    registry: Arc<Mutex<Registry>>,
    runner: Runner,
    shutdown: Arc<AtomicBool>,
}

impl Worker {
    fn serve(self) {
        loop {
            match self.rx.recv_timeout(IDLE_RETIRE) {
                Ok(Some(job)) => {
                    (self.runner)(job);
                    self.job_done();
                }
                Ok(None) => break, // shutdown pill
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    // Retire: deregister first, then drain. Deliveries that
                    // land after deregistration belong to a fresh mailbox.
                    {
                        let mut registry = self.registry.lock().unwrap();
                        if registry
                            .map
                            .get(&self.key)
                            .is_some_and(|mailbox| mailbox.id == self.id)
                        {
                            registry.map.remove(&self.key);
                        }
                    }
                    while let Ok(Some(job)) = self.rx.try_recv() {
                        (self.runner)(job);
                        self.job_done();
                    }
                    break;
                }
            }
        }
    }

    /// Called after a job finishes, so `waiting` keeps meaning "enqueued
    /// but not finished" — a blocked in-flight job keeps its mailbox full
    /// and admission stops piling work behind it.
    fn job_done(&self) {
        let mut registry = self.registry.lock().unwrap();
        if let Some(mailbox) = registry.map.get_mut(&self.key) {
            if mailbox.id == self.id && mailbox.waiting > 0 {
                mailbox.waiting -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Message;

    fn key(chat: &str) -> ConversationKey {
        ConversationKey {
            channel: "feishu".into(),
            chat_id: chat.into(),
            topic_id: None,
        }
    }

    fn job(chat: &str, tag: &str) -> Job {
        Job {
            key: key(chat),
            message: Message {
                id: format!("om_{}", tag),
                sender: "ou_self".into(),
                created_at_ms: 0,
                chat_type: "p2p".into(),
                chat_id: key(chat).chat_id,
                topic_id: None,
                text: Some(format!("/ping {}", tag)),
                top_level: true,
                recalled: false,
                link: None,
            },
            route: crate::config::Route {
                prefix: "/ping".into(),
                cmd: "echo pong".into(),
                dir: None,
                timeout: None,
                session: None,
            },
        }
    }

    /// A runner that appends the job tag to a shared log, optionally slow.
    fn logging_runner(log: Arc<Mutex<Vec<String>>>, delay: Option<Duration>) -> Runner {
        Arc::new(move |job: Job| {
            let tag = job.message.id.clone();
            if let Some(delay) = delay {
                std::thread::sleep(delay);
            }
            log.lock().unwrap().push(tag);
        })
    }

    #[test]
    fn same_conversation_runs_in_order_different_conversations_interleave() {
        let boxes = Mailboxes::new(Arc::new(AtomicBool::new(false)));
        let log = Arc::new(Mutex::new(Vec::new()));
        // Slow runner widens the window for out-of-order execution if the
        // mailbox ever serialized conversations globally or parallelized
        // within one.
        boxes.set_runner(logging_runner(
            Arc::clone(&log),
            Some(Duration::from_millis(20)),
        ));
        let mut jobs = Vec::new();
        for n in 0..3 {
            jobs.push(job("chat_a", &format!("a{n}")));
        }
        for n in 0..3 {
            jobs.push(job("chat_b", &format!("b{n}")));
        }
        boxes.deliver(jobs);
        boxes.drain_and_join();

        let log = log.lock().unwrap();
        let order_a: Vec<_> = log.iter().filter(|t| t.contains("_a")).collect();
        let order_b: Vec<_> = log.iter().filter(|t| t.contains("_b")).collect();
        assert_eq!(
            order_a,
            ["om_a0", "om_a1", "om_a2"],
            "per-conversation FIFO"
        );
        assert_eq!(
            order_b,
            ["om_b0", "om_b1", "om_b2"],
            "per-conversation FIFO"
        );
        // Interleaving is scheduling-dependent; assert ordering here and rely
        // on the gated concurrency test below to prove simultaneous progress.
    }

    #[test]
    fn conversations_progress_in_parallel() {
        let boxes = Mailboxes::new(Arc::new(AtomicBool::new(false)));
        // Prove both runners entered before either may complete. Wall-clock
        // speed depends on CI load and cannot establish concurrency reliably.
        let gate = Arc::new((Mutex::new((0usize, false)), std::sync::Condvar::new()));
        let runner_gate = Arc::clone(&gate);
        boxes.set_runner(Arc::new(move |_job: Job| {
            let (lock, signal) = &*runner_gate;
            let mut state = lock.lock().unwrap();
            state.0 += 1;
            signal.notify_all();
            while !state.1 {
                state = signal.wait(state).unwrap();
            }
        }));
        boxes.deliver(vec![job("c1", "x"), job("c2", "y")]);
        let (lock, signal) = &*gate;
        let (mut state, _) = signal
            .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(5), |state| {
                state.0 < 2
            })
            .unwrap();
        let concurrent = state.0;
        state.1 = true;
        signal.notify_all();
        drop(state);
        boxes.drain_and_join();
        assert_eq!(
            concurrent, 2,
            "both conversations must enter before either finishes"
        );
    }

    #[test]
    fn full_mailbox_reports_full_and_new_conversations_exhaust() {
        let boxes = Mailboxes::new(Arc::new(AtomicBool::new(false)));
        // A runner blocked until we release it: jobs stay "waiting".
        let gate = Arc::new(Mutex::new(false));
        let signal = Arc::new(std::sync::Condvar::new());
        let gate2 = Arc::clone(&gate);
        let signal2 = Arc::clone(&signal);
        boxes.set_runner(Arc::new(move |_job: Job| {
            let mut open = gate2.lock().unwrap();
            while !*open {
                open = signal2.wait(open).unwrap();
            }
        }));

        // Fill chat_c's mailbox to its cap: the blocked runner keeps every
        // delivered job unfinished, so the room is deterministic.
        let mut jobs = Vec::new();
        for n in 0..MAX_WAITING_PER_CONVERSATION {
            jobs.push(job("chat_c", &format!("c{n}")));
        }
        boxes.deliver(jobs);
        assert_eq!(boxes.room(&key("chat_c"), 0, 0), Room::MailboxFull);
        assert_eq!(boxes.room(&key("other"), 0, 0), Room::Available);

        // Exhaust every conversation slot.
        let mut more = Vec::new();
        for n in 0..MAX_ACTIVE_CONVERSATIONS {
            more.push(job(&format!("fill_{n}"), &format!("f{n}")));
        }
        boxes.deliver(more);
        assert_eq!(boxes.room(&key("overflow"), 0, 0), Room::MailboxesExhausted);

        // Release the gate; workers drain and the room returns.
        *gate.lock().unwrap() = true;
        signal.notify_all();
        boxes.drain_and_join();
        assert_eq!(boxes.room(&key("overflow"), 0, 0), Room::Available);
    }

    #[test]
    fn shutdown_pill_finishes_queued_jobs() {
        let boxes = Mailboxes::new(Arc::new(AtomicBool::new(false)));
        let log = Arc::new(Mutex::new(Vec::new()));
        boxes.set_runner(logging_runner(Arc::clone(&log), None));
        boxes.deliver((0..3).map(|n| job("chat_d", &format!("d{n}"))).collect());
        // Drain without waiting: the pill must still flush the queue.
        boxes.drain_and_join();
        assert_eq!(
            log.lock().unwrap().len(),
            3,
            "shutdown drains every accepted job"
        );
    }
}
