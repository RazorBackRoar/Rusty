# Rusty AGENTS

Guidance for AI agents working in this repository. Use with `../AGENTS.md`.

## Purpose and entry points

Rusty is a native macOS duplicate photo & video finder: Tauri 2 frontend, Rust
backend, persistent BLAKE3 hash cache in SQLite (WAL). Unlike the Python apps in
this workspace, Rusty is **Rust, not Python** — `uv`/`ruff`/`ty`/`pytest` do not
apply. Use `cargo`.

- Workspace root: `Cargo.toml` (member crate: `src-tauri`, package `rusty`,
  lib `rusty_core`)
- Binary entry: `src-tauri/src/main.rs`; Tauri builder in `src-tauri/src/lib.rs`
- Spec-aligned modules: `appinfo.rs`, `updates.rs`, `data_dir.rs`, `logs.rs`
- Frontend: `ui/` — plain HTML/CSS/JS, no bundler, no Node.js

Behavioral SSOT for cross-app contracts: `../Docs/razorcore-api-spec.md`
(Rusty implements contracts in-app; it does not import Python `razorcore`).

## Identifiers and paths

| Surface | Value |
| --- | --- |
| Bundle ID | `com.rusty.desktop` (legacy: `com.rusty.app`) |
| Binary / process name | `rusty` (lowercase — use `pgrep -x rusty`) |
| App support | `~/Library/Application Support/com.rusty.desktop/` |
| Update cache | `~/Library/Caches/Rusty/` |

## Environment

- macOS, Apple Silicon (arm64) only
- Homebrew-installed Rust (`brew install rust`)
- Workspace-local Tauri CLI: `cargo install --root .cargo tauri-cli@^2 --locked`

## Commands

```zsh
cargo check --workspace
cargo clippy --workspace
cargo test --workspace
zsh scripts/release-build.zsh
```

Run `cargo test --workspace` before claiming success on any change.

## Safety rules — preserve the dedup/quarantine contract

These behaviors are the product. Do not change them unless explicitly requested:

- **Exact duplicates only**: detection is by BLAKE3 content hash. Never add
  fuzzy, same-name, visual-similarity, or metadata-only matching to the
  duplicate plan. (`perceptual.rs` is review-only and must never feed the plan
  or move anything.)
- **Dry mode is the default** and must never delete, move, rename, or modify
  user files. Dry still saves valid hashes to the cache — keep that.
- **Real mode never deletes**: it moves confirmed duplicates to
  `~/Desktop/Quarantine` with a manifest, only after explicit confirmation.
  At least one copy per group is always kept. Originals are never overwritten.
- **Hash failures are non-fatal**: a file that fails to hash is logged,
  skipped, never grouped as a duplicate, never touched. One bad file must
  never abort a scan.
- **Cache writes stay atomic** (single transaction per file on the WAL DB).
  The hash database is never deleted except by explicit user request.
- **Cancel must stay safe**: stops promptly, never corrupts the DB, preserves
  hashes already saved, never undoes completed moves.
- Sources are read-only — Rusty never writes into scanned folders.

## Repository rules

- Use minimal, targeted changes; do not mix refactors with feature work.
- Prefer existing tooling and patterns; do not add dependencies unless
  necessary (and keep the no-Node, no-bundler frontend).
- Preserve the UI/backend separation: UI calls `invoke()` into
  `#[tauri::command]` handlers in `commands.rs`; core logic lives in
  `rusty_core` modules (`scanner.rs`, `memory.rs`, `dedupe.rs`,
  `quarantine.rs`, `appinfo.rs`, `updates.rs`, …).
- `_archive_pre_tauri/` is the frozen pre-Tauri implementation — read-only
  reference, never edit or revive it.
- `target/` and `build-logs/` are generated — never treat them as source.
- Do not commit, push, branch-switch, or create worktrees unless explicitly
  requested.
- Do not modify unrelated apps in the workspace.
