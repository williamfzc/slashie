---
type: design
title: Slashie continuous integration
description: Canonical CI checks for every push and pull request.
---

# Slashie continuous integration

GitHub Actions runs on every push and pull request. The single canonical
definition of the check matrix — formatting, strict clippy, locked tests,
headless and tray release builds, and the GUI-free dependency guard — is
[.github/workflows/ci.yml](../.github/workflows/ci.yml); reuse its commands
rather than re-deriving flags. Both a Linux headless job and a macOS tray
job must pass.

Commits follow Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`).
Rust dependencies are locked through the committed `Cargo.lock`.
