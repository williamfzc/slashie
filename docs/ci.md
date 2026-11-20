---
type: design
title: Slashie continuous integration
description: Canonical CI checks for every push and pull request.
---

# Slashie continuous integration

GitHub Actions (`.github/workflows/ci.yml`) runs this matrix on every push
and pull request. The checks mirror the local gates and are the canonical
definitions; reuse them rather than re-deriving flags.

| Check | Command | Linux | macOS |
| --- | --- | --- | --- |
| Formatting | `cargo fmt --all --check` | yes | yes |
| Strict lint | `cargo clippy --all-targets -- -D warnings` (add `--no-default-features` on Linux, `--features tray` on macOS) | yes | yes |
| Tests (locked) | `cargo test --locked` with the same feature flags | yes | yes |
| Headless artifact | `cargo build --release --locked --no-default-features` | yes | yes |
| Tray artifact | `cargo build --release --locked --features tray` | no | yes |
| GUI-free graph | `cargo tree --no-default-features` must not contain `tauri` or `rfd` | yes | no |

Commits follow Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`).
Rust dependencies are locked through the committed `Cargo.lock`.
