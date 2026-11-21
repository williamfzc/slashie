# Slashie

<p align="center">
  <a href="https://github.com/williamfzc/slashie/actions/workflows/ci.yml"><img src="https://github.com/williamfzc/slashie/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey.svg" alt="macOS and Linux">
  <img src="https://img.shields.io/badge/built%20with-Rust-orange.svg" alt="Rust">
</p>

Send `/command message` in Feishu. Slashie runs it on your machine and replies with the outcome — a menu-bar agent runtime that turns chat messages into local work.

No bots, no webhooks, no servers: Slashie polls through [Lark CLI](https://github.com/larksuite/cli) with your own account, so anything your Feishu identity can see, your machine can act on. One persistent agent session per conversation (chat or topic), and any local coding agent — Claude Code, Codex, Pi — can configure Slashie for you through its CLI.

## Why Slashie

- **Chat is the interface.** `/ping`, `/deploy prod`, `/claude refactor this` — send from any chat your account can reach, on one machine or via systemd on a server.
- **Agents configure it.** `slashie --skill` prints a complete operating contract, so Claude Code or Codex can add routes for you; the Settings window mirrors `config.json` live while they work.
- **One session per conversation.** Routes with `"session": "chat"` keep a per-chat (or per-topic) agent session, so follow-ups continue where they left off — different topics never bleed into each other.
- **Safe concurrency.** A mailbox per conversation: commands in one chat run in order, different chats run in parallel, and nothing claimed is ever silently dropped.
- **Instant config.** Route and interval edits hot-apply at the next scan — no restart.

## Quick start

1. Install the [Lark CLI](https://github.com/larksuite/cli) and authenticate: `lark-cli login`. Note its absolute path.
2. Launch Slashie. First run creates `~/.slashie/config.json` with starter routes (`/ping`, `/pi`, `/claude`, `/codex`) and opens Settings; set `lark_cli.path` there, then restart.
3. Enable `Polling` from the menu-bar icon.
4. Send `/ping hello` in any Feishu chat your account can reach.

Grab the latest build from [Releases](https://github.com/williamfzc/slashie/releases) (built automatically on version tags), or build from source:

```sh
cargo build --release --locked                 # headless binary (Linux too)
cargo tauri build --features tray --bundles app  # macOS menu-bar .app
```

## Configuration, by you or by an agent

Everything lives in one file, `~/.slashie/config.json` — routes, poll interval, and the Lark CLI binding. Three equivalent ways to change it:

```sh
slashie route add /deploy --cmd 'cd ~/app && ./deploy.sh "$SLASHIE_ARGS"' --timeout 120
slashie route remove /deploy
slashie route list          # the route table as JSON
```

Or open Settings and hit **Edit Config** — the window is a live mirror of the file, so CLI, agent, and window always agree. Commands receive the message text through `$SLASHIE_ARGS` and `$SLASHIE_PROMPT`; the full contract (variables, sessions, polling, limits) is one command away:

```sh
slashie --skill
```

## Reset or uninstall

Quit Slashie from its menu, then delete `~/.slashie` to remove its configuration, state, agent-session workspaces, and log; the next start is a fresh install. To reset one conversation's agent session instead, delete that conversation's directory under `~/.slashie/chats/` while Slashie is stopped.

## How it works

```
Feishu chat ──poll──▶ slashie (menu bar) ──▶ /bin/sh -c "<route cmd>"
     ▲                    │  claims, queues        │ output
     └──── reply ◀────────┘  mailbox per chat      ▼
                        per-chat agent sessions   local tools
```

A shared Rust runtime drives everything: durable per-message claims in `state.json`, a FIFO mailbox per conversation (serial within a chat, parallel across chats), a channel boundary that keeps Feishu wire handling behind the `Channel` trait, and configuration that hot-reloads from disk at every scan. The macOS shell is a Tauri 2 tray app whose icon is a live state indicator; the same binary runs headless for servers. See the [technical design](docs/DESIGN.md).

| Docs | |
| --- | --- |
| [Product specification](docs/product.md) | What a user can rely on: triggers, replies, faults, recovery. |
| [Menu bar & Settings](docs/menu-bar.md) | The tray states, status copy, and the read-only Settings mirror. |
| [Technical design](docs/DESIGN.md) | Runtime boundaries, the Feishu channel, mailboxes, claims, config schema. |
| [Continuous integration](docs/ci.md) | The check matrix and the macOS packaging job. |

## Status

macOS menu-bar app and headless mode are working today; the live chat-session flow is verified with Pi and Claude Code. Linux headless (systemd) is planned next — see [delivery status](docs/product.md#delivery-and-evidence).

Start with the [documentation index](docs/index.md).

## License

[MIT](LICENSE)
