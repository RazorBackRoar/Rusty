use std::path::{Component, Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

/// NFC-normalize a path's textual form for stable database lookups. macOS APFS
/// returns NFD by default, which breaks `==` comparisons against user-typed paths.
pub fn normalize_for_storage(path: &Path) -> String {
    path.to_string_lossy().nfc().collect::<String>()
}

/// Strip any sneaky `..` or empty components and canonicalize separators. Does
/// not touch the filesystem — safe to call on paths that don't exist.
pub fn sanitize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                // Drop ".." — never let user input climb out of the intended root.
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Returns the last path segment, or "" if there is none. Used as a fallback
/// identity when a file is moved and we want to spot rename-only cases.
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().nfc().collect::<String>())
        .unwrap_or_default()
}

/// True for macOS bookkeeping files that should never count as duplicates.
pub fn is_macos_metadata(name: &str) -> bool {
    let s = name.trim_end_matches('\r');
    matches!(
        s,
        ".DS_Store"
            | ".AppleDouble"
            | ".LSOverride"
            | ".Spotlight-V100"
            | ".Trashes"
            | ".fseventsd"
            | ".VolumeIcon.icns"
            | ".com.apple.timemachine.donotpresent"
    ) || s == "Icon"
        || s.starts_with("._")
}

/// True for directories we should not descend into.
pub fn is_macos_metadata_dir(name: &str) -> bool {
    matches!(
        name,
        ".Spotlight-V100"
            | ".Trashes"
            | ".fseventsd"
            | ".DocumentRevisions-V100"
            | ".TemporaryItems"
    )
}

/// Directories that are almost always derived/cache content rather than user
/// data worth deduping (version-control internals, package/build output,
/// language caches) plus opaque application/library bundles. Pruned during the
/// walk when the "skip dev/cache folders" option is on.
pub fn is_dev_or_cache_dir(name: &str) -> bool {
    if matches!(
        name,
        ".git"
            | ".hg"
            | ".svn"
            | "node_modules"
            | "bower_components"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".tox"
            | ".mypy_cache"
            | ".pytest_cache"
            | ".cargo"
            | "target"
            | ".gradle"
            | ".m2"
            | ".next"
            | ".nuxt"
            | ".parcel-cache"
            | "DerivedData"
            | ".cache"
            | "Caches"
    ) {
        return true;
    }
    // Opaque packages — treat as a single unit, don't dedupe their innards.
    // Photo libraries are intentionally not listed here; their package
    // contents are user media and should be scanned when a user selects them.
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".app")
        || lower.ends_with(".framework")
        || lower.ends_with(".bundle")
        || lower.ends_with(".xcodeproj")
}

/// Returns true if `child` sits under `parent` (lexical check, no symlink follow).
pub fn is_under(parent: &Path, child: &Path) -> bool {
    let parent = sanitize(parent);
    let child = sanitize(child);
    child.starts_with(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_parent_components() {
        let p = Path::new("/a/b/../c/./d");
        assert_eq!(sanitize(p), PathBuf::from("/a/c/d"));
    }

    #[test]
    fn metadata_names_filtered() {
        assert!(is_macos_metadata(".DS_Store"));
        assert!(is_macos_metadata("._Photo.jpg"));
        assert!(is_macos_metadata("Icon\r"));
        assert!(!is_macos_metadata("Photo.jpg"));
    }

    #[test]
    fn photo_libraries_are_not_dev_cache_dirs() {
        assert!(!is_dev_or_cache_dir("Vacation.photoslibrary"));
        assert!(!is_dev_or_cache_dir("Archive.photolibrary"));
        assert!(is_dev_or_cache_dir("target"));
        assert!(is_dev_or_cache_dir("Example.app"));
    }
}
