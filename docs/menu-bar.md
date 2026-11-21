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

The window is a mirror of `config.json`, shown verbatim — not an editor. Edits happen in the file itself or through the [slashie CLI](DESIGN.md#configuration-and-files), so the window, an agent, and a terminal all see and change the same artifact. The polling switch stays in the menu. [Apple settings guidance](https://developer.apple.com/design/human-interface-guidelines/settings#macOS)

| Area | Content |
| --- | --- |
| Runtime status | A read-only line with a small dot. It mirrors the [menu status](#status-item-and-menu): paused, the copy of any fault or retry, `Scanning…` while a scan runs, or a countdown to the next scan. It is not a live region, so screen readers read it on focus instead of announcing every refresh. Motion respects the system reduced-motion setting. |
| Account | One muted line: the Feishu identity verified at startup and how its binding stands — `connected`, or `unavailable` while intake is blocked. Hidden when no identity was verified. |
| Configuration | The raw `config.json`, verbatim, in a selectable monospace block under a small `config.json` label. |
| Footer | `Edit Config` and `Open Log` on the left, and one sentence on the right: an agent can manage this configuration too, pointed at the slashie CLI. Disable `Open Log` when the log is unavailable. |

`Edit Config` opens `config.json` in the system's default text editor, like `Open Log` opens the log. The mirror refreshes about once a second while the window is visible, so file and CLI edits appear without reopening the window. When the configuration cannot be parsed, the mirror still shows the file verbatim with an explanation, and recovers by itself once the file is valid. Closing the window never loses anything: there is nothing to save.

## Acceptance

- From Finder, check the native menu in light and dark appearances. Verify keyboard navigation, Escape dismissal, and VoiceOver reading `Polling` and its checked state.
- Launch both a bundled and an unbundled dev build and verify that neither shows a Dock icon or a Cmd+Tab entry while the tray item stays available.
- Verify the dimmed glyph completely still while polling is off, and the whole glyph breathing smoothly while it is on: a one-second cycle between scans, double speed during a scan, full brightness held briefly after a scan completes (including an empty one) and then resumed without a snap. Verify the plain glyph during scan failure, retry, and shutdown, and confirm the icon and neighboring items stay in place throughout. The motion must match the switch: nothing animates while polling is off, and state changes morph rather than snap.
- Verify the frames read at a glance in light and dark appearances and that the breath is visible without relying on color alone.
- Turn polling off during a command and verify the [pause behavior](product.md#pause-and-recovery). Exercise backoff, failed setting writes, and an uncertain reply. Confirm normal states use the checkmark alone.
- Open Settings while polling and verify the runtime status line counts down to the next scan, switches to `Scanning…` while a scan runs, and shows `Paused` when polling is off. Verify it carries the status-line copy during a fault, the account line names the verified identity and its binding status, and the dot does not animate when reduced motion is set.
- Open Settings and verify the mirror shows `config.json` verbatim. Run `slashie route add` in a terminal and confirm the new route appears within a couple of seconds without reopening the window. Use `Edit Config`, change a value, save the file, and confirm the mirror and the next scan pick it up; introduce a syntax error and confirm the mirror still shows the file with the explanation, then repair it and confirm recovery. Confirm that the log opens from Settings.
- Quit during a command and verify cleanup before another instance can start. Launch from Finder while headless holds the lock and verify the startup alert.
