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
| Polling paused | The whole glyph dimmed. |
| Startup, shutdown, a fault, or a retry (any state with a status line) | The plain glyph; the read-only line carries the detail. |
| A scan in flight | A small dot orbiting the slash in the clear space beside its tail, about one revolution per second. |
| Within about a second and a half after a complete scan, including an empty one | The same dot flaring bright. |
| Polling healthy between scans | The dot resting dim. |

A primary click or keyboard activation opens the menu. This is the enabled state:

```text
✓ Polling
  Settings…
  ─────────────
  Quit Slashie
```

`Polling` is one native checkable menu item. Checked means intake is enabled. Unchecked means it is paused. The checkmark reflects the saved setting even when an enabled poller is retrying. Confirm the change after the runtime writes it, and restore the previous checkmark if the write fails. Disable the toggle while saving, shutting down, or when the setting is unavailable. [Apple guidance on toggled menu items](https://developer.apple.com/design/human-interface-guidelines/menus#Toggled-items) · [Tauri checkable menu item](https://docs.rs/tauri/latest/tauri/menu/struct.CheckMenuItem.html)

`Settings…` opens or focuses the single settings window. On the first run — when this launch created the local state — the window opens by itself, so setup starts visibly. `Quit Slashie` requests [shared shutdown](DESIGN.md#launch-and-shutdown). If Finder launches a second instance, show `Slashie is already running.` in a native alert, then exit.

A startup fault must not remove the menu: when the Lark CLI is missing, unauthenticated, or the configuration is invalid, the icon stays with the matching [status row](#status-text) (`Check Settings and restart` or `Sign in to Feishu and restart`) instead of exiting silently.

## Status text

The checkmark covers normal enabled and paused states. Show a read-only line above it only when more context is useful. Use the first applicable row.

| Condition | Copy |
| --- | --- |
| Starting or shutting down | `Starting…` or `Quitting…` |
| Operator action needed | `Check Settings and restart`, `Sign in to Feishu and restart`, `Local state unavailable`, `Could not save polling setting`, or `Config rejected · using last valid settings` |
| Latest reply failed or its outcome is uncertain | `Reply failed` or `Reply unconfirmed · Check Feishu` |
| Polling is paused while accepted work remains | `Finishing accepted requests` |
| Polling is retrying a transient failure | `Retrying · Feishu unavailable` or `Retrying · Rate limited` |

A confirmed later reply clears the reply status. The log retains operational details and earlier faults.

## Settings

Use one window with a single form. The polling switch stays in the menu. The settings window has a read-only runtime status line, two editable sections, and one secondary diagnostic action. [Apple settings guidance](https://developer.apple.com/design/human-interface-guidelines/settings#macOS)

| Area | Controls |
| --- | --- |
| Runtime status | A read-only line at the top with a small dot. It mirrors the [menu status](#status-item-and-menu): paused, the copy of any fault or retry, the scan in flight, or the interval and how long ago the last complete scan ran. It refreshes about once a second while the window is visible, and the dot breathes only while a scan is in flight. It is not a live region, so screen readers read it on focus instead of announcing every refresh. Motion respects the system reduced-motion setting. |
| Polling Interval | One selector with 10, 20, 30, or 60 seconds. The default is 20 seconds. |
| Routes | An editable list. Each row has a `Prefix`, a `Command`, and a remove action. `Add Route` adds a blank row. |
| Footer | `Open Log` on the left, `Save` on the right. Disable `Open Log` when the log is unavailable. |

Below Routes, show the two supported input forms: `"$SLASHIE_ARGS"` passes the text after the slash command, and `"$SLASHIE_PROMPT"` passes that text plus the message source. For example, `codex exec "$SLASHIE_PROMPT"`. The operator composes a command in the Command field and supplies the quoted input where the CLI expects it. A fixed command such as `echo pong` needs no input.

A route may set `"session": "chat"` in the configuration file to keep one agent session per conversation (chat or topic). The window preserves that option when it edits the route; the command uses `SLASHIE_CONTINUE` to choose between starting and resuming.

Under the route list, a `Configure with an agent` disclosure guides hand-off to a local agent instead of hand-editing. It explains that any local agent can edit routes with the [slashie CLI](DESIGN.md#configuration-and-files) while the app runs, offers `Copy agent prompt` with a ready-made instruction — naming the resolved CLI path and pointing the agent at `slashie --skill`, the printed operating contract — and shows a copyable example command. The window never runs the agent itself; the user reviews whatever the agent changed.

Save validates the entire draft. Mark invalid fields in place, including blank commands and duplicate prefixes. If the configuration file cannot be parsed, show its path and disable Save until the file is repaired. On success, keep the window open and show `Saved. Slashie picks it up within about <interval> seconds.` Closing the window leaves saved settings intact and discards unsaved edits. Preserve per-route working directories and timeouts when editing existing rows. Lark CLI and subprocess path options stay in the [shared configuration file](DESIGN.md#configuration-and-files).

## Acceptance

- From Finder, check the native menu in light and dark appearances. Verify keyboard navigation, Escape dismissal, and VoiceOver reading `Polling` and its checked state.
- Launch both a bundled and an unbundled dev build and verify that neither shows a Dock icon or a Cmd+Tab entry while the tray item stays available.
- Verify the dimmed glyph while polling is paused and the resting dot while it is healthy. Watch a scan in flight and confirm the dot orbits the slash, then flares after the scan completes (including an empty scan) and returns to resting. Verify the plain glyph, without the dot, during startup, scan failure, retry, and shutdown, and confirm the icon and neighboring items stay in place throughout.
- Verify the frames read at a glance in light and dark appearances and that the dot is visible without relying on color alone.
- Turn polling off during a command and verify the [pause behavior](product.md#pause-and-recovery). Exercise backoff, failed setting writes, and an uncertain reply. Confirm normal states use the checkmark alone.
- Open Settings while polling and verify the runtime status line shows the interval and scan age, switches to the breathing dot while a scan runs, and shows the paused copy when polling is off. Verify it carries the status-line copy during a fault, and that the dot does not animate when reduced motion is set.
- Open Settings twice. Change the interval and add a route, then save and verify the copy promises the next scan; confirm a message that matches the new route runs without a restart. Test both input forms with multiline text. Reject a duplicate prefix and verify that existing route options survive edits. Confirm that the log opens from Settings.
- Open `Configure with an agent`, copy the prompt, and verify the pasted text names the running binary path and the route subcommands. Verify the example command line reflects the same path, that copying leaves the save state untouched, and that a blocked clipboard falls back to showing the prompt text for manual selection.
- Quit during a command and verify cleanup before another instance can start. Launch from Finder while headless holds the lock and verify the startup alert.
