use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Serialize;

use crate::scanner::ScannedFile;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChangeType {
    Moved,
    Renamed,
    Changed,
    New,
    Gone,
}

impl ChangeType {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeType::Moved => "MOVED",
            ChangeType::Renamed => "RENAMED",
            ChangeType::Changed => "CHANGED",
            ChangeType::New => "NEW",
            ChangeType::Gone => "GONE",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FileChange {
    pub change_type: ChangeType,
    pub hash: String,
    pub prev_path: String,
    pub new_path: String,
    pub file_name: String,
    pub size: i64,
    pub source_root: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct FolderManifestRow {
    pub path: String,
    pub source_root: String,
    pub file_count: i64,
    pub total_bytes: i64,
    pub folder_hash: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileManifestRow {
    pub path: String,
    pub normalized_path: String,
    pub source_root: String,
    pub size: i64,
    pub content_hash: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ScanManifests {
    pub folders: Vec<FolderManifestRow>,
    pub files: Vec<FileManifestRow>,
}

#[derive(Clone, Debug)]
pub struct ScanFileSnapshot {
    pub hash: String,
    pub path: String,
    pub normalized_path: String,
    pub file_name: String,
    pub source_root: String,
    pub size: i64,
    pub modified_ns: i64,
}

pub fn compute_changes(prev: &[ScanFileSnapshot], curr: &[ScanFileSnapshot]) -> Vec<FileChange> {
    let mut prev_by_path: HashMap<&str, usize> = HashMap::with_capacity(prev.len());
    let mut prev_by_hash: HashMap<&str, Vec<usize>> = HashMap::new();
    for (idx, row) in prev.iter().enumerate() {
        prev_by_path.insert(row.normalized_path.as_str(), idx);
        prev_by_hash.entry(row.hash.as_str()).or_default().push(idx);
    }

    let mut matched_prev: HashSet<usize> = HashSet::new();
    let mut changes = Vec::new();

    for row in curr {
        if let Some(&prev_idx) = prev_by_path.get(row.normalized_path.as_str()) {
            matched_prev.insert(prev_idx);
            let old = &prev[prev_idx];
            if old.hash != row.hash || old.size != row.size || old.modified_ns != row.modified_ns {
                changes.push(FileChange {
                    change_type: ChangeType::Changed,
                    hash: row.hash.clone(),
                    prev_path: old.path.clone(),
                    new_path: row.path.clone(),
                    file_name: row.file_name.clone(),
                    size: row.size,
                    source_root: row.source_root.clone(),
                });
            }
            continue;
        }

        let mut matched_by_hash = None;
        if let Some(bucket) = prev_by_hash.get(row.hash.as_str()) {
            matched_by_hash = bucket
                .iter()
                .copied()
                .find(|idx| !matched_prev.contains(idx) && prev[*idx].file_name == row.file_name)
                .or_else(|| {
                    bucket
                        .iter()
                        .copied()
                        .find(|idx| !matched_prev.contains(idx))
                });
        }

        if let Some(prev_idx) = matched_by_hash {
            matched_prev.insert(prev_idx);
            let old = &prev[prev_idx];
            let change_type = if old.file_name == row.file_name {
                ChangeType::Moved
            } else {
                ChangeType::Renamed
            };
            changes.push(FileChange {
                change_type,
                hash: row.hash.clone(),
                prev_path: old.path.clone(),
                new_path: row.path.clone(),
                file_name: row.file_name.clone(),
                size: row.size,
                source_root: row.source_root.clone(),
            });
        } else {
            changes.push(FileChange {
                change_type: ChangeType::New,
                hash: row.hash.clone(),
                prev_path: String::new(),
                new_path: row.path.clone(),
                file_name: row.file_name.clone(),
                size: row.size,
                source_root: row.source_root.clone(),
            });
        }
    }

    for (idx, old) in prev.iter().enumerate() {
        if matched_prev.contains(&idx) {
            continue;
        }
        if Path::new(&old.path).exists() {
            continue;
        }
        changes.push(FileChange {
            change_type: ChangeType::Gone,
            hash: old.hash.clone(),
            prev_path: old.path.clone(),
            new_path: String::new(),
            file_name: old.file_name.clone(),
            size: old.size,
            source_root: old.source_root.clone(),
        });
    }

    changes.sort_by(|a, b| {
        change_order(a.change_type)
            .cmp(&change_order(b.change_type))
            .then_with(|| a.prev_path.cmp(&b.prev_path))
            .then_with(|| a.new_path.cmp(&b.new_path))
    });
    changes
}

pub fn build_manifests(files: &[ScannedFile]) -> ScanManifests {
    struct FolderAcc {
        source_root: String,
        file_count: i64,
        total_bytes: i64,
        direct_files: Vec<(String, String)>,
    }

    let mut folders: HashMap<String, FolderAcc> = HashMap::new();
    let mut file_rows = Vec::with_capacity(files.len());

    for file in files {
        file_rows.push(FileManifestRow {
            path: file.path.clone(),
            normalized_path: file.normalized_path.clone(),
            source_root: file.source_root.clone(),
            size: file.size,
            content_hash: file.hash.clone(),
        });

        let Some(parent) = Path::new(&file.normalized_path).parent() else {
            continue;
        };
        let folder_path = parent.to_string_lossy().into_owned();
        let entry = folders.entry(folder_path).or_insert_with(|| FolderAcc {
            source_root: file.source_root.clone(),
            file_count: 0,
            total_bytes: 0,
            direct_files: Vec::new(),
        });
        entry.file_count += 1;
        entry.total_bytes += file.size;
        entry
            .direct_files
            .push((file.file_name.clone(), file.hash.clone()));
    }

    let mut folder_rows: Vec<FolderManifestRow> = folders
        .into_iter()
        .map(|(path, mut acc)| {
            acc.direct_files.sort();
            let mut hasher = blake3::Hasher::new();
            for (name, hash) in &acc.direct_files {
                hasher.update(name.as_bytes());
                hasher.update(b"\0");
                hasher.update(hash.as_bytes());
                hasher.update(b"\0");
            }
            FolderManifestRow {
                path,
                source_root: acc.source_root,
                file_count: acc.file_count,
                total_bytes: acc.total_bytes,
                folder_hash: hasher.finalize().to_hex().to_string(),
            }
        })
        .collect();

    folder_rows.sort_by(|a, b| a.path.cmp(&b.path));
    file_rows.sort_by(|a, b| a.normalized_path.cmp(&b.normalized_path));

    ScanManifests {
        folders: folder_rows,
        files: file_rows,
    }
}

fn change_order(change_type: ChangeType) -> u8 {
    match change_type {
        ChangeType::Moved => 0,
        ChangeType::Renamed => 1,
        ChangeType::Changed => 2,
        ChangeType::New => 3,
        ChangeType::Gone => 4,
    }
}
