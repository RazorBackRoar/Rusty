# Architecture — Rusty

Developer map for the duplicate photo & video finder (Tauri 2 + Rust + plain
HTML/JS frontend).

## Crate layout

| Path | Role |
|------|------|
| `src-tauri/src/lib.rs` | Tauri builder, command registration |
| `src-tauri/src/commands.rs` | `#[tauri::command]` handlers |
| `src-tauri/src/scanner.rs` | Recursive walk, hash, progress |
| `src-tauri/src/memory.rs` | SQLite WAL BLAKE3 hash cache |
| `src-tauri/src/dedupe.rs` | Group by content hash |
| `src-tauri/src/quarantine.rs` | Move duplicates, manifest, undo |
| `src-tauri/src/similar.rs` | Review-only video similarity (ffmpeg) |
| `src-tauri/src/folder_map.rs` | Map tab folder tree |
| `src-tauri/src/paths.rs` | NFC normalize, media filtering |
| `ui/` | HTML/CSS/JS — no bundler, no Node deps |

## UI modes

### Scan mode (default sidebar)

Add folder sources → **Scan** walks all roots in one pass → results in five tabs:

| Tab | Color | Contents |
|-----|-------|----------|
| **Files** | Orange | Scan summary + per-file list (photo/video + source folder) |
| **Duplicates** | Blue | Exact BLAKE3 duplicate groups; Real-mode quarantine plan |
| **Similar** | Red | Review-only perceptual hash matches (requires ffmpeg) |
| **Map** | Green | Folder tree with subtree counts (no filenames) |
| **Logs** | Grey | Live log, export button |

### Compare mode (sidebar toggle)

Two folder drop zones. Dry scan with `media_only: false` (all files). Results
render in **Duplicates** tab. `commit_results: false` — Compare never affects
the main scan plan or quarantine state.

## Safety contract

- **Dry mode (default):** never moves, deletes, or renames user files. Still
  writes valid hashes to the cache.
- **Real mode:** moves confirmed duplicates to `~/Desktop/Quarantine` with a
  manifest — **never deletes**. At least one copy per group is kept.
- **Exact duplicates only** in automated Real-mode quarantine. Similar tab is
  review-only.
- **Hash failures are non-fatal** — logged and skipped.
- **Cancel** stops promptly without corrupting the WAL database.

## Data paths

| Path | Contents |
|------|----------|
| `~/Library/Application Support/com.rusty.desktop/` | Hash DB, settings |
| `~/Library/Caches/Rusty/` | Update check cache |
| `~/Desktop/Quarantine/` | Real-mode duplicate moves |

## Frontend ↔ backend

UI calls `invoke('command_name', …)` into Rust handlers. No npm build step.

Optional frontend tests (not in CI):

```bash
node --test ui/app.test.js
```

## Verification

```bash
cargo check --workspace
cargo clippy --workspace
cargo test --workspace
```

## Related docs

- [README.md](../README.md) — user-facing usage
- [BUILD_AND_RELEASE.md](../BUILD_AND_RELEASE.md)
- [AGENTS.md](../AGENTS.md) — safety rules (do not change without approval)
