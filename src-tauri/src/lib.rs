pub mod commands;
pub mod data_dir;
pub mod dedupe;
pub mod error;
pub mod logs;
pub mod memory;
pub mod paths;
pub mod perceptual;
pub mod quarantine;
pub mod reports;
pub mod scanner;
pub mod state;

pub use data_dir::DataDir;
pub use error::{AppError, AppResult};
pub use logs::LogSink;
pub use memory::MemoryBank;

/// Build and run the Tauri application. Called from `main.rs`.
pub fn run() {
    let _ = fix_working_dir();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(state::setup_app_state)
        .invoke_handler(tauri::generate_handler![
            commands::pick_folders,
            commands::list_remembered_folders,
            commands::peek_folder,
            commands::forget_folder,
            commands::run_scan,
            commands::get_last_results,
            commands::get_default_plan,
            commands::set_keeper_rule,
            commands::set_plan_action,
            commands::apply_plan,
            commands::undo_run,
            commands::recent_scans,
            commands::memory_stats,
            commands::get_logs,
            commands::clear_logs,
            commands::export_report,
            commands::data_paths,
            commands::reveal_path,
            commands::cancel_scan,
            commands::find_similar_images,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn fix_working_dir() -> std::io::Result<()> {
    // When the .app is double-clicked on macOS the CWD lands in `/`. Move to
    // the app data dir so any accidental relative paths resolve to a safe spot.
    if let Ok(home) = std::env::var("HOME") {
        let _ = std::env::set_current_dir(home);
    }
    Ok(())
}
