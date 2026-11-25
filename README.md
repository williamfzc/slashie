# Slashie — run local commands and AI agents from chat slash commands

<p align="center">
  <a href="https://github.com/williamfzc/slashie/actions/workflows/ci.yml"><img src="https://github.com/williamfzc/slashie/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License"></a>
  <img src="https://img.shields.io/badge/CLI-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey.svg" alt="CLI for macOS, Linux, and Windows">
  <img src="https://img.shields.io/badge/built%20with-Rust-orange.svg" alt="Rust">
</p>

Slashie is a CLI that turns chat messages into local work. It watches a chat channel for slash commands such as `/deploy prod` or `/claude refactor this`, and hands each request, with a link to its source message, to a shell script or an AI coding agent such as [Claude Code](https://docs.anthropic.com/en/docs/claude-code), [Codex](https://github.com/openai/codex), or Pi. Feishu is the channel available today.

Slashie runs on your machine under your own chat account. It needs no hosted Slashie service or webhook.

## Why use Slashie for chat-to-local automation?

- **Chat is the interface.** Send `/ping`, `/deploy prod`, or `/claude refactor this` from a conversation in the connected channel.
- **Use the tools you already run.** Routes can invoke shell scripts, Claude Code, Codex, Pi, or another local command.
- **Keep conversations separate.** Routes with `"session": "chat"` preserve one agent session and workspace per chat or topic.
- **Queue by conversation.** Commands in one conversation run in order. Different conversations can run in parallel.
- **Configure from the CLI.** `slashie --skill`, the route subcommands, and `config.json` share the same validated configuration.

## Channels

A channel is the chat platform Slashie listens to. In `config.json`, `channel.type` selects one channel and holds its settings.

Slashie's runtime is built around a `Channel` interface, not around one platform. A channel owns its wire operations, how it identifies a conversation, and where that conversation's workspace lives. Routes, sessions, and queues are the same across channels, so another chat platform is a new adapter rather than a different runtime.

| Channel | Status |
| --- | --- |
| [Feishu](docs/DESIGN.md#feishu-channel), reached through [Lark CLI](https://github.com/larksuite/cli) | Available. The first channel implementation. |

The interface and the rules for adding a channel are in [Channels](docs/DESIGN.md#channels) in the technical design.

## Quick start

### Install the CLI

On Apple Silicon macOS 11 or newer, or x86-64 Linux with glibc 2.39 or newer:

```sh
curl -fsSL https://raw.githubusercontent.com/williamfzc/slashie/main/install.sh | sh
```

The installer puts `slashie` in `~/.local/bin`. Add that directory to `PATH`. Other platforms and install options are under [Install Slashie](#install-slashie).

### Sign in to the channel

Feishu is reached through Lark CLI. Follow the [Lark CLI installation guide](https://github.com/larksuite/cli#quick-start-human-users), then on first setup initialize the CLI and sign in as the Feishu user whose messages Slashie should watch.

```sh
lark-cli config init
lark-cli auth login
```

Note the absolute path to `lark-cli` and the profile you signed in with.

### Create the configuration

1. Make sure `slashie` is on your `PATH`. On Windows, use `slashie.exe` and ensure [Git for Windows](https://gitforwindows.org/) supplies `sh` on the same `PATH`.
2. Run `slashie --poll off` while Slashie is stopped. This creates `~/.slashie/config.json` with the starter routes. On Windows the default directory is `%USERPROFILE%\.slashie` when `HOME` is unset.
3. Add the [Feishu channel configuration](#configure-the-feishu-channel) to that file.

### Start Slashie

Enable polling, then run Slashie in the foreground:

```sh
slashie --poll on
slashie
```

### Configure the Feishu channel

Set this `channel` object in the existing `config.json`, keeping the starter routes and other settings. `type` names the channel; `feishu` is the only value today.

```json
{
  "channel": {
    "type": "feishu",
    "lark_cli": {
      "path": "/absolute/path/to/lark-cli",
      "profile": ""
    }
  }
}
```

An empty `profile` uses the active Lark CLI profile. If an npm-installed Lark CLI cannot find Node, add Node's directory to [`process_path`](docs/DESIGN.md#configuration-and-files) alongside the directories required by your shell and routes. Changing the channel binding later requires a restart.

### Verify the default `/ping` route

Send `/ping hello` yourself in a supported Feishu chat. The default route runs `echo pong`.

- Expect a `DONE` reaction on success when [reaction access](docs/DESIGN.md#feishu-channel) is available. A red cross indicates failure, timeout, or a missing source link.
- To receive a text reply of `pong`, set `"reply_mode": "auto"` and grant reply access. The default mode leaves text replies to the target.
- If you see no reaction, run `slashie log --last 20`.

## Install Slashie

### Install the CLI in one command

On Apple Silicon macOS 11 or newer, or x86-64 Linux with glibc 2.39 or newer, install the latest CLI from GitHub Releases. This command uses public GitHub downloads.

```sh
curl -fsSL https://raw.githubusercontent.com/williamfzc/slashie/main/install.sh | sh
```

The installer puts `slashie` in `~/.local/bin`. Use `~/.local/bin/slashie --version` to confirm the installation. Add that directory to `PATH` before using the quick start steps.

The published Linux binary does not start with an older glibc, as on Ubuntu 22.04, Debian 12, or RHEL 9. Check with `ldd --version`; there, use the [build from source](#other-platforms-or-build-from-source) steps instead.

To pin a release or choose the install directory:

```sh
curl -fsSL https://raw.githubusercontent.com/williamfzc/slashie/main/install.sh | sh -s -- v0.3.1
curl -fsSL https://raw.githubusercontent.com/williamfzc/slashie/main/install.sh | INSTALL_DIR="$HOME/bin" sh
```

`INSTALL_DIR` goes in front of `sh`, which runs the script. Run the install command again to upgrade. A failed download or archive check leaves the existing `slashie` in place.

### Other platforms, or build from source

Download the Windows binary from [GitHub Releases](https://github.com/williamfzc/slashie/releases). For changes newer than the latest release, or a Linux system with an older glibc, build from the source checkout with Rust and Cargo on Linux, macOS, or Windows.

```sh
cargo build --release --locked --no-default-features
```

The CLI is the headless build named in the technical design. Windows builds are covered by CI; live Windows acceptance remains a separate validation step.

## Triggers, permissions, and local execution

Routes are self-only by default: only messages authored by your own signed-in channel account trigger a route. On Feishu that is the bound Lark CLI user. To allow another person to run a route in a searchable group, set `"allow_mentions": true` and require a real mention of the bound account before the slash command. Review the [mention trigger and local-permission rules](docs/product.md#trigger-and-reply) before enabling this for a command that can change files, deploy software, or access private data. Every route runs with the local user's permissions.

The command receives the message text and source context through environment variables, including `$SLASHIE_ARGS`, `$SLASHIE_PROMPT`, and `$SLASHIE_CONTEXT`. A local agent can read the linked conversation with its own authorized tools; Slashie does not impersonate the agent or bulk-forward chat history.

## Configure routes and agents

All persistent settings live under `~/.slashie/`. `config.json` holds the route table, poll interval, and channel binding; `state.json` holds the polling state, durable claims, and session mappings. Route and interval edits apply at the next scan without a restart; changing the channel binding requires a restart.

Use the offline route commands so the shared validator checks the complete file:

```sh
slashie route add /deploy --cmd 'cd ~/app && ./deploy.sh "$SLASHIE_ARGS"' --timeout 120
slashie route remove /deploy
slashie route list
```

The operating contract for local agents is available with:

```sh
slashie --skill
```

The stock configuration includes `/ping`, `/pi`, `/claude`, and `/codex`. Add the required agent binaries and runtimes to `process_path` when they are not already on the host `PATH`. The [technical design](docs/DESIGN.md) documents route fields, environment variables, polling, limits, and the channel boundary.

## Conversation sessions and reset behavior

To reset a single conversation, stop Slashie and follow the [session reset procedure](docs/DESIGN.md#chat-sessions) for removing its workspace and its mapping in `state.json`. For a full local reset, stop Slashie and remove its [data directory](docs/DESIGN.md#configuration-and-files).

## Experimental: macOS menu-bar app

A macOS menu-bar app embeds the same runtime behind a Tauri shell. It offers a Polling toggle, a Settings window, and Quit from the menu bar. It is experimental; the CLI is the primary way to run Slashie.

- The bundle is ad-hoc signed. It is not Developer ID signed or notarized, so Gatekeeper does not accept it on another Mac. See the [build and bundle limits](docs/DESIGN.md#desktop-build).
- Native acceptance is still pending. The checks are listed in [menu bar and Settings](docs/menu-bar.md#acceptance).

Download `Slashie_<version>_aarch64.app.tar.gz` or the DMG from [GitHub Releases](https://github.com/williamfzc/slashie/releases), or build it yourself with the [Tauri CLI](https://v2.tauri.app/reference/cli/):

```sh
cargo tauri build --features tray --bundles app,dmg
```

## How Slashie works

```text
Chat channel ──poll──▶ slashie ──▶ sh -c "<route command>"
     ▲                    │ claims, queues       │ local output
     └── optional agent text/reaction ◀──────────┘
                         per-conversation workspaces
```

The shared Rust runtime owns polling, durable per-message claims, FIFO mailboxes, process execution, and configuration reloads. Each channel is an adapter behind the `Channel` interface; the Feishu adapter keeps Lark CLI wire handling there. The CLI uses this runtime without GUI dependencies. The experimental macOS app embeds the same runtime behind a Tauri menu bar. Read the [technical design](docs/DESIGN.md) for the architecture and the [product specification](docs/product.md) for user-visible behavior.

## Documentation and verification

| Documentation | What it covers |
| --- | --- |
| [Product specification](docs/product.md) | Triggers, replies, reactions, faults, recovery, and acceptance cases. |
| [Technical design](docs/DESIGN.md) | Runtime boundaries, the channel interface, the Feishu channel, sessions, config schema, and build limits. |
| [Menu bar and Settings](docs/menu-bar.md) | The experimental macOS controls, status copy, and native acceptance checks. |
| [Continuous integration](docs/ci.md) | The canonical check matrix and release workflow. |
| [Doubao acceptance notes](docs/doubao.md#current-three-entry-acceptance) | Tested versions, live evidence, and known acceptance limits. |

CI builds and checks the CLI on Linux, macOS, and Windows, and the macOS app on macOS. A successful build is not proof of end-to-end channel or native macOS acceptance; use the linked evidence and acceptance documents for those boundaries.

## License

[MIT](LICENSE)
