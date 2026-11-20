# Slashie

Send `/prefix message` in Feishu. Slashie finds your message through Lark CLI, runs a configured command on your machine, and replies to the original message using your Feishu identity.

Agent routes can opt into one persistent session per Feishu chat with `"session": "chat"`, so follow-up commands continue the same conversation.

Build the headless binary with `cargo build --release --locked`. On macOS, add `--features tray` to build the menu-bar app. See the [configuration contract](docs/DESIGN.md#configuration-and-files), [delivery status](docs/product.md#delivery-and-evidence), and [CI checks](docs/ci.md).

Start with the [documentation index](docs/index.md).
