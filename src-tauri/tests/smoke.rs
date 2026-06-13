//! End-to-end smoke test for the duplicate-finder core.
//!
//! Builds a temp tree with known duplicates, scans it twice, and verifies:
//!   - duplicate groups are detected by hash
//!   - the memory bank persists across "process restarts" (re-opening the DB)
//!   - re-scanning the same folder reuses hashes from cache (no re-hash)
//!   - moving a file to a new path is detected by hash (filename fallback hit)
//!   - dry runs never modify the source tree
//!   - the default plan keeps at least one copy per group
//!   - quarantine actually moves the file, manifest writes, undo restores

use std::fs;
use std::path::PathBuf;
use std::sync::{atomic::AtomicBool, Arc};

use rusty_core::data_dir::DataDir;
use rusty_core::dedupe::{self, PlanAction};
use rusty_core::logs::LogSink;
use rusty_core::memory::MemoryBank;
use rusty_core::paths;
use rusty_core::quarantine;
use rusty_core::scanner;

fn make_tree(root: &PathBuf) {
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b/sub")).unwrap();
    fs::write(root.join("a/photo.jpg"), b"hello world").unwrap();
    fs::write(root.join("b/photo_copy.jpg"), b"hello world").unwrap();
    fs::write(root.join("b/sub/photo_again.jpg"), b"hello world").unwrap();
    fs::write(root.join("a/unique.txt"), b"only one copy of me").unwrap();
    fs::write(root.join("b/big.bin"), vec![7u8; 4096]).unwrap();
    fs::write(root.join("b/sub/big_copy.bin"), vec![7u8; 4096]).unwrap();
}

fn make_data_dir(temp: &PathBuf) -> DataDir {
    DataDir::at(temp.join("appdata")).unwrap()
}

#[test]
fn detects_duplicates_by_hash() {
    let temp = tempfile::tempdir().unwrap();
    let temp_path = temp.path().to_path_buf();
    let src = temp_path.join("src");
    make_tree(&src);
    let data = make_data_dir(&temp_path);
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (files, _counters) =
        scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();
    let report = dedupe::group_duplicates(&files);

    // Two duplicate groups: "hello world" x3 and 4096-byte buffer x2.
    assert_eq!(report.groups.len(), 2, "expected 2 dup groups");
    let three_copies = report
        .groups
        .iter()
        .find(|g| g.copies == 3)
        .expect("3-copy group");
    let two_copies = report
        .groups
        .iter()
        .find(|g| g.copies == 2)
        .expect("2-copy group");
    assert_eq!(three_copies.wasted_bytes as usize, 2 * "hello world".len());
    assert_eq!(two_copies.wasted_bytes, 4096);

    // Sorted by wasted bytes desc: the 2-copy 4 KiB beats the 3-copy 22-byte.
    assert!(report.groups[0].wasted_bytes >= report.groups[1].wasted_bytes);
}

#[test]
fn dry_run_does_not_modify_source() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    make_tree(&src);
    let before: Vec<PathBuf> = walk_files(&src);

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let _ = scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();

    let after: Vec<PathBuf> = walk_files(&src);
    assert_eq!(before, after, "dry run must not change the source tree");
}

#[test]
fn setting_desktop_quarantine_output_does_not_create_folder() {
    let temp = tempfile::tempdir().unwrap();
    let mut data = make_data_dir(&temp.path().to_path_buf());
    let quarantine_out = temp.path().join("Desktop").join("Quarantine");

    data.set_quarantine_out(quarantine_out.clone()).unwrap();

    assert_eq!(data.quarantine_out_dir, quarantine_out);
    assert!(
        !data.quarantine_out_dir.exists(),
        "pointing Rusty at Desktop/Quarantine must not create it until confirmed quarantine"
    );
}

#[test]
fn rescan_reuses_hashes() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    make_tree(&src);
    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));

    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (_, first) = scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();
    assert!(first.hashes_computed > 0, "first scan should hash files");

    let scan_id2 = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (_, second) =
        scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id2).unwrap();
    assert_eq!(
        second.hashes_computed, 0,
        "rescan must not re-hash unchanged files"
    );
    assert!(second.hashes_reused >= first.hashes_computed);
}

#[test]
fn progress_reports_real_reused_and_hashed_counts() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    make_tree(&src);
    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));

    let first_events = std::sync::Mutex::new(Vec::new());
    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (files, first) = scanner::scan_roots_with_progress(
        &[src.clone()],
        &memory,
        &logs,
        &cancel,
        scan_id,
        &scanner::ScanOptions::default(),
        &|p| first_events.lock().unwrap().push(p),
    )
    .unwrap();

    assert_eq!(first.hashes_reused, 0);
    assert_eq!(first.hashes_computed, files.len());
    let first_events = first_events.into_inner().unwrap();
    assert!(
        first_events.iter().any(|p| {
            p.phase == "hashing"
                && p.total == files.len()
                && p.processed > 0
                && p.percent > 0
                && p.hashes_computed > 0
        }),
        "first scan should advance hashing progress from real hashed files"
    );
    assert!(
        first_events.iter().any(|p| {
            p.phase == "done"
                && p.processed == files.len()
                && p.total == files.len()
                && p.percent == 100
        }),
        "scan should finish at 100%"
    );

    let rescan_events = std::sync::Mutex::new(Vec::new());
    let scan_id2 = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (_, second) = scanner::scan_roots_with_progress(
        &[src.clone()],
        &memory,
        &logs,
        &cancel,
        scan_id2,
        &scanner::ScanOptions::default(),
        &|p| rescan_events.lock().unwrap().push(p),
    )
    .unwrap();

    assert_eq!(second.hashes_computed, 0);
    assert_eq!(second.hashes_reused, files.len());
    let rescan_events = rescan_events.into_inner().unwrap();
    assert!(
        rescan_events.iter().any(|p| {
            p.phase == "hashing"
                && p.processed == files.len()
                && p.total == files.len()
                && p.percent == 100
                && p.hashes_reused == files.len()
                && p.hashes_computed == 0
        }),
        "rescan should report reused cached files as already processed"
    );
}

#[test]
fn memory_bank_persists_across_reopens() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    make_tree(&src);
    let data = make_data_dir(&temp.path().to_path_buf());

    {
        let memory = MemoryBank::open(&data.memory_db).unwrap();
        let logs = LogSink::new(data.current_log_path());
        let cancel = Arc::new(AtomicBool::new(false));
        let scan_id = memory
            .start_scan("dry", &[src.to_string_lossy().into()])
            .unwrap();
        let (files, counters) =
            scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();
        let report = dedupe::group_duplicates(&files);
        memory
            .finish_scan(
                scan_id,
                counters.files_walked as i64,
                counters.bytes_walked as i64,
                counters.hashes_reused as i64,
                counters.hashes_computed as i64,
                report.groups.len() as i64,
                report.total_wasted_bytes,
            )
            .unwrap();
    }

    // "Restart" the app by reopening the DB.
    let memory2 = MemoryBank::open(&data.memory_db).unwrap();
    let stats = memory2.stats(&data.memory_db).unwrap();
    assert!(stats.folders >= 1, "folder should be remembered");
    assert!(stats.files >= 5, "files should be remembered");
    assert!(stats.distinct_hashes >= 3, "distinct hashes remembered");
    assert_eq!(
        stats.duplicate_hashes, 2,
        "memory stats should count only hashes with more than one file"
    );
    assert!(stats.last_scan_ts.is_some(), "scan history persists");
}

#[test]
fn detects_moved_file_by_hash() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    let original = src.join("interesting.dat");
    fs::write(&original, b"my unique payload that lives forever").unwrap();

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));

    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (first_files, _) =
        scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();
    assert_eq!(first_files.len(), 1);
    let original_hash = first_files[0].hash.clone();

    // Pretend the user moved + renamed the file. We preserve mtime so the
    // filename-fallback lookup is the path that fires.
    let moved = src.join("moved/renamed.dat");
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::rename(&original, &moved).unwrap();

    let scan_id2 = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (second_files, _counters) =
        scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id2).unwrap();
    assert_eq!(second_files.len(), 1);
    assert_eq!(
        second_files[0].hash, original_hash,
        "hash identity preserved across move"
    );
    assert!(
        second_files[0].moved_from.is_some(),
        "move detection populated; got {:?}",
        second_files[0].moved_from
    );
    // Hashing once is acceptable here (the rename changes both path AND filename,
    // so identity can only be re-established by content). What matters is that
    // the move is recognized and the memory bank doesn't grow a stale row.
    let stats = memory.stats(std::path::Path::new("/tmp")).unwrap();
    assert_eq!(
        stats.distinct_hashes, 1,
        "no duplicate hash rows after move"
    );
}

#[test]
fn pure_move_without_rename_reuses_hash() {
    // Same filename, different parent directory. The filename-fallback lookup
    // should fire and we should NOT re-hash.
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    fs::create_dir_all(src.join("nested")).unwrap();
    let original = src.join("payload.dat");
    fs::write(&original, b"identical body").unwrap();
    let original_mtime = fs::metadata(&original).unwrap().modified().unwrap();

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));

    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let _ = scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();

    // Move into a subdir but keep the filename identical.
    let moved = src.join("nested/payload.dat");
    fs::rename(&original, &moved).unwrap();
    // Restore the mtime so the filename-fallback lookup matches.
    fs::File::open(&moved)
        .unwrap()
        .set_modified(original_mtime)
        .unwrap();

    let scan_id2 = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (_, counters) =
        scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id2).unwrap();
    assert_eq!(
        counters.hashes_computed, 0,
        "pure move with same filename should not re-hash"
    );
}

#[test]
fn default_plan_keeps_one_per_group() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    make_tree(&src);
    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (files, _) = scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();
    let report = dedupe::group_duplicates(&files);
    let plan = dedupe::default_plan(&report);

    use std::collections::HashMap;
    let mut keepers: HashMap<&str, usize> = HashMap::new();
    for entry in &plan {
        if matches!(entry.action, PlanAction::Keep) {
            *keepers.entry(entry.hash.as_str()).or_insert(0) += 1;
        }
    }
    for g in &report.groups {
        assert_eq!(
            keepers.get(g.hash.as_str()).copied().unwrap_or(0),
            1,
            "exactly one keeper per group"
        );
    }
    quarantine::validate_plan(&plan).expect("default plan must validate");
}

#[test]
fn apply_then_undo_roundtrips() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    make_tree(&src);
    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("real", &[src.to_string_lossy().into()])
        .unwrap();
    let (files, _) = scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();
    let report = dedupe::group_duplicates(&files);
    let plan = dedupe::default_plan(&report);

    let before_count = walk_files(&src).len();
    let result = quarantine::apply(&plan, &data, &memory, &logs, &cancel).unwrap();
    assert!(result.quarantined > 0);

    let after_count = walk_files(&src).len();
    assert!(
        after_count < before_count,
        "some files should be moved out (before={before_count}, after={after_count})"
    );

    // Undo should restore everything.
    let undo = quarantine::undo(std::path::Path::new(&result.manifest_path), &logs).unwrap();
    assert_eq!(undo.restored as i64, result.quarantined);
    let restored_count = walk_files(&src).len();
    assert_eq!(restored_count, before_count, "undo restored every file");
}

#[test]
fn quarantine_cancel_leaves_pending_files_unmoved_and_reported() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    make_tree(&src);
    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("real", &[src.to_string_lossy().into()])
        .unwrap();
    let (files, _) = scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();
    let report = dedupe::group_duplicates(&files);
    let plan = dedupe::default_plan(&report);
    let to_quarantine = plan
        .iter()
        .filter(|entry| matches!(entry.action, PlanAction::Quarantine))
        .count() as i64;
    let before_count = walk_files(&src).len();

    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    let result = quarantine::apply(&plan, &data, &memory, &logs, &cancel).unwrap();

    assert!(result.canceled);
    assert_eq!(result.quarantined, 0);
    assert_eq!(result.not_processed, to_quarantine);
    assert_eq!(
        walk_files(&src).len(),
        before_count,
        "cancel must not move files"
    );

    let manifest_bytes = fs::read(&result.manifest_path).unwrap();
    let manifest: quarantine::Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
    let skipped = manifest
        .entries
        .iter()
        .filter(|entry| {
            entry.status == "skipped" && entry.reason.as_deref() == Some("canceled before move")
        })
        .count() as i64;
    assert_eq!(
        skipped, to_quarantine,
        "pending files should be reported as skipped"
    );
}

#[test]
fn min_size_and_exclude_filters_apply() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    fs::create_dir_all(src.join("skipme")).unwrap();
    fs::write(src.join("tiny.txt"), b"x").unwrap(); // 1 byte
    fs::write(src.join("big_a.bin"), vec![1u8; 5000]).unwrap();
    fs::write(src.join("big_b.bin"), vec![1u8; 5000]).unwrap();
    fs::write(src.join("skipme/dup.bin"), vec![1u8; 5000]).unwrap();

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let opts = scanner::ScanOptions {
        min_size: 2,
        skip_dev_dirs: false,
        exclude: vec!["skipme".into()],
        media_only: false,
        follow_symlinks: false,
    };
    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (files, _) =
        scanner::scan_roots_with_options(&[src.clone()], &memory, &logs, &cancel, scan_id, &opts)
            .unwrap();

    assert!(
        files.iter().all(|f| !f.path.contains("tiny.txt")),
        "min_size should drop the 1-byte file"
    );
    assert!(
        files.iter().all(|f| !f.path.contains("skipme")),
        "exclude substring should drop skipme/*"
    );
    let report = dedupe::group_duplicates(&files);
    assert_eq!(
        report.groups.len(),
        1,
        "only the two big files remain as a pair"
    );
    assert_eq!(report.groups[0].copies, 2);
}

#[test]
fn unique_files_are_full_hashed_once_then_reused() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    // Distinct sizes are still full-hashed once; correctness beats a partial
    // identity when users expect a full content-hash database.
    fs::write(src.join("a.bin"), vec![1u8; 4096]).unwrap();
    fs::write(src.join("b.bin"), vec![2u8; 8192]).unwrap();
    fs::write(src.join("c.bin"), vec![3u8; 16384]).unwrap();

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (files, counters) =
        scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();

    assert_eq!(files.len(), 3, "all three files recorded");
    assert_eq!(
        counters.hashes_computed, 3,
        "size-unique files are still full-file hashed on first scan"
    );
    assert!(
        files.iter().all(|f| !f.hash.starts_with("p:")),
        "scan results must not expose partial hashes"
    );
    // Still recognized next time: a rescan reuses their full hashes.
    let scan_id2 = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (_, second) =
        scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id2).unwrap();
    assert_eq!(second.hashes_computed, 0);
    assert!(second.hashes_reused >= 3, "rescan reuses cached identities");
    assert_eq!(dedupe::group_duplicates(&files).groups.len(), 0);
}

#[test]
fn opening_memory_bank_clears_legacy_partial_hash_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let data = make_data_dir(&temp.path().to_path_buf());
    let legacy_path = temp.path().join("legacy/photo.jpg");
    fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
    fs::write(&legacy_path, b"legacy").unwrap();

    {
        let memory = MemoryBank::open(&data.memory_db).unwrap();
        let scan_id = memory.start_scan("dry", &["/legacy".into()]).unwrap();
        memory
            .upsert_file(
                "p:legacy-partial-hash",
                &legacy_path.to_string_lossy(),
                &paths::normalize_for_storage(&legacy_path),
                "photo.jpg",
                "/legacy",
                6,
                1,
                scan_id,
            )
            .unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&data.memory_db).unwrap();
        conn.execute("UPDATE schema_version SET version = 2", [])
            .unwrap();
    }

    let _ = MemoryBank::open(&data.memory_db).unwrap();

    let conn = rusqlite::Connection::open(&data.memory_db).unwrap();
    let partial_files: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE hash LIKE 'p:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let scan_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM scans", [], |r| r.get(0))
        .unwrap();
    let snapshot_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM scan_files", [], |r| r.get(0))
        .unwrap();

    assert_eq!(partial_files, 0);
    assert_eq!(
        scan_rows, 0,
        "legacy partial snapshots cannot be a baseline"
    );
    assert_eq!(snapshot_rows, 0);
}

#[cfg(unix)]
#[test]
fn hardlink_of_keeper_is_kept_not_quarantined() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    let original = src.join("a_original.bin");
    fs::write(&original, vec![9u8; 2048]).unwrap();
    fs::hard_link(&original, src.join("b_hardlink.bin")).unwrap();

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (files, _) = scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();
    let report = dedupe::group_duplicates(&files);
    let plan = dedupe::default_plan(&report);

    assert!(
        plan.iter().all(|e| matches!(e.action, PlanAction::Keep)),
        "hardlinked copies must not be quarantined: {:?}",
        plan.iter().map(|e| (&e.path, e.action)).collect::<Vec<_>>()
    );
}

#[test]
fn quarantine_refuses_on_content_mismatch() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("keep.bin"), vec![1u8; 3000]).unwrap();
    fs::write(src.join("victim.bin"), vec![1u8; 3000]).unwrap();

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("real", &[src.to_string_lossy().into()])
        .unwrap();
    let (files, _) = scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();
    let report = dedupe::group_duplicates(&files);
    let plan = dedupe::default_plan(&report);

    // Corrupt the victim AFTER the scan (same length, different bytes) to
    // simulate a hash collision / on-disk change. Apply must refuse the move.
    let victim = plan
        .iter()
        .find(|e| matches!(e.action, PlanAction::Quarantine))
        .expect("a victim in the plan");
    let victim_path = victim.path.clone();
    fs::write(&victim_path, vec![2u8; 3000]).unwrap();

    let result = quarantine::apply(&plan, &data, &memory, &logs, &cancel).unwrap();
    assert_eq!(
        result.quarantined, 0,
        "mismatched victim must not be quarantined"
    );
    assert!(
        result.failed >= 1,
        "the refused move should count as failed"
    );
    assert!(
        std::path::Path::new(&victim_path).exists(),
        "victim must remain in place"
    );
}

#[test]
fn keeper_rule_oldest_keeps_oldest() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    let old = src.join("old_copy.bin");
    let new = src.join("a.bin"); // shorter path — would win under ShortestPath
    fs::write(&old, vec![5u8; 4096]).unwrap();
    fs::write(&new, vec![5u8; 4096]).unwrap();
    let t_old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    let t_new = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000);
    fs::File::open(&old).unwrap().set_modified(t_old).unwrap();
    fs::File::open(&new).unwrap().set_modified(t_new).unwrap();

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (files, _) = scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();
    let report = dedupe::group_duplicates(&files);

    let plan = dedupe::default_plan_with_rule(&report, dedupe::KeeperRule::Oldest);
    let keeper = plan
        .iter()
        .find(|e| matches!(e.action, PlanAction::Keep))
        .unwrap();
    assert!(
        keeper.path.ends_with("old_copy.bin"),
        "oldest kept, got {}",
        keeper.path
    );

    let plan_short = dedupe::default_plan_with_rule(&report, dedupe::KeeperRule::ShortestPath);
    let keeper_short = plan_short
        .iter()
        .find(|e| matches!(e.action, PlanAction::Keep))
        .unwrap();
    assert!(
        keeper_short.path.ends_with("a.bin"),
        "shortest-path rule keeps a.bin"
    );
}

#[test]
fn duplicate_dirs_detected() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    fs::create_dir_all(src.join("dirA")).unwrap();
    fs::create_dir_all(src.join("dirB")).unwrap();
    fs::write(src.join("dirA/x.txt"), b"alpha").unwrap();
    fs::write(src.join("dirA/y.txt"), b"beta").unwrap();
    fs::write(src.join("dirB/x.txt"), b"alpha").unwrap();
    fs::write(src.join("dirB/y.txt"), b"beta").unwrap();

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (files, _) = scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();
    let report = dedupe::group_duplicates(&files);

    assert_eq!(
        report.duplicate_dirs.len(),
        1,
        "dirA and dirB are identical"
    );
    let dd = &report.duplicate_dirs[0];
    assert_eq!(dd.dirs.len(), 2);
    assert_eq!(dd.file_count, 2);
}

#[test]
fn full_hashes_prevent_partial_fingerprint_false_duplicates() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    fs::create_dir_all(&src).unwrap();

    let mut first = vec![b'a'; 16 * 1024];
    first.extend(vec![b'1'; 12 * 1024]);
    first.extend(vec![b'z'; 16 * 1024]);
    let mut second = vec![b'a'; 16 * 1024];
    second.extend(vec![b'2'; 20 * 1024]);
    second.extend(vec![b'z'; 16 * 1024]);
    fs::write(src.join("first.mp4"), first).unwrap();
    fs::write(src.join("second.mp4"), second).unwrap();

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (files, counters) =
        scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();
    let report = dedupe::group_duplicates(&files);

    assert_eq!(
        counters.hashes_computed, 2,
        "both media files must be full-file hashed before comparison"
    );
    assert_eq!(
        report.groups.len(),
        0,
        "same head/tail bytes with different full content must not be duplicates"
    );
    assert!(
        files.iter().all(|f| !f.hash.starts_with("p:")),
        "scan results must not expose partial hashes as duplicate identities"
    );
}

#[test]
fn media_only_scan_hashes_supported_media_and_skips_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("photo.jpg"), b"photo bytes").unwrap();
    fs::write(src.join("video.mov"), b"video bytes").unwrap();
    fs::write(src.join("notes.txt"), b"not media").unwrap();
    fs::write(src.join("Icon\r"), b"").unwrap();

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let options = scanner::ScanOptions {
        media_only: true,
        ..scanner::ScanOptions::default()
    };
    let (files, counters) = scanner::scan_roots_with_options(
        &[src.clone()],
        &memory,
        &logs,
        &cancel,
        scan_id,
        &options,
    )
    .unwrap();

    assert_eq!(files.len(), 2);
    assert_eq!(counters.hashes_computed, 2);
    assert_eq!(counters.files_skipped, 2);
    assert!(files.iter().any(|f| f.path.ends_with("/photo.jpg")));
    assert!(files.iter().any(|f| f.path.ends_with("/video.mov")));
    assert!(files.iter().all(|f| !f.hash.starts_with("p:")));
}

#[test]
fn filename_cache_hit_is_not_trusted_when_original_path_still_exists() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    fs::create_dir_all(src.join("a")).unwrap();
    fs::create_dir_all(src.join("b")).unwrap();
    let original = src.join("a/photo.jpg");
    let different = src.join("b/photo.jpg");
    fs::write(&original, b"AAAA").unwrap();
    fs::write(&different, b"BBBB").unwrap();

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let modified_ns = fs::metadata(&different)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    let seed_scan = memory.start_scan("dry", &["seed".into()]).unwrap();
    memory
        .upsert_file(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &original.to_string_lossy(),
            &paths::normalize_for_storage(&original),
            "photo.jpg",
            &paths::normalize_for_storage(&src),
            4,
            modified_ns,
            seed_scan,
        )
        .unwrap();

    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (files, counters) =
        scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();
    let report = dedupe::group_duplicates(&files);

    assert!(
        counters.hashes_computed >= 1,
        "same filename/size/mtime at a different existing path must be verified by content"
    );
    assert_eq!(
        report.groups.len(),
        0,
        "different files with same filename/size/mtime must not be reported as duplicates"
    );
}

/// A file that cannot be hashed (e.g. a permission-denied file on an external
/// volume) must never abort the scan or be treated as a duplicate, and valid
/// files around it must still be hashed, grouped, and saved to the cache.
#[cfg(unix)]
#[test]
fn unreadable_file_is_skipped_not_treated_as_duplicate() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    fs::create_dir_all(&src).unwrap();

    // A genuine duplicate pair that must still be detected.
    fs::write(src.join("keep.jpg"), b"identical bytes please").unwrap();
    fs::write(src.join("copy.jpg"), b"identical bytes please").unwrap();
    // A media file we make unreadable so hashing fails for it specifically.
    let locked = src.join("locked.mov");
    fs::write(&locked, b"some footage that cannot be read").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    // If we can still open it (e.g. the test runs as root), the failure path
    // wouldn't trigger — skip rather than report a misleading pass/fail.
    if fs::File::open(&locked).is_ok() {
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o644));
        eprintln!("skipping: process can read 0o000 files (likely root)");
        return;
    }

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();

    // The scan must NOT error just because one file couldn't be hashed.
    let (files, counters) = scanner::scan_roots_with_options(
        &[src.clone()],
        &memory,
        &logs,
        &cancel,
        scan_id,
        &scanner::ScanOptions {
            media_only: true,
            ..Default::default()
        },
    )
    .expect("one unreadable file must not abort the scan");

    // The unreadable file produced an error and is absent from the results.
    assert!(
        counters.errors >= 1,
        "the locked file should count as an error"
    );
    assert!(
        !files.iter().any(|f| f.path.ends_with("locked.mov")),
        "a file that failed to hash must never appear in the results"
    );

    // The real duplicate pair is still detected.
    let report = dedupe::group_duplicates(&files);
    assert_eq!(report.groups.len(), 1, "the readable duplicate pair stands");
    assert_eq!(report.groups[0].copies, 2);

    // Valid hashes were saved even though one file failed.
    let stats = memory.stats(&data.memory_db).unwrap();
    assert!(stats.files >= 2, "valid hashes must be persisted");

    // Restore perms so the tempdir can be cleaned up.
    let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o644));
}

/// The headline bug: scanning a root with nested folders must count the whole
/// recursive tree, not just the one selected root.
#[test]
fn folder_taxonomy_counts_recursive_tree() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    make_tree(&src); // creates a/, b/, b/sub/

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("dry", &[src.to_string_lossy().into()])
        .unwrap();
    let (_files, counters) =
        scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();

    let f = &counters.folders;
    assert_eq!(f.selected_roots, 1, "one root was selected");
    assert_eq!(
        f.total_discovered, 3,
        "a/, b/, and b/sub/ must all be discovered — not just the root"
    );
    assert_eq!(f.top_level, 2, "a/ and b/ are top-level children");
    assert_eq!(f.nested, 1, "b/sub/ is a nested folder");
    assert!(
        f.total_discovered > f.selected_roots,
        "recursive folder count must exceed the selected-root count"
    );
    assert!(
        f.with_supported >= 1,
        "folders holding media are classified"
    );
}

/// Quarantine must be a FLAT folder (no recreated /Volumes or source trees),
/// and duplicate basenames must be renamed `_2`, `_3`, … never overwritten.
#[test]
fn quarantine_is_flat_with_conflict_safe_names() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("src");
    for d in ["a", "b", "c"] {
        let dir = src.join(d);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("dup.jpg"), b"identical content").unwrap();
    }
    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("real", &[src.to_string_lossy().into()])
        .unwrap();
    let (files, _) = scanner::scan_roots(&[src.clone()], &memory, &logs, &cancel, scan_id).unwrap();
    let report = dedupe::group_duplicates(&files);
    let plan = dedupe::default_plan(&report);

    let result = quarantine::apply(&plan, &data, &memory, &logs, &cancel).unwrap();
    assert_eq!(
        result.quarantined, 2,
        "two of three identical copies quarantined"
    );
    assert!(!result.canceled);

    let out = &data.quarantine_out_dir;
    let mut names: Vec<String> = fs::read_dir(out)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert!(
        names.contains(&"dup.jpg".to_string()),
        "first victim keeps its name"
    );
    assert!(
        names.contains(&"dup_2.jpg".to_string()),
        "second victim gets a _2 suffix"
    );
    assert!(
        names.contains(&"Quarantine-Log.csv".to_string()),
        "in-folder log written"
    );
    // Output must be flat — no recreated source folders (a/b/c/Volumes/Users).
    for n in &names {
        assert!(
            !out.join(n).is_dir(),
            "quarantine output must be flat; found dir: {n}"
        );
    }
    // The original paths are preserved in the log, not the folder tree.
    let log = fs::read_to_string(out.join("Quarantine-Log.csv")).unwrap();
    assert!(log.contains("moved"), "log records the moved status");
    assert!(
        log.contains("/src/"),
        "log preserves the original source paths"
    );
}

/// Dropping an external drive (a root with many nested folders) must count
/// every folder inside it — not just the one dropped root.
#[test]
fn external_drive_like_tree_counts_every_nested_folder() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("MyExternalSSD");
    // 5 top-level albums, each with 2 sub-albums, each holding a photo.
    for i in 0..5 {
        for j in 0..2 {
            let leaf = root.join(format!("Album{i}")).join(format!("Sub{j}"));
            fs::create_dir_all(&leaf).unwrap();
            fs::write(leaf.join("p.jpg"), format!("img {i}-{j}")).unwrap();
        }
    }

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan("dry", &[root.to_string_lossy().into()])
        .unwrap();
    let (_files, counters) =
        scanner::scan_roots(&[root.clone()], &memory, &logs, &cancel, scan_id).unwrap();

    let f = &counters.folders;
    assert_eq!(f.selected_roots, 1, "one drive was dropped");
    assert_eq!(f.top_level, 5, "all 5 top-level albums counted");
    assert_eq!(f.nested, 10, "all 10 sub-albums counted");
    assert_eq!(
        f.total_discovered, 15,
        "every folder inside the drive is counted, not just the 1 dropped root"
    );
    assert!(f.total_discovered > 1, "must never report just 1 folder");
}

/// Adding several folders (e.g. an SSD plus Desktop and Downloads) must count
/// each one separately and also produce combined totals.
#[test]
fn multiple_folders_are_counted_separately_with_combined_totals() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("FolderA");
    let b = temp.path().join("FolderB");
    for i in 0..2 {
        let d = a.join(format!("a{i}"));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("x.jpg"), format!("photo a{i}")).unwrap();
    }
    for i in 0..3 {
        let d = b.join(format!("b{i}"));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("v.mov"), format!("video b{i}")).unwrap();
    }

    let data = make_data_dir(&temp.path().to_path_buf());
    let memory = MemoryBank::open(&data.memory_db).unwrap();
    let logs = LogSink::new(data.current_log_path());
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_id = memory
        .start_scan(
            "dry",
            &[a.to_string_lossy().into(), b.to_string_lossy().into()],
        )
        .unwrap();
    let (_files, counters) =
        scanner::scan_roots(&[a.clone(), b.clone()], &memory, &logs, &cancel, scan_id).unwrap();

    // Per-folder breakdown.
    assert_eq!(
        counters.per_folder.len(),
        2,
        "each added folder reported separately"
    );
    let pa = counters
        .per_folder
        .iter()
        .find(|p| p.root_display.ends_with("FolderA"))
        .expect("Folder A stats");
    let pb = counters
        .per_folder
        .iter()
        .find(|p| p.root_display.ends_with("FolderB"))
        .expect("Folder B stats");
    assert_eq!(pa.folders.total_discovered, 2, "Folder A has 2 subfolders");
    assert_eq!(pa.photos, 2);
    assert_eq!(pa.videos, 0);
    assert_eq!(pb.folders.total_discovered, 3, "Folder B has 3 subfolders");
    assert_eq!(pb.videos, 3);
    assert_eq!(pb.photos, 0);

    // Combined totals.
    assert_eq!(counters.folders.selected_roots, 2);
    assert_eq!(
        counters.folders.total_discovered, 5,
        "2 + 3 subfolders combined"
    );
    assert_eq!(counters.photos, 2);
    assert_eq!(counters.videos, 3);
    assert_eq!(counters.files_walked, 5);
}

fn walk_files(root: &PathBuf) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in jwalk::WalkDir::new(root) {
        if let Ok(e) = entry {
            if e.file_type().is_file() {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}
