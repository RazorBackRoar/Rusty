use std::path::{Path, PathBuf};

const MACOS_BUNDLE_ID: &str = "com.rusty.desktop";
const LEGACY_MACOS_BUNDLE_ID: &str = "com.rusty.app";

use crate::error::{AppError, AppResult};
use crate::paths;

/// Resolved on-disk layout for everything Rusty writes.
///
/// On macOS this lives under `~/Library/Application Support/com.rusty.desktop/`.
/// App state — memory bank, logs, exports, and manifests — lives here.
/// The real app points quarantine output at `~/Desktop/Quarantine`, but that
/// folder is not created until the user confirms a Real quarantine run.
#[derive(Debug, Clone)]
pub struct DataDir {
    pub root: PathBuf,
    pub memory_db: PathBuf,
    pub logs_dir: PathBuf,
    pub exports_dir: PathBuf,
    pub quarantine_dir: PathBuf,
    pub manifests_dir: PathBuf,
    /// Where quarantined files are actually moved to. The real app points this
    /// at `~/Desktop/Quarantine`; tests leave it under the (temp) data root.
    pub quarantine_out_dir: PathBuf,
}

impl DataDir {
    /// Build with an explicit root. Used by tests and by the Tauri app, which
    /// hands in `app.path().app_data_dir()` so the location matches the bundle ID.
    pub fn at(root: PathBuf) -> AppResult<Self> {
        let memory_db = root.join("memory_bank.sqlite");
        let logs_dir = root.join("logs");
        let exports_dir = root.join("exports");
        let quarantine_dir = root.join("quarantine");
        let manifests_dir = root.join("manifests");

        for dir in [
            &root,
            &logs_dir,
            &exports_dir,
            &quarantine_dir,
            &manifests_dir,
        ] {
            std::fs::create_dir_all(dir)?;
        }

        Ok(Self {
            root,
            memory_db,
            logs_dir,
            exports_dir,
            quarantine_dir: quarantine_dir.clone(),
            manifests_dir,
            // Default: under the data root (keeps integration tests off the real
            // Desktop). The Tauri app overrides this to ~/Desktop/Quarantine.
            quarantine_out_dir: quarantine_dir,
        })
    }

    /// Build from the app-data root Tauri resolves for the current bundle.
    /// If the previous macOS fallback exists, move missing entries into the
    /// canonical bundle-ID directory first without overwriting existing data.
    pub fn at_app_data_root(root: PathBuf) -> AppResult<Self> {
        migrate_legacy_app_data_root(&root)?;
        Self::at(root)
    }

    /// `~/Desktop/Quarantine`, if a home directory can be resolved. Quarantined
    /// files are moved here as a flat list (conflict-safe names), with original
    /// paths recorded in the manifest + the in-folder log rather than recreated
    /// as nested `/Volumes/...` directories.
    pub fn desktop_quarantine() -> Option<PathBuf> {
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join("Desktop").join("Quarantine"))
    }

    /// Point the quarantine output at `dir` without creating it. The quarantine
    /// folder is only created after the user confirms a Real quarantine run.
    pub fn set_quarantine_out(&mut self, dir: PathBuf) -> AppResult<()> {
        self.quarantine_out_dir = dir;
        Ok(())
    }

    /// Fallback used when no Tauri AppHandle is available (e.g. integration tests).
    /// Honors `RUSTY_DATA_DIR` if set, otherwise picks a per-OS default.
    pub fn default_for_environment() -> AppResult<Self> {
        if let Ok(custom) = std::env::var("RUSTY_DATA_DIR") {
            return Self::at(PathBuf::from(custom));
        }
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .map_err(|_| AppError::BadInput("HOME not set".into()))?;
        let root = default_root_for_home(&home);
        Self::at_app_data_root(root)
    }

    pub fn ensure_subdir(&self, name: &str) -> AppResult<PathBuf> {
        let path = self.root.join(name);
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    pub fn quarantine_for_run(&self, run_id: &str) -> AppResult<PathBuf> {
        let path = self.quarantine_dir.join(run_id);
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    pub fn manifest_for_run(&self, run_id: &str) -> PathBuf {
        self.manifests_dir.join(format!("{run_id}.json"))
    }

    pub fn current_log_path(&self) -> PathBuf {
        self.logs_dir.join("rusty.log")
    }
}

/// Returns true when `path` sits inside the configured data dir. Used as a sanity
/// check before any destructive operation we initiated ourselves.
pub fn is_under(data: &DataDir, path: &Path) -> bool {
    paths::sanitize(path).starts_with(&data.root)
}

fn default_root_for_home(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Application Support")
            .join(MACOS_BUNDLE_ID)
    } else {
        home.join(".local").join("share").join("rusty")
    }
}

/// Update-check cache directory (`~/Library/Caches/Rusty` on macOS).
/// App state stays under the bundle-ID Application Support path; only the
/// GitHub Releases response cache uses this display-name layout.
pub fn app_cache_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("RUSTY_CACHE_DIR") {
        return PathBuf::from(custom);
    }
    match std::env::var("HOME") {
        Ok(home) => default_cache_for_home(Path::new(&home)),
        Err(_) => PathBuf::from("/tmp/rusty-cache"),
    }
}

fn default_cache_for_home(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library").join("Caches").join("Rusty")
    } else {
        home.join(".cache").join("rusty")
    }
}

fn migrate_legacy_app_data_root(root: &Path) -> AppResult<()> {
    let Some(legacy_root) = legacy_root_for_canonical(root) else {
        return Ok(());
    };
    if !legacy_root.exists() {
        return Ok(());
    }
    if root.exists() {
        move_missing_entries(&legacy_root, root)?;
        remove_dir_if_empty(&legacy_root)?;
    } else {
        std::fs::rename(legacy_root, root)?;
    }
    Ok(())
}

fn legacy_root_for_canonical(root: &Path) -> Option<PathBuf> {
    if cfg!(target_os = "macos") && root.file_name()? == MACOS_BUNDLE_ID {
        Some(root.with_file_name(LEGACY_MACOS_BUNDLE_ID))
    } else {
        None
    }
}

fn move_missing_entries(from: &Path, to: &Path) -> AppResult<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        if !target.exists() {
            std::fs::rename(source, target)?;
        } else if source.is_dir() && target.is_dir() {
            move_missing_entries(&source, &target)?;
            remove_dir_if_empty(&source)?;
        }
    }
    Ok(())
}

fn remove_dir_if_empty(path: &Path) -> AppResult<()> {
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(e)
            if e.kind() == std::io::ErrorKind::DirectoryNotEmpty
                || e.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_default_root_uses_bundle_identifier() {
        let root = default_root_for_home(Path::new("/Users/example"));

        assert_eq!(
            root,
            PathBuf::from("/Users/example/Library/Application Support/com.rusty.desktop")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_cache_dir_uses_display_name() {
        let cache = default_cache_for_home(Path::new("/Users/example"));
        assert_eq!(
            cache,
            PathBuf::from("/Users/example/Library/Caches/Rusty")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn migrates_legacy_app_data_root_when_canonical_root_is_absent() {
        let temp = tempfile::tempdir().unwrap();
        let support = temp.path().join("Library").join("Application Support");
        let legacy = support.join("com.rusty.app");
        let canonical = support.join("com.rusty.desktop");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("memory_bank.sqlite"), b"existing cache").unwrap();

        DataDir::at_app_data_root(canonical.clone()).unwrap();

        assert!(!legacy.exists());
        assert_eq!(
            std::fs::read(canonical.join("memory_bank.sqlite")).unwrap(),
            b"existing cache"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn does_not_merge_or_overwrite_when_canonical_root_already_exists() {
        let temp = tempfile::tempdir().unwrap();
        let support = temp.path().join("Library").join("Application Support");
        let legacy = support.join("com.rusty.app");
        let canonical = support.join("com.rusty.desktop");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::write(legacy.join("memory_bank.sqlite"), b"legacy cache").unwrap();
        std::fs::write(canonical.join("memory_bank.sqlite"), b"canonical cache").unwrap();

        DataDir::at_app_data_root(canonical.clone()).unwrap();

        assert_eq!(
            std::fs::read(legacy.join("memory_bank.sqlite")).unwrap(),
            b"legacy cache"
        );
        assert_eq!(
            std::fs::read(canonical.join("memory_bank.sqlite")).unwrap(),
            b"canonical cache"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn moves_missing_legacy_entries_without_overwriting_canonical_entries() {
        let temp = tempfile::tempdir().unwrap();
        let support = temp.path().join("Library").join("Application Support");
        let legacy = support.join("com.rusty.app");
        let canonical = support.join("com.rusty.desktop");
        std::fs::create_dir_all(legacy.join("logs")).unwrap();
        std::fs::create_dir_all(canonical.join("logs")).unwrap();
        std::fs::write(legacy.join("memory_bank.sqlite"), b"legacy cache").unwrap();
        std::fs::write(legacy.join("logs").join("rusty.log"), b"legacy log").unwrap();
        std::fs::write(canonical.join("logs").join("rusty.log"), b"canonical log").unwrap();

        DataDir::at_app_data_root(canonical.clone()).unwrap();

        assert_eq!(
            std::fs::read(canonical.join("memory_bank.sqlite")).unwrap(),
            b"legacy cache"
        );
        assert_eq!(
            std::fs::read(canonical.join("logs").join("rusty.log")).unwrap(),
            b"canonical log"
        );
        assert_eq!(
            std::fs::read(legacy.join("logs").join("rusty.log")).unwrap(),
            b"legacy log"
        );
    }
}
