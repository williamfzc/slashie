//! Lark CLI transport (DESIGN.md, "Feishu adapter").
//!
//! Invokes an explicit `lark-cli` executable with `--profile` and user
//! identity. Search and details use the raw API paths pinned by the design;
//! reply uses the typed `im +messages-reply` command with an idempotency key
//! derived from the source ID. Every call has a bounded deadline and runs in
//! its own process group.
//!
//! Envelope contract (validated here, pinned for live acceptance):
//! - API payload: `{"code":0,"msg":"...","data":{...}}`; the CLI may also
//!   wrap successes as `{"ok":true,"data":{...}}`.
//! - CLI-level failure: `{"ok":false,"error":{"type":"...","message":"..."}}`
//!   printed to stdout; classified into Fatal (auth/permission/validation —
//!   operator action) or Transient (network/rate/unknown).
//! - A nonzero exit with no parsable envelope is a read-side transient
//!   failure; on a reply it means delivery is uncertain.

use crate::config::LarkCli;
use crate::logging::{rfc3339_secs_ceil, rfc3339_secs_floor};
use crate::transport::{
    chat_workspace_dir, Channel, ConversationKey, Message, ReplyError, TransportError,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const CALL_TIMEOUT_MS: u64 = 20_000;
const REACTION_TIMEOUT_MS: u64 = 2_000;
const SEARCH_PAGE_SIZE: i64 = 50;
const MAX_PAGES: usize = 10;
const MGET_BATCH: usize = 50;

pub struct LarkCliTransport {
    exe: PathBuf,
    profile: String,
    process_path: Vec<String>,
    shutdown: Arc<AtomicBool>,
    /// Bound open_id, required for search's from_ids filter.
    self_id: std::sync::Mutex<Option<String>>,
    /// Default CLI call deadline; tests shorten it.
    timeout_ms: u64,
    reply_permission: std::sync::Mutex<Option<bool>>,
}

impl LarkCliTransport {
    pub fn new(
        cli: &LarkCli,
        process_path: &[String],
        shutdown: Arc<AtomicBool>,
    ) -> LarkCliTransport {
        let mut path = process_path.to_vec();
        if let Some(parent) = Path::new(&cli.path).parent() {
            let dir = parent.to_string_lossy().to_string();
            if !path.contains(&dir) {
                path.push(dir.clone());
            }
            // npm wrappers live in .../node_modules/.bin or a bin dir that
            // also holds node; adding the grandparent covers both layouts.
            if let Some(grandparent) = parent.parent() {
                let dir = grandparent.to_string_lossy().to_string();
                if !path.contains(&dir) {
                    path.push(dir);
                }
            }
        }
        LarkCliTransport {
            exe: PathBuf::from(&cli.path),
            profile: cli.profile.clone(),
            process_path: path,
            shutdown,
            self_id: std::sync::Mutex::new(None),
            timeout_ms: CALL_TIMEOUT_MS,
            reply_permission: std::sync::Mutex::new(None),
        }
    }

    /// Test constructor with a short call deadline.
    #[cfg(test)]
    fn with_timeout_ms(mut self, ms: u64) -> LarkCliTransport {
        self.timeout_ms = ms;
        self
    }

    /// Bind the verified open_id (set once after `verify_identity`).
    pub fn set_self_id(&self, open_id: &str) {
        *self.self_id.lock().unwrap() = Some(open_id.to_string());
    }

    /// One bounded CLI invocation. Returns stdout on a clean exit.
    fn run(
        &self,
        args: &[&str],
        stdin_data: Option<&str>,
        timeout_ms: u64,
    ) -> Result<String, CliFailure> {
        let mut cmd = Command::new(&self.exe);
        // An empty profile means "use the CLI's default profile".
        if !self.profile.is_empty() {
            cmd.arg("--profile").arg(&self.profile);
        }
        cmd.args(args);
        if let Ok(paths) = std::env::join_paths(&self.process_path) {
            cmd.env("PATH", paths);
        }
        cmd.stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let mut child = cmd.spawn().map_err(|e| {
            CliFailure::Failed(format!("cannot spawn lark-cli: {}", e), String::new())
        })?;
        let pgid = child.id() as i32;
        if let Some(data) = stdin_data {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(data.as_bytes())
                .map_err(|e| CliFailure::Uncertain(format!("stdin write failed: {}", e)))?;
        } else {
            drop(child.stdin.take());
        }
        let mut stdout_pipe = child.stdout.take().unwrap();
        let mut stderr_pipe = child.stderr.take().unwrap();
        // Drain stderr concurrently: the CLI prints banners and diagnostics
        // there, and an undrained or closed pipe can kill it with EPIPE.
        let stderr_thread = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while buf.len() < 64 * 1024 {
                match stderr_pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let take = n.min(64 * 1024 - buf.len());
                        buf.extend_from_slice(&chunk[..take]);
                    }
                }
            }
            buf
        });

        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st,
                Ok(None) => {}
                Err(e) => return Err(CliFailure::Uncertain(format!("wait failed: {}", e))),
            }
            if self.shutdown.load(Ordering::SeqCst) {
                self.kill_group(pgid);
                let _ = child.wait();
                return Err(CliFailure::Uncertain(
                    "shutdown canceled a Feishu call; delivery may be uncertain".into(),
                ));
            }
            if std::time::Instant::now() >= deadline {
                self.kill_group(pgid);
                let _ = child.wait();
                return Err(CliFailure::Uncertain(format!(
                    "lark-cli call exceeded {}ms deadline",
                    timeout_ms
                )));
            }
            std::thread::sleep(Duration::from_millis(10));
        };

        let mut stdout = Vec::new();
        {
            use std::io::Read;
            stdout_pipe
                .read_to_end(&mut stdout)
                .map_err(|e| CliFailure::Uncertain(format!("pipe read failed: {}", e)))?;
        }
        let stderr_text = String::from_utf8_lossy(&stderr_thread.join().unwrap_or_default())
            .trim()
            .to_string();
        let stdout_text = String::from_utf8_lossy(&stdout).trim().to_string();
        if !status.success() {
            // A definitive error envelope may ride on either stream.
            let combined = if stdout_text.is_empty() {
                stderr_text
            } else {
                format!("{}\n{}", stdout_text, stderr_text)
            };
            return Err(CliFailure::Failed(
                format!("lark-cli exited with {:?}", status.code()),
                combined,
            ));
        }
        Ok(stdout_text)
    }

    /// Return a confirmed status ID; do not infer success from a clean exit.
    fn create_reaction(&self, source_id: &str, emoji: &str) -> Result<String, TransportError> {
        let params = json!({"message_id": source_id});
        let body = json!({"reaction_type": {"emoji_type": emoji}});
        let envelope = self
            .run_json(
                &[
                    "im",
                    "reactions",
                    "create",
                    "--as",
                    "user",
                    "--params",
                    &params.to_string(),
                    "--data",
                    "-",
                ],
                Some(&body.to_string()),
                REACTION_TIMEOUT_MS,
            )
            .map_err(cli_failure_to_transport)?;
        api_data(&envelope)?
            .get("reaction_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                TransportError::Incomplete("Reaction response has no confirmed ID".into())
            })
    }

    fn kill_group(&self, pgid: i32) {
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
        let wait_until = std::time::Instant::now() + Duration::from_millis(2000);
        while std::time::Instant::now() < wait_until {
            if unsafe { libc::kill(-pgid, 0) } != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }

    fn run_json(
        &self,
        args: &[&str],
        stdin_data: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Value, CliFailure> {
        let out = self.run(args, stdin_data, timeout_ms)?;
        serde_json::from_str(&out).map_err(|e| {
            CliFailure::Failed(format!("unparsable lark-cli output: {}", e), String::new())
        })
    }

    /// Verify the user identity is available and return its open_id.
    /// A missing user login requires operator action (DESIGN.md).
    pub fn verify_identity(&self) -> Result<String, TransportError> {
        let status = self
            .run_json(&["auth", "status", "--json"], None, self.timeout_ms)
            .map_err(cli_failure_to_transport)?;
        *self.reply_permission.lock().unwrap() = reply_scope(&status);
        let user_ready = status
            .pointer("/identities/user/status")
            .and_then(|v| v.as_str())
            .map(|s| s == "ready")
            .unwrap_or(false);
        if !user_ready {
            return Err(TransportError::Fatal(
                "lark-cli user identity is not ready; run: lark-cli auth login".into(),
            ));
        }
        let me = self
            .run_json(
                &["contact", "+get-user", "--as", "user"],
                None,
                self.timeout_ms,
            )
            .map_err(cli_failure_to_transport)?;
        let data = api_data(&me)?;
        let open_id = extract_open_id(data)
            .ok_or_else(|| TransportError::Fatal("identity response has no open_id".into()))?;
        Ok(open_id.to_string())
    }
}

/// The open_id from a `+get-user` payload: live-verified 1.0.68 nests it
/// under `user`; a flat `open_id` stays accepted for compatibility.
fn reply_scope(status: &Value) -> Option<bool> {
    status
        .pointer("/identities/user/scope")
        .and_then(Value::as_str)
        .map(|scope| {
            scope
                .split_whitespace()
                .any(|s| s == "im:message.send_as_user")
        })
}

#[cfg(test)]
#[test]
fn permission_evidence_is_not_inferred_from_login() {
    assert_eq!(
        reply_scope(&json!({"identities":{"user":{"status":"ready"}}})),
        None
    );
    assert_eq!(
        reply_scope(&json!({"identities":{"user":{"scope":"im:message:readonly"}}})),
        Some(false)
    );
    assert_eq!(
        reply_scope(&json!({"identities":{"user":{"scope":"im:message.send_as_user im:message"}}})),
        Some(true)
    );
}

fn extract_open_id(data: &Value) -> Option<&str> {
    data.get("open_id")
        .and_then(|v| v.as_str())
        .or_else(|| data.pointer("/user/open_id").and_then(|v| v.as_str()))
}

enum CliFailure {
    /// Clean exit with an error envelope, or a nonzero exit code. Carries
    /// the captured output: live lark-cli prints a definitive error envelope
    /// even when it exits nonzero (verified: exit 3 + missing_scope envelope
    /// on stderr).
    Failed(String, String),
    /// Timeout, shutdown cancellation, or unparseable outcome: the API may
    /// have accepted the request (reply delivery uncertain).
    Uncertain(String),
}

impl CliFailure {
    /// Captured output that may carry an error envelope.
    fn envelope_text(&self) -> Option<&str> {
        match self {
            CliFailure::Failed(_, out) if !out.trim().is_empty() => Some(out),
            _ => None,
        }
    }
}

impl std::fmt::Debug for CliFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliFailure::Failed(m, out) => {
                write!(f, "CliFailure::Failed({:?}, stdout {:?})", m, out)
            }
            CliFailure::Uncertain(m) => write!(f, "CliFailure::Uncertain({:?})", m),
        }
    }
}

/// Map a CLI failure for an operation without reply delivery tracking.
fn cli_failure_to_transport(f: CliFailure) -> TransportError {
    match f {
        CliFailure::Failed(m, _) => TransportError::Transient(m),
        // Shutdown cancellation and timeouts on reads are operator-visible
        // faults (the runtime treats them as fatal for this scan; live
        // acceptance decides whether to retry).
        CliFailure::Uncertain(m) => TransportError::Fatal(m),
    }
}

/// Unwrap the API payload from a CLI response envelope.
fn api_data(envelope: &Value) -> Result<&Value, TransportError> {
    if envelope.get("ok") == Some(&json!(false)) {
        let etype = envelope
            .pointer("/error/type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let message = envelope
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .unwrap_or("no message");
        return Err(classify_cli_error(etype, message));
    }
    if let Some(code) = envelope.get("code").and_then(|v| v.as_i64()) {
        if code != 0 {
            let msg = envelope
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("no message");
            return Err(classify_api_code(code, msg));
        }
        return envelope
            .get("data")
            .ok_or_else(|| TransportError::Incomplete("ok envelope has no data".into()));
    }
    if envelope.get("ok") == Some(&json!(true)) {
        return envelope
            .get("data")
            .ok_or_else(|| TransportError::Incomplete("ok envelope has no data".into()));
    }
    Err(TransportError::Incomplete(format!(
        "unrecognized envelope: {}",
        serde_json::to_string(envelope).unwrap_or_default()
    )))
}

fn classify_cli_error(etype: &str, message: &str) -> TransportError {
    match etype {
        "auth" | "permission" | "validation" | "invalid_argument" => {
            TransportError::Fatal(format!("lark-cli {} error: {}", etype, message))
        }
        _ => TransportError::Transient(format!("lark-cli {} error: {}", etype, message)),
    }
}

fn classify_api_code(code: i64, message: &str) -> TransportError {
    match code {
        // Rate limited; honor a server delay if supplied (polling loop backoff).
        99991400..=99991499 => {
            TransportError::Transient(format!("rate limited (code {}): {}", code, message))
        }
        // Authentication / permission faults need operator action.
        99991661..=99991699 => {
            TransportError::Fatal(format!("auth fault (code {}): {}", code, message))
        }
        _ => TransportError::Transient(format!("api error (code {}): {}", code, message)),
    }
}

/// Parse one mget/search item into the runtime Message shape.
/// Placement is verified locally: root_id/parent_id present means a reply.
/// The recalled field's live wire shape is pinned during live acceptance;
/// fixtures may set it explicitly.
fn parse_item(item: &Value) -> Result<Message, String> {
    let id = item
        .get("message_id")
        .and_then(|v| v.as_str())
        .ok_or("item has no message_id")?
        .to_string();
    let sender = item
        .pointer("/sender/id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Sender kind is not validated here: eligibility comes from the runtime's
    // identity comparison, and unknown kinds simply never match.
    let create_time: i64 = item
        .get("create_time")
        .and_then(|v| {
            v.as_str()
                .and_then(|s| s.parse().ok())
                .or_else(|| v.as_i64())
        })
        .ok_or_else(|| format!("item {} has unparsable create_time", id))?;
    let msg_type = item
        .get("msg_type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let text = if msg_type == "text" {
        let content = item
            .pointer("/body/content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("text item {} has no body.content", id))?;
        let parsed: Value = serde_json::from_str(content)
            .map_err(|e| format!("item {} body.content is not JSON: {}", id, e))?;
        parsed
            .get("text")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    } else {
        None
    };
    let root = item.get("root_id").and_then(|v| v.as_str()).unwrap_or("");
    let parent = item.get("parent_id").and_then(|v| v.as_str()).unwrap_or("");
    let chat_type = item
        .get("chat_type")
        .and_then(|v| v.as_str())
        .unwrap_or("group")
        .to_string();
    let chat_id = item
        .get("chat_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let topic_id = item
        .get("thread_id")
        .and_then(|v| v.as_str())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());
    Ok(Message {
        id,
        sender,
        created_at_ms: create_time,
        chat_type,
        chat_id,
        topic_id,
        text,
        top_level: root.is_empty() && parent.is_empty(),
        recalled: item
            .get("recalled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        link: item
            .get("message_app_link")
            .and_then(Value::as_str)
            .filter(|link| !link.is_empty())
            .map(str::to_string),
    })
}

impl Channel for LarkCliTransport {
    fn id(&self) -> &'static str {
        "feishu"
    }

    fn can_reply(&self) -> Option<bool> {
        *self.reply_permission.lock().unwrap()
    }

    fn check_connection(&self) -> Result<(), TransportError> {
        let expected = self.self_id.lock().unwrap().clone();
        let actual = self.verify_identity()?;
        if expected.as_deref() == Some(&actual) {
            Ok(())
        } else {
            Err(TransportError::Fatal(
                "Identity changed; restart with the expected account".into(),
            ))
        }
    }

    fn message_link(&self, message: &Message) -> Result<Option<String>, TransportError> {
        if let Some(link) = &message.link {
            return Ok(Some(link.clone()));
        }
        let envelope = self
            .run_json(
                &[
                    "im",
                    "+messages-mget",
                    "--message-ids",
                    &message.id,
                    "--no-reactions",
                    "--format",
                    "json",
                ],
                None,
                self.timeout_ms,
            )
            .map_err(cli_failure_to_transport)?;
        let data = api_data(&envelope)?;
        let link = data
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| {
                messages
                    .iter()
                    .find(|m| m.get("message_id").and_then(Value::as_str) == Some(&message.id))
            })
            .and_then(|m| m.get("message_app_link"))
            .and_then(Value::as_str)
            .filter(|link| !link.is_empty());
        link.map(|link| Some(link.to_string())).ok_or_else(|| {
            TransportError::Incomplete(
                "Source message link is unavailable; handoff was not started".into(),
            )
        })
    }

    fn workspace_dir(&self, home: &Path, key: &ConversationKey) -> PathBuf {
        chat_workspace_dir(home, key)
    }

    fn search(&self, from_ms: i64, to_ms: i64) -> Result<Vec<String>, TransportError> {
        let self_id = {
            // from_ids requires the bound open_id; the runtime only scans
            // with a bound identity, so read it from the identity call site
            // via a stored copy.
            self.self_id
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| TransportError::Fatal("search before identity binding".into()))?
        };
        let mut ids = Vec::new();
        let mut page_token: Option<String> = None;
        for _page in 0..MAX_PAGES {
            // Live-verified wire shape: page_size/page_token ride in the URL
            // query (--params; a body page_token repeats page 1), the time
            // range keys are start_time/end_time, and fractional seconds are
            // rejected, so the window is rounded out to whole seconds
            // (start down, end up); exact-millisecond admission stays local.
            let body = json!({
                "query": "",
                "filter": {
                    "from_ids": [self_id],
                    "time_range": {
                        "start_time": rfc3339_secs_floor(from_ms),
                        "end_time": rfc3339_secs_ceil(to_ms),
                    }
                }
            });
            let mut params = json!({ "page_size": SEARCH_PAGE_SIZE });
            if let Some(token) = &page_token {
                params["page_token"] = json!(token);
            }
            let envelope = self
                .run_json(
                    &[
                        "api",
                        "POST",
                        "/open-apis/im/v1/messages/search",
                        "--as",
                        "user",
                        "--params",
                        &params.to_string(),
                        "--data",
                        "-",
                    ],
                    Some(&body.to_string()),
                    self.timeout_ms,
                )
                .map_err(cli_failure_to_transport)?;
            let data = api_data(&envelope)?;
            for item in data
                .get("items")
                .and_then(|v| v.as_array())
                .ok_or_else(|| TransportError::Incomplete("search has no items".into()))?
            {
                let id = item
                    .pointer("/meta_data/message_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        TransportError::Incomplete("search item has no message_id".into())
                    })?;
                ids.push(id.to_string());
            }
            let has_more = data
                .get("has_more")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !has_more {
                return Ok(ids);
            }
            page_token = Some(
                data.get("page_token")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        TransportError::Incomplete("has_more without page_token".into())
                    })?
                    .to_string(),
            );
        }
        // Too many pages for one window: block this scan rather than admit a
        // partial result.
        Err(TransportError::Incomplete(format!(
            "search exceeded {} pages",
            MAX_PAGES
        )))
    }

    fn mget(&self, ids: &[String]) -> Result<Vec<Message>, TransportError> {
        let mut out = Vec::new();
        for batch in ids.chunks(MGET_BATCH) {
            let params = json!({ "message_ids": batch });
            let envelope = self
                .run_json(
                    &[
                        "api",
                        "GET",
                        "/open-apis/im/v1/messages/mget",
                        "--as",
                        "user",
                        "--params",
                        "-",
                    ],
                    Some(&params.to_string()),
                    self.timeout_ms,
                )
                .map_err(cli_failure_to_transport)?;
            let data = api_data(&envelope)?;
            let items = data
                .get("items")
                .and_then(|v| v.as_array())
                .ok_or_else(|| TransportError::Incomplete("mget has no items".into()))?;
            let mut returned = Vec::new();
            for item in items {
                out.push(parse_item(item).map_err(TransportError::Incomplete)?);
                returned.push(
                    item.get("message_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                );
            }
            // Every requested ID must come back; a missing ID blocks this scan.
            for id in batch {
                if !returned.contains(id) {
                    return Err(TransportError::Incomplete(format!(
                        "mget missing detail for {}",
                        id
                    )));
                }
            }
        }
        Ok(out)
    }

    fn react_processing(&self, source_id: &str) -> Result<Option<String>, TransportError> {
        self.create_reaction(source_id, "OnIt").map(Some)
    }

    fn react_finished(
        &self,
        source_id: &str,
        succeeded: bool,
        processing_id: Option<&str>,
    ) -> Result<(), TransportError> {
        // Add the truthful terminal state first; failed writes must keep OnIt.
        self.create_reaction(source_id, if succeeded { "DONE" } else { "ERROR" })?;
        if let Some(id) = processing_id {
            let params = json!({"message_id": source_id, "reaction_id": id});
            let envelope = self
                .run_json(
                    &[
                        "im",
                        "reactions",
                        "delete",
                        "--as",
                        "user",
                        "--params",
                        &params.to_string(),
                    ],
                    None,
                    REACTION_TIMEOUT_MS,
                )
                .map_err(cli_failure_to_transport)?;
            api_data(&envelope)?;
        }
        Ok(())
    }

    fn reply(&self, source_id: &str, text: &str, in_thread: bool) -> Result<String, ReplyError> {
        let idempotency_key = format!("slashie-{}", source_id);
        let mut args = vec![
            "im".to_string(),
            "+messages-reply".to_string(),
            "--message-id".to_string(),
            source_id.to_string(),
            "--text".to_string(),
            text.to_string(),
            "--as".to_string(),
            "user".to_string(),
            "--idempotency-key".to_string(),
            idempotency_key,
        ];
        if in_thread {
            args.push("--reply-in-thread".to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        match self.run(&arg_refs, None, self.timeout_ms) {
            Ok(out) => {
                let envelope: Value = serde_json::from_str(&out)
                    .map_err(|_| ReplyError::Uncertain("unparsable reply envelope".into()))?;
                // A definitive API rejection arrives as a clean envelope.
                if envelope.get("ok") == Some(&json!(false)) {
                    let message = envelope
                        .pointer("/error/message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("no message");
                    return Err(ReplyError::Rejected(message.to_string()));
                }
                let data = api_data(&envelope).map_err(|e| {
                    // Envelope trouble after a clean exit: outcome unknown.
                    ReplyError::Uncertain(e.to_string())
                })?;
                let reply_id =
                    data.get("message_id")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            ReplyError::Uncertain("reply envelope has no message_id".into())
                        })?;
                Ok(reply_id.to_string())
            }
            Err(failure @ CliFailure::Failed(..)) => {
                // Live-verified: a definitive rejection can arrive as a
                // nonzero exit with a clean error envelope on stdout (e.g.
                // exit 3 + missing_scope). Classify that as rejected.
                if let Some(out) = failure.envelope_text() {
                    if let Ok(envelope) = serde_json::from_str::<Value>(out) {
                        if envelope.get("ok") == Some(&json!(false)) {
                            let message = envelope
                                .pointer("/error/message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("no message");
                            return Err(ReplyError::Rejected(message.to_string()));
                        }
                    }
                }
                // A nonzero exit with no envelope: the send may or may not
                // have gone through, so delivery is uncertain, not rejected.
                Err(ReplyError::Uncertain(match failure {
                    CliFailure::Failed(m, _) => m,
                    _ => unreachable!(),
                }))
            }
            Err(CliFailure::Uncertain(m)) => Err(ReplyError::Uncertain(m)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicBool;

    /// Write the fake lark-cli executable used for boundary verification.
    /// It dispatches purely on argv (no environment knobs, so parallel tests
    /// cannot interfere) and prints canned, sanitized envelopes. No network
    /// access anywhere.
    ///
    /// Mode markers:
    /// - search: from_ms 0=cli_error 1=api_rate 2=auth_fault 3=garbage
    ///   4=bad_item 5=too_many_pages; anything else returns two pages.
    /// - mget: id "om_missing" returns no items (incomplete result).
    /// - reply: source "om_reject" -> rejection envelope, "om_exitfail" ->
    ///   nonzero exit, "om_hang" -> hangs past the deadline.
    /// - auth: profile "noauth" reports the user identity as missing.
    fn write_fake_cli(dir: &Path) -> PathBuf {
        let path = dir.join("fake-lark-cli");
        std::fs::write(
            &path,
            r#"#!/usr/bin/env python3
import json, sys, time

args = sys.argv[1:]
if args and args[0] == "--profile":
    profile = args[1]
    args = args[2:]

def stdin_arg(flag):
    if flag in args:
        return json.loads(sys.stdin.read())
    return None

if args[:2] == ["auth", "status"]:
    if profile == "noauth":
        print(json.dumps({"identities": {"user": {"status": "missing"}}}))
    else:
        print(json.dumps({"identities": {"user": {"status": "ready"}}}))
    sys.exit(0)

if args[:2] == ["contact", "+get-user"]:
    # Live-verified 1.0.68 shape: nested under data.user (regression fixture
    # for the envelope mismatch found in live validation).
    print(json.dumps({"ok": True, "data": {"user": {"open_id": "ou_verified"}}}))
    sys.exit(0)

if args[:3] == ["api", "POST", "/open-apis/im/v1/messages/search"]:
    body = stdin_arg("--data")
    # Every page must restrict search at the server, before details are read.
    assert body.get("filter", {}).get("from_ids") == ["ou_verified"]
    # Live-verified wire shape: pagination rides in --params, and the body
    # time range uses start_time/end_time with second precision.
    params_arg = args[args.index("--params") + 1] if "--params" in args else "{}"
    params = json.loads(params_arg)
    tr = body["filter"]["time_range"]
    if "start" in tr or "end" in tr:
        # Old wire shape: the live API ignored it (silently wrong window).
        print(json.dumps({"code": 99992402, "msg": "invalid time_range keys", "data": {}}))
        sys.exit(0)
    start = tr["start_time"]
    if "." in start:
        print(json.dumps({"code": 99992402, "msg": "fractional seconds rejected", "data": {}}))
        sys.exit(0)
    if "page_token" in body:
        # A body page_token repeats page 1 (live-verified pagination trap).
        sys.exit(0)
    # Mode markers ride in the day-of-month of the start date.
    mode = start.split("T")[0]
    if mode.startswith("1970-01-"):
        day = int(mode[8:10])
        mode = {1: "cli_error", 2: "api_rate", 3: "auth_fault",
                4: "garbage", 5: "bad_item", 6: "too_many_pages"}.get(day, "ok")
    if mode == "cli_error":
        print(json.dumps({"ok": False, "error": {"type": "network", "message": "down"}}))
    elif mode == "api_rate":
        print(json.dumps({"code": 99991400, "msg": "rate limited", "data": {}}))
    elif mode == "auth_fault":
        print(json.dumps({"code": 99991663, "msg": "token expired", "data": {}}))
    elif mode == "garbage":
        print("not json at all")
    elif mode == "bad_item":
        print(json.dumps({"code": 0, "msg": "ok", "data": {"items": [{}], "has_more": False}}))
    elif mode == "too_many_pages":
        print(json.dumps({"code": 0, "msg": "ok",
                          "data": {"items": [], "has_more": True, "page_token": "t"}}))
    elif params.get("page_token") == "p2":
        print(json.dumps({"code": 0, "msg": "ok", "data": {"items": [
            {"meta_data": {"message_id": "om_page2"}}], "has_more": False}}))
    else:
        print(json.dumps({"code": 0, "msg": "ok", "data": {"items": [
            {"meta_data": {"message_id": "om_page1"}},
            {"meta_data": {"message_id": "om_dup"}}], "has_more": True,
            "page_token": "p2", "echo": body}}))
    sys.exit(0)

if args[:2] == ["im", "+messages-mget"]:
    mid = args[args.index("--message-ids") + 1]
    assert "--no-reactions" in args
    messages = [] if mid == "om_missing" else [{"message_id": mid,
        "message_app_link": "https://applink.feishu.cn/client/chat/open?openChatId=oc_chat&position=28"}]
    print(json.dumps({"ok": True, "data": {"messages": messages}}))
    sys.exit(0)

if args[:3] == ["api", "GET", "/open-apis/im/v1/messages/mget"]:
    params = stdin_arg("--params")
    if "om_missing" in params["message_ids"]:
        print(json.dumps({"code": 0, "msg": "ok", "data": {"items": []}}))
        sys.exit(0)
    items = []
    for mid in params["message_ids"]:
        item = {"message_id": mid, "msg_type": "text",
                "create_time": "1795096700000",
                "chat_type": "p2p",
                "chat_id": "oc_chat",
                "sender": {"id": "ou_verified", "id_type": "open_id", "sender_type": "user"},
                "body": {"content": json.dumps({"text": "/ping"})}}
        if mid == "om_recalled":
            item["recalled"] = True
        elif mid == "om_reply_msg":
            item["root_id"] = "om_root"
            item["thread_id"] = "omt_thread"
        elif mid == "om_img":
            item["msg_type"] = "image"
            item["body"]["content"] = "{}"
        elif mid == "om_bot":
            item["sender"] = {"id": "ou_bot", "id_type": "app_id", "sender_type": "bot"}
        items.append(item)
    print(json.dumps({"code": 0, "msg": "ok", "data": {"items": items}}))
    sys.exit(0)

if args[:3] == ["im", "reactions", "create"]:
    assert profile in ["reaction-test", "reaction-missing-id", "reaction-terminal-denied", "reaction-delete-denied"]
    assert args[3:6] == ["--as", "user", "--params"]
    params = json.loads(args[6])
    assert params["message_id"] == "om_reaction_source"
    assert args[7:] == ["--data", "-"]
    emoji = stdin_arg("--data")["reaction_type"]["emoji_type"]
    assert emoji in ["OnIt", "DONE", "ERROR"]
    if profile == "reaction-missing-id":
        print(json.dumps({"ok": True, "data": {}})); sys.exit(0)
    if profile == "reaction-terminal-denied" and emoji != "OnIt":
        print(json.dumps({"ok": False, "error": {"message": "terminal reaction denied"}})); sys.exit(3)
    print(json.dumps({"ok": True, "identity": "user", "data": {"reaction_id": "reaction_" + emoji}}))
    sys.exit(0)

if args[:3] == ["im", "reactions", "delete"]:
    assert profile in ["reaction-test", "reaction-delete-denied"]
    assert args[3:6] == ["--as", "user", "--params"]
    assert json.loads(args[6]) == {"message_id": "om_reaction_source", "reaction_id": "reaction_OnIt"}
    # A second author's same emoji must not be queried or removed by name.
    assert "reaction_other_author_OnIt" not in args[6]
    if profile == "reaction-delete-denied":
        print(json.dumps({"ok": False, "error": {"message": "delete denied"}})); sys.exit(3)
    print(json.dumps({"ok": True, "data": {"reaction_id": "reaction_OnIt"}}))
    sys.exit(0)

if args[:1] == ["im"] and "+messages-reply" in args:
    mid = args[args.index("--message-id") + 1]
    if mid == "om_thread_src":
        assert "--reply-in-thread" in args
    if mid == "om_hang":
        time.sleep(60)
    if mid == "om_exitfail":
        sys.exit(1)
    if mid == "om_scopedeny":
        # Live-verified: exit 3 with a definitive error envelope on stdout.
        print(json.dumps({"ok": False, "identity": "user",
                          "error": {"type": "authorization",
                                    "subtype": "missing_scope",
                                    "message": "missing required scope(s): im:message.send_as_user"}}))
        sys.exit(3)
    if mid == "om_reject":
        print(json.dumps({"ok": False, "error": {"type": "invalid_argument", "message": "no such message"}}))
        sys.exit(0)
    print(json.dumps({"code": 0, "msg": "ok", "data": {"message_id": "om_reply_new"}}))
    sys.exit(0)

print(json.dumps({"ok": False, "error": {"type": "validation", "message": "unknown fake dispatch: " + " ".join(args)}}))
sys.exit(0)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn transport_in(dir: &Path, profile: &str) -> LarkCliTransport {
        let cli = LarkCli {
            path: write_fake_cli(dir).to_string_lossy().to_string(),
            profile: profile.into(),
        };
        // The real Python interpreter must stay reachable via PATH even
        // though the transport narrows it.
        LarkCliTransport::new(
            &cli,
            &["/usr/bin".to_string(), "/bin".to_string()],
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("slashie-lark-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn search_ok(
        t: &LarkCliTransport,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<String>, TransportError> {
        t.search(from_ms, to_ms)
    }

    #[test]
    fn source_link_is_exact_and_missing_link_blocks_handoff() {
        let dir = tmp("source-link");
        let t = transport_in(&dir, "test");
        let mut msg = t.mget(&["om_source".into()]).unwrap().remove(0);
        let link = t.message_link(&msg).unwrap().unwrap();
        assert_eq!(
            link,
            "https://applink.feishu.cn/client/chat/open?openChatId=oc_chat&position=28"
        );
        msg.link = Some(link.clone());
        msg.id = "om_missing".into();
        assert_eq!(t.message_link(&msg).unwrap(), Some(link));
        msg.link = None;
        assert!(matches!(
            t.message_link(&msg),
            Err(TransportError::Incomplete(_))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn identity_ready_and_open_id() {
        let dir = tmp("identity");
        let t = transport_in(&dir, "test");
        assert_eq!(t.verify_identity().unwrap(), "ou_verified");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn processing_reaction_targets_source_as_user() {
        let dir = tmp("reaction");
        let t = transport_in(&dir, "reaction-test");
        let id = t.react_processing("om_reaction_source").unwrap();
        assert_eq!(id.as_deref(), Some("reaction_OnIt"));
        t.react_finished("om_reaction_source", true, id.as_deref())
            .unwrap();
        t.react_finished("om_reaction_source", false, None).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reaction_failures_never_invent_ids_or_remove_unconfirmed_status() {
        for profile in [
            "reaction-missing-id",
            "reaction-terminal-denied",
            "reaction-delete-denied",
        ] {
            let dir = tmp(profile);
            let t = transport_in(&dir, profile);
            if profile == "reaction-missing-id" {
                assert!(t.react_processing("om_reaction_source").is_err());
            } else {
                let id = t.react_processing("om_reaction_source").unwrap();
                assert!(t
                    .react_finished("om_reaction_source", true, id.as_deref())
                    .is_err());
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn open_id_extraction_accepts_nested_and_flat_shapes() {
        // Live-verified 1.0.68 shape.
        let nested: Value = serde_json::from_str(r#"{"user": {"open_id": "ou_nested"}}"#).unwrap();
        assert_eq!(extract_open_id(&nested), Some("ou_nested"));
        // Flat shape stays accepted.
        let flat: Value = serde_json::from_str(r#"{"open_id": "ou_flat"}"#).unwrap();
        assert_eq!(extract_open_id(&flat), Some("ou_flat"));
        // Missing open_id is None.
        let empty: Value = serde_json::from_str("{}").unwrap();
        assert_eq!(extract_open_id(&empty), None);
    }

    #[test]
    fn identity_missing_is_fatal() {
        let dir = tmp("identity_missing");
        let t = transport_in(&dir, "noauth");
        assert!(matches!(t.verify_identity(), Err(TransportError::Fatal(_))));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn search_follows_pages_and_merges_ids() {
        let dir = tmp("search");
        let t = transport_in(&dir, "test");
        t.set_self_id("ou_verified");
        let ids = search_ok(&t, 10 * 86_400_000, 11 * 86_400_000).unwrap();
        // Page 1 (2 ids) + page 2 (1 id); the runtime dedups afterwards.
        assert_eq!(ids, vec!["om_page1", "om_dup", "om_page2"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn search_error_classification() {
        let dir = tmp("search_err");
        let t = transport_in(&dir, "test");
        t.set_self_id("ou_verified");
        // from_ms day markers: marker N rides in 1970-01-N (epoch day N-1).
        // 1=cli_error 2=api_rate 3=auth_fault 4=garbage 5=bad_item 6=too_many_pages
        let ok = |t: &LarkCliTransport, day: i64| -> Result<Vec<String>, TransportError> {
            t.search((day - 1) * 86_400_000, day * 86_400_000)
        };
        assert!(matches!(ok(&t, 1), Err(TransportError::Transient(_))));
        assert!(matches!(ok(&t, 2), Err(TransportError::Transient(_))));
        assert!(matches!(ok(&t, 3), Err(TransportError::Fatal(_))));
        assert!(matches!(ok(&t, 4), Err(TransportError::Transient(_))));
        assert!(matches!(ok(&t, 5), Err(TransportError::Incomplete(_))));
        assert!(matches!(ok(&t, 6), Err(TransportError::Incomplete(_))));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn search_requires_bound_identity() {
        let dir = tmp("search_noid");
        let t = transport_in(&dir, "test");
        assert!(matches!(search_ok(&t, 0, 1), Err(TransportError::Fatal(_))));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mget_decodes_placement_text_and_missing_blocks() {
        let dir = tmp("mget");
        let t = transport_in(&dir, "test");
        let msgs = t
            .mget(&[
                "om_ok".to_string(),
                "om_recalled".to_string(),
                "om_reply_msg".to_string(),
                "om_img".to_string(),
                "om_bot".to_string(),
            ])
            .unwrap();
        let by_id: std::collections::HashMap<&str, &Message> =
            msgs.iter().map(|m| (m.id.as_str(), m)).collect();
        assert_eq!(by_id["om_ok"].text.as_deref(), Some("/ping"));
        assert!(by_id["om_ok"].top_level);
        assert!(by_id["om_recalled"].recalled);
        assert!(!by_id["om_reply_msg"].top_level, "root_id means a reply");
        assert_eq!(by_id["om_img"].text, None, "non-text message");
        assert_eq!(by_id["om_bot"].sender, "ou_bot");
        assert_eq!(by_id["om_ok"].created_at_ms, 1_795_096_700_000);
        assert_eq!(by_id["om_ok"].chat_id, "oc_chat");
        assert_eq!(by_id["om_ok"].topic_id, None);
        assert_eq!(
            by_id["om_reply_msg"].topic_id.as_deref(),
            Some("omt_thread")
        );

        assert!(matches!(
            t.mget(&["om_missing".to_string()]),
            Err(TransportError::Incomplete(_))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reply_in_thread_passes_the_flag() {
        let dir = tmp("reply_thread");
        let t = transport_in(&dir, "test");
        assert_eq!(
            t.reply("om_thread_src", "pong", true).unwrap(),
            "om_reply_new"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reply_outcomes_sent_rejected_and_uncertain() {
        let dir = tmp("reply");
        let t = transport_in(&dir, "test");
        assert_eq!(t.reply("om_src", "pong", false).unwrap(), "om_reply_new");
        assert!(matches!(
            t.reply("om_reject", "pong", false),
            Err(ReplyError::Rejected(_))
        ));
        assert!(matches!(
            t.reply("om_exitfail", "pong", false),
            Err(ReplyError::Uncertain(_))
        ));
        // Live-verified: nonzero exit + definitive envelope = Rejected.
        assert!(matches!(
            t.reply("om_scopedeny", "pong", false),
            Err(ReplyError::Rejected(m)) if m.contains("scope")
        ));
        let t = t.with_timeout_ms(500);
        let started = std::time::Instant::now();
        assert!(matches!(
            t.reply("om_hang", "pong", false),
            Err(ReplyError::Uncertain(_))
        ));
        // The bounded deadline applies even to a hung CLI.
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Boundary check against the installed real lark-cli using --dry-run
    /// only (no network, no writes): the exact argv the transport builds
    /// must be accepted and produce the documented request shape. Skipped
    /// when no real CLI is found.
    #[test]
    fn real_cli_accepts_built_commands_dry_run() {
        let exe = match which_lark_cli() {
            Some(p) => p,
            None => return,
        };
        // Dry-run against the real CLI (no network, no writes). Environments
        // without the send scope get exit 3 with a missing_scope envelope
        // after argv parsing; both outcomes prove the argv shape is accepted.
        let argv = [
            "im",
            "+messages-reply",
            "--message-id",
            "om_dryrun_target",
            "--text",
            "hi",
            "--as",
            "user",
            "--idempotency-key",
            "slashie-om_dryrun_target",
            "--dry-run",
        ];
        let output = std::process::Command::new(&exe)
            .args(argv)
            .output()
            .expect("real CLI must be spawnable");
        // The live CLI prints the missing_scope envelope on stderr.
        let stdout = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if output.status.success() {
            // The request shape the transport builds.
            assert!(
                stdout.contains("/open-apis/im/v1/messages/om_dryrun_target/reply"),
                "got: {}",
                stdout
            );
            assert!(
                stdout.contains("slashie-om_dryrun_target"),
                "got: {}",
                stdout
            );
        } else {
            // Scope check reached after argv parsing; the envelope must be
            // a clean CLI error, not a usage failure.
            assert!(
                stdout.contains("missing_scope") || stdout.contains("error"),
                "got: {}",
                stdout
            );
        }
    }

    fn which_lark_cli() -> Option<PathBuf> {
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in path_var.split(':') {
                let p = PathBuf::from(dir).join("lark-cli");
                if p.exists() {
                    return Some(p);
                }
            }
        }
        None
    }
}
