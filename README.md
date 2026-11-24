# Slashie — Feishu automation for local commands and AI agents

<p align="center">
  <a href="https://github.com/williamfzc/slashie/actions/workflows/ci.yml"><img src="https://github.com/williamfzc/slashie/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20menu%20bar%20%7C%20Linux%20%2F%20Windows%20headless-lightgrey.svg" alt="macOS menu bar, Linux and Windows headless">
  <img src="https://img.shields.io/badge/built%20with-Rust-orange.svg" alt="Rust">
</p>

Run local shell commands and AI coding agents from Feishu slash commands. Slashie polls Feishu through [Lark CLI](https://github.com/larksuite/cli) and passes each request and its source link to a script, [Claude Code](https://docs.anthropic.com/en/docs/claude-code), [Codex](https://github.com/openai/codex), or Pi. Use the macOS menu-bar app or run headless on Linux, macOS, and Windows.

Slashie runs on your machine using your authorized Feishu account. It needs no hosted Slashie service or webhook.

## Why use Slashie for Feishu-to-local automation?

- **Chat is the interface.** Send `/ping`, `/deploy prod`, or `/claude refactor this` from a supported Feishu conversation.
- **Use the tools you already run.** Routes can invoke shell scripts, Claude Code, Codex, Pi, or another local command.
- **Keep conversations separate.** Routes with `"session": "chat"` preserve one agent session and workspace per Feishu chat or topic.
- **Queue by conversation.** Commands in one conversation run in order. Different conversations can run in parallel.
- **Configure from the CLI or menu bar.** `slashie --skill`, route subcommands, the macOS Settings window, and `config.json` share the same validated configuration.

## Quick start

### Install and sign in to Lark CLI

Follow the [Lark CLI installation guide](https://github.com/larksuite/cli#quick-start-human-users). On first setup, initialize the CLI and sign in as the Feishu user whose messages Slashie should watch.

```sh
lark-cli config init
lark-cli auth login
```

Note the absolute path to `lark-cli` and the profile you signed in with. [Install Slashie](#install-slashie), then follow the steps for your build.

### Start the macOS menu-bar app

1. Open `Slashie.app`. Settings opens on the first launch. You can also open it from the menu-bar icon.
2. In Settings, use **Advanced → Edit file**. Set the Feishu channel and Lark CLI path in the [configuration](#configure-the-feishu-channel), then quit and reopen Slashie.
3. Enable **Polling** from the menu-bar icon.

### Start Slashie headless

1. [Install](#install-slashie) the headless `slashie` binary and put it on your `PATH`. On Windows, use `slashie.exe` and ensure [Git for Windows](https://gitforwindows.org/) supplies `sh` on the same `PATH`.
2. Run `slashie --poll off` while Slashie is stopped to create `~/.slashie/config.json` with the starter routes. On Windows the default directory is `%USERPROFILE%\.slashie` when `HOME` is unset.
3. Edit the [Feishu channel configuration](#configure-the-feishu-channel), then enable polling and start Slashie in the foreground.

```sh
slashie --poll on
slashie
```

### Configure the Feishu channel

Set this `channel` object in the existing `config.json`, keeping the starter routes and other settings.

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

An empty `profile` uses the active Lark CLI profile. If an npm-installed Lark CLI cannot find Node, add Node's directory to [`process_path`](docs/DESIGN.md#configuration-and-files) alongside the directories required by your shell and routes.

### Verify the default `/ping` route

Send `/ping hello` yourself in a supported Feishu chat. The default route runs `echo pong`.

- Expect a `DONE` reaction on success when [reaction access](docs/DESIGN.md#feishu-channel) is available. A red cross indicates failure, timeout, or a missing source link.
- To receive a text reply of `pong`, set `"reply_mode": "auto"` and grant reply access. The default mode leaves text replies to the target.
- If you see no reaction, check the app log or run `slashie log --last 20`.

## Install Slashie

### Install the headless binary in one command

On Apple Silicon macOS 11 or newer, or x86-64 Linux with glibc 2.39 or newer, install the latest headless binary from GitHub Releases. This command uses public GitHub downloads.

```sh
curl -fsSL https://raw.githubusercontent.com/williamfzc/slashie/main/install.sh | sh
```

The installer puts `slashie` in `~/.local/bin`. Use `~/.local/bin/slashie --version` to confirm the installation. Add that directory to `PATH` before using the headless setup steps.

The published Linux binary does not start with an older glibc, as on Ubuntu 22.04, Debian 12, or RHEL 9. Check with `ldd --version`; there, use the [build from source](#download-other-builds-or-build-from-source) steps instead.

To pin a release or choose the install directory:

```sh
curl -fsSL https://raw.githubusercontent.com/williamfzc/slashie/main/install.sh | sh -s -- v0.3.1
curl -fsSL https://raw.githubusercontent.com/williamfzc/slashie/main/install.sh | INSTALL_DIR="$HOME/bin" sh
```

`INSTALL_DIR` goes in front of `sh`, which runs the script. Run the install command again to upgrade. A failed download or archive check leaves the existing `slashie` in place.

### Download other builds or build from source

Download the macOS menu-bar app or Windows headless binary from [GitHub Releases](https://github.com/williamfzc/slashie/releases). For changes newer than the latest release, build from the source checkout.

Build the foreground binary on Linux, macOS, or Windows with Rust and Cargo.

```sh
cargo build --release --locked --no-default-features
```

To build the macOS menu-bar app, install the [Tauri CLI](https://v2.tauri.app/reference/cli/) with Cargo and run:

```sh
cargo tauri build --features tray --bundles app,dmg
```

A locally built macOS bundle has an ad-hoc signature; it is not Developer ID signed or notarized and does not establish Gatekeeper acceptance on another Mac. See the [macOS desktop build and bundle limits](docs/DESIGN.md#desktop-build) before distributing it. Windows headless builds are covered by CI; live Windows acceptance remains a separate validation step.

## Feishu triggers, permissions, and local execution

Routes are self-only by default: only messages authored by the bound Lark CLI user trigger a route. To allow another person to run a route in a searchable group, set `"allow_mentions": true` and require a real mention of the bound account before the slash command. Review the [mention trigger and local-permission rules](docs/product.md#trigger-and-reply) before enabling this for a command that can change files, deploy software, or access private data. Every route runs with the local user's permissions.

The command receives the message text and source context through environment variables, including `$SLASHIE_ARGS`, `$SLASHIE_PROMPT`, and `$SLASHIE_CONTEXT`. A local agent can read the linked Feishu conversation with its own authorized tools; Slashie does not impersonate the agent or bulk-forward chat history.

## Configure routes and agents

All persistent settings live under `~/.slashie/`. `config.json` holds the route table, poll interval, and Feishu binding; `state.json` holds the polling state, durable claims, and session mappings. Route and interval edits apply at the next scan without a restart; changing the Feishu channel binding requires a restart.

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

## How Slashie works

```text
Feishu chat ──poll──▶ slashie ──▶ sh -c "<route command>"
     ▲                    │ claims, queues       │ local output
     └── optional agent text/reaction ◀──────────┘
                         per-conversation workspaces
```

The shared Rust runtime owns polling, durable per-message claims, FIFO mailboxes, process execution, and configuration reloads. The Feishu adapter keeps Lark CLI wire handling behind the channel boundary. The macOS host adds the Tauri menu bar and Settings controls; headless builds use the same runtime without GUI dependencies. Read the [technical design](docs/DESIGN.md) for the architecture and the [product specification](docs/product.md) for user-visible behavior.

## Documentation and verification

| Documentation | What it covers |
| --- | --- |
| [Product specification](docs/product.md) | Triggers, replies, reactions, faults, recovery, and acceptance cases. |
| [Menu bar and Settings](docs/menu-bar.md) | macOS controls, status copy, and native acceptance checks. |
| [Technical design](docs/DESIGN.md) | Runtime boundaries, Feishu integration, sessions, config schema, and build limits. |
| [Continuous integration](docs/ci.md) | The canonical check matrix and release workflow. |
| [Doubao acceptance notes](docs/doubao.md#current-three-entry-acceptance) | Tested versions, live evidence, and known acceptance limits. |

CI builds and checks the Linux headless, macOS tray, and Windows headless artifacts. A successful build is not proof of end-to-end Feishu or native macOS acceptance; use the linked evidence and acceptance documents for those boundaries.

## License

[MIT](LICENSE)
