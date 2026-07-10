# Rusty

[![CI](https://img.shields.io/github/actions/workflow/status/RazorBackRoar/Rusty/ci.yml?branch=main&style=for-the-badge&label=CI)](https://github.com/RazorBackRoar/Rusty/actions/workflows/ci.yml)
[![Version](https://img.shields.io/badge/version-0.2.1-blue?style=for-the-badge)](Cargo.toml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blueviolet?style=for-the-badge)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2021-e8710a?style=for-the-badge&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Tauri](https://img.shields.io/badge/Tauri-f5c518?style=for-the-badge&logo=tauri&logoColor=white)](https://tauri.app/)
[![macOS](https://img.shields.io/badge/mac%20os-Apple%20Silicon-d32f2f?style=for-the-badge&logo=apple&logoColor=white)](https://support.apple.com/en-us/HT211814)

<!-- Workspace Health Layer -->
[![Status](https://img.shields.io/badge/status-active-2ea44f?style=for-the-badge)]()
[![Tests](https://img.shields.io/badge/tests-present-2ea44f?style=for-the-badge)]()

> **TL;DR:** Safe, native macOS duplicate photo and video finder. Exact BLAKE3 hash matching only — never fuzzy. Default **Dry** mode is read-only; **Real** mode quarantines confirmed duplicates after explicit confirmation.

A safe, native macOS **duplicate photo & video finder**. Tauri frontend, Rust
backend, with a persistent hash cache so re-scanning the same folders (or an
external SSD) is fast and a file's identity follows it as it moves or is renamed.

Rusty only ever finds **exact, byte-for-byte duplicates**. It never deletes:
the destructive step *moves* duplicates into a quarantine folder with a manifest
you can undo. The default mode is **Dry** — a fast, read-only preview that
changes nothing on disk.

---

## What Rusty does

- Recursively walks the folders (or external volumes) you give it — every
  nested subfolder, in a single pass — and reports exactly how many folders and
  files it discovered.
- Lets you **add multiple folders** (e.g. an external SSD plus Desktop and
  Downloads) and reports **each folder's counts separately** alongside the
  **combined totals**, while still finding duplicates across all of them.
- Computes a **BLAKE3 content hash** of every supported photo/video.
- Reports files that are **exact duplicates** — identical bytes, identical hash.
- Saves every valid hash to a **persistent cache** so later scans skip work, and
  reuses those hashes on rescans whenever the file is unchanged.
- In **Real** mode, moves the extra copies of each duplicate group into
  quarantine (keeping one), leaving a manifest so the whole batch can be undone.

It detects duplicates by **content hash only**. It does **not** do similar-image
detection, fuzzy matching, same-name matching, visual similarity, or
metadata-only matching when deciding what is a duplicate. (A separate,
review-only "similar images" tool exists for browsing visually-alike photos — it
never feeds the duplicate plan and never moves anything.)

---

## The two modes

Rusty has a two-position mode slider in the toolbar: **Dry · Real**. The orange
thumb slides to the half you pick. **Dry is selected by default.**

| Mode | Touches your files? | What it does |
|------|:-------------------:|--------------|
| **Dry** *(default)* | ❌ Never | A fast, read-only preview scan. Walks, hashes, and reports **exact duplicates only**. Computes and **saves valid hashes** to the persistent cache — this is the whole point of running Dry before Real. Never deletes, moves, renames, overwrites, or modifies user files. |
| **Real** | ✅ Only after you confirm | A live run. Scans (reusing cached hashes where safe), shows the plan/action bar, and — after you explicitly confirm — moves the **confirmed exact duplicates** of each group into quarantine. Also **saves valid hashes**. Only one copy per group is moved; at least one copy is always kept. |

**Default mode is Dry.**

> **Developer note:** the app's own test suite lives in `cargo test --workspace`
> and is run from the source checkout, not from the slider. It verifies Rusty's
> logic (grouping, cache reuse, undo) — it has nothing to do with your files.

---

## Why run Dry first

Dry is not just a no-op preview. **Dry computes and persists hashes to the
cache/database**, so it safely builds your hash database *before* anything
destructive happens. When you later run **Real**, it reuses those saved hashes
instead of re-reading every file — Real only re-hashes files that are **new,
changed, missing from the cache, or otherwise unsafe to trust.**

Both modes (Dry and Real) save valid hashes. The hashes persist across app
launches.

---

## Hash cache behavior

The cache lives in SQLite (WAL mode) under the app data directory and survives
restarts. For each file, Rusty decides whether it can trust a cached hash using
three checks, in priority order:

1. **Primary** — `(normalized path, size, modified-time)` all match a cached
   row → unchanged file, reuse the cached hash, no re-read.
2. **Filename move** — `(file name, size, modified-time)` match at a *different*
   path whose old location no longer exists → the file moved; reuse the hash and
   relocate the row.
3. **Content fallback** — the file is hashed, and that exact hash is already
   known at a stale path that no longer exists → a move/rename; relocate the row.

If a file **changed** (different size or modified-time), the cache misses, the
file is **re-hashed**, and the cache row is updated. Cache entries are only ever
reused when it is safe to do so (matching path/size/mtime, or a reliable
identity match) — never on name alone when a real file still sits at the old path.

**Cache writes are atomic.** Each file's `files` + `scan_files` rows are written
in a single transaction on a WAL database, so a cancel, crash, or hash error can
never leave the database half-written or corrupted. Hashes already saved before
a cancel/crash are preserved.

The hash database is **never deleted unless you explicitly request it** (e.g.
"Forget folder" in the UI, or deleting the DB file yourself).

---

## Hash error handling

Some files can't be read — most often because macOS is blocking access to an
external/removable volume (a "Operation not permitted" / permission-denied
error) until you grant the app access. Rusty handles this **safely and
non-destructively**:

- A file that fails to hash is **logged and skipped** — the scan keeps going.
- **One bad file never crashes or aborts the scan.**
- A failed-hash file is **never treated as a duplicate**, and is **never moved,
  deleted, renamed, or modified**.
- **Valid hashes are still saved** for every file that *did* hash, even when
  some files in the same scan failed.
- To avoid flooding the log, the first 20 errors are shown in full; the rest are
  aggregated into a summary count at the end of the scan, including how many were
  permission errors and how to fix them.

**If you see many permission errors scanning an external SSD:** grant Rusty
access under **System Settings → Privacy & Security → Files and Folders** (enable
the volume) or **Full Disk Access**, then rescan. Rusty will hash the remaining
files and reuse everything it already cached.

---

## What counts as a photo or video

Matching is **case-insensitive** — `.mov`, `.MOV`, `.Mov`, `.jpeg`, `.JPEG`,
`.mp4`, `.MP4`, etc. are all recognized.

- **Photos:** `jpg`, `jpeg`, `png`, `gif`, `bmp`, `tif`, `tiff`, `webp`, `heic`,
  `heif`, `avif`, `dng`, `cr2`, `cr3`, `nef`, `arw`, `orf`, `rw2`, `raf`, `pef`,
  `srw`
- **Videos:** `mp4`, `mov`, `m4v`, `avi`, `mkv`, `webm`, `mpg`, `mpeg`, `mts`,
  `m2ts`, `3gp`, `3g2`, `wmv`, `flv`, `ts`, `vob`, `hevc`, `asf`, `mod`, `tod`

Photos and videos are detected in the **same unified scan** of one folder tree.
macOS bookkeeping files (`.DS_Store`, AppleDouble `._*`, `.Spotlight-V100`,
`.Trashes`, etc.) are skipped and never counted as duplicates.

---

## The three tabs

The results area has three tabs:

- **Files** — the **Scan summary** (folder and file counts, see below) plus a
  unified list of the supported files found, each labelled photo or video **and
  tagged with the folder it came from**, so you can see they all came from one
  recursive scan.
- **Duplicates** — the exact-duplicate groups (photos and videos together,
  across every added folder; each file shows its source folder). In Real mode
  the plan/action bar appears here.
- **Logs** — the live scan log, skipped-file/folder reasons, the change and
  cache summaries, and the **Export** button (pinned to the bottom-right, only
  visible while the Logs tab is open).

### Scan summary (folder counts)

Because the original bug made it look like only one folder was scanned, every
scan now reports a full breakdown in the Files tab and the log:

```
Selected roots, Top-level folders, Nested folders,
Total folders discovered, Total folders scanned,
Folders pruned (dev/cache/system), Folders skipped (read errors),
Empty folders, Folders with / without supported files,
Folders with photos / videos / both,
Supported files (photos, videos), Unsupported files ignored, Filtered files,
Hash cache hits / misses, Stale records ignored, New hashes saved,
Moved-file matches reused, Hash errors.
```

Folder counting happens **during the recursive walk, before any media filtering,
size checks, hashing, or duplicate grouping** — so a folder is counted even if it
is empty, holds only unsupported files, has only photos, only videos, no
duplicates, or never appears in the duplicate report. If a folder was visited it
is counted; if it was skipped (pruned or unreadable) it is counted with a reason.

### Adding multiple folders — per-folder counts + combined totals

You can keep adding sources one after another (an external SSD, then Desktop,
then Downloads, …) — each **+ Add Folder** or drag-drop **appends** to the list;
nothing is replaced until you remove a row or press **Clear**. A single scan then
walks every added folder once, and the Scan summary shows:

- **Combined totals** across all added folders, and
- a **Per folder** section with each folder's own counts (top-level / nested /
  discovered / scanned / pruned / errors / empty / with-media / supported files /
  photos / videos / unsupported / cache hits / misses / stale ignored / new
  saved). So Folder A's counts are reported separately from Folder B's.

Duplicate detection still runs **across all added folders together**, and each
duplicate file shows which folder it came from.

### Progress

While scanning, the progress bar shows the current phase: discovering folders &
files → checking the memory bank → hashing (with a real 0–100%) → saving → done.
Progress is driven by real scan counts, never faked.

---

## External SSD safety

- Drag any folder or external SSD from Finder onto the window to add it as a
  source, or use **+ Add Folder**.
- **Dropping a drive scans the whole drive.** The dropped path is the scan root,
  and Rusty walks it recursively — so it counts **every nested folder inside the
  drive**, not just the one dropped item. The folder count climbs live in the
  Memory Bank panel during the walk, and the Files-tab Scan summary breaks it
  down (top-level vs. nested, scanned vs. pruned, empty, with/without media).
- Rusty only reads from your sources; it never writes into them.
- The first scan of a drive builds the hash cache; the next scan of the same
  drive reuses every unchanged file's hash and only re-reads files whose size or
  modified-time changed.
- Quarantined files are moved into `~/Desktop/Quarantine` only after you
  explicitly confirm a **Real** quarantine run. Opening the app, scanning, Dry
  mode, and reviewing results do not create that Desktop folder.
- Cross-device moves fall back to copy-then-delete only when the OS reports the
  source and destination are on different volumes (EXDEV).

---

## Cancel / stop behavior

- A **Cancel** button appears in the toolbar while a scan is running.
- Cancellation is a live, shared signal checked by the directory walker and by
  every parallel hashing worker, so it takes effect promptly.
- Cancelling a scan is **safe**: the active scan stops, the database is never
  left corrupted, and nothing is moved or deleted.
- During a Real quarantine run, the button changes to **Cancel Remaining**. That
  stops pending moves when safe, reports files already moved and files left
  untouched, and never undoes completed moves.
- After a quarantine run finishes, the button changes to **Clear**. Clear only
  resets the UI for a fresh start; it does not undo or move files back.

---

## Logs

- The **Logs** tab streams everything: which folders are walked, the folder
  taxonomy and file counts, cache reuse (hits / misses / stale-ignored /
  new-hashes-saved / moved-file matches), the "since last scan" change summary,
  moves/renames detected, hash errors (with the actionable permission hint), and
  start/complete markers for each Dry/Real run.
- The **Export** button lives at the bottom-right of the Logs tab and is hidden
  on the Files and Duplicates tabs.
- Logs are also appended to a file under the app data directory
  (`logs/rusty.log`).

---

## Basic usage

1. Launch **Rusty**.
2. Add sources: drag a folder or external SSD onto the window, or click
   **+ Add Folder**.
3. Pick a mode with the slider (**Dry** is the default).
4. Press **Scan**.
5. Review the **Files** tab (Scan summary + the files found) and the
   **Duplicates** tab (exact-duplicate groups).
6. In **Real** mode, review the plan/action bar, choose which copy to keep per
   group, then confirm to move the duplicates to quarantine.
7. Open the **Logs** tab and use **Export** (bottom-right) to save a CSV/JSON
   report, or **Undo** to reverse a quarantined batch from its manifest.

### Recommended workflow

1. **Dry** — run on the target folders or external SSD to build the hash cache
   and preview duplicates.
2. **Review** the Scan summary in the Files tab and the exact-duplicate results
   in the Duplicates tab.
3. **Real** — run only after you've confirmed the results look right.

---

## Safety guarantees

- **Dry never deletes, moves, renames, or modifies user files.** It only reads
  and hashes.
- **Files that fail to hash are skipped and logged** — never treated as
  duplicates, never touched.
- **Duplicate detection is exact content/hash only** — no similar-file, fuzzy,
  same-name, visual-similarity, or metadata-only matching.
- **Real acts only on confirmed exact duplicates**, after explicit confirmation.
- **Real moves duplicates to quarantine instead of permanently deleting them**,
  and writes a manifest so the batch can be undone.
- **At least one copy of every duplicate group is always kept** — Rusty refuses
  any plan that would remove the only remaining copy.
- **Originals are never overwritten.**
- **The hash database is never deleted unless you explicitly request it.**
- **Cancel always works**, and cancelling preserves hashes already saved.

---

## App data directory

Rusty's app state goes here (macOS):

```
~/Library/Application Support/com.rusty.desktop/
├── memory_bank.sqlite        # SQLite WAL DB: folders, files, scans (the hash cache)
├── logs/rusty.log            # appended log of every run
├── exports/                  # CSV/JSON exports (manual)
└── manifests/<run_id>.json   # JSON manifest for each quarantine batch (used by Undo)
```

If an older build wrote state under `~/Library/Application Support/com.rusty.app/`,
Rusty moves missing entries into the canonical `com.rusty.desktop` directory on
startup. Existing canonical entries are never overwritten; conflicting legacy
entries are left in the legacy directory.

Quarantined files are written here only after an explicitly confirmed Real
quarantine run:

```
~/Desktop/Quarantine/
├── <quarantined files>       # flat list, conflict-safe names
└── Quarantine-Log.csv        # original path -> quarantine path, size, hash, status
```

Rusty does **not** create `~/Desktop/Quarantine` when the app opens, when you
scan, or during Dry mode. It creates the folder only when a confirmed Real
quarantine run needs to move files.

The quarantine folder is flat. Rusty does not recreate `/Volumes/...` or source
folder trees inside it. If two quarantined files have the same name, Rusty uses
safe suffixes such as `photo.jpg`, `photo_2.jpg`, `photo_3.jpg` and never
overwrites existing files. Original full paths are preserved in the manifest and
`Quarantine-Log.csv`.

---

## Layout

```
Rusty/
├── Cargo.toml                # workspace
├── src-tauri/
│   ├── Cargo.toml            # the tauri crate (package: rusty, lib: rusty_core)
│   ├── tauri.conf.json       # bundle ID (com.rusty.desktop), window config, CSP
│   ├── build.rs
│   ├── capabilities/main.json
│   ├── icons/                # app icon assets
│   ├── src/
│   │   ├── main.rs           # binary entry
│   │   ├── lib.rs            # Tauri builder + command registration
│   │   ├── commands.rs       # #[tauri::command] handlers
│   │   ├── scanner.rs        # walk + hash + move detection + hash-error handling
│   │   ├── memory.rs         # SQLite hash cache / memory bank
│   │   ├── dedupe.rs         # group by hash + build plan
│   │   ├── quarantine.rs     # safe move + manifest + undo
│   │   ├── perceptual.rs     # review-only similar-image search (never deletes)
│   │   ├── paths.rs          # NFC normalize + sanitize + media filtering
│   │   ├── logs.rs           # ring-buffer log feed
│   │   ├── data_dir.rs       # resolves app-data layout
│   │   ├── appinfo.rs        # startup banner / About metadata
│   │   ├── updates.rs        # GitHub Releases update check
│   │   ├── state.rs          # shared app state
│   │   └── error.rs          # typed errors that serialize to JS
│   └── tests/smoke.rs        # integration tests
├── ui/
│   ├── index.html            # one window: Files / Duplicates / Logs tabs
│   ├── app.css               # orange + white theme, Dry/Real slider
│   └── app.js                # invoke() calls, no bundler
└── .cargo/bin/cargo-tauri    # workspace-local Tauri CLI
```

---

## Build

Requires Homebrew-installed Rust (`brew install rust`). No Node.js.

```zsh
cd /Users/home/Workspace/Apps/Rusty

# Install the Tauri CLI into the workspace-local .cargo dir (once)
cargo install --root .cargo tauri-cli@^2 --locked

# Release .app + .dmg bundle (Apple Silicon) with repo-local preflight checks
zsh scripts/release-build.zsh
```

Bundles land at:

```
target/release/bundle/macos/Rusty.app
target/release/bundle/dmg/Rusty_0.2.1_aarch64.dmg
```

The Mach-O binary inside the app is named `rusty` (lowercase).

### Install from release

1. Download the latest `.dmg` from [Releases](https://github.com/RazorBackRoar/Rusty/releases)
2. Open the DMG and drag `Rusty.app` to `/Applications`
3. First launch — right-click → **Open** if Gatekeeper blocks the ad-hoc signed build

Each release run writes a timestamped log under `build-logs/`.

---

## Tests

The app's test suite runs from the source checkout:

```zsh
cargo test --workspace
```

Tests cover, among others:

- Duplicate detection by content hash (exact only)
- Recursive folder counting (a nested tree reports every folder, not just the root)
- Dry-run leaves the source tree untouched
- A file that fails to hash is skipped, never grouped as a duplicate, and valid
  hashes are still saved
- Rescanning reuses cached hashes (zero re-hashing on unchanged files)
- Hash cache persists across process restarts (DB reopens)
- Moved/renamed files re-tagged by hash fallback; pure moves reuse the hash
- Media-only scans hash supported photos/videos and skip artifacts
- Default plan keeps exactly one copy per group
- Apply → undo round-trips every file back to its original path
