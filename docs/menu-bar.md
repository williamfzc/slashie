---
type: design
title: Slashie menu bar and settings
description: Native macOS menu, single settings window, status copy, and acceptance checks.
---

# Slashie menu bar and settings

Feishu carries requests and results. The menu bar controls polling, and Settings holds the options that change less often.

## Status item and menu

Use one fixed-size `/` template icon with the accessible name `Slashie`. macOS supplies its appearance and native menu behavior. The icon is a state indicator; the first applicable frame wins.

| State | Icon |
| --- | --- |
| Polling paused | The whole glyph dimmed, completely still. |
| Shutdown, a fault, or a retry — any state with a status line while polling is on | The plain glyph; the read-only line carries the detail. |
| Polling healthy | The whole glyph breathing smoothly — a code-driven sine at 30 fps on a one-second cycle, never disappearing while the switch is on. A scan doubles the breath speed, and a completed scan holds full brightness briefly; the breath phase never jumps, so every state change morphs instead of snapping. |

A primary click or keyboard activation opens the menu. This is the enabled state:

```text
✓ Polling
  Settings…
  ─────────────
  Quit Slashie
```

`Polling` is one native checkable menu item titled `Polling: On` or `Polling: Off`. On means intake is enabled; off means it is paused. The title always names the saved state, so the distinction never depends on the checkmark rendering. The checkmark reflects the saved setting even when an enabled poller is retrying. Confirm the change after the runtime writes it, and restore the previous checkmark if the write fails. Disable the toggle while saving, shutting down, or when the setting is unavailable. [Apple guidance on toggled menu items](https://developer.apple.com/design/human-interface-guidelines/menus#Toggled-items) · [Tauri checkable menu item](https://docs.rs/tauri/latest/tauri/menu/struct.CheckMenuItem.html)

`Settings…` opens or focuses the single settings window. On the first run — when this launch created the local state — the window opens by itself, so setup starts visibly. `Quit Slashie` requests [shared shutdown](DESIGN.md#launch-and-shutdown). If Finder launches a second instance, show `Slashie is already running.` in a native alert, then exit.

A startup fault must not remove the menu: when the Lark CLI is missing, unauthenticated, or the configuration is invalid, the icon stays with the matching [status row](#status-text) (`Check Settings and restart` or `Sign in to Feishu and restart`) instead of exiting silently.

## Status text

The checkmark covers normal enabled and paused states. Show a read-only line above it only when more context is useful. Use the first applicable row.

| Condition | Copy |
| --- | --- |
| Shutting down | `Quitting…` (startup goes straight to the healthy frames once identity is bound) |
| Operator action needed | `Check Settings and restart`, `Sign in to Feishu and restart`, `Local state unavailable`, `Could not save polling setting`, or `Config rejected · using last valid settings` |
| Latest reply failed or its outcome is uncertain | `Reply failed` or `Reply unconfirmed · Check Feishu` |
| Polling is paused while accepted work remains | `Finishing accepted requests` |
| Polling is retrying a transient failure | `Retrying · Feishu unavailable` or `Retrying · Rate limited` |

A confirmed later reply clears the reply status. The log retains operational details and earlier faults.

## Settings

The window answers two primary questions: is listening enabled, and which part of the handoff can currently be trusted? A source message link is passed to the configured AI or script; that target reads context through its own authorized tools. Configuration is secondary. [Apple settings guidance](https://developer.apple.com/design/human-interface-guidelines/settings#macOS)

| Area | Content |
| --- | --- |
| Listening | A saved Listening/Stopped switch in the window, synchronized with the menu. The adjacent 10/20/30/60-second interval and next-query countdown show polling cost. Off means no new queries. |
| Channel availability | One compact status and a refresh icon. Default agent-owned delivery does not treat missing send scope as unavailable; explicit auto mode shows missing reply scope. Receiving/reply evidence and failure details remain local runtime data and can be inspected in the log, not persistent explanatory rows. Recent scan evidence expires after two minutes; prior success is not current all-chat or target readiness. |
| Handoffs | Hidden under Configuration by default. Expand to see exact prefix → configured target and edit a program/default prompt. |
| Per-handoff configuration | Expand Configure to edit Script and Default prompt. Save validates the full file and rejects stale edits; Cancel discards only the draft. No automatic test execution of a user's script. |
| Advanced and log | Raw config and external editor under Advanced; operational Log is secondary. |

The mirror refreshes once per second from local state, without a new remote permission query each tick. The refresh icon runs a bounded, read-only identity/permission check no more than once a minute and never sends a message. External config edits appear without reopening; an open draft is retained rather than silently overwritten. Invalid files remain available for repair. App disconnection disables controls and must not leave a green availability signal.

## Acceptance

- From Finder, check the native menu in light and dark appearances. Verify keyboard navigation, Escape dismissal, and VoiceOver reading `Polling` and its checked state.
- Launch both a bundled and an unbundled dev build and verify that neither shows a Dock icon or a Cmd+Tab entry while the tray item stays available.
- Verify the dimmed glyph completely still while polling is off, and the whole glyph breathing smoothly while it is on: a one-second cycle between scans, double speed during a scan, full brightness held briefly after a scan completes (including an empty one) and then resumed without a snap. Verify the plain glyph during scan failure, retry, and shutdown, and confirm the icon and neighboring items stay in place throughout. The motion must match the switch: nothing animates while polling is off, and state changes morph rather than snap.
- Verify the frames read at a glance in light and dark appearances and that the breath is visible without relying on color alone.
- Turn polling off during a command and verify the [pause behavior](product.md#pause-and-recovery). Exercise backoff, failed setting writes, and an uncertain reply. Confirm normal states use the checkmark alone.
- Open Settings while polling and verify Listening/Stopped, next-query countdown, and separate receiving/reply/work evidence. A connected account in agent mode must not become unavailable merely because it lacks send scope. Explicit auto mode remains Cannot reply when send scope is absent. Verify retry, configuration, local state and sign-in guidance plus reduced-motion behavior.
- Copy the exact prefix; edit and save a program/default prompt, then reopen to verify persistence. Invalid and stale writes must not overwrite the file, and external changes must not destroy an open draft. Verify raw config under Advanced and recovery after invalid config or app disconnection. Buttons never send duplicate pending actions.
- Quit during a command and verify cleanup before another instance can start. Launch from Finder while headless holds the lock and verify the startup alert.
