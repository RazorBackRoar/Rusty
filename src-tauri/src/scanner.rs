//! File-walking and content hashing.
//!
//! Two-pass strategy:
//!   1. One recursive walk per root with `jwalk`. In this single pass we count
//!      every directory (the folder taxonomy — before any filtering), classify
//!      each file as photo/video/unsupported, and collect supported files as
//!      (path, size, modified) tuples.
//!   2. Ask the memory bank whether we already have a usable full hash for each
//!      file, tallying cache hits/misses and stale records ignored.
//!   3. BLAKE3 every cache miss in parallel with `rayon`, then group by hash.
//!
//! The hash is BLAKE3 over the entire file. Faster than SHA-256, collision
//! resistant enough that we treat hash equality as identity.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use blake3::Hasher;
use jwalk::WalkDir;
use rayon::prelude::*;
use serde::Serialize;

use crate::error::{AppError, AppResult};
use crate::logs::LogSink;
use crate::memory::{CacheHit, MemoryBank};
use crate::paths;

const READ_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// Hash failures are logged in full up to this many lines; past that we switch
/// to an aggregated summary so a whole unreadable volume can't flood the log
/// with thousands of identical warnings.
const MAX_DETAILED_HASH_ERRORS: usize = 20;

#[derive(Clone, Debug, Serialize)]
pub struct ScannedFile {
    pub hash: String,
    pub path: String,
    pub normalized_path: String,
    pub file_name: String,
    pub media_kind: MediaKind,
    pub source_root: String,
    pub size: i64,
    pub modified_ns: i64,
    pub reused_from_cache: bool,
    pub moved_from: Option<String>,
    /// Physical-file identity (unix dev+inode). Internal — not sent to the UI.
    #[serde(skip)]
    pub dev: u64,
    #[serde(skip)]
    pub ino: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ScanCounters {
    pub files_walked: usize,
    pub files_skipped: usize,
    /// Files whose extension is not a supported photo/video type.
    pub unsupported_files: usize,
    pub bytes_walked: u64,
    pub photos: usize,
    pub videos: usize,
    pub hashes_reused: usize,
    pub hashes_computed: usize,
    pub bytes_hashed: u64,
    pub errors: usize,
    /// Files served straight from the memory bank without re-reading bytes.
    pub cache_hits: usize,
    /// Files not found in the cache (needed hashing).
    pub cache_misses: usize,
    /// Cached rows that existed but were ignored as stale/unsafe to trust.
    pub stale_ignored: usize,
    /// Freshly computed hashes written back to the memory bank this scan.
    pub new_hashes_saved: usize,
    /// Files re-identified at a new path via filename or content-hash match.
    pub moved_reused: usize,
    /// Recursive folder-discovery taxonomy (combined across all added folders).
    pub folders: FolderStats,
    /// One entry per added folder, with that folder's own counts — so Folder A's
    /// stats are reported separately from Folder B's, alongside the combined
    /// totals above.
    pub per_folder: Vec<PerFolderStats>,
    /// Capped sample of supported files found, for the Files tab.
    pub sample_files: Vec<SampleFile>,
}

/// Counts for one added folder (one scan root). Combined totals live on
/// [`ScanCounters`]; this is the per-folder breakdown the user sees when several
/// folders are added (e.g. an external SSD plus Desktop and Downloads).
#[derive(Clone, Debug, Default, Serialize)]
pub struct PerFolderStats {
    /// Normalized root path (matches `ScannedFile::source_root`).
    pub root: String,
    /// Raw root path, for display.
    pub root_display: String,
    pub folders: FolderStats,
    pub photos: usize,
    pub videos: usize,
    pub unsupported_files: usize,
    /// Supported files found in this folder (the dedup candidates).
    pub files_walked: usize,
    pub files_skipped: usize,
    pub bytes_walked: u64,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub stale_ignored: usize,
    pub new_hashes_saved: usize,
    pub errors: usize,
}

/// Per-folder discovery taxonomy. Counted during the single recursive walk —
/// before any media filtering, size grouping, or hashing — so every visited
/// folder is accounted for even if it is empty, unsupported, or duplicate-free.
#[derive(Clone, Debug, Default, Serialize)]
pub struct FolderStats {
    pub selected_roots: usize,
    pub total_discovered: usize,
    pub top_level: usize,
    pub nested: usize,
    pub scanned: usize,
    pub pruned: usize,
    pub skipped_errors: usize,
    pub empty: usize,
    pub with_supported: usize,
    pub without_supported: usize,
    pub with_photos: usize,
    pub with_videos: usize,
    pub with_both: usize,
}

/// A lightweight, capped sample of supported files so the Files tab can show
/// that real files came from the recursive tree without serialising tens of
/// thousands of rows.
#[derive(Clone, Debug, Serialize)]
pub struct SampleFile {
    pub path: String,
    pub file_name: String,
    pub media_kind: MediaKind,
    pub size: i64,
    /// Which added folder this file came from (normalized root path).
    pub source_root: String,
}

/// Cap on [`ScanCounters::sample_files`].
const SAMPLE_CAP: usize = 500;

#[derive(Clone, Debug, Serialize)]
pub struct ScanProgress {
    pub phase: String,
    pub processed: usize,
    pub total: usize,
    pub percent: u8,
    /// Cached full-file hashes reused so far in this scan phase.
    pub hashes_reused: usize,
    /// Fresh full-file hashes computed so far in this scan phase.
    pub hashes_computed: usize,
    /// Folders discovered so far (live during the walk, final afterwards) so the
    /// UI can show the recursive folder count climb instead of sitting at 1.
    pub folders: usize,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Photo,
    Video,
    Other,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Photo => "photo",
            Self::Video => "video",
            Self::Other => "other",
        }
    }
}

/// Walk-time filters. [`ScanOptions::default`] is unfiltered for library tests;
/// the Tauri UI passes media-only defaults.
#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Skip files strictly smaller than this many bytes (0 = no minimum).
    pub min_size: i64,
    /// Prune well-known developer/cache directories and opaque app bundles.
    pub skip_dev_dirs: bool,
    /// Skip any file whose normalized path contains one of these
    /// (case-insensitive) substrings.
    pub exclude: Vec<String>,
    /// Only include supported photo/video files.
    pub media_only: bool,
    /// Follow symlinks while walking. Off by default — symlinked files are not
    /// treated as regular files and never quarantined.
    pub follow_symlinks: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            min_size: 0,
            skip_dev_dirs: false,
            exclude: Vec::new(),
            media_only: false,
            follow_symlinks: false,
        }
    }
}

/// One scan, end-to-end. Returns hashed files plus counters for the UI.
/// Convenience wrapper that runs with the default (unfiltered) options.
pub fn scan_roots(
    roots: &[PathBuf],
    memory: &MemoryBank,
    logs: &LogSink,
    cancel: &Arc<AtomicBool>,
    scan_id: i64,
) -> AppResult<(Vec<ScannedFile>, ScanCounters)> {
    scan_roots_with_options(
        roots,
        memory,
        logs,
        cancel,
        scan_id,
        &ScanOptions::default(),
    )
}

/// Same as [`scan_roots`] but with explicit walk-time filters.
pub fn scan_roots_with_options(
    roots: &[PathBuf],
    memory: &MemoryBank,
    logs: &LogSink,
    cancel: &Arc<AtomicBool>,
    scan_id: i64,
    options: &ScanOptions,
) -> AppResult<(Vec<ScannedFile>, ScanCounters)> {
    scan_roots_with_progress(roots, memory, logs, cancel, scan_id, options, &|_| {})
}

pub fn scan_roots_with_progress(
    roots: &[PathBuf],
    memory: &MemoryBank,
    logs: &LogSink,
    cancel: &Arc<AtomicBool>,
    scan_id: i64,
    options: &ScanOptions,
    progress: &(dyn Fn(ScanProgress) + Sync),
) -> AppResult<(Vec<ScannedFile>, ScanCounters)> {
    let mut counters = ScanCounters::default();
    progress(ScanProgress {
        phase: "inventory".into(),
        processed: 0,
        total: 0,
        percent: 0,
        hashes_reused: 0,
        hashes_computed: 0,
        folders: 0,
    });

    // Phase 1: one recursive walk per added folder. During this single pass we
    // count every directory (before any media filtering or hashing), classify
    // each file as photo/video/unsupported, and collect supported files for
    // dedupe — accumulating each folder's own counts separately.
    let mut all_entries: Vec<WalkedEntry> = Vec::new();
    let mut per_folder: Vec<PerFolderStats> = Vec::with_capacity(roots.len());
    let mut root_index: HashMap<String, usize> = HashMap::new();
    // Live count of subdirectories discovered across all roots, emitted during
    // the walk so the UI's folder count climbs instead of waiting for the end.
    let mut discovered_dirs = 0usize;

    for root in roots {
        if cancel.load(Ordering::SeqCst) {
            return Err(AppError::BadInput("scan cancelled".into()));
        }
        let root_str = paths::normalize_for_storage(root);
        let raw = root.to_string_lossy().into_owned();
        // Register this root in the memory bank's folder table. Idempotent.
        memory.remember_folder(&raw, &root_str)?;
        logs.info(format!("walking {}", root.display()));

        // Per-folder accumulators — each added folder keeps its own counts.
        let mut dir_aggs: HashMap<String, DirAgg> = HashMap::new();
        let pruned_paths: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let mut dir_errors = 0usize;
        let mut r_errors = 0usize;
        let mut r_photos = 0usize;
        let mut r_videos = 0usize;
        let mut r_unsupported = 0usize;
        let mut r_files_skipped = 0usize;
        let mut per_root_files = 0usize;
        let mut per_root_bytes: i64 = 0;

        let skip_dev = options.skip_dev_dirs;
        let pruned_for_walk = pruned_paths.clone();
        for dirent in WalkDir::new(root)
            .skip_hidden(false)
            .follow_links(options.follow_symlinks)
            .process_read_dir(move |_depth, parent_path, _state, children| {
                children.iter_mut().for_each(|res| {
                    if let Ok(child) = res {
                        let name = child.file_name.to_string_lossy();
                        let prune = paths::is_macos_metadata_dir(&name)
                            || (skip_dev && paths::is_dev_or_cache_dir(&name));
                        if child.file_type.is_dir() && prune {
                            // The folder is still counted as discovered, but with
                            // a "pruned" reason, and we don't descend into it.
                            let full = parent_path.join(&child.file_name);
                            if let Ok(mut set) = pruned_for_walk.lock() {
                                set.insert(paths::normalize_for_storage(&full));
                            }
                            child.read_children_path = None;
                        }
                    }
                });
            })
        {
            let dirent = match dirent {
                Ok(d) => d,
                Err(e) => {
                    // jwalk surfaces unreadable directories here.
                    r_errors += 1;
                    dir_errors += 1;
                    logs.warn(format!("walk error (unreadable folder): {e}"));
                    continue;
                }
            };

            // Count directories during traversal, before any filtering.
            if dirent.file_type.is_dir() {
                let norm = paths::normalize_for_storage(&dirent.path());
                let depth = dirent.depth;
                dir_aggs.entry(norm).or_default().depth = depth;
                if depth >= 1 {
                    discovered_dirs += 1;
                    if discovered_dirs % 64 == 0 {
                        progress(ScanProgress {
                            phase: "inventory".into(),
                            processed: all_entries.len(),
                            total: 0,
                            percent: 0,
                            hashes_reused: 0,
                            hashes_computed: 0,
                            folders: discovered_dirs,
                        });
                    }
                }
                continue;
            }
            if !dirent.file_type.is_file() {
                continue; // symlink / device / fifo — not a regular file
            }

            let file_name = dirent.file_name.to_string_lossy();
            if paths::is_macos_metadata(&file_name) {
                r_files_skipped += 1;
                continue;
            }
            let media_kind = media_kind_for_name(&file_name);

            // Per-folder classification covers every real (non-metadata) file,
            // so empty / photo-only / video-only / unsupported-only folders are
            // all counted correctly regardless of the media filter below.
            let parent_norm = dirent
                .path()
                .parent()
                .map(paths::normalize_for_storage)
                .unwrap_or_default();
            {
                let agg = dir_aggs.entry(parent_norm).or_default();
                agg.files += 1;
                match media_kind {
                    MediaKind::Photo => {
                        agg.supported += 1;
                        agg.photos += 1;
                    }
                    MediaKind::Video => {
                        agg.supported += 1;
                        agg.videos += 1;
                    }
                    MediaKind::Other => {}
                }
            }
            match media_kind {
                MediaKind::Photo => r_photos += 1,
                MediaKind::Video => r_videos += 1,
                MediaKind::Other => r_unsupported += 1,
            }

            if options.media_only && media_kind == MediaKind::Other {
                r_files_skipped += 1;
                continue;
            }
            let path = dirent.path();
            let metadata = match dirent.metadata() {
                Ok(m) => m,
                Err(e) => {
                    r_errors += 1;
                    logs.warn(format!("metadata error on {}: {e}", path.display()));
                    continue;
                }
            };
            let size = metadata.len() as i64;
            if options.min_size > 0 && size < options.min_size {
                r_files_skipped += 1;
                continue;
            }
            let normalized_path = paths::normalize_for_storage(&path);
            if !options.exclude.is_empty() {
                let haystack = normalized_path.to_lowercase();
                let excluded = options.exclude.iter().any(|pat| {
                    let needle = pat.trim().to_lowercase();
                    !needle.is_empty() && haystack.contains(&needle)
                });
                if excluded {
                    r_files_skipped += 1;
                    continue;
                }
            }
            let modified_ns = metadata
                .modified()
                .map(|t| match t.duration_since(SystemTime::UNIX_EPOCH) {
                    Ok(d) => d.as_nanos().min(i64::MAX as u128) as i64,
                    // Pre-1970 mtime: store as a unique negative value so that
                    // files modified before the epoch don't all collide at 0
                    // and trigger false cache hits against each other.
                    Err(e) => -(e.duration().as_nanos().min(i64::MAX as u128) as i64),
                })
                .unwrap_or(0);
            let (dev, ino) = file_identity(&metadata);
            if counters.sample_files.len() < SAMPLE_CAP {
                counters.sample_files.push(SampleFile {
                    path: path.to_string_lossy().into_owned(),
                    file_name: file_name.to_string(),
                    media_kind,
                    size,
                    source_root: root_str.clone(),
                });
            }
            all_entries.push(WalkedEntry {
                path,
                normalized_path,
                file_name: file_name.to_string(),
                media_kind,
                size,
                modified_ns,
                source_root: root_str.clone(),
                dev,
                ino,
            });
            if all_entries.len() % 256 == 0 {
                progress(ScanProgress {
                    phase: "inventory".into(),
                    processed: all_entries.len(),
                    total: 0,
                    percent: 0,
                    hashes_reused: 0,
                    hashes_computed: 0,
                    folders: discovered_dirs,
                });
            }
            per_root_files += 1;
            per_root_bytes += size;
        }
        // Fold this folder's per-directory aggregates into its own taxonomy.
        let pruned = pruned_paths.lock().map(|s| s.clone()).unwrap_or_default();
        let mut fstats = FolderStats {
            selected_roots: 1,
            pruned: pruned.len(),
            skipped_errors: dir_errors,
            ..FolderStats::default()
        };
        for (path, agg) in &dir_aggs {
            if agg.depth == 0 {
                continue; // the selected root itself — counted as a root
            }
            fstats.total_discovered += 1;
            if agg.depth == 1 {
                fstats.top_level += 1;
            } else {
                fstats.nested += 1;
            }
            if pruned.contains(path) {
                continue; // discovered + counted as pruned; not classified further
            }
            if agg.files == 0 {
                fstats.empty += 1;
            }
            if agg.supported > 0 {
                fstats.with_supported += 1;
            } else {
                fstats.without_supported += 1;
            }
            if agg.photos > 0 {
                fstats.with_photos += 1;
            }
            if agg.videos > 0 {
                fstats.with_videos += 1;
            }
            if agg.photos > 0 && agg.videos > 0 {
                fstats.with_both += 1;
            }
        }
        fstats.scanned = fstats.total_discovered.saturating_sub(fstats.pruned);

        logs.info(format!(
            "folder \"{}\" — discovered: {}, top-level: {}, nested: {}, scanned: {}, pruned: {}, \
             empty: {}, with-media: {}, supported files: {} (photos: {}, videos: {}), \
             unsupported: {}, read-errors: {}",
            root.display(),
            fstats.total_discovered,
            fstats.top_level,
            fstats.nested,
            fstats.scanned,
            fstats.pruned,
            fstats.empty,
            fstats.with_supported,
            per_root_files,
            r_photos,
            r_videos,
            r_unsupported,
            fstats.skipped_errors,
        ));

        // Accumulate this folder into the combined totals.
        counters.files_walked += per_root_files;
        counters.bytes_walked += per_root_bytes as u64;
        counters.files_skipped += r_files_skipped;
        counters.unsupported_files += r_unsupported;
        counters.photos += r_photos;
        counters.videos += r_videos;
        counters.errors += r_errors;
        {
            let cf = &mut counters.folders;
            cf.total_discovered += fstats.total_discovered;
            cf.top_level += fstats.top_level;
            cf.nested += fstats.nested;
            cf.scanned += fstats.scanned;
            cf.pruned += fstats.pruned;
            cf.skipped_errors += fstats.skipped_errors;
            cf.empty += fstats.empty;
            cf.with_supported += fstats.with_supported;
            cf.without_supported += fstats.without_supported;
            cf.with_photos += fstats.with_photos;
            cf.with_videos += fstats.with_videos;
            cf.with_both += fstats.with_both;
        }

        memory.update_folder_stats(&root_str, per_root_files as i64, per_root_bytes as i64)?;

        root_index.insert(root_str.clone(), per_folder.len());
        per_folder.push(PerFolderStats {
            root: root_str.clone(),
            root_display: raw,
            folders: fstats,
            photos: r_photos,
            videos: r_videos,
            unsupported_files: r_unsupported,
            files_walked: per_root_files,
            files_skipped: r_files_skipped,
            bytes_walked: per_root_bytes as u64,
            errors: r_errors,
            ..PerFolderStats::default()
        });
    }
    counters.folders.selected_roots = roots.len();

    logs.info(format!(
        "combined — folders discovered: {}, scanned: {}, supported files: {} \
         (photos: {}, videos: {}), unsupported: {}, filtered: {}, across {} folder(s)",
        counters.folders.total_discovered,
        counters.folders.scanned,
        counters.files_walked,
        counters.photos,
        counters.videos,
        counters.unsupported_files,
        counters.files_skipped,
        roots.len(),
    ));
    // Final discovered-folder count, reused by every progress event from here on
    // so the UI keeps showing the real number through hashing and saving.
    let total_folders = counters.folders.total_discovered;

    if cancel.load(Ordering::SeqCst) {
        return Err(AppError::BadInput("scan cancelled".into()));
    }

    // Phase 2: identify duplicates among the walked files.
    progress(ScanProgress {
        phase: "checking-cache".into(),
        processed: 0,
        total: all_entries.len(),
        percent: 0,
        hashes_reused: 0,
        hashes_computed: 0,
        folders: total_folders,
    });

    // Look every file up in the memory bank first. A hit means we already hold
    // a trusted full hash and can skip re-reading the bytes; we also tally how
    // many cached rows were ignored as stale/unsafe so the reuse is auditable.
    let mut lookups: Vec<LookupResult> = Vec::with_capacity(all_entries.len());
    let mut stale_ignored = 0usize;
    for (i, e) in all_entries.iter().enumerate() {
        let raw = memory
            .lookup(&e.normalized_path, &e.file_name, e.size, e.modified_ns)
            .unwrap_or(CacheHit::Miss);
        let usable = usable_cache_hit(e, raw.clone());
        let pf = root_index.get(&e.source_root).copied();
        if !matches!(raw, CacheHit::Miss) && matches!(usable, CacheHit::Miss) {
            stale_ignored += 1;
            if let Some(fi) = pf {
                per_folder[fi].stale_ignored += 1;
            }
        }
        if matches!(usable, CacheHit::Miss) {
            if let Some(fi) = pf {
                per_folder[fi].cache_misses += 1;
            }
        }
        lookups.push(LookupResult {
            idx: i,
            hit: usable,
        });
    }

    let reused = Arc::new(AtomicUsize::new(0));
    let computed = Arc::new(AtomicUsize::new(0));
    let bytes_hashed = Arc::new(AtomicU64::new(0));
    // Clone the caller's Arc so rayon workers check the live cancel flag,
    // not a one-time snapshot.
    let cancel_arc = cancel.clone();

    // Every cache miss is full-file hashed before duplicate comparison. The
    // report must be based on identical bytes, not partial fingerprints.
    let misses: Vec<usize> = lookups
        .iter()
        .filter(|l| matches!(l.hit, CacheHit::Miss))
        .map(|l| l.idx)
        .collect();
    let total = all_entries.len();
    let reused_for_progress = total - misses.len();
    let initial_percent = ((reused_for_progress * 100) / total.max(1)) as u8;
    progress(ScanProgress {
        phase: "hashing".into(),
        processed: reused_for_progress,
        total,
        percent: initial_percent,
        hashes_reused: reused_for_progress,
        hashes_computed: 0,
        folders: total_folders,
    });

    let processed = Arc::new(AtomicUsize::new(0));
    let last_pct = Arc::new(AtomicU8::new(initial_percent));

    let full_hashed: Vec<(usize, HashOutcome)> = misses
        .par_iter()
        .map(|&idx| {
            if cancel_arc.load(Ordering::SeqCst) {
                return (idx, HashOutcome::Cancelled);
            }
            let entry = &all_entries[idx];
            match hash_file(&entry.path) {
                Ok(h) => {
                    bytes_hashed.fetch_add(entry.size as u64, Ordering::Relaxed);
                    let hashes_computed = computed.fetch_add(1, Ordering::Relaxed) + 1;
                    let done = reused_for_progress + processed.fetch_add(1, Ordering::Relaxed) + 1;
                    let pct = ((done * 100) / total.max(1)) as u8;
                    if pct > last_pct.fetch_max(pct, Ordering::Relaxed) {
                        progress(ScanProgress {
                            phase: "hashing".into(),
                            processed: done,
                            total,
                            percent: pct,
                            hashes_reused: reused_for_progress,
                            hashes_computed,
                            folders: total_folders,
                        });
                    }
                    (idx, HashOutcome::Ok(h))
                }
                // A single unreadable file must never abort the scan. Keep the
                // error so the caller can classify it, then drop the file from
                // the results so it can't be grouped, moved, or deleted.
                Err(e) => {
                    let done = reused_for_progress + processed.fetch_add(1, Ordering::Relaxed) + 1;
                    let pct = ((done * 100) / total.max(1)) as u8;
                    if pct > last_pct.fetch_max(pct, Ordering::Relaxed) {
                        progress(ScanProgress {
                            phase: "hashing".into(),
                            processed: done,
                            total,
                            percent: pct,
                            hashes_reused: reused_for_progress,
                            hashes_computed: computed.load(Ordering::Relaxed),
                            folders: total_folders,
                        });
                    }
                    (idx, HashOutcome::Failed(e))
                }
            }
        })
        .collect();

    if cancel.load(Ordering::SeqCst)
        || full_hashed
            .iter()
            .any(|(_, outcome)| matches!(outcome, HashOutcome::Cancelled))
    {
        logs.warn("scan canceled during hash detection".to_string());
        return Err(AppError::BadInput(
            "scan cancelled during hash detection".into(),
        ));
    }

    // Resolve each successfully hashed miss to its BLAKE3 identity. Files that
    // failed to hash (or weren't reached before cancel) contribute no hash and
    // are simply absent from the report below — they are never treated as
    // duplicates and are never moved or deleted.
    let mut identity_by_idx: HashMap<usize, String> = HashMap::new();
    let mut hash_errors = 0usize;
    let mut permission_errors = 0usize;
    for (idx, outcome) in full_hashed {
        match outcome {
            HashOutcome::Ok(h) => {
                identity_by_idx.insert(idx, h);
            }
            HashOutcome::Cancelled => {
                // Not an error: the user stopped the scan before this file's turn.
            }
            HashOutcome::Failed(e) => {
                counters.errors += 1;
                hash_errors += 1;
                if let Some(fi) = root_index.get(&all_entries[idx].source_root).copied() {
                    per_folder[fi].errors += 1;
                }
                let denied = e.kind() == ErrorKind::PermissionDenied
                    // EPERM ("Operation not permitted") — typically macOS TCC
                    // blocking the app from reading an external/removable volume.
                    || e.raw_os_error() == Some(1);
                if denied {
                    permission_errors += 1;
                }
                // Log the first handful in full; aggregate the rest so a whole
                // unreadable volume doesn't bury the log in identical lines.
                if hash_errors <= MAX_DETAILED_HASH_ERRORS {
                    logs.warn(format!(
                        "hash error on {}: {e}",
                        all_entries[idx].path.display()
                    ));
                } else if hash_errors == MAX_DETAILED_HASH_ERRORS + 1 {
                    logs.warn(
                        "… more hash errors — suppressing per-file lines (summary at end)"
                            .to_string(),
                    );
                }
            }
        }
    }
    if hash_errors > 0 {
        logs.warn(format!(
            "{hash_errors} file(s) could not be hashed and were skipped — they are NOT \
             reported as duplicates and were not moved or deleted."
        ));
        if permission_errors > 0 {
            logs.warn(format!(
                "{permission_errors} of those were permission errors. macOS may be blocking \
                 access to this volume — grant Rusty access under System Settings → Privacy & \
                 Security → Files and Folders (enable the volume) or Full Disk Access, then \
                 rescan to hash the remaining files."
            ));
        }
    }

    progress(ScanProgress {
        phase: "saving".into(),
        processed: total,
        total,
        percent: ((reused_for_progress + processed.load(Ordering::Relaxed)) * 100 / total.max(1))
            as u8,
        hashes_reused: reused_for_progress,
        hashes_computed: computed.load(Ordering::Relaxed),
        folders: total_folders,
    });

    let mut cache_hits = 0usize;
    let mut moved_reused = 0usize;
    let mut out: Vec<ScannedFile> = Vec::new();
    for lookup in lookups {
        let entry = &all_entries[lookup.idx];
        let pf = root_index.get(&entry.source_root).copied();
        let (hash, reused_from_cache, moved_from) = match lookup.hit {
            CacheHit::Primary { hash, .. } => {
                reused.fetch_add(1, Ordering::Relaxed);
                cache_hits += 1;
                if let Some(fi) = pf {
                    per_folder[fi].cache_hits += 1;
                }
                (hash, true, None)
            }
            CacheHit::Filename {
                hash,
                previous_path,
                previous_normalized_path,
                ..
            } => {
                reused.fetch_add(1, Ordering::Relaxed);
                cache_hits += 1;
                moved_reused += 1;
                if let Some(fi) = pf {
                    per_folder[fi].cache_hits += 1;
                }
                logs.info(format!(
                    "moved/renamed match: {} -> {}",
                    previous_path, entry.normalized_path
                ));
                // Move the row instead of leaving the old normalized_path orphaned.
                if previous_normalized_path != entry.normalized_path {
                    let _ = memory.relocate_file(
                        &previous_normalized_path,
                        &entry.path.to_string_lossy(),
                        &entry.normalized_path,
                        &entry.file_name,
                        &entry.source_root,
                        scan_id,
                    );
                }
                (hash, true, Some(previous_path))
            }
            CacheHit::Miss => {
                let Some(h) = identity_by_idx.remove(&lookup.idx) else {
                    // Couldn't be hashed, usually due to an I/O error.
                    // Still record what we can — but without a hash there's no
                    // identity to track. Skip recording for now.
                    continue;
                };
                if let Some(fi) = pf {
                    per_folder[fi].new_hashes_saved += 1;
                }
                // Hash-based move detection. If the bank already knows this hash
                // at a *different* normalized_path AND that path doesn't currently
                // exist, treat this entry as the relocated copy.
                let prior = memory.records_for_hash(&h, 8).unwrap_or_default();
                let stale = prior.into_iter().find(|row| {
                    row.normalized_path != entry.normalized_path
                        && !std::path::Path::new(&row.path).exists()
                });
                if let Some(row) = stale {
                    reused.fetch_add(1, Ordering::Relaxed);
                    moved_reused += 1;
                    logs.info(format!(
                        "rename/move by hash: {} -> {}",
                        row.normalized_path, entry.normalized_path
                    ));
                    let _ = memory.relocate_file(
                        &row.normalized_path,
                        &entry.path.to_string_lossy(),
                        &entry.normalized_path,
                        &entry.file_name,
                        &entry.source_root,
                        scan_id,
                    );
                    (h, true, Some(row.normalized_path))
                } else {
                    (h, false, None)
                }
            }
        };

        memory.upsert_file(
            &hash,
            &entry.path.to_string_lossy(),
            &entry.normalized_path,
            &entry.file_name,
            &entry.source_root,
            entry.size,
            entry.modified_ns,
            scan_id,
        )?;

        out.push(ScannedFile {
            hash,
            path: entry.path.to_string_lossy().into_owned(),
            normalized_path: entry.normalized_path.clone(),
            file_name: entry.file_name.clone(),
            media_kind: entry.media_kind,
            source_root: entry.source_root.clone(),
            size: entry.size,
            modified_ns: entry.modified_ns,
            reused_from_cache,
            moved_from,
            dev: entry.dev,
            ino: entry.ino,
        });
    }

    counters.hashes_reused = reused.load(Ordering::Relaxed);
    counters.hashes_computed = computed.load(Ordering::Relaxed);
    counters.bytes_hashed = bytes_hashed.load(Ordering::Relaxed);
    counters.cache_hits = cache_hits;
    counters.cache_misses = misses.len();
    counters.stale_ignored = stale_ignored;
    counters.moved_reused = moved_reused;
    counters.new_hashes_saved = counters.hashes_computed;
    counters.per_folder = per_folder;

    progress(ScanProgress {
        phase: "done".into(),
        processed: total,
        total,
        percent: 100,
        hashes_reused: counters.hashes_reused,
        hashes_computed: counters.hashes_computed,
        folders: total_folders,
    });

    Ok((out, counters))
}

fn usable_cache_hit(entry: &WalkedEntry, hit: CacheHit) -> CacheHit {
    match hit {
        CacheHit::Primary { hash, source_root } if is_legacy_partial_hash(&hash) => {
            let _ = source_root;
            CacheHit::Miss
        }
        CacheHit::Filename {
            hash,
            source_root,
            previous_path,
            previous_normalized_path,
        } if is_legacy_partial_hash(&hash)
            || (previous_normalized_path != entry.normalized_path
                && Path::new(&previous_path).exists()) =>
        {
            let _ = (hash, source_root, previous_path, previous_normalized_path);
            CacheHit::Miss
        }
        other => other,
    }
}

fn is_legacy_partial_hash(hash: &str) -> bool {
    hash.starts_with("p:")
}

/// Result of trying to hash one cache-miss file.
enum HashOutcome {
    /// Full BLAKE3 hex digest of the file's bytes.
    Ok(String),
    /// The user cancelled before this file was reached — not an error.
    Cancelled,
    /// The file could not be opened or read. Carried back so the caller can
    /// classify (e.g. permission denied) and report it; the file is then
    /// dropped from the results and never treated as a duplicate.
    Failed(std::io::Error),
}

fn hash_file(path: &Path) -> std::io::Result<String> {
    // Read straight into a large buffer; wrapping the File in a BufReader would
    // only copy the same bytes a second time. A read error here propagates to
    // the caller, which records it per file without aborting the whole scan.
    let mut file = File::open(path)?;
    let mut hasher = Hasher::new();
    let mut buf = vec![0u8; READ_BUFFER_BYTES];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Running aggregate for one directory, used to classify folders (empty,
/// photo-only, video-only, mixed, unsupported-only) from the single walk.
#[derive(Default)]
struct DirAgg {
    depth: usize,
    files: u64,
    supported: u64,
    photos: u64,
    videos: u64,
}

struct WalkedEntry {
    path: PathBuf,
    normalized_path: String,
    file_name: String,
    media_kind: MediaKind,
    size: i64,
    modified_ns: i64,
    source_root: String,
    dev: u64,
    ino: u64,
}

pub fn media_kind_for_name(name: &str) -> MediaKind {
    let Some((_, ext)) = name.rsplit_once('.') else {
        return MediaKind::Other;
    };
    match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "tif" | "tiff" | "webp" | "heic" | "heif"
        | "avif" | "dng" | "cr2" | "cr3" | "nef" | "arw" | "orf" | "rw2" | "raf" | "pef"
        | "srw" => MediaKind::Photo,
        "mp4" | "mov" | "m4v" | "avi" | "mkv" | "webm" | "mpg" | "mpeg" | "mts" | "m2ts"
        | "3gp" | "3g2" | "wmv" | "flv" | "ts" | "vob" | "hevc" | "asf" | "mod" | "tod" => {
            MediaKind::Video
        }
        _ => MediaKind::Other,
    }
}

/// `(dev, ino)` uniquely identifies a physical file on unix; two paths sharing
/// it are hardlinks to the same bytes, not independent copies. Zero on other
/// platforms (treated as "unknown / not a hardlink").
#[cfg(unix)]
fn file_identity(m: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (m.dev(), m.ino())
}
#[cfg(not(unix))]
fn file_identity(_m: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

struct LookupResult {
    idx: usize,
    hit: CacheHit,
}
