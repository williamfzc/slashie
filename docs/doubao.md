---
type: design
title: Doubao CLI session route
description: Local Doubao session route and reproducible real Feishu acceptance.
---

# Doubao CLI session route

`scripts/doubao_session.py` bridges the existing [conversation workspace](DESIGN.md#chat-sessions) to Doubao's explicit `--resume <session_id>` interface. The ordinary and topic-group handoff flow has been verified with real Feishu messages and real Doubao on macOS; native menu interaction is separate acceptance.

## Configuration

Authenticate the installed Doubao CLI through its normal login command. Set the real Lark CLI path and its runtime directories in Slashie's configuration. A Work-provided `lark-cli` command entry may not expose `auth status`; use an independently installed CLI that implements the [Feishu adapter contract](DESIGN.md#feishu-channel).

Add a session route with an absolute interpreter and script path:

    slashie route add /doubao --session chat --timeout 90 --cmd 'python3 "/absolute/path/to/slashie/scripts/doubao_session.py"'

The adapter locates `doubao` on `PATH`; `DOUBAO_CLI` can select an explicit executable. Successful requests atomically save the session ID in `doubao-session.json` inside the conversation workspace. Follow-ups use that exact ID, never a global latest-session lookup. Credentials stay in the provider's own store. `DOUBAO_PERMISSION_MODE` forwards an explicitly chosen host mode (`ask_always`, `ask_risky`, or `always_allow`) to the provider; unset preserves its defaults. This is local execution authorization only and never grants Feishu API scopes. Never enable it to override a user's denial or an enterprise policy. A missing continuation marker is an explicit failure rather than a silent new conversation.

The adapter reads the combined `SLASHIE_PROMPT` first, including the source message link; `SLASHIE_ARGS` is a compatibility fallback. Doubao must use its own authorized tools to read the link when the task requires it. A navigation link is not a copied sharing snapshot, and missing permissions are not worked around. Doubao print mode must see stdin EOF. The adapter uses closed input and captures JSON, printing only the answer to Slashie. It records request/answer evidence to `doubao-turns.jsonl` only when `SLASHIE_RECORD_TURNS=1`; the acceptance script opts in for invented data. Keep these files private and out of commits. Normal routes retain only the resume marker.

## Reproducible local verification

Build the headless binary with the canonical [CI checks](ci.md), then run:

    python3 scripts/test_doubao_local.py --output /absolute/path/to/fresh-evidence

The test uses Slashie's existing mock channel and the real authenticated Doubao CLI. It does not call Feishu, read historic messages or send group replies. It verifies first/continued turns, two-chat and two-topic isolation, latest conflicting update, rapid-message ordering, restart continuity, ignored messages, literal multiline input, failure/timeout recovery, and graceful shutdown. A fresh output directory avoids resuming old synthetic sessions. The test normalizes the inherited child signal mask because desktop command hosts may block termination signals.

## Real Feishu verification

The canonical acceptance entry is `scripts/test_feishu_coverage.py`. It sends real inputs through an explicitly selected authorized input CLI, while Slashie independently reads them through its configured CLI. Pass exact self-DM, ordinary, and topic targets, a fresh output directory, and the absolute input/read CLI paths. The script verifies complete membership before any send, uses natural office requests and authored example materials, and preserves actual source links, AI turns, claims, and work products. It never substitutes mock messages. Explicit stage resume retains earlier evidence; interrupted attempts remain in the history and cannot satisfy missing coverage.

On 2026-11-21 the live checks covered preceding-context reads, ordinary and topic continuations, exact-session isolation, conflicting updates, different-chat concurrency, burst ordering, literal multiline input, ignored non-commands and ordinary non-topic replies, timeout/failure recovery, paused-message exclusion, restart continuity, and a file produced from the AI's remembered source context. The current runtime owns status reactions: `OnIt` acknowledges durable acceptance and successful execution changes it to `DONE`; failures never mark completion. Doubao owns only the text reply decision and must not duplicate software status reactions. The earlier office-business evidence below predates this status correction and does not prove automatic software reactions. A separate positive check allowed the target to use the user's already-authorized Work tools: the real reply was read back from the source ordinary group and referenced the original trigger. Slashie's read identity and adapter did not change. Running and queued requests also settled correctly on shutdown, and new work succeeded after a clean restart. A missing send scope is not a work failure. Nonzero target exit is recorded as failed work, distinct from silent successful completion.

The current business acceptance entry is `scripts/test_feishu_coverage.py`. It uses natural office requests and real uploaded quotation/training files: meeting to-dos, deadline/owner changes, purchase budgets, registration costs, parallel work, 18-topic and nine-request queue loads, source retrieval restricted server-side to the bound user, pause/restart and shutdown. Pass exact self-DM/ordinary/topic targets and absolute authorized input/read CLI paths. Run stages `messages`, `topics`, `attachments`, `recovery`, `closeout`, `capacity`, and `sender`. Each stage installs only its own routes; do not share trigger prefixes across concurrent runtime instances. Every confirmed send is recorded before readback. Expected errors and timeout cases are judged on recovery and actual work, not on a forced reply.

On 2026-11-21, the natural-office run produced correct actual work: quotation totals and budget balance, updated meeting assignments, independently maintained topic decisions, and a confirmed target-owned reply under the existing Work authorization. The server-side search audit retained a single bound sender on every search page and never requested the other speaker's message details. Capacity checks used the production limits without weakening them: 18 real topics exceeded the 10-pending-job bound, and nine requests in one topic exceeded its four-job bound; overflow eventually executed once and left its work files. All 36 final business checks passed after completed retests; interrupted attempts remain in private evidence, not committed.

Older synthetic and code/character input runs are not the current office-business acceptance and are not shipped as additional acceptance entry points. Native menu interaction is not verified: application discovery was unavailable in an earlier run and a later desktop attempt stopped at the locked screen. CLI start/stop/restart checks and release builds are separate and cannot replace native interaction. See [product acceptance](product.md#release-acceptance) for the broader release matrix.

## Default delivery requirements

Customize delivery in the existing route program, not a separate `default_prompts` configuration field; that retired field is ignored. Prepend an instruction to `SLASHIE_PROMPT` before invoking the session adapter, keeping the argument and original source intact. This does not change `reply_mode: agent`, grant send permission, or move status ownership to the target.

A tested requirement is:

> After completing the requested work, use your already-authorized Feishu tools to reply to the source message, inside its topic when applicable. Send a short conclusion and necessary deliverables, then read back the actual reply. Output only inside Doubao is not a group reply. If tools or send permission are unavailable, do not change identity or work around the restriction; report that delivery did not occur. An explicit request not to reply overrides this default. Slashie owns acceptance and completion reactions; do not operate them.

For example, inside the route `cmd`:

```sh
SLASHIE_PROMPT="Complete the office task. Use your existing authorized Feishu tools to reply briefly to the original source and read it back. Do not claim a group reply from Doubao-only output. If delivery is unauthorized, do not send. An explicit no-reply request overrides this default. Slashie owns all status reactions.

$SLASHIE_PROMPT" python3 "/absolute/path/to/slashie/scripts/doubao_session.py"
```

Use a timeout appropriate for actual work rather than treating the short demonstration timeout as a production recommendation. A prompt is an instruction, not a delivery guarantee: verify the work file, saved continuation, actual `reply_to`, and source reaction separately.

## Current three-entry acceptance

The 2026-11-23 validation used real Feishu office messages and uploaded example materials, not simulated ingress. Versions: Slashie v0.2.0, Lark CLI 1.0.100, Doubao CLI 2.37.2. Private messages, source IDs, work products, local paths and provider sessions remain outside the repository.

| Entry | Actual result | Remaining boundary |
| --- | --- | --- |
| macOS CLI | Ordinary group, self-DM, topics, exact continuation and isolation, office files, uploaded quotation calculation, target-owned original-message replies and lifecycle checks passed. The configured default-delivery route passed three turns: first work, continued owner change, and explicit no-reply override. | Self-DM does not establish every external direct-message relationship. |
| Linux CLI | A user systemd service completed real group/self-DM/topic script work, three topic AI turns, an uploaded-file calculation, original-message reply, three default-delivery turns, software status and start/stop checks. | The compatible 2.37.2 provider was run from its official standalone package with local authorization; the old system installation was not upgraded. |
| macOS App | Rebuilt bundle metadata is 0.2.0 and strict ad-hoc resource-signature verification passed. | Locked desktop prevented menu, Settings, button-driven start/stop and App-originated live work. DMG rebuilding failed with a resource-busy error. No claim of native acceptance or notarization. |

Capacity was separately verified on both CLI hosts: 18 real topic requests exceeded the ten-job bound, and nine real requests in one topic exceeded its four-job bound. All 27 eventually ran once per host and produced work files; all 27 source messages per host were read back with `DONE` and no `OnIt`. On macOS, shutdown interrupted one final reaction cleanup: a paused restart retried only status, removed its confirmed acknowledgment, and left the execution count unchanged. A real second speaker's office request was excluded from every server-filtered discovery page and never requested by the software's detail reads; the bound user's request still completed.

Failures remain evidence, not passes: Doubao 2.31.2 rejected the adapter output arguments; one 2.37.2 ordinary-group file task timed out, and its follow-up ended during shutdown. An initial default-delivery instruction produced a file and `DONE` but no group reply; the explicit requirement above then passed. Linux capacity startup initially mistook a refreshable saved user login for a missing login, so 18 already-sent requests were not accepted. Normal CLI refresh restored access and a new batch passed; the adapter now permits `needs_refresh` only through a successful authenticated identity lookup. Controlled expiry, forced-crash timing and the full native matrix are not established by these results.
