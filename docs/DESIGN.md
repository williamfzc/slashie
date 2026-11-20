---
type: design
title: Slashie technical design
description: Shared Rust runtime, constrained Feishu adapter, and durable claims.
---

# Slashie technical design

The [product specification](product.md) owns user-visible behavior. The [menu bar specification](menu-bar.md) owns native menu interactions and copy.

## Runtime

Use one Rust package and one in-process runtime. Both launch modes share configuration, the real user's `~/.slashie/` directory, and an exclusive OS lock on a stable `app.lock`. Hold the lock until shutdown and close its descriptor on subprocess exec. `slashie` launches the runtime. `lark-cli` provides external Feishu access.

| Owner | Responsibility |
| --- | --- |
| `src/main.rs` | Parse arguments, acquire the lock, and start the selected host. |
| `src/app` | Run Tauri on the macOS main thread. Render runtime status, submit controls, and host Settings. |
| `src/runtime` | Coordinate scans, claims, and a serial worker. Own private storage, transport, and process adapters. |

Give each host a control handle for status, polling changes, and shutdown. A coordinator serializes state transitions. Search and the worker's command and reply I/O run off the UI thread, so controls can take effect while a subprocess is waiting.

### Launch and shutdown

The macOS bundle starts in menu-bar mode. `--headless` selects foreground mode before Tauri initializes. A headless build uses foreground mode by default. `--help` and `--version` exit before opening runtime files. The headless control command `--poll on|off` updates the same persisted polling setting as the menu under the lock, then exits. A headless lock conflict reports an error and exits.

Menu Quit, Cmd+Q, and SIGINT/SIGTERM request shared shutdown. The menu-bar host exits after the runtime drains. Closing Settings only hides its window and leaves the runtime running. Shutdown stops intake, cancels active Feishu calls, terminates and reaps the active command group, and releases the lock after cleanup. Abandoned claims remain consumed. Canceling a request after Feishu may have accepted it leaves delivery uncertain.

### Desktop build

Use Tauri 2 for the macOS shell. Enable `tray` only in native macOS builds. Gate the adapter, Tauri, and the optional `tauri-build` dependency and invocation behind that feature. The committed `Info.plist` merges `LSUIElement` into the bundle so it stays menu-bar only, and the bundle uses a native icon. Open the single Settings window on demand from bundled local assets. Reuse the WebKit runtime already linked by the desktop binary. The window bridge can read and save settings and open the fixed log path. A pure headless artifact excludes the GUI dependency graph at build time. [Cargo features](https://doc.rust-lang.org/cargo/reference/features.html) · [Tauri tray](https://v2.tauri.app/learn/system-tray/) · [Tauri macOS bundling](https://v2.tauri.app/distribute/macos-application-bundle/)

| Artifact | Build and acceptance |
| --- | --- |
| macOS bundle | `cargo tauri build --features tray --bundles app`. Run the [menu acceptance checks](menu-bar.md#acceptance) on the installed bundle. |
| Headless binary | `cargo build --release --no-default-features`. Inspect `cargo tree --no-default-features --edges normal,build` for GUI dependencies. Build and run on Linux without a display session. |
| Shared runtime | Run the same controlled search and `/ping` cases through both macOS launch modes. Check lock contention and verify that switching modes preserves the polling setting and durable claims. |

For v1.2, use a systemd user service under the authorized account. Invoke the foreground binary with the saved polling setting. Use `KillMode=mixed` and allow shared shutdown to finish within the stop timeout. Restart on unexpected failures. A lock conflict or unrecoverable startup fault requires operator action. Validate service credential access separately before promising operation outside a login session. [systemd service lifecycle](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html) · [systemd kill modes](https://www.freedesktop.org/software/systemd/man/latest/systemd.kill.html)

## Feishu adapter

Use `--as user` and the configured Lark CLI profile. An empty profile uses the CLI’s active default. Verify the authorized user's `open_id` and persist the binding. A changed identity blocks intake and delivery. The operator must restore the expected identity. CLI version 1.0.68 is the local inspection baseline. Pin the shipping version after live acceptance.

| Operation | Contract |
| --- | --- |
| Search | Call raw `POST /open-apis/im/v1/messages/search` with `filter.from_ids` and whole-second RFC 3339 `filter.time_range.start_time` and `end_time`. Round the start down and the end up. Pass `page_size` and `page_token` in the query string. Read IDs from `data.items[].meta_data.message_id` and follow `has_more`. Use one strategy that passes recall tests in the release account. Try a sender-and-time search with an empty query first, then test configured-prefix queries if needed. |
| Read details | Batch up to 50 IDs through raw `GET /open-apis/im/v1/messages/mget`. Decode `body.content`, verify sender and creation time locally, and use the verified meanings of `root_id` and `parent_id` to admit original messages only. Validate the response shape and skip unclassifiable placement. |
| Source | Use a verified message link if available, then a conversation link if available. Always include the message ID. |
| Processing reaction | Call `im reactions create --as user` with the original message ID and `OnIt`. The serial worker makes one bounded attempt immediately before launching the command, after durable admission. The reaction requires `im:message.reactions:write_only`. Its failure does not affect execution or reply delivery. |
| Reply | Call `im +messages-reply --as user` with the source message ID, literal text, verified placement, and an idempotency key derived from the source ID. Use one adapter attempt. A confirmed reply returns a reply ID. Distinguish definitive rejection from uncertain delivery. |

The search shortcut projects out placement fields and can report successful exit with IDs only if detail retrieval fails. Use the raw adapter and validate the CLI envelope and required fields. An incomplete result or missing page blocks admission from that scan. Release requires live tests for search recall, original versus reply classification, reply placement, timestamps, permissions, and text that resembles mentions. [Pinned CLI search](https://github.com/larksuite/cli/blob/v1.0.68/shortcuts/im/im_messages_search.go) · [Pinned CLI conversion](https://github.com/larksuite/cli/blob/v1.0.68/shortcuts/im/convert_lib/content_convert.go) · [Lark CLI reply reference](https://github.com/larksuite/cli/blob/main/skills/lark-im/references/lark-im-messages-reply.md)

## Polling and claims

- At the configured polling interval, start at most one scan with a fixed end time. Look back at most five minutes and locally require creation after the current start or resume time, even when the search API uses coarser timestamps. Retry transient scan failures with exponential backoff up to five minutes, honoring a longer server retry delay if supplied. Clear backoff on a complete scan. Authentication and local state faults require operator action.
- Merge duplicate IDs and consider eligible messages in creation order, with message ID as the tiebreaker. Split at the first whitespace and do an exact prefix lookup. Preserve the internal line breaks of the remaining input. Exclude all replies using the validated Feishu placement rules.
- Run one worker with at most 10 waiting jobs. Reserve space, then durably claim the message ID before handing it off. Leave candidates unclaimed when the queue is full so a later scan can find them within the window. The worker finishes a command and its reply attempt before taking the next job.
- Keep pending and settled claims keyed by message ID. Settle each claim after its command and reply attempt. On restart, report pending claims as interrupted and settle them before pruning or polling. Replace state atomically using a synced file and directory. Missing or invalid state after initialization blocks intake.
- Keep unsettled claims and settled claims inside the current scan horizon. Persist the latest completed scan end for diagnostics. Each scan overlaps the previous window within the five-minute horizon so late-indexed and unclaimed requests can be found. Durable claims prevent duplicate execution. Pause and claim updates are serialized. Resume sets a new time boundary, and a pause invalidates results from a scan already in flight.

## Command execution

Run the trusted route program with `/bin/sh -c`. Pass message data through quoted `SLASHIE_ARGS` or `SLASHIE_PROMPT` values. The prompt is the argument text, a blank line, and the source. The shell program comes entirely from the local configuration.

Close the command's stdin pipe writer to signal EOF. Give each subprocess its own Unix process group. Apply a bounded deadline to Feishu calls and each route's configured timeout to its command. On timeout, shutdown, or ordinary exit with surviving group members, send SIGTERM to the group, wait up to two seconds, then SIGKILL remaining members. Reap the direct child. A child that leaves the group is outside this guarantee. Bound the final pipe drain in case a detached child holds a pipe open. [Rust Unix process groups](https://doc.rust-lang.org/std/os/unix/process/trait.CommandExt.html#tymethod.process_group)

Drain stdout and stderr concurrently and keep at most 64 KiB of each in memory. Send the outcome text described in the [product contract](product.md#trigger-and-reply). For diagnostics use stderr, then stdout if stderr is empty. Limit the complete reply to 4,000 Unicode characters and the verified API byte limit, including any truncation notice. Persist only operational metadata. Log command exit or timeout separately from reply sent, rejected, or uncertain.

## Configuration and files

Read and validate configuration once at launch. Configure the Lark CLI executable and optional profile. Give both hosts the same subprocess `PATH`. Add the Node directory if the installed Lark CLI is an npm wrapper. [Tauri bundle environment](https://v2.tauri.app/distribute/macos-application-bundle/) · [Lark CLI npm entry point](https://github.com/larksuite/cli/blob/main/package.json)

```json
{
  "lark_cli": { "path": "/absolute/path/to/lark-cli", "profile": "" },
  "process_path": ["/usr/bin", "/bin", "/usr/sbin", "/sbin"],
  "poll_interval": 20,
  "routes": [
    { "prefix": "/ping", "cmd": "echo pong" }
  ]
}
```

On initial setup, the stock configuration includes `/ping` (replies `pong`), `/pi` (Pi), `/claude` (Claude Code), and `/codex` (Codex). These are editable routes. Start with polling paused. Put the installed tools and any required runtime, such as Node, in `process_path`. The agent routes forward the question and source metadata. Reading conversation history requires a separate context tool.

`poll_interval` is 10, 20, 30, or 60 seconds and defaults to 20. Each prefix is a unique slash-prefixed token. Each `cmd` is a nonempty shell program. An optional route `dir` is an absolute path or starts with `~/`. It defaults to the user's home directory. An optional `timeout` is a positive integer in seconds, defaulting to 30. `process_path` contains ordered absolute directories and defaults to the example.

| File | Source of truth |
| --- | --- |
| `config.json` | Polling interval, CLI and path settings, and route definitions. Restart after edits. |
| `state.json` | Polling setting, identity binding, latest completed scan end, and consumed IDs. |
| `app.lock` | Instance ownership via a held descriptor. |
| `slashie.log` | Timestamps, IDs, durations, and outcomes. Rotate at 2 MiB and keep one previous log. |

Settings and headless use the same configuration schema and validator. Save atomically and preserve fields the window does not edit. If the file changed since the window loaded it, reject the save and ask the user to reopen Settings. Apply the saved configuration at the next startup.

Keep the runtime directory and files accessible only to the local user. Credentials remain in Lark CLI's credential store.
