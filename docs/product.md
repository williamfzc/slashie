---
type: specification
title: Slashie product specification
description: Feishu-to-local workflow, release scope, and acceptance criteria.
---

# Slashie product specification

Send a slash command in Feishu. Slashie finds the message under an existing authorized Lark CLI user identity, hands the request and source message link to a configured local agent or script. The agent decides whether to send through its own authorized tools; completing work without replying is valid. The intended setup is one account and one host.

## Trigger and reply

1. The operator configures a route and enables polling. A new installation starts paused with [editable starter routes](DESIGN.md#configuration-and-files).
2. The first whitespace-delimited token of a self-authored, searchable, top-level or topic text message must match a configured prefix exactly. `/pingpong` does not match `/ping`. Multiline arguments keep their internal line breaks.
3. Direct messages, ordinary groups, and Feishu topics are release targets. A topic root and replies carrying the same topic ID are eligible; an ordinary reply without a topic ID is ignored. Recalled messages, non-text messages, and Slashie's own delivered replies are ignored.
4. Routes receive the request and exact source message link by default through `SLASHIE_PROMPT`; scripts can also use `SLASHIE_LINK` separately. AI or scripts read the linked conversation/topic with their own authorized tools, then continue the task. Passing a link is not proof that the target has read it. Feishu refuses to start a handoff if it cannot resolve that link. An explicitly arguments-only script can choose `SLASHIE_ARGS`.
5. Software-owned status is independent of `reply_mode`: after a message is durably accepted (including queued work), Slashie attempts `OnIt` on the original message before launching its target. After successful execution and durable settlement, it adds `DONE`, then removes only its own confirmed `OnIt`. Failed execution, timeout, or an unavailable source gets `ERROR`, never `DONE`. Reaction failure is logged and does not change the work result. Paused, unclaimed, or capacity-deferred messages receive no acknowledgment. Shutdown or a crash can leave a prior status unchanged; do not infer success from it.
6. Default `reply_mode: agent` performs no automatic text reply. The target decides whether to send text through its own authorized tools; it is not responsible for software status reactions. Missing text-send access or choosing not to reply is not a work failure. Explicit `reply_mode: auto` retains legacy stdout forwarding only; it does not control status reactions.

A conversation is one channel, one chat, and optionally one topic. Direct-message and ordinary-group top-level messages share the chat conversation; a topic root and its replies share a topic conversation, and different topics are isolated. Routes configured with `session: chat` keep one local agent session per conversation. The first successful request starts the session; later requests in the same conversation resume it. A different chat or topic starts a different session. A failed first command does not create the mapping. Continuity depends on the agent's own session store and its resume command. Resetting a conversation is a local operation on its workspace and mapping.

Enabling polling covers supported conversations visible to the authorized account. A group can see messages the agent explicitly sends, if authorized. Route commands run with the local user's permissions. An agent may send request data to its provider according to its own configuration.

## Controls and faults

Use the [macOS menu](menu-bar.md) to toggle polling, open Settings, and quit. Quitting records shutdown for affected requests; the default mode does not send a cancellation message. Explicit auto mode forwards `Canceled: Slashie is shutting down.` when authorized. Settings edits the polling interval and route commands. Saved edits apply at the next scan without a restart, and the diagnostic log is available from Settings. Headless mode runs the same service in the foreground and logs status. Feishu carries command outcomes. The menu and the local log carry operational faults, including when a reply failed or its delivery is uncertain.

## Pause and recovery

Each start or resume begins with messages sent after that point. Pausing stops new admissions and lets accepted work finish. The polling setting survives a restart. A running process searches a bounded recent window to absorb indexing delays and short interruptions. Requests sent before startup, while paused, or outside that window require a fresh message.

Polling continues while the menu-bar app has no window open; the Mac may still sleep. A claimed message gets at most one execution attempt on the same host while its state is intact. A crash can leave the command or reply outcome uncertain. Check Feishu and the log before sending a fresh request, which can repeat local side effects.

Commands in one conversation run in the order they were sent, one at a time; different conversations run in parallel. A conversation keeps at most a handful of commands queued behind the running one, and a bounded number of conversations hold queues at once — anything beyond that is left for a later scan within the recent window, and a message that ages out of that window before a slot frees is never run and never replied to. If a burst of commands goes unanswered, send a fresh one rather than assuming it is queued.

## Data flow

Message text travels from Feishu to the local command as environment variables (`SLASHIE_PROMPT`, `SLASHIE_ARGS`) and never leaves the machine except through the command itself. The target owns its output and any sending decision. Only explicit auto mode forwards stdout to Feishu. The local log records IDs, durations, and outcomes — not message text or command output.

## Delivery and evidence

| Milestone | Scope and evidence |
| --- | --- |
| macOS prototype | Commit `a23ce0a` records the Feishu-to-command loop and a single-command `/doubao` observation of 6.8–8.8 seconds under the former name `fsq`. |
| v1.1 | The macOS tray, Settings, and foreground headless mode share the Rust runtime. Local build and mock checks passed. A live self-chat test verified that a temporary `/pi` route could read a preceding Feishu message and reply correctly to the original question. Chat-session mapping is verified live with Pi and Claude Code in the self chat: a second request resumed the first conversation after a restart. Topic/thread isolation is verified live in a self-chat thread: same-topic continuation, chat-versus-topic separation, and delivered replies not re-admitted. A Feishu topic group uses the same `thread_id` fields. Codex resume remains pending. |
| v0.2.0 validation | macOS and Linux headless routes were exercised through real Feishu with Doubao CLI 2.37.2 and Lark CLI 1.0.100. The Linux user service completed office work, topic continuations, attachment calculation, source replies, status reactions and lifecycle checks. Native macOS bundle acceptance remains pending; see the [current three-entry matrix](doubao.md#current-three-entry-acceptance). |

The `/pi` context reader was part of the test harness. The shipped route interface passes a source message ID to configured commands. Release acceptance still includes the macOS bundle and native interactions, real direct-message and ordinary-group coverage, and search recall. If search cannot reliably discover requests in the release-target conversations, revisit the trigger mechanism before release.

## Release acceptance

| Case | Pass condition |
| --- | --- |
| Send `/ping` in a direct message and ordinary group. | Slashie adds `OnIt` after durable acceptance, then replaces its own acknowledgment with `DONE` on successful execution. The target receives the exact source link. Agent mode does not force a text reply. |
| Send two commands in each of two topics in one chat, then a follow-up in the first topic. Send a command in an ordinary reply thread without a topic. | Each topic has its own session and workspace. The follow-up resumes the first topic. The ordinary reply is ignored. The target receives the correct topic source; no reply is required in agent mode. A message whose text starts with a command but is Slashie's own delivered reply never executes. |
| Exercise both route input forms with multiline text and shell metacharacters. Send a near-prefix, another author's message, and a reply beginning with a command. | The selected input form arrives as one argument. Source context is included when requested. Message text cannot change the shell program, and ineligible messages never execute. |
| Deliver duplicate and late-indexed results. Kill the process after a claim and during a reply attempt. | Eligible results inside the window run once. Claims survive restart and uncertain attempts appear in the log. Corrupt state blocks new work. |
| Send more commands than the queues hold: a burst in one conversation, and messages from more conversations than slots. | Each scan admits at most the caps ([design](DESIGN.md#polling-and-claims)); the overflow stays unclaimed and later scans admit it. Nothing claimed is ever left unrun. |
| Quit while a command runs and while others wait in its conversation. | Affected claims settle as shutdown; agent mode sends nothing. Auto mode, if authorized, forwards cancellation rather than a command failure. |
| Pause during a command, send messages while paused, resume, and restart. | Accepted work can finish during pause. Earlier messages remain outside every new activation window. |
| Submit work while a command runs, then time out a command that has ordinary child processes. | Execution stays serial, polling proceeds while queue space remains, and the timed-out process group exits. |
| Exercise empty stdout, long output, invalid configuration, expired credentials, and an ambiguous reply. | Replies and local fault states follow the contracts above. The reply marks truncated output. Ambiguous delivery does not trigger a resend. |
| Send two agent requests in one chat, then the same pair in another chat. Reset one workspace and send again. | The second request resumes the first session in each chat. The other chat starts a separate session. After a reset, the next request starts fresh rather than failing. |
| Send a request to two different agent routes in one topic, then follow up with each route. | Each route keeps its own session: each follow-up resumes only that route's earlier conversation, and neither route continues the other's context. |
| Measure repeated `/ping` round trips with an empty queue and healthy Feishu access. | Target the [polling interval](DESIGN.md#polling-and-claims) plus two seconds. Record sample count, median, maximum, and outliers due to indexing or delivery. |
| Launch each macOS artifact, then validate the Linux service in v1.2. | Pass the [menu](menu-bar.md#acceptance) and [build and launch](DESIGN.md#desktop-build) checks. |
