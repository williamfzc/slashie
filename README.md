# Slashie

Send `/prefix message` in Feishu. Slashie finds your message through Lark CLI, runs a configured command on your machine, and replies to the original message using your Feishu identity.

Agent routes can opt into one persistent session per Feishu conversation (chat or topic) with `"session": "chat"`, so follow-up commands continue the same conversation.

Routes can also be configured by a local agent instead of by hand: `slashie route list`, `slashie route add <PREFIX> --cmd <COMMAND>`, and `slashie route remove <PREFIX>` edit `config.json` offline through the shared validator, and the Settings window can copy a ready-made agent prompt.

Build the headless binary with `cargo build --release --locked`. On macOS, add `--features tray` to build the menu-bar app. See the [configuration contract](docs/DESIGN.md#configuration-and-files), [delivery status](docs/product.md#delivery-and-evidence), and [CI checks](docs/ci.md).

Start with the [documentation index](docs/index.md).
