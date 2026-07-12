//! Tauri commands. Every command is `async fn` so long ops don't block the
//! main thread. State is read via `tauri::State<AppState>`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::dedupe::{self, DedupeReport, KeeperRule, PlanAction, PlanEntry};
use crate::error::AppError;
use crate::logs::LogEntry;
use crate::memory::{FolderRow, MemoryStats, ScanRow};
use crate::quarantine::{self, ApplyResult, UndoResult};
use crate::scanner::{self, ScanCounters};
use crate::state::{AppState, LastResults};

struct ScanGuard(Arc<AtomicBool>);

impl Drop for ScanGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn default_commit_results() -> bool {
    true
}

#[derive(Deserialize)]
pub struct ScanRequest {
    pub roots: Vec<String>,
    pub mode: String, // "dry" | "real"
    #[serde(default)]
    pub min_size: Option<i64>, // skip files smaller than this many bytes
    #[serde(default)]
    pub skip_dev_dirs: Option<bool>, // prune .git/node_modules/app bundles/...
    #[serde(default)]
    pub exclude: Option<Vec<String>>, // case-insensitive path substrings to skip
    #[serde(default)]
    pub media_only: Option<bool>, // default true for the Tauri app: photos/videos only
    /// When false, return the report without replacing `last_results` /
    /// `current_plan`. Used by Folder Compare so a side-by-side scan cannot
    /// silently retarget a pending Apply Plan.
    #[serde(default = "default_commit_results")]
    pub commit_results: bool,
}

/// Stash scan output for later apply/export, or leave the session untouched.
///
/// Compare-tab scans must pass `commit_results = false`; otherwise Apply would
/// move files from the compare roots while the Duplicates UI still showed the
/// previous plan.
fn commit_scan_session(
    commit_results: bool,
    last_results: &Mutex<Option<LastResults>>,
    current_plan: &Mutex<Vec<PlanEntry>>,
    session: LastResults,
    plan: Vec<PlanEntry>,
) {
    if !commit_results {
        return;
    }
    *last_results.lock() = Some(session);
    *current_plan.lock() = plan;
}

#[derive(Serialize, Clone, Debug)]
pub struct ScanResponse {
    pub mode: String,
    pub roots: Vec<String>,
    pub report: DedupeReport,
    pub counters: ScanCounters,
    pub scan_id: i64,
    pub dry_run: bool,
}

#[tauri::command]
pub async fn pick_folders(app: AppHandle) -> Result<Vec<String>, AppError> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog()
        .file()
        .set_title("Choose folders to scan")
        .pick_folders(move |result| {
            let paths: Vec<String> = result
                .map(|paths| {
                    paths
                        .into_iter()
                        .filter_map(|p| p.into_path().ok())
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            let _ = tx.send(paths);
        });
    Ok(rx
        .recv_timeout(std::time::Duration::from_secs(600))
        .unwrap_or_default())
}

#[tauri::command]
pub async fn list_remembered_folders(
    state: State<'_, AppState>,
) -> Result<Vec<FolderRow>, AppError> {
    state.memory.list_folders()
}

#[tauri::command]
pub async fn peek_folder(
    state: State<'_, AppState>,
    path: String,
) -> Result<Option<FolderRow>, AppError> {
    let normalized = crate::paths::normalize_for_storage(std::path::Path::new(&path));
    state.memory.peek_folder(&normalized)
}

#[derive(Deserialize)]
pub struct ForgetRequest {
    pub normalized_path: String,
}

#[tauri::command]
pub async fn forget_folder(
    state: State<'_, AppState>,
    request: ForgetRequest,
) -> Result<(), AppError> {
    state.memory.forget_folder(&request.normalized_path)
}

#[tauri::command]
pub async fn run_scan(
    app: AppHandle,
    state: State<'_, AppState>,
    request: ScanRequest,
) -> Result<ScanResponse, AppError> {
    let mode = match request.mode.as_str() {
        "dry" => "dry",
        "real" => "real",
        _ => return Err(AppError::BadInput("mode must be 'dry' or 'real'".into())),
    };
    if request.roots.is_empty() {
        return Err(AppError::BadInput("at least one folder is required".into()));
    }
    if state.scan_running.swap(true, Ordering::SeqCst) {
        return Err(AppError::ScanAlreadyRunning);
    }
    let _guard = ScanGuard(state.scan_running.clone());
    let dry_run = mode == "dry";
    if dry_run {
        state.logs.dry(format!(
            "=== DRY RUN START — roots: {:?} ===",
            request.roots
        ));
    } else {
        state.logs.real(format!(
            "=== REAL RUN START — roots: {:?} ===",
            request.roots
        ));
    }

    let roots: Vec<PathBuf> = request.roots.iter().map(PathBuf::from).collect();
    for root in &roots {
        let raw = root.to_string_lossy().into_owned();
        let normalized = crate::paths::normalize_for_storage(root);
        state.memory.remember_folder(&raw, &normalized)?;
    }

    let scan_id = state.memory.start_scan(mode, &request.roots)?;

    // Reset the cancellation flag and grab a live Arc for the worker thread.
    state.cancel.store(false, Ordering::SeqCst);
    let cancel_arc = state.cancel.clone();

    // Always exclude the app's own data directory so quarantined files are
    // never re-walked on the next scan and reported as MOVED.
    let mut exclude = request.exclude.clone().unwrap_or_default();
    let data_root = crate::paths::normalize_for_storage(&state.data.root).to_lowercase();
    exclude.push(data_root);

    let options = scanner::ScanOptions {
        min_size: request.min_size.unwrap_or(0).max(0),
        skip_dev_dirs: request.skip_dev_dirs.unwrap_or(true),
        exclude,
        media_only: request.media_only.unwrap_or(true),
        follow_symlinks: false,
    };
    if options.skip_dev_dirs
        || options.min_size > 0
        || !options.exclude.is_empty()
        || options.media_only
    {
        state.logs.info(format!(
            "filters — min_size: {} bytes, skip_dev_dirs: {}, media_only: {}, exclude: {:?}",
            options.min_size, options.skip_dev_dirs, options.media_only, options.exclude
        ));
    }

    // Run the scan on a blocking thread so we don't tie up the async runtime.
    let memory = state.memory.clone();
    let logs = state.logs.clone();
    let roots_clone = roots.clone();
    let app_for_progress = app.clone();
    let progress = move |p: scanner::ScanProgress| {
        let _ = app_for_progress.emit("scan-progress", p);
    };
    let scan_result = tauri::async_runtime::spawn_blocking(move || {
        scanner::scan_roots_with_progress(
            &roots_clone,
            &memory,
            &logs,
            &cancel_arc,
            scan_id,
            &options,
            &progress,
        )
    })
    .await
    .map_err(|e| AppError::BadInput(format!("scan task join error: {e}")))?;

    // If the scan itself failed, close the scan row so scan_changes never
    // picks up a NULL-ended_ts entry and poisons future diffs.
    let (files, counters) = match scan_result {
        Err(e) => {
            let _ = state.memory.finish_scan(scan_id, 0, 0, 0, 0, 0, 0);
            return Err(e);
        }
        Ok(v) => v,
    };
    let report = dedupe::group_duplicates(&files);

    state.memory.finish_scan(
        scan_id,
        counters.files_walked as i64,
        counters.bytes_walked as i64,
        counters.hashes_reused as i64,
        counters.hashes_computed as i64,
        report.groups.len() as i64,
        report.total_wasted_bytes,
    )?;

    state.logs.info(format!(
        "cache — hits: {}, misses: {}, stale ignored: {}, new hashes saved: {}, \
         moved-file matches: {}, hash errors: {}",
        counters.cache_hits,
        counters.cache_misses,
        counters.stale_ignored,
        counters.new_hashes_saved,
        counters.moved_reused,
        counters.errors,
    ));
    state.logs.info(format!(
        "scanned {} folder(s) under {} root(s) — {} files ({} photos, {} videos), \
         {} unsupported ignored",
        counters.folders.total_discovered,
        counters.folders.selected_roots,
        counters.files_walked,
        counters.photos,
        counters.videos,
        counters.unsupported_files,
    ));

    if dry_run {
        state.logs.dry(format!(
            "DRY RUN COMPLETE — {} dup groups, {} wasted bytes. No files changed.",
            report.groups.len(),
            report.total_wasted_bytes
        ));
    } else {
        state.logs.real(format!(
            "SCAN COMPLETE — {} dup groups, {} wasted bytes. Choose actions then apply_plan().",
            report.groups.len(),
            report.total_wasted_bytes
        ));
    }

    // Stash for later "apply" calls — unless this was a non-committing probe
    // (Folder Compare), which must not retarget a pending quarantine plan.
    commit_scan_session(
        request.commit_results,
        &state.last_results,
        &state.current_plan,
        LastResults {
            report: report.clone(),
            counters: counters.clone(),
            roots: roots.clone(),
            mode: mode.to_string(),
            scan_id,
        },
        dedupe::default_plan(&report),
    );

    Ok(ScanResponse {
        mode: mode.to_string(),
        roots: request.roots,
        report,
        counters,
        scan_id,
        dry_run,
    })
}

#[tauri::command]
pub async fn get_last_results(
    state: State<'_, AppState>,
) -> Result<Option<ScanResponse>, AppError> {
    let guard = state.last_results.lock();
    Ok(guard.as_ref().map(|last| ScanResponse {
        mode: last.mode.clone(),
        roots: last
            .roots
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        report: last.report.clone(),
        counters: last.counters.clone(),
        scan_id: last.scan_id,
        dry_run: last.mode == "dry",
    }))
}

#[tauri::command]
pub async fn get_default_plan(state: State<'_, AppState>) -> Result<Vec<PlanEntry>, AppError> {
    Ok(state.current_plan.lock().clone())
}

#[derive(Deserialize)]
pub struct KeeperRuleRequest {
    pub rule: String, // "shortest_path" | "longest_path" | "oldest" | "newest"
}

/// Regenerate the current plan from the last scan, choosing keepers by `rule`.
#[tauri::command]
pub async fn set_keeper_rule(
    state: State<'_, AppState>,
    request: KeeperRuleRequest,
) -> Result<Vec<PlanEntry>, AppError> {
    let rule = match request.rule.as_str() {
        "shortest_path" => KeeperRule::ShortestPath,
        "longest_path" => KeeperRule::LongestPath,
        "oldest" => KeeperRule::Oldest,
        "newest" => KeeperRule::Newest,
        other => return Err(AppError::BadInput(format!("unknown keeper rule {other}"))),
    };
    let last = state
        .last_results
        .lock()
        .clone()
        .ok_or(AppError::NoPendingPlan)?;
    let plan = dedupe::default_plan_with_rule(&last.report, rule);
    quarantine::validate_plan(&plan)?;
    *state.current_plan.lock() = plan.clone();
    Ok(plan)
}

#[derive(Deserialize)]
pub struct PlanActionUpdate {
    pub normalized_path: String,
    pub action: String, // "keep" | "quarantine"
}

#[tauri::command]
pub async fn set_plan_action(
    state: State<'_, AppState>,
    update: PlanActionUpdate,
) -> Result<Vec<PlanEntry>, AppError> {
    let action = match update.action.as_str() {
        "keep" => PlanAction::Keep,
        "quarantine" => PlanAction::Quarantine,
        _ => {
            return Err(AppError::BadInput(
                "action must be 'keep' or 'quarantine'".into(),
            ))
        }
    };
    let mut plan = state.current_plan.lock();
    // Mutate a clone first; only swap into the shared Mutex after validation
    // succeeds so an invalid action (e.g. removing the last keeper) never
    // leaves the stored plan in a broken state.
    let mut new_plan = plan.clone();
    let mut matched = false;
    for entry in new_plan.iter_mut() {
        if entry.normalized_path == update.normalized_path {
            entry.action = action;
            entry.reason = match action {
                PlanAction::Keep => "user override: keep".into(),
                PlanAction::Quarantine => "user override: quarantine".into(),
            };
            matched = true;
            break;
        }
    }
    if !matched {
        return Err(AppError::BadInput(format!(
            "no plan entry for {}",
            update.normalized_path
        )));
    }
    quarantine::validate_plan(&new_plan)?;
    *plan = new_plan;
    Ok(plan.clone())
}

#[derive(Deserialize)]
pub struct ApplyRequest {
    pub confirmed: bool,
}

#[tauri::command]
pub async fn apply_plan(
    state: State<'_, AppState>,
    request: ApplyRequest,
) -> Result<ApplyResult, AppError> {
    if !request.confirmed {
        return Err(AppError::NotConfirmed);
    }
    let plan = state.current_plan.lock().clone();
    if plan.is_empty() {
        return Err(AppError::NoPendingPlan);
    }
    quarantine::validate_plan(&plan)?;
    state.logs.real("=== APPLY PLAN: user confirmed ===");

    // Fresh cancel flag so "Cancel Remaining" only affects this apply, and run
    // the moves off the async runtime so the UI (and cancel) stay responsive.
    state.cancel.store(false, Ordering::SeqCst);
    let plan_for_task = plan.clone();
    let data = state.data.clone();
    let memory = state.memory.clone();
    let logs = state.logs.clone();
    let cancel = state.cancel.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        quarantine::apply(&plan_for_task, &data, &memory, &logs, &cancel)
    })
    .await
    .map_err(|e| AppError::BadInput(format!("apply task join error: {e}")))??;

    state.logs.real(format!(
        "Applied: quarantined {} files, freed {} bytes, kept {} per-group keepers{}",
        result.quarantined,
        result.bytes_freed,
        result.kept_per_group,
        if result.canceled {
            format!(" (canceled — {} left untouched)", result.not_processed)
        } else {
            String::new()
        }
    ));
    // Drop the now-stale plan; another scan needs to happen first.
    state.current_plan.lock().clear();
    Ok(result)
}

#[derive(Deserialize)]
pub struct UndoRequest {
    pub manifest_path: String,
}

#[tauri::command]
pub async fn undo_run(
    state: State<'_, AppState>,
    request: UndoRequest,
) -> Result<UndoResult, AppError> {
    let manifest = crate::paths::sanitize(&PathBuf::from(&request.manifest_path));
    if !manifest.starts_with(&state.data.manifests_dir) {
        return Err(AppError::BadInput(
            "manifest path is outside the app data dir".into(),
        ));
    }
    state
        .logs
        .real(format!("=== UNDO RUN: {} ===", request.manifest_path));
    quarantine::undo(&manifest, &state.logs)
}

#[tauri::command]
pub async fn recent_scans(state: State<'_, AppState>) -> Result<Vec<ScanRow>, AppError> {
    state.memory.recent_scans(50)
}

#[tauri::command]
pub async fn memory_stats(state: State<'_, AppState>) -> Result<MemoryStats, AppError> {
    state.memory.stats(&state.data.memory_db)
}

#[derive(Serialize)]
pub struct LogTail {
    pub entries: Vec<LogEntry>,
    pub total: usize,
}

#[tauri::command]
pub async fn get_logs(state: State<'_, AppState>, since: usize) -> Result<LogTail, AppError> {
    let (entries, total) = state.logs.tail(since);
    Ok(LogTail { entries, total })
}

#[tauri::command]
pub async fn clear_logs(state: State<'_, AppState>) -> Result<(), AppError> {
    state.logs.clear();
    Ok(())
}

#[derive(Deserialize)]
pub struct ExportRequest {
    pub format: String, // "csv" | "json"
}

#[derive(Serialize)]
pub struct ExportResult {
    pub path: String,
    pub format: String,
    pub bytes_written: u64,
}

#[tauri::command]
pub async fn export_report(
    state: State<'_, AppState>,
    request: ExportRequest,
) -> Result<ExportResult, AppError> {
    let last = state
        .last_results
        .lock()
        .clone()
        .ok_or(AppError::NoPendingPlan)?;
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    match request.format.as_str() {
        "json" => {
            let path = state
                .data
                .exports_dir
                .join(format!("Rusty-Report-{ts}.json"));
            let bytes = serde_json::to_vec_pretty(&serde_json::json!({
                "mode": last.mode,
                "roots": last.roots.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
                "scan_id": last.scan_id,
                "report": last.report,
            }))?;
            std::fs::write(&path, &bytes)?;
            state
                .logs
                .info(format!("exported JSON: {}", path.display()));
            Ok(ExportResult {
                path: path.to_string_lossy().into_owned(),
                format: "json".into(),
                bytes_written: bytes.len() as u64,
            })
        }
        "csv" => {
            let path = state.data.exports_dir.join(format!("Rusty-Scan-{ts}"));
            std::fs::create_dir_all(&path)?;
            let report_path = path.join("Rusty-Report.csv");
            let mut out = String::from(
                "hash,media_kind,group_size_bytes,copies,wasted_bytes,path,normalized_path,source_root\n",
            );
            for group in &last.report.groups {
                for file in &group.files {
                    out.push_str(&format!(
                        "{},{},{},{},{},{},{},{}\n",
                        group.hash,
                        group.media_kind.as_str(),
                        group.size,
                        group.copies,
                        group.wasted_bytes,
                        csv_escape(&file.path),
                        csv_escape(&file.normalized_path),
                        csv_escape(&file.source_root)
                    ));
                }
            }
            std::fs::write(&report_path, out.as_bytes())?;

            let bytes_written = out.len();
            state.logs.info(format!("exported CSV: {}", path.display()));
            Ok(ExportResult {
                path: path.to_string_lossy().into_owned(),
                format: "csv".into(),
                bytes_written: bytes_written as u64,
            })
        }
        other => Err(AppError::BadInput(format!("unknown export format {other}"))),
    }
}

#[derive(Serialize)]
pub struct DataPaths {
    pub data_root: String,
    pub memory_db: String,
    pub logs_dir: String,
    pub exports_dir: String,
    pub quarantine_dir: String,
    pub manifests_dir: String,
}

#[tauri::command]
pub async fn data_paths(state: State<'_, AppState>) -> Result<DataPaths, AppError> {
    Ok(DataPaths {
        data_root: state.data.root.to_string_lossy().into_owned(),
        memory_db: state.data.memory_db.to_string_lossy().into_owned(),
        logs_dir: state.data.logs_dir.to_string_lossy().into_owned(),
        exports_dir: state.data.exports_dir.to_string_lossy().into_owned(),
        quarantine_dir: state.data.quarantine_dir.to_string_lossy().into_owned(),
        manifests_dir: state.data.manifests_dir.to_string_lossy().into_owned(),
    })
}

/// Signal an in-progress scan to stop. Safe to call at any time.
#[tauri::command]
pub async fn cancel_scan(state: State<'_, AppState>) -> Result<(), AppError> {
    state.cancel.store(true, Ordering::SeqCst);
    Ok(())
}

#[tauri::command]
pub async fn reveal_path(app: AppHandle, path: String) -> Result<(), AppError> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .reveal_item_in_dir(path)
        .map_err(|e| AppError::BadInput(format!("reveal failed: {e}")))?;
    Ok(())
}

#[tauri::command]
pub async fn get_app_info() -> Result<crate::appinfo::AppInfo, AppError> {
    Ok(crate::appinfo::AppInfo::current())
}

#[tauri::command]
pub async fn check_for_updates() -> Result<crate::updates::UpdateResult, AppError> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let result = tauri::async_runtime::spawn_blocking(move || {
        crate::updates::check_for_updates(&current)
    })
    .await
    .map_err(|e| AppError::BadInput(format!("update check join error: {e}")))?;
    Ok(result)
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedupe::PlanAction;

    fn sample_plan(path: &str) -> Vec<PlanEntry> {
        vec![PlanEntry {
            hash: "abc".into(),
            path: path.into(),
            normalized_path: path.to_lowercase(),
            size: 10,
            action: PlanAction::Quarantine,
            reason: "test".into(),
        }]
    }

    fn sample_session(scan_id: i64, root: &str) -> LastResults {
        LastResults {
            report: DedupeReport {
                groups: vec![],
                total_files: 0,
                total_duplicate_files: 0,
                total_wasted_bytes: 0,
                largest_single_file_in_dup: 0,
                duplicate_dirs: vec![],
            },
            counters: ScanCounters::default(),
            roots: vec![PathBuf::from(root)],
            mode: "real".into(),
            scan_id,
        }
    }

    #[test]
    fn scan_request_defaults_commit_results_true() {
        let req: ScanRequest =
            serde_json::from_str(r#"{"roots":["/photos"],"mode":"real"}"#).unwrap();
        assert!(req.commit_results);
    }

    #[test]
    fn scan_request_accepts_commit_results_false() {
        let req: ScanRequest = serde_json::from_str(
            r#"{"roots":["/a","/b"],"mode":"dry","commit_results":false}"#,
        )
        .unwrap();
        assert!(!req.commit_results);
    }

    #[test]
    fn compare_scan_must_not_replace_pending_apply_plan() {
        let last_results = Mutex::new(Some(sample_session(1, "/photos")));
        let current_plan = Mutex::new(sample_plan("/photos/dup.jpg"));

        commit_scan_session(
            false,
            &last_results,
            &current_plan,
            sample_session(2, "/downloads"),
            sample_plan("/downloads/other.jpg"),
        );

        let kept = last_results.lock().clone().expect("session kept");
        assert_eq!(kept.scan_id, 1);
        assert_eq!(kept.roots, vec![PathBuf::from("/photos")]);
        assert_eq!(current_plan.lock().len(), 1);
        assert_eq!(current_plan.lock()[0].path, "/photos/dup.jpg");
    }

    #[test]
    fn normal_scan_replaces_pending_apply_plan() {
        let last_results = Mutex::new(Some(sample_session(1, "/photos")));
        let current_plan = Mutex::new(sample_plan("/photos/dup.jpg"));

        commit_scan_session(
            true,
            &last_results,
            &current_plan,
            sample_session(2, "/downloads"),
            sample_plan("/downloads/other.jpg"),
        );

        let kept = last_results.lock().clone().expect("session replaced");
        assert_eq!(kept.scan_id, 2);
        assert_eq!(kept.roots, vec![PathBuf::from("/downloads")]);
        assert_eq!(current_plan.lock()[0].path, "/downloads/other.jpg");
    }
}
