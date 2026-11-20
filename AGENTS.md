# Working in Slashie

Use `main` for day-to-day development and as the integration branch. Keep changes
small enough to verify together. Check the staged diff and the
[CI gates](docs/ci.md) before pushing. Leave the worktree clean and report the
exact commit and evidence when handing work off.

Start with the [documentation index](docs/index.md). Find the existing owner of
the behavior before adding code. The [technical design](docs/DESIGN.md) owns
runtime boundaries. Keep channel process and wire handling in the channel adapter
(`src/lark.rs` for Feishu); the runtime depends on the `Channel` trait.
Admission and execution belong to the shared runtime, and presentation and
controls belong to the Tauri host. A new channel implements `Channel` and
supplies its conversation key and workspace layout. Keep headless Rust free of
GUI dependencies. Each source file opens with a short description of its
responsibility in the system.

Verify the boundary you changed. Use isolated state and fake transport for
repeatable checks. For live Feishu checks, confirm the target and scope before
sending. Keep local fixtures, state, credentials, and private chat content out
of commits. Label test evidence as synthetic, live, or still pending.

Write code, comments, commits, and docs in English. Follow the commit convention
in [CI](docs/ci.md). Update the owning document in the same change as a durable
contract. Keep docs in `docs/`, with `type`, `title`, and `description` YAML
frontmatter, an entry in the containing directory's index, and relative links
to canonical pages. Keep transient progress in the task or room.
