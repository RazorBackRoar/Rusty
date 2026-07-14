//! Shared application state passed to every Tauri command.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use parking_lot::Mutex;
use tauri::Manager;

use crate::data_dir::DataDir;
use crate::dedupe::{DedupeReport, PlanEntry};
use crate::logs::LogSink;
use crate::memory::MemoryBank;

use crate::scanner::ScanCounters;

pub struct AppState {
    pub data: DataDir,
    pub memory: MemoryBank,
    pub logs: LogSink,
    pub scan_running: Arc<AtomicBool>,
    /// Set to true by `cancel_scan` to interrupt an in-progress scan.
    pub cancel: Arc<AtomicBool>,
    /// True while `apply_plan` is moving files to quarantine.
    pub apply_running: Arc<AtomicBool>,
    pub last_results: Mutex<Option<LastResults>>,
    pub current_plan: Mutex<Vec<PlanEntry>>,
}

#[derive(Clone, Debug)]
pub struct LastResults {
    pub report: DedupeReport,
    pub counters: ScanCounters,
    pub roots: Vec<PathBuf>,
    pub mode: String,
    pub scan_id: i64,
}

pub fn setup_app_state(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let app_data_root = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?;
    let mut data = DataDir::at_app_data_root(app_data_root)?;
    // Quarantined files go to ~/Desktop/Quarantine (flat, conflict-safe names).
    if let Some(desktop_q) = DataDir::desktop_quarantine() {
        if let Err(e) = data.set_quarantine_out(desktop_q) {
            eprintln!("could not set ~/Desktop/Quarantine, using data dir: {e}");
        }
    }
    let memory = MemoryBank::open(&data.memory_db)?;
    let logs = LogSink::new(data.current_log_path());
    logs.info(format!("Rusty started; data dir: {}", data.root.display()));
    logs.info(format!(
        "quarantine output (created only after confirmed Real quarantine): {}",
        data.quarantine_out_dir.display()
    ));

    app.manage(AppState {
        data,
        memory,
        logs,
        scan_running: Arc::new(AtomicBool::new(false)),
        cancel: Arc::new(AtomicBool::new(false)),
        apply_running: Arc::new(AtomicBool::new(false)),
        last_results: Mutex::new(None),
        current_plan: Mutex::new(Vec::new()),
    });

    #[cfg(target_os = "macos")]
    if let Some(window) = app.get_webview_window("main") {
        let _ = window_vibrancy::apply_vibrancy(
            &window,
            window_vibrancy::NSVisualEffectMaterial::HudWindow,
            None,
            None,
        );
    }

    Ok(())
}
