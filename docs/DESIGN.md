---
type: design
title: Slashie technical design
description: Shared Rust runtime, constrained Feishu adapter, and durable claims.
---

# Slashie technical design

The [product specification](product.md) owns user-visible behavior. The [menu bar specification](menu-bar.md) owns native menu interactions and copy.

## Runtime

Use one Rust package and one in-process runtime. Both artifacts (the macOS bundle and the headless binary) share configuration, the real user's home runtime directory (`~/.slashie/`; `%USERPROFILE%\.slashie` on Windows), and an exclusive OS lock on a stable `app.lock` (`flock` on Unix, `LockFileEx` through `File::try_lock` on Windows). Hold the lock until shutdown and close its descriptor on subprocess exec. `slashie` launches the runtime. `lark-cli` provides external Feishu access.

| Owner | Responsibility |
| --- | --- |
| `src/main.rs` | Parse arguments, acquire the lock, and start the selected host. |
| `src/app` | Run Tauri on the macOS main thread. Render runtime status, submit controls, and host validated Settings edits. |
| `src/runtime` | Coordinate scans, claims, and per-conversation mailboxes. Own private storage and process execution. |
| `src/transport.rs` | Define the `Channel` boundary, `ConversationKey`, the message shape, and the fixture channel. |
| `src/lark.rs` | Implement the Feishu channel: CLI process, search, details, reaction, reply, and identity. |

Give each host a control handle for status, polling changes, and shutdown. A coordinator serializes state transitions. Search and the worker's command and reply I/O run off the UI thread, so controls can take effect while a subprocess is waiting.

### Launch and shutdown

The macOS bundle starts in menu-bar mode and contains no foreground host; a headless build is foreground-only. Host selection is a build-time property of each artifact, with no runtime override. `--help`, `--version`, and the offline `route` subcommands exit before opening runtime files. The offline `log` subcommand also runs before the instance lock and opens only the operational log. The headless control command `--poll on|off` updates the same persisted polling setting as the menu under the lock, then exits. A headless lock conflict reports an error and exits.

Menu Quit, Cmd+Q, and SIGINT/SIGTERM request shared shutdown; on Windows, console control (Ctrl+C, console close) sets the same flag through a handler so the drain behaves the same. The menu-bar host exits after the runtime drains. Closing Settings only hides its window and leaves the runtime running. The host holds a user-initiated activity token so App Nap cannot suspend polling while no window is visible; idle system sleep is still allowed. Shutdown stops intake, cancels active Feishu calls, terminates and reaps the active command group, and releases the lock after cleanup. Abandoned claims remain consumed. Canceling a request after Feishu may have accepted it leaves delivery uncertain.

### Desktop build

Use Tauri 2 for the macOS shell. Keep the Tauri bundle version aligned with the Rust package version. Local bundles receive an ad-hoc resource signature; this is not a Developer ID signature or notarization and does not establish Gatekeeper acceptance on another machine. Enable `tray` only in native macOS builds. Gate the adapter, Tauri, and the optional `tauri-build` dependency and invocation behind that feature. The committed `Info.plist` merges `LSUIElement` into the bundle, and the host also sets the accessory activation policy at startup so bundled and unbundled dev runs alike show no Dock icon; the tray item is the only presence. The bundle uses a native icon. Open the single Settings window on demand from bundled local assets. Reuse the WebKit runtime already linked by the desktop binary. The window bridge can read and save settings and open the fixed log path. A pure headless artifact excludes the GUI dependency graph at build time. [Cargo features](https://doc.rust-lang.org/cargo/reference/features.html) · [Tauri tray](https://v2.tauri.app/learn/system-tray/) · [Tauri macOS bundling](https://v2.tauri.app/distribute/macos-application-bundle/)

| Artifact | Build and acceptance |
| --- | --- |
| macOS bundle | `cargo tauri build --features tray --bundles app,dmg`. Pushing a `v*` tag builds the bundle through the release workflow and publishes it on a GitHub Release. Run the [menu acceptance checks](menu-bar.md#acceptance) on the installed bundle. |
| Headless binary | `cargo build --release --no-default-features` on Linux, macOS (aarch64), and Windows (x86_64). Inspect `cargo tree --no-default-features --edges normal,build` for GUI dependencies. Build and run on Linux without a display session. Windows acceptance beyond CI is pending live validation. |
| Shared runtime | Run the same controlled search and `/ping` cases through both artifacts (bundle and headless binary). Check lock contention and verify that switching artifacts preserves the polling setting and durable claims. |

For v1.2, use a systemd user service under the authorized account. Invoke the foreground binary with the saved polling setting. Use `KillMode=mixed` and allow shared shutdown to finish within the stop timeout. Restart on unexpected failures. A lock conflict or unrecoverable startup fault requires operator action. Validate service credential access separately before promising operation outside a login session. [systemd service lifecycle](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html) · [systemd kill modes](https://www.freedesktop.org/software/systemd/man/latest/systemd.kill.html)

## Channels

The runtime depends on `Channel`, not on Feishu. A channel owns its wire operations, its conversation identity, and its workspace layout. Feishu is the first implementation (`id = feishu`); the fixture-backed mock implements the same trait for tests.

- `conversation_key(message)` returns `{ channel, chat_id, topic_id }`. Feishu maps `thread_id` to `topic_id`; a chat without a topic uses the chat alone.
- `workspace_dir(home, key)` returns the channel's stable session path. Feishu keeps `~/.slashie/chats/<chat-id>[/<topic-id>]` so existing agent sessions keep their working directory.
- `search`, `mget`, `react_processing`, `react_finished`, and `reply` are the channel's I/O. Software reactions report acceptance and terminal work state in every text delivery mode. `reply` receives whether the source is in a topic and keeps text in that thread.

A future channel implements `Channel` and is selected by the host. The `channel` configuration selects one adapter and holds its settings. Session state already keys by channel, and each channel chooses its workspace layout, so the runtime and session mapping do not change when another channel is added.

### Feishu channel

Use `--as user` and the configured Lark CLI profile. An empty profile uses the CLI’s active default. Verify the authorized user's `open_id` and persist the binding. Both `ready` and `needs_refresh` allow the authenticated `contact +get-user` lookup; only a successful lookup binds the identity. The CLI, not Slashie, refreshes its saved user login. A missing login or failed refresh never falls back to bot access. A changed identity blocks intake and delivery. The operator must restore the expected identity. CLI version 1.0.100 has been exercised on macOS and Linux; 1.0.68 remains the earlier wire-inspection baseline.

| Operation | Contract |
| --- | --- |
| Search | Call raw `POST /open-apis/im/v1/messages/search` with `filter.from_ids` and whole-second RFC 3339 `filter.time_range.start_time` and `end_time`. Round the start down and the end up. Pass `page_size` and `page_token` in the query string. Read IDs from `data.items[].meta_data.message_id` and follow `has_more`. When a route opts into mentions, also run an empty-query search with `is_at_me: true`, `chat_type: "group"`, and `from_types: ["user"]` in the filter for the same window. Take only mention hits with `meta_data.is_p2p_chat: false` and merge the IDs. A recent live probe confirmed that a topic root and a reply inside that topic are returned as separate IDs after indexing. |
| Read details | Batch up to 50 IDs through raw `GET /open-apis/im/v1/messages/mget`. Decode `body.content`, verify sender and creation time locally, and decode `chat_id` and `thread_id`. For mention triggers, match the leading text placeholder to the structured `mentions` entry and compare its open ID to the bound account. A message with a `thread_id` is a topic message and is eligible even when it is a reply; a reply without a `thread_id` is an ordinary reply and is skipped. Validate the response shape and skip unclassifiable placement. |
| Source | For each accepted handoff, preserve `message_app_link` when supplied or resolve it once through `im +messages-mget --message-ids <source> --no-reactions --format json`. Preserve the returned URL verbatim, never invent a position or sharing token. Missing source links fail the handoff before the target program starts. Include the message ID, channel, chat, and topic as additional data. The target reads context using its own authorized tools; Slashie does not bulk-fetch history. |
| Processing reaction | Independent of text delivery mode. Immediately after a durable claim, call `im reactions create --as user` on the source message with `OnIt` or the [queued status](product.md#trigger-and-reply), using a bounded deadline. Store the confirmed reaction ID and queue marker kind in that claim before handing it to a worker. Remove the owned queue marker before executing the waiting job. Keep the recorded ID for terminal cleanup when the removal fails. This requires existing `im:message.reactions:write_only` access, separate from text-send access. Never acknowledge a message that was not durably claimed. Reaction errors are recorded, not a reason to rerun or reject work. |
| Terminal reaction | After execution and durable settlement, add `DONE` for exit code zero or `ERROR` for failure, timeout, or unavailable source. Once the terminal reaction is confirmed, delete only the original `OnIt` by its stored ID. Do not delete an acknowledgment after a failed terminal write, invent an ID, or mark canceled/uncertain work DONE. A turn stopped by steering schedules only the removal of its owned acknowledgment and persists that removal for retries. Shutdown may leave the previous acknowledgment; restart does not replay old work or fabricate completion. |
| Reply | In explicit auto mode only, call `im +messages-reply --as user` with the source message ID, literal text, `--reply-in-thread` when the source has a `thread_id`, and an idempotency key derived from the source ID. Use one adapter attempt. A confirmed reply returns a reply ID. Record that ID so it is never admitted as a new command. Distinguish definitive rejection from uncertain delivery. |

The search shortcut projects out placement fields and can report successful exit with IDs only if detail retrieval fails. Use the raw adapter and validate the CLI envelope and required fields. An incomplete result or missing page blocks admission from that scan. Release requires live tests for search recall, original versus reply classification, reply placement, timestamps, permissions, and text that resembles mentions. [Pinned CLI search](https://github.com/larksuite/cli/blob/v1.0.68/shortcuts/im/im_messages_search.go) · [Pinned CLI conversion](https://github.com/larksuite/cli/blob/v1.0.68/shortcuts/im/convert_lib/content_convert.go) · [Lark CLI reply reference](https://github.com/larksuite/cli/blob/main/skills/lark-im/references/lark-im-messages-reply.md)

## Polling and claims

- At the configured polling interval, start at most one scan with a fixed end time. Look back at most five minutes and locally require creation after the current start or resume time, even when the search API uses coarser timestamps. Retry transient scan failures on the next scan (exponential backoff up to five minutes is planned, honoring a longer server retry delay if supplied). Clear the retry state on a complete scan. Authentication and local state faults require operator action.
- Merge duplicate IDs and consider eligible messages in creation order, with message ID as the tiebreaker. Split at the first whitespace and do an exact prefix lookup. Preserve the internal line breaks of the remaining input. A topic root or a reply carrying a `thread_id` is eligible; an ordinary reply without a `thread_id` is skipped. Never admit a message ID recorded as Slashie's own reply.
- Run one mailbox per conversation (`channel + chat + topic`): a FIFO queue and a dedicated worker thread. Commands in one conversation run strictly in admission order — chat sessions therefore never race their own resume chain — while different conversations progress in parallel, so one slow chat never blocks another. Workers spawn on first delivery and retire after idling a minute; retirement deregisters before draining, so a re-delivery on the same conversation only ever reaches a fresh mailbox.
- Admit against mailbox capacity before claiming: one mailbox holds at most a few unfinished jobs (including the one in flight), and only a bounded number of conversations hold mailboxes at once. Admission counts both live occupancy and the scan's own earlier claims, since occupancy moves only at delivery and completion. Candidates beyond a cap stay unclaimed so a later scan can find them within the window. Durably claim before handing off, and never deliver a job without a claim.
- Shutdown sends every mailbox a stop marker after its queued jobs, then joins the workers; active command groups terminate through the shared shutdown flag and queued commands settle as shutdown without starting. If the process dies before durable settlement, the claim recovers as interrupted on restart.
- Keep pending and settled claims keyed by message ID. Settle each claim after its command and reply attempt. On restart, report pending claims as interrupted and settle them before pruning or polling. Replace state atomically using a synced file and directory. Missing or invalid state after initialization blocks intake.
- Build a candidate state for each admission and persist it before replacing memory. A failed later claim stops further admission but returns earlier durable jobs for delivery; the failed candidate stays unclaimed for another scan.
- Terminal feedback intent is stored with settlement. If settlement cannot be saved, keep it in memory and flush the full state before any terminal write. Each subsequent scan (even paused) and restart retries only persisted terminal intent, without repeating commands or text replies. A pending terminal intent is not pruned. Serialize concurrent status flushes. API rejection/uncertainty keeps intent for the next bounded scan; the API's per-user/emoji identity makes creation idempotent, and deleting an already-removed owned reaction is a no-op. No other author's status ID is selected.
- Keep unsettled claims and settled claims inside the current scan horizon. Persist the latest completed scan end for diagnostics. Each scan overlaps the previous window within the five-minute horizon so late-indexed and unclaimed requests can be found. Durable claims prevent duplicate execution. Pause and claim updates are serialized. Resume sets a new time boundary, and a pause invalidates results from a scan already in flight.

## Command execution

Run the trusted route program with `/bin/sh -c` (`sh -c` resolved from PATH on Windows, supplied by Git for Windows). Pass message data through the environment variables below. The prompt is the argument text, a blank line, and the source. The shell program comes entirely from the local configuration.

| Variable | Value |
| --- | --- |
| `SLASHIE_ARGS` | Argument text after the prefix, with internal line breaks preserved. |
| `SLASHIE_PROMPT` | Arguments, a blank line, and the source description including the exact message link. |
| `SLASHIE_ID` | Source message ID. |
| `SLASHIE_LINK` | Exact source message navigation URL. Feishu refuses to launch a handoff when resolution fails. |
| `SLASHIE_CONTEXT` | Source description: message ID, channel, chat ID and type, topic ID, link, and sender. |
| `SLASHIE_CHANNEL` | Channel ID, currently `feishu`. |
| `SLASHIE_CHAT` | Chat ID, or empty when the channel cannot provide one. |
| `SLASHIE_TOPIC` | Topic/thread ID, or empty for a chat without a topic. |
| `SLASHIE_SESSION_DIR` | Conversation workspace when the route has `session: "chat"`; otherwise empty. |
| `SLASHIE_CONTINUE` | `1` when that conversation route may resume its agent session; otherwise `0`. |

Quote values that may contain whitespace. Commands consuming message data should expand the variables in quotes. The runtime never interpolates message text into the shell program.

Close the command's stdin pipe writer to signal EOF. Isolate each subprocess at spawn — its own Unix process group; a plain child on Windows. Apply a bounded deadline to Feishu calls and each route's configured timeout to its command. On timeout or shutdown, apply the graceful-then-force contract (`src/process.rs`): SIGTERM to the group — `taskkill /PID <pid> /T` on Windows — wait up to two seconds, then SIGKILL remaining members — `taskkill /T /F` on Windows. Reap the direct child. A child that leaves the group is outside this guarantee. The Windows liveness probe watches only the direct child; `taskkill /T /F` removes any pipe-holding survivor it might have left behind. Bound the final pipe drain in case a detached child holds a pipe open. [Rust Unix process groups](https://doc.rust-lang.org/std/os/unix/process/trait.CommandExt.html#tymethod.process_group)

Drain stdout and stderr concurrently and keep at most 64 KiB of each in memory. Default `reply_mode: agent` delegates text sending to the target; software status reactions remain enabled in both modes. Explicit `auto` sends the outcome text described in the [product contract](product.md#trigger-and-reply). For diagnostics use stderr, then stdout if stderr is empty. Limit the complete reply to 4,000 Unicode characters and the verified API byte limit, including any truncation notice. Persist only operational metadata. Log command exit or timeout separately from reply sent, rejected, or uncertain.

### Chat sessions

A route with `"session": "chat"` keeps one agent conversation per conversation, where a conversation is `channel + chat + topic`. It runs in the channel's workspace; Feishu uses `~/.slashie/chats/<chat-id>[/<topic-id>]/`, created with user-only permissions. `dir` is not allowed on such a route because the workspace must stay stable for the agent's own session lookup.

A direct-message or ordinary-group top-level message has no topic and uses the chat conversation. A topic root and every reply inside that topic share the Feishu `thread_id` and therefore share one topic conversation; different topics in the same chat are isolated. Replies are eligible commands, so a follow-up inside a topic continues the same agent session. This follows the topic-group model used by [botmux](https://github.com/deepcoldy/botmux/blob/master/docs-site/docs/zh/session-model.md): each new topic is an independent session, and a reply inside the topic continues it.

The first successful command for a `(conversation, route)` pair records that mapping in `state.json`. Later commands are told to resume: `SLASHIE_CONTINUE=1`, and the stock agent routes call `--continue` or `exec resume --last` in the same workspace. Unmanaged routes leave the session ID to the agent CLI. Managed Codex uses JSONL output to read the session ID and saves it with successful and deliberately interrupted turns. On the first managed turn after upgrading an existing session, Slashie may use the normal workspace-scoped resume and then record its exact ID. The active handle becomes interruptible after Codex starts an item in the turn. Stop and reap the old command using the existing process-tree termination path, then run the queued follow-up with `codex exec resume <id>`. The acknowledgment and reply rules are in [the product specification](product.md#pause-and-recovery).

A different chat or topic gets a different workspace, so it starts fresh. A failed first command does not create the mapping. To reset an existing conversation, stop Slashie, delete its workspace, and remove its entries from `state.json`; the next command starts a new agent session. If the agent's own session store is deleted, the resume command fails and the reply carries the diagnostic.

## Configuration and files

Read and validate configuration at launch, and again whenever the file's modification time changes: every scan starts by re-reading `config.json`, so saved edits and CLI route changes apply without a restart. A file that fails to read or validate keeps the previous configuration and logs the reason. Routes and `poll_interval` apply from the next scan; `process_path` applies to commands started afterwards; the Feishu channel binding (`channel.lark_cli`) is built at startup and still requires a restart. Configure the Lark CLI executable and optional profile under the Feishu channel. Existing configs with top-level `lark_cli` still load. Give both hosts the same subprocess `PATH`. Add the Node directory if the installed Lark CLI is an npm wrapper. [Tauri bundle environment](https://v2.tauri.app/distribute/macos-application-bundle/) · [Lark CLI npm entry point](https://github.com/larksuite/cli/blob/main/package.json)

```json
{
  "channel": {
    "type": "feishu",
    "lark_cli": { "path": "/absolute/path/to/lark-cli", "profile": "" }
  },
  "process_path": [
    "/usr/bin", "/bin", "/usr/sbin", "/sbin", "/usr/local/bin", "/opt/homebrew/bin"
  ],
  "poll_interval": 20,
  "routes": [
    { "prefix": "/ping", "cmd": "echo pong", "allow_mentions": true },
    {
      "prefix": "/claude",
      "cmd": "if [ \"$SLASHIE_CONTINUE\" = 1 ]; then claude -p --continue -- \"$SLASHIE_PROMPT\"; else claude -p -- \"$SLASHIE_PROMPT\"; fi",
      "session": "chat"
    }
  ]
}
```

On initial setup, the stock configuration includes `/ping` (runs `echo pong`; forwarded only in auto mode), `/pi` (Pi), `/claude` (Claude Code), and `/codex` (Codex). These are editable routes. Start with polling paused. Put the installed tools and any required runtime, such as Node, in `process_path`. The three agent routes use `session: "chat"` and switch to the agent's resume form when `SLASHIE_CONTINUE` is `1`. Their own conversation is resumed per chat or topic; reading the rest of the Feishu chat still requires a separate context tool.

`poll_interval` is any whole number of seconds from 1 to 60, and defaults to 5. Choose a rate that fits the operator's Feishu API budget; 5 seconds is 12 scans per minute and 1 second is 60. Each prefix is a unique slash-prefixed token. Each `cmd` is a nonempty shell program. The optional `allow_mentions` flag defaults to false and enables [group mention triggers](product.md#trigger-and-reply) for that route. The optional `follow_up` field defaults to `queue`. `steer` is accepted only for the stock `/codex` command with `session: "chat"`. An optional route `dir` is an absolute path or starts with `~/`. It defaults to the user's home directory. An optional `timeout` is a positive integer in seconds, defaulting to 30. An optional `session` may be `"chat"` for one agent session per conversation (chat or topic); it cannot be combined with `dir`. `process_path` contains ordered absolute directories and defaults to the example: on Unix, the system dirs plus the common Homebrew prefixes, so the stock agent routes resolve without edits; on Windows the default is empty, so a route command inherits the ambient PATH. A tool in a private directory (such as `~/.local/bin`) needs its directory added here. Windows additionally requires `sh` (Git for Windows) on the binary's own PATH for route resolution.

| File | Source of truth |
| --- | --- |
| `config.json` | Polling interval, CLI and path settings, and route definitions. Routes and the interval apply at the next scan; `channel` is read at startup. |
| `state.json` | Polling setting, identity binding, latest completed scan end, consumed IDs, the channel/chat/topic session mapping, and delivered reply IDs. |
| `chats/<chat-id>[/<topic-id>]/` | Per-conversation agent workspace for routes with `session: "chat"`. User-only permissions. |
| `app.lock` | Instance ownership via a held descriptor. |
| `slashie.log` | Timestamps, IDs, durations, and outcomes. Rotate at 2 MiB and keep one previous log. Empty scans log nothing; a completed scan reports itself only when it found an eligible message. Retrieve recent entries with `slashie log`. |

Settings controls listening and polling frequency. Missing reply permission is not an availability fault in default agent mode. Settings also edits each route's program. It preserves unknown JSON properties, compares the editor's starting file with the latest contents to reject stale writes, validates the whole configuration and saves atomically. The raw configuration remains under Advanced; external file and CLI edits are supported. All paths share this schema and validator. Route programs and interval apply without restart; changing the channel binding requires a restart. A read-only connection check is limited to once per minute per app instance; it does not send a message or prove a target program is working.

Manage routes offline through the CLI: `slashie route list` prints the route table as JSON, `slashie route add <PREFIX> --cmd <COMMAND> [--dir <PATH>] [--timeout <SECS>] [--session chat] [--allow-mentions] [--follow-up queue|steer]` adds one route, and `slashie route remove <PREFIX>` removes it. `slashie log [--last N] [--id MESSAGE_ID]` prints recent operational log lines so a target agent can diagnose its own handoff; it is strictly read-only and never admits, claims, or re-runs work. `slashie --skill` prints this operating contract as a SKILL.md-style document, so any local agent that discovers the binary can teach itself how to operate Slashie. These subcommands run before the instance lock, so they work while the app is running and never trigger the second-instance alert. Route edits reuse the schema and validator, validate the whole file before writing atomically, and refuse to edit an unparsable configuration. This is the interface agents use to configure routes on the user's behalf; the runtime applies the saved file at its next scan.

Keep the runtime directory and files accessible only to the local user. Credentials remain in Lark CLI's credential store.

### Delivery ownership

`reply_mode` defaults to `agent`; omitted old configuration also uses agent-owned text sending. Slashie owns acceptance and completion reactions regardless of that mode. A target may decide not to send text, or send via its own authorized tools; lack of text-send permission never prompts another identity or transport. Claims record execution plus `agent_delivery`, not a fabricated confirmed text send. `auto` is an explicit compatibility mode that forwards stdout and retains rejection/uncertain-send state; it does not enable or disable reactions. This mode is not selected by target-name detection or login status.
