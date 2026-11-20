---
type: specification
title: Slashie product specification
description: Feishu-to-local workflow, release scope, and acceptance criteria.
---

# Slashie product specification

Send a slash command in Feishu. Slashie finds the message under an existing authorized Lark CLI user identity, runs a configured command on the user's machine, and replies to the original message. The intended setup is one account and one host.

## Trigger and reply

1. The operator configures a route and enables polling. A new installation starts paused with [editable starter routes](DESIGN.md#configuration-and-files).
2. The first whitespace-delimited token of a self-authored, searchable, top-level text message must match a configured prefix exactly. `/pingpong` does not match `/ping`. Multiline arguments keep their internal line breaks.
3. Direct messages and ordinary groups are release targets. Replies, recalled messages, and non-text messages are ignored. Topic support requires its own evidence of root classification and reply placement.
4. Routes can pass message arguments as one value, with or without source context. Context includes the message ID and an available message or conversation link.
5. When an accepted request starts processing, Slashie attempts one Feishu `OnIt` reaction on the original message. A reaction failure is logged and processing continues.
6. Slashie attempts one text reply targeting the original message. Success returns stdout, with a short acknowledgment for empty output. Failure or timeout returns the reason and available diagnostics. Long replies include a truncation notice. Slashie discards output beyond the excerpt.

Enabling polling covers supported conversations visible to the authorized account. A group can see the reply. Route commands run with the local user's permissions. An agent may send request data to its provider according to its own configuration.

## Controls and faults

Use the [macOS menu](menu-bar.md) to toggle polling, open Settings, and quit. Settings edits the polling interval and route commands. Saved edits take effect after restart, and the diagnostic log is available from Settings. Headless mode runs the same service in the foreground and logs status. Feishu carries command outcomes. The menu and the local log carry operational faults, including when a reply failed or its delivery is uncertain.

## Pause and recovery

Each start or resume begins with messages sent after that point. Pausing stops new admissions and lets accepted work finish. The polling setting survives a restart. A running process searches a bounded recent window to absorb indexing delays and short interruptions. Requests sent before startup, while paused, or outside that window require a fresh message.

A claimed message gets at most one execution attempt on the same host while its state is intact. A crash can leave the command or reply outcome uncertain. Check Feishu and the log before sending a fresh request, which can repeat local side effects.

## Delivery and evidence

| Milestone | Scope and evidence |
| --- | --- |
| macOS prototype | Commit `a23ce0a` records the Feishu-to-command loop and a single-command `/doubao` observation of 6.8–8.8 seconds under the former name `fsq`. |
| v1.1 | The macOS tray, Settings, and foreground headless mode share the Rust runtime. Local build and mock checks passed. A live self-chat test verified that a temporary `/pi` route could read a preceding Feishu message and reply correctly to the original question. |
| v1.2 | Linux headless build and systemd user service. Acceptance is pending Linux validation. |

The `/pi` context reader was part of the test harness. The shipped route interface passes a source message ID to configured commands. Release acceptance still includes the macOS bundle and native interactions, real direct-message and ordinary-group coverage, and search recall. If search cannot reliably discover requests in the release-target conversations, revisit the trigger mechanism before release.

## Release acceptance

| Case | Pass condition |
| --- | --- |
| Send `/ping` in a direct message and ordinary group. | An `OnIt` reaction is attempted when processing starts. One reply references the original message in each released surface. |
| Exercise both route input forms with multiline text and shell metacharacters. Send a near-prefix, another author's message, and a reply beginning with a command. | The selected input form arrives as one argument. Source context is included when requested. Message text cannot change the shell program, and ineligible messages never execute. |
| Deliver duplicate and late-indexed results. Kill the process after a claim and during a reply attempt. | Eligible results inside the window run once. Claims survive restart and uncertain attempts appear in the log. Corrupt state blocks new work. |
| Pause during a command, send messages while paused, resume, and restart. | Accepted work can finish during pause. Earlier messages remain outside every new activation window. |
| Submit work while a command runs, then time out a command that has ordinary child processes. | Execution stays serial, polling proceeds while queue space remains, and the timed-out process group exits. |
| Exercise empty stdout, long output, invalid configuration, expired credentials, and an ambiguous reply. | Replies and local fault states follow the contracts above. The reply marks truncated output. Ambiguous delivery does not trigger a resend. |
| Measure repeated `/ping` round trips with an empty queue and healthy Feishu access. | Target the [polling interval](DESIGN.md#polling-and-claims) plus two seconds. Record sample count, median, maximum, and outliers due to indexing or delivery. |
| Launch each macOS mode, then validate the Linux service in v1.2. | Pass the [menu](menu-bar.md#acceptance) and [build and launch](DESIGN.md#desktop-build) checks. |
