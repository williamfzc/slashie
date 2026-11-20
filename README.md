# Slashie

Send `/prefix message` in Feishu. Slashie finds your message through Lark CLI, runs a configured command on your machine, and replies to the original message using your Feishu identity.

Agent routes can opt into one persistent session per Feishu conversation (chat or topic) with `"session": "chat"`, so follow-up commands continue the same conversation.

## Getting started

1. Install the [Lark CLI](https://github.com/larksuite/cli) and authenticate it (`lark-cli login`). Note the executable's absolute path.
2. Launch Slashie. On first run it creates `~/.slashie/config.json` with starter routes (`/ping`, `/pi`, `/claude`, `/codex`) and opens Settings; set `lark_cli.path` (and optionally `lark_cli.profile`) in that file, then restart.
3. Enable `Polling` in the menu-bar icon's menu (it starts paused).
4. Send `/ping hello` in a Feishu chat you can access with that account; the reply should be `pong`-style output from the command.

Routes can also be configured by a local agent instead of by hand: `slashie --skill` prints the operating contract, `slashie route list`, `slashie route add <PREFIX> --cmd <COMMAND>`, and `slashie route remove <PREFIX>` edit `config.json` offline through the shared validator, and the Settings window can copy a ready-made agent prompt. Edits apply at the next scan without a restart.

## Reset or uninstall

Quit Slashie from its menu, then delete `~/.slashie` to remove its configuration, state, agent-session workspaces, and log; the next start is a fresh install. To reset one conversation's agent session instead, delete that conversation's directory under `~/.slashie/chats/` while Slashie is stopped.

Build the headless binary with `cargo build --release --locked`. On macOS, add `--features tray` to build the menu-bar app. See the [configuration contract](docs/DESIGN.md#configuration-and-files), [delivery status](docs/product.md#delivery-and-evidence), and [CI checks](docs/ci.md).

Start with the [documentation index](docs/index.md).
