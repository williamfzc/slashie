---
type: design
title: Slashie menu bar and settings
description: Native macOS menu, single settings window, status copy, and acceptance checks.
---

# Slashie menu bar and settings

Feishu carries requests and results. The menu bar controls polling, and Settings holds the options that change less often.

## Status item and menu

Use one fixed-size `/` template icon with the accessible name `Slashie`. macOS supplies its appearance and native menu behavior. A small dot briefly appears beside the slash after a complete, successful Feishu scan, including an empty scan. Startup, pause, scan failures, retries, and shutdown show the plain slash.

A primary click or keyboard activation opens the menu. This is the enabled state:

```text
✓ Polling
  Settings…
  ─────────────
  Quit Slashie
```

`Polling` is one native checkable menu item. Checked means intake is enabled. Unchecked means it is paused. The checkmark reflects the saved setting even when an enabled poller is retrying. Confirm the change after the runtime writes it, and restore the previous checkmark if the write fails. Disable the toggle while saving, shutting down, or when the setting is unavailable. [Apple guidance on toggled menu items](https://developer.apple.com/design/human-interface-guidelines/menus#Toggled-items) · [Tauri checkable menu item](https://docs.rs/tauri/latest/tauri/menu/struct.CheckMenuItem.html)

`Settings…` opens or focuses the single settings window. `Quit Slashie` requests [shared shutdown](DESIGN.md#launch-and-shutdown). If Finder launches a second instance, show `Slashie is already running.` in a native alert, then exit.

## Status text

The checkmark covers normal enabled and paused states. Show a read-only line above it only when more context is useful. Use the first applicable row.

| Condition | Copy |
| --- | --- |
| Starting or shutting down | `Starting…` or `Quitting…` |
| Operator action needed | `Check Settings and restart`, `Sign in to Feishu and restart`, `Local state unavailable`, or `Could not save polling setting` |
| Latest reply failed or its outcome is uncertain | `Reply failed` or `Reply unconfirmed · Check Feishu` |
| Polling is paused while accepted work remains | `Finishing accepted requests` |
| Polling is retrying a transient failure | `Retrying · Feishu unavailable` or `Retrying · Rate limited` |

A confirmed later reply clears the reply status. The log retains operational details and earlier faults.

## Settings

Use one window with a single form. The polling switch stays in the menu. The settings window has two editable sections and one secondary diagnostic action. [Apple settings guidance](https://developer.apple.com/design/human-interface-guidelines/settings#macOS)

| Area | Controls |
| --- | --- |
| Polling Interval | One selector with 10, 20, 30, or 60 seconds. The default is 20 seconds. |
| Routes | An editable list. Each row has a `Prefix`, a `Command`, and a remove action. `Add Route` adds a blank row. |
| Footer | `Open Log` on the left, `Save` on the right. Disable `Open Log` when the log is unavailable. |

Below Routes, show the two supported input forms: `"$SLASHIE_ARGS"` passes the text after the slash command, and `"$SLASHIE_PROMPT"` passes that text plus the message source. For example, `codex exec "$SLASHIE_PROMPT"`. The operator composes a command in the Command field and supplies the quoted input where the CLI expects it. A fixed command such as `echo pong` needs no input.

A route may set `"session": "chat"` in the configuration file to keep one agent session per conversation (chat or topic). The window preserves that option when it edits the route; the command uses `SLASHIE_CONTINUE` to choose between starting and resuming.

Save validates the entire draft. Mark invalid fields in place, including blank commands and duplicate prefixes. If the configuration file cannot be parsed, show its path and disable Save until the file is repaired. On success, keep the window open and show `Saved. Restart Slashie to apply.` Closing the window leaves saved settings intact and discards unsaved edits. Preserve per-route working directories and timeouts when editing existing rows. Lark CLI and subprocess path options stay in the [shared configuration file](DESIGN.md#configuration-and-files).

## Acceptance

- From Finder, check the native menu in light and dark appearances. Verify keyboard navigation, Escape dismissal, and VoiceOver reading `Polling` and its checked state.
- With polling enabled, watch a successful scan with no matching requests. Verify that the dot appears briefly and leaves the icon and neighboring items in place. Verify the plain slash during pause, scan failure, and shutdown.
- Turn polling off during a command and verify the [pause behavior](product.md#pause-and-recovery). Exercise backoff, failed setting writes, and an uncertain reply. Confirm normal states use the checkmark alone.
- Open Settings twice. Change the interval and add a route, then save and restart. Test both input forms with multiline text. Reject a duplicate prefix and verify that existing route options survive edits. Confirm that the log opens from Settings.
- Quit during a command and verify cleanup before another instance can start. Launch from Finder while headless holds the lock and verify the startup alert.
