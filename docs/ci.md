---
type: design
title: Slashie continuous integration
description: Canonical CI checks for every push and pull request.
---

# Slashie continuous integration

GitHub Actions runs on every push and pull request. The single canonical
definition of the check matrix — formatting, strict clippy, locked tests,
documentation structure, headless and tray release builds, and the GUI-free
dependency guard — is
[.github/workflows/ci.yml](../.github/workflows/ci.yml); reuse its commands
rather than re-deriving flags. A Linux headless job, a macOS tray job, and
a Windows headless job must pass. The [test coverage](testing.md) document
maps user stories to the tests that guard them; a behavior change updates
its guarding test in the same commit.

Commits follow Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`).
Rust dependencies are locked through the committed `Cargo.lock`.

## Releases

A separate workflow (`.github/workflows/release.yml`) runs only when a `v*`
tag is pushed. It publishes a GitHub Release with five artifacts: the macOS
`.app` bundle and DMG, and headless CLI binaries for Linux (x86_64),
macOS (aarch64), and Windows (x86_64). The packages include the project's
[MIT license](../LICENSE). Keep the tag, Cargo package, and Tauri bundle at
the same version. Ordinary pushes run CI without building release packages.
