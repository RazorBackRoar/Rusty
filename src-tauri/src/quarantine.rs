//! Safe move-to-quarantine.
//!
//! "Destructive" action in Rusty means: move a duplicate file into the
//! quarantine output folder (the app points this at `~/Desktop/Quarantine`) as
//! a flat list with conflict-safe names, and write a manifest entry recording
//! the original path. Original `/Volumes/...` trees are NOT recreated inside the
//! quarantine folder — the mapping lives in the manifest and the in-folder log.
//! Permanent deletion is opt-in and only ever runs on the manifest, never on the
//! user's source tree directly.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::data_dir::DataDir;
use crate::dedupe::{PlanAction, PlanEntry};
use crate::error::{AppError, AppResult};
use crate::logs::LogSink;
use crate::memory::MemoryBank;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub hash: String,
    pub original_path: String,
    /// Final path inside the quarantine folder. Empty when the file was not
    /// actually moved (status != "moved").
    pub quarantine_path: String,
    pub size: i64,
    pub moved_ts: String,
    /// "moved" | "skipped" | "failed".
    #[serde(default)]
    pub status: String,
    /// Why a file was skipped or failed (None when moved).
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub run_id: String,
    pub started_ts: String,
    pub mode: String,
    pub entries: Vec<ManifestEntry>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApplyResult {
    pub run_id: String,
    pub manifest_path: String,
    pub quarantined: i64,
    pub failed: i64,
    pub bytes_freed: i64,
    pub kept_per_group: i64,
    /// True when the user pressed "Cancel Remaining" mid-batch: already-moved
    /// files stayed moved, the rest were left untouched.
    pub canceled: bool,
    /// Quarantine victims that were not processed because of cancellation.
    pub not_processed: i64,
    /// The folder files were moved into (e.g. ~/Desktop/Quarantine).
    pub output_dir: String,
}

/// Validate a plan before touching anything. Catches:
///   - groups where every copy is marked Quarantine (would orphan the content)
///   - paths that don't match anything in the plan
///   - plans where the destination would collide with itself
pub fn validate_plan(plan: &[PlanEntry]) -> AppResult<()> {
    use std::collections::HashMap;
    let mut per_hash: HashMap<&str, (usize, usize)> = HashMap::new();
    for entry in plan {
        let counts = per_hash.entry(entry.hash.as_str()).or_insert((0, 0));
        match entry.action {
            PlanAction::Keep => counts.0 += 1,
            PlanAction::Quarantine => counts.1 += 1,
        }
    }
    for (hash, (keepers, victims)) in per_hash {
        if keepers == 0 && victims > 0 {
            return Err(AppError::WouldDeleteUniqueCopy(hash.to_string()));
        }
    }
    Ok(())
}

/// Build a human-readable preview without touching the filesystem.
pub fn preview(plan: &[PlanEntry]) -> Vec<String> {
    let mut out = Vec::with_capacity(plan.len());
    for entry in plan {
        let tag = match entry.action {
            PlanAction::Keep => "KEEP    ",
            PlanAction::Quarantine => "QUARANT ",
        };
        out.push(format!(
            "{tag} {:>12} bytes  {}  ({})",
            entry.size, entry.path, entry.reason
        ));
    }
    out
}

/// Move every Quarantine entry into the quarantine output folder
/// (`data.quarantine_out_dir`, e.g. `~/Desktop/Quarantine`) as a **flat** list
/// with conflict-safe names. Original paths are recorded in the manifest and an
/// in-folder CSV log — the source directory tree is never recreated.
///
/// `cancel` lets the UI stop the *remaining* moves ("Cancel Remaining"); files
/// already moved stay moved and are never undone here.
pub fn apply(
    plan: &[PlanEntry],
    data: &DataDir,
    memory: &MemoryBank,
    logs: &LogSink,
    cancel: &Arc<AtomicBool>,
) -> AppResult<ApplyResult> {
    validate_plan(plan)?;
    let run_id = format!(
        "{}-{}",
        Utc::now().format("%Y%m%d-%H%M%S"),
        Uuid::new_v4().simple()
    );
    let out_dir = &data.quarantine_out_dir;
    fs::create_dir_all(out_dir)?;
    let manifest_path = data.manifest_for_run(&run_id);
    let mut manifest = Manifest {
        run_id: run_id.clone(),
        started_ts: Utc::now().to_rfc3339(),
        mode: "quarantine".into(),
        entries: Vec::new(),
    };
    write_manifest_atomic(&manifest_path, &manifest)?;

    // A keeper path for each hash, so a victim can be byte-verified against the
    // copy that will survive before we move it.
    use std::collections::HashMap;
    let keeper_by_hash: HashMap<&str, &str> = plan
        .iter()
        .filter(|e| matches!(e.action, PlanAction::Keep))
        .map(|e| (e.hash.as_str(), e.path.as_str()))
        .collect();

    // Names already used inside the quarantine folder this run, so duplicate
    // basenames (very common — that's why they're duplicates!) never collide.
    let mut used_names: HashSet<String> = HashSet::new();

    let mut quarantined = 0i64;
    let mut failed = 0i64;
    let mut bytes_freed = 0i64;
    let mut kept = 0i64;
    let mut canceled = false;
    let mut not_processed = 0i64;

    for entry in plan {
        match entry.action {
            PlanAction::Keep => {
                kept += 1;
                logs.real(format!("KEEP {}", entry.path));
            }
            PlanAction::Quarantine => {
                // Stop remaining moves if the user pressed Cancel. Anything
                // already moved is left in place — never undone.
                if cancel.load(Ordering::SeqCst) {
                    canceled = true;
                    not_processed += 1;
                    logs.warn(format!("quarantine canceled before moving {}", entry.path));
                    push_manifest_entry(
                        &mut manifest,
                        &manifest_path,
                        "skipped",
                        entry,
                        "",
                        Some("canceled before move".into()),
                    )?;
                    continue;
                }
                let source = Path::new(&entry.path);
                if !source.exists() {
                    failed += 1;
                    logs.warn(format!("source vanished (skipped): {}", entry.path));
                    push_manifest_entry(
                        &mut manifest,
                        &manifest_path,
                        "skipped",
                        entry,
                        "",
                        Some("source file no longer exists".into()),
                    )?;
                    continue;
                }
                // Last-line safety: confirm this victim is byte-identical to the
                // copy we're keeping. Guards against a hash collision or the file
                // changing on disk since the scan. If we can't verify, we refuse
                // to move rather than risk losing unique content.
                if let Some(keeper_path) = keeper_by_hash.get(entry.hash.as_str()) {
                    match files_equal(Path::new(keeper_path), source) {
                        Ok(true) => {}
                        Ok(false) => {
                            failed += 1;
                            logs.error(format!(
                                "content mismatch — refusing to quarantine {} (differs from keeper {})",
                                entry.path, keeper_path
                            ));
                            push_manifest_entry(
                                &mut manifest,
                                &manifest_path,
                                "failed",
                                entry,
                                "",
                                Some(format!("content differs from keeper {keeper_path}")),
                            )?;
                            continue;
                        }
                        Err(e) => {
                            failed += 1;
                            logs.error(format!(
                                "could not verify {} against keeper {}: {e}",
                                entry.path, keeper_path
                            ));
                            push_manifest_entry(
                                &mut manifest,
                                &manifest_path,
                                "failed",
                                entry,
                                "",
                                Some(format!("verify error: {e}")),
                            )?;
                            continue;
                        }
                    }
                }
                let dest = conflict_safe_dest(out_dir, source, &mut used_names);
                if let Err(e) = move_with_fallback(source, &dest) {
                    failed += 1;
                    logs.error(format!(
                        "move failed {} -> {}: {e}",
                        source.display(),
                        dest.display()
                    ));
                    push_manifest_entry(
                        &mut manifest,
                        &manifest_path,
                        "failed",
                        entry,
                        "",
                        Some(format!("move error: {e}")),
                    )?;
                    continue;
                }
                quarantined += 1;
                bytes_freed += entry.size;
                logs.real(format!("QUARANTINED {} -> {}", entry.path, dest.display()));
                if let Err(e) = push_manifest_entry(
                    &mut manifest,
                    &manifest_path,
                    "moved",
                    entry,
                    &dest.to_string_lossy(),
                    None,
                ) {
                    if let Err(rollback) = move_with_fallback(&dest, source) {
                        logs.error(format!(
                            "manifest write failed after move and rollback failed: {rollback}"
                        ));
                    } else {
                        logs.warn(format!(
                            "manifest write failed; reverted move of {}",
                            entry.path
                        ));
                        quarantined -= 1;
                        bytes_freed -= entry.size;
                    }
                    return Err(e);
                }
                let _ = memory.delete_file_by_path(&entry.normalized_path);
            }
        }
    }

    logs.real(format!("manifest written: {}", manifest_path.display()));
    // Human-readable log alongside the quarantined files (original → quarantine
    // mapping), so the source is recoverable without recreating folder trees.
    if let Err(e) = append_quarantine_log(out_dir, &manifest) {
        logs.warn(format!("could not write quarantine log: {e}"));
    }
    if canceled {
        logs.real(format!(
            "CANCELED REMAINING — {quarantined} moved, {not_processed} left untouched (not undone)"
        ));
    }

    Ok(ApplyResult {
        run_id,
        manifest_path: manifest_path.to_string_lossy().into_owned(),
        quarantined,
        failed,
        bytes_freed,
        kept_per_group: kept,
        canceled,
        not_processed,
        output_dir: out_dir.to_string_lossy().into_owned(),
    })
}

/// Restore every entry from a manifest back to its original path. Used by the
/// UI's "Undo" button.
pub fn undo(manifest_path: &Path, logs: &LogSink) -> AppResult<UndoResult> {
    let bytes = fs::read(manifest_path)?;
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    let mut restored = 0i64;
    let mut failed = 0i64;
    for e in &manifest.entries {
        // Only files we actually moved can be restored; skip skipped/failed rows.
        if (!e.status.is_empty() && e.status != "moved") || e.quarantine_path.is_empty() {
            continue;
        }
        let q = Path::new(&e.quarantine_path);
        let orig = Path::new(&e.original_path);
        if let Some(parent) = orig.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if !q.exists() {
            failed += 1;
            logs.warn(format!("missing quarantine source: {}", q.display()));
            continue;
        }
        match move_with_fallback(q, orig) {
            Ok(()) => {
                restored += 1;
                logs.real(format!("RESTORED {} -> {}", q.display(), orig.display()));
            }
            Err(err) => {
                failed += 1;
                logs.error(format!("restore failed: {err}"));
            }
        }
    }
    Ok(UndoResult {
        run_id: manifest.run_id,
        restored,
        failed,
    })
}

#[derive(Debug, Serialize)]
pub struct UndoResult {
    pub run_id: String,
    pub restored: i64,
    pub failed: i64,
}

/// Pick a flat, collision-free destination inside `out_dir` for `source`:
/// keep the original file name, and on a clash (existing file on disk or a name
/// already used this run) append `_2`, `_3`, … before the extension. Never
/// overwrites. Handles spaces, unicode, punctuation, and extensionless names.
fn conflict_safe_dest(out_dir: &Path, source: &Path, used: &mut HashSet<String>) -> PathBuf {
    let file_name = source
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let (stem, ext) = split_stem_ext(&file_name);

    let mut candidate = file_name.clone();
    let mut n = 2;
    while used.contains(&candidate) || out_dir.join(&candidate).exists() {
        candidate = match &ext {
            Some(e) => format!("{stem}_{n}.{e}"),
            None => format!("{stem}_{n}"),
        };
        n += 1;
    }
    used.insert(candidate.clone());
    out_dir.join(candidate)
}

/// Split "photo.jpg" → ("photo", Some("jpg")); "Makefile" → ("Makefile", None);
/// ".gitignore" → (".gitignore", None) (leading dot is not an extension).
fn split_stem_ext(name: &str) -> (String, Option<String>) {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => {
            (name[..i].to_string(), Some(name[i + 1..].to_string()))
        }
        _ => (name.to_string(), None),
    }
}

/// Append the run's entries to `Quarantine-Log.csv` in the output folder, so the
/// original → quarantine mapping (and any skips/failures) is readable in place.
fn append_quarantine_log(out_dir: &Path, manifest: &Manifest) -> std::io::Result<()> {
    use std::io::Write;
    let path = out_dir.join("Quarantine-Log.csv");
    let new = !path.exists();
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if new {
        writeln!(
            f,
            "timestamp,status,original_path,quarantine_path,size_bytes,hash,reason"
        )?;
    }
    for e in &manifest.entries {
        writeln!(
            f,
            "{},{},{},{},{},{},{}",
            csv_field(&e.moved_ts),
            csv_field(&e.status),
            csv_field(&e.original_path),
            csv_field(&e.quarantine_path),
            e.size,
            csv_field(&e.hash),
            csv_field(e.reason.as_deref().unwrap_or("")),
        )?;
    }
    Ok(())
}

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Stream two files and compare their bytes. Returns `Ok(false)` on any length
/// or content difference. Used as a pre-quarantine safety check.
fn files_equal(a: &Path, b: &Path) -> std::io::Result<bool> {
    if fs::metadata(a)?.len() != fs::metadata(b)?.len() {
        return Ok(false);
    }
    let mut ra = std::io::BufReader::new(fs::File::open(a)?);
    let mut rb = std::io::BufReader::new(fs::File::open(b)?);
    let mut ba = [0u8; 64 * 1024];
    let mut bb = [0u8; 64 * 1024];
    loop {
        let na = fill(&mut ra, &mut ba)?;
        let nb = fill(&mut rb, &mut bb)?;
        if na != nb {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
        if ba[..na] != bb[..nb] {
            return Ok(false);
        }
    }
}

/// Read until `buf` is full or EOF, so two readers compare on equal boundaries.
fn fill(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = r.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

fn push_manifest_entry(
    manifest: &mut Manifest,
    manifest_path: &Path,
    status: &str,
    entry: &PlanEntry,
    qpath: &str,
    reason: Option<String>,
) -> AppResult<()> {
    manifest.entries.push(ManifestEntry {
        hash: entry.hash.clone(),
        original_path: entry.path.clone(),
        quarantine_path: qpath.to_string(),
        size: entry.size,
        moved_ts: Utc::now().to_rfc3339(),
        status: status.to_string(),
        reason,
    });
    write_manifest_atomic(manifest_path, manifest)
}

/// Persist the manifest atomically so a crash or late I/O failure never leaves
/// moved files without an undo mapping on disk.
fn write_manifest_atomic(path: &Path, manifest: &Manifest) -> AppResult<()> {
    let tmp = path.with_extension("json.tmp");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&tmp, serde_json::to_vec_pretty(manifest)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn move_with_fallback(src: &Path, dst: &Path) -> std::io::Result<()> {
    // Try rename first (fast, atomic within a volume). Fall back to copy+delete
    // only on EXDEV (cross-device) — all other rename errors are returned as-is
    // to avoid creating ghost copies in the quarantine dir.
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) => {
            // EXDEV = 18 on Linux and macOS.
            #[cfg(unix)]
            let is_cross_device = e.raw_os_error() == Some(18);
            #[cfg(not(unix))]
            let is_cross_device = false;
            if is_cross_device {
                if let Err(e) = fs::copy(src, dst) {
                    let _ = fs::remove_file(dst);
                    return Err(e);
                }
                fs::remove_file(src)?;
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedupe::{PlanAction, PlanEntry};

    #[test]
    fn write_manifest_atomic_roundtrips_without_tmp_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("20260101-test.json");
        let manifest = Manifest {
            run_id: "20260101-test".into(),
            started_ts: "2026-01-01T00:00:00Z".into(),
            mode: "quarantine".into(),
            entries: vec![ManifestEntry {
                hash: "abc".into(),
                original_path: "/tmp/a.jpg".into(),
                quarantine_path: "/tmp/Quarantine/a.jpg".into(),
                size: 10,
                moved_ts: "2026-01-01T00:00:01Z".into(),
                status: "moved".into(),
                reason: None,
            }],
        };

        write_manifest_atomic(&path, &manifest).unwrap();

        assert!(path.exists());
        assert!(
            !path.with_extension("json.tmp").exists(),
            "atomic write must not leave a temp manifest behind"
        );
        let loaded: Manifest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].status, "moved");
    }

    #[test]
    fn push_manifest_entry_appends_and_persists_incrementally() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.json");
        let mut manifest = Manifest {
            run_id: "run".into(),
            started_ts: "2026-01-01T00:00:00Z".into(),
            mode: "quarantine".into(),
            entries: Vec::new(),
        };
        write_manifest_atomic(&path, &manifest).unwrap();

        let entry = PlanEntry {
            path: "/tmp/victim.jpg".into(),
            normalized_path: "/tmp/victim.jpg".into(),
            hash: "deadbeef".into(),
            size: 42,
            action: PlanAction::Quarantine,
            reason: "duplicate".into(),
        };
        push_manifest_entry(&mut manifest, &path, "moved", &entry, "/tmp/q/victim.jpg", None)
            .unwrap();

        let loaded: Manifest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].quarantine_path, "/tmp/q/victim.jpg");
    }

    #[test]
    fn push_manifest_entry_persists_each_append_for_partial_undo() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.json");
        let mut manifest = Manifest {
            run_id: "run".into(),
            started_ts: "2026-01-01T00:00:00Z".into(),
            mode: "quarantine".into(),
            entries: Vec::new(),
        };
        write_manifest_atomic(&path, &manifest).unwrap();

        let first = PlanEntry {
            path: "/tmp/a.jpg".into(),
            normalized_path: "/tmp/a.jpg".into(),
            hash: "aaa".into(),
            size: 10,
            action: PlanAction::Quarantine,
            reason: "duplicate".into(),
        };
        push_manifest_entry(&mut manifest, &path, "moved", &first, "/tmp/q/a.jpg", None)
            .unwrap();

        let after_first: Manifest =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(after_first.entries.len(), 1);
        assert_eq!(after_first.entries[0].original_path, "/tmp/a.jpg");

        let second = PlanEntry {
            path: "/tmp/b.jpg".into(),
            normalized_path: "/tmp/b.jpg".into(),
            hash: "bbb".into(),
            size: 20,
            action: PlanAction::Quarantine,
            reason: "duplicate".into(),
        };
        push_manifest_entry(&mut manifest, &path, "moved", &second, "/tmp/q/b.jpg", None)
            .unwrap();

        let after_second: Manifest =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(after_second.entries.len(), 2);
        assert_eq!(
            after_second.entries[1].quarantine_path,
            "/tmp/q/b.jpg",
            "each move must be on disk before the next append"
        );
    }
}
