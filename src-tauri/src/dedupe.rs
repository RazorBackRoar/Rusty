//! Group scanned files by hash and rank duplicate groups by wasted space.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::scanner::{MediaKind, ScannedFile};

#[derive(Clone, Debug, Serialize)]
pub struct DuplicateGroup {
    pub hash: String,
    pub media_kind: MediaKind,
    pub size: i64,
    pub copies: i64,
    pub wasted_bytes: i64,
    pub files: Vec<ScannedFile>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DedupeReport {
    pub groups: Vec<DuplicateGroup>,
    pub total_files: i64,
    pub total_duplicate_files: i64,
    pub total_wasted_bytes: i64,
    pub largest_single_file_in_dup: i64,
    pub duplicate_dirs: Vec<DuplicateDir>,
}

/// A set of directories whose direct file contents (file name + content hash)
/// are identical — i.e. the same folder copied to several places.
#[derive(Clone, Debug, Serialize)]
pub struct DuplicateDir {
    pub signature: String,
    pub dirs: Vec<String>,
    pub file_count: i64,
    /// Bytes in one copy of the directory.
    pub total_bytes: i64,
    /// Bytes that could be reclaimed by removing all but one copy.
    pub wasted_bytes: i64,
}

pub fn group_duplicates(files: &[ScannedFile]) -> DedupeReport {
    // Key only on hash (not media_kind) so that identical bytes with different
    // extensions — e.g. a .mov renamed to .jpg — are correctly grouped.
    let mut by_hash: HashMap<String, Vec<ScannedFile>> = HashMap::new();
    for f in files {
        by_hash.entry(f.hash.clone()).or_default().push(f.clone());
    }

    let mut groups: Vec<DuplicateGroup> = by_hash
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(hash, mut v)| {
            // Sort copies inside a group by path for stable display.
            v.sort_by(|a, b| a.normalized_path.cmp(&b.normalized_path));
            let size = v.first().map(|f| f.size).unwrap_or(0);
            let media_kind = v.first().map(|f| f.media_kind).unwrap_or(MediaKind::Other);
            let copies = v.len() as i64;
            let wasted_bytes = size * (copies - 1);
            DuplicateGroup {
                hash,
                media_kind,
                size,
                copies,
                wasted_bytes,
                files: v,
            }
        })
        .collect();

    // Largest waste first, then largest individual file size, then hash for
    // deterministic ordering.
    groups.sort_by(|a, b| {
        b.wasted_bytes
            .cmp(&a.wasted_bytes)
            .then_with(|| b.size.cmp(&a.size))
            .then_with(|| a.hash.cmp(&b.hash))
    });

    let total_duplicate_files: i64 = groups.iter().map(|g| g.copies).sum();
    let total_wasted_bytes: i64 = groups.iter().map(|g| g.wasted_bytes).sum();
    let largest_single_file_in_dup = groups.iter().map(|g| g.size).max().unwrap_or(0);

    DedupeReport {
        groups,
        total_files: files.len() as i64,
        total_duplicate_files,
        total_wasted_bytes,
        largest_single_file_in_dup,
        duplicate_dirs: find_duplicate_dirs(files),
    }
}

/// Find directories with identical *direct* contents. For each directory we
/// build a signature from its (file_name, content_hash) pairs; directories that
/// share a signature hold the same files and are reported as a duplicate set.
/// Subdirectories are not considered here — this catches the common
/// "copied a folder of files" case.
pub fn find_duplicate_dirs(files: &[ScannedFile]) -> Vec<DuplicateDir> {
    let mut dir_files: HashMap<String, BTreeMap<String, String>> = HashMap::new();
    let mut dir_bytes: HashMap<String, i64> = HashMap::new();
    for f in files {
        let parent = std::path::Path::new(&f.normalized_path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        dir_files
            .entry(parent.clone())
            .or_default()
            .insert(f.file_name.clone(), f.hash.clone());
        *dir_bytes.entry(parent).or_insert(0) += f.size;
    }

    let mut by_sig: HashMap<String, Vec<(String, i64, i64)>> = HashMap::new();
    for (dir, fmap) in &dir_files {
        if fmap.is_empty() {
            continue;
        }
        let mut hasher = blake3::Hasher::new();
        for (name, hash) in fmap {
            hasher.update(name.as_bytes());
            hasher.update(b"\0");
            hasher.update(hash.as_bytes());
            hasher.update(b"\0");
        }
        let sig = hasher.finalize().to_hex().to_string();
        let bytes = *dir_bytes.get(dir).unwrap_or(&0);
        by_sig
            .entry(sig)
            .or_default()
            .push((dir.clone(), fmap.len() as i64, bytes));
    }

    let mut out: Vec<DuplicateDir> = by_sig
        .into_iter()
        .filter(|(_, dirs)| dirs.len() > 1)
        .map(|(sig, mut dirs)| {
            dirs.sort_by(|a, b| a.0.cmp(&b.0));
            let file_count = dirs[0].1;
            let total_bytes = dirs[0].2;
            let wasted_bytes = total_bytes * (dirs.len() as i64 - 1);
            DuplicateDir {
                signature: sig.chars().take(12).collect(),
                dirs: dirs.into_iter().map(|(d, _, _)| d).collect(),
                file_count,
                total_bytes,
                wasted_bytes,
            }
        })
        .collect();
    // Most wasted space first.
    out.sort_by(|a, b| {
        b.wasted_bytes
            .cmp(&a.wasted_bytes)
            .then_with(|| b.file_count.cmp(&a.file_count))
    });
    out
}

/// Build a default deletion plan: for each group, KEEP the file with the
/// shortest normalized path (a common heuristic for "the original"), QUARANTINE
/// the rest. We never plan to remove every copy in a group.
#[derive(Clone, Debug, Serialize)]
pub struct PlanEntry {
    pub hash: String,
    pub path: String,
    pub normalized_path: String,
    pub size: i64,
    pub action: PlanAction,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PlanAction {
    Keep,
    Quarantine,
}

/// Which copy in a duplicate group is kept; the rest are quarantined.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum KeeperRule {
    #[default]
    ShortestPath,
    LongestPath,
    Oldest,
    Newest,
}

impl KeeperRule {
    fn reason(self) -> &'static str {
        match self {
            KeeperRule::ShortestPath => "keeper: shortest path",
            KeeperRule::LongestPath => "keeper: longest path",
            KeeperRule::Oldest => "keeper: oldest modified time",
            KeeperRule::Newest => "keeper: newest modified time",
        }
    }
}

pub fn default_plan(report: &DedupeReport) -> Vec<PlanEntry> {
    default_plan_with_rule(report, KeeperRule::ShortestPath)
}

/// Like [`default_plan`] but lets the caller choose which copy survives.
pub fn default_plan_with_rule(report: &DedupeReport, rule: KeeperRule) -> Vec<PlanEntry> {
    let mut out = Vec::with_capacity(report.total_duplicate_files as usize);
    for group in &report.groups {
        if group.files.len() < 2 {
            continue;
        }
        let keeper_idx = pick_keeper(&group.files, rule);
        let keeper = &group.files[keeper_idx];
        for (i, f) in group.files.iter().enumerate() {
            // A file sharing the keeper's (dev, ino) is a hardlink to the same
            // bytes — quarantining it frees no space and just splits the link,
            // so keep it. `ino == 0` means identity is unknown (non-unix); fall
            // back to treating it as an independent copy.
            let is_hardlink_of_keeper =
                i != keeper_idx && f.ino != 0 && f.ino == keeper.ino && f.dev == keeper.dev;
            let action = if i == keeper_idx || is_hardlink_of_keeper {
                PlanAction::Keep
            } else {
                PlanAction::Quarantine
            };
            out.push(PlanEntry {
                hash: group.hash.clone(),
                path: f.path.clone(),
                normalized_path: f.normalized_path.clone(),
                size: f.size,
                action,
                reason: if i == keeper_idx {
                    rule.reason().into()
                } else if is_hardlink_of_keeper {
                    "hardlink of keeper — no space freed".into()
                } else {
                    "duplicate of keeper".into()
                },
            });
        }
    }
    out
}

/// Choose which file in a group to keep, per the selected rule.
fn pick_keeper(files: &[ScannedFile], rule: KeeperRule) -> usize {
    let shorter = |a: &ScannedFile, b: &ScannedFile| {
        a.normalized_path
            .len()
            .cmp(&b.normalized_path.len())
            .then_with(|| a.normalized_path.cmp(&b.normalized_path))
    };
    files
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| match rule {
            KeeperRule::ShortestPath => shorter(a, b),
            KeeperRule::LongestPath => shorter(b, a),
            KeeperRule::Oldest => a
                .modified_ns
                .cmp(&b.modified_ns)
                .then_with(|| shorter(a, b)),
            KeeperRule::Newest => b
                .modified_ns
                .cmp(&a.modified_ns)
                .then_with(|| shorter(a, b)),
        })
        .map(|(i, _)| i)
        .unwrap_or(0)
}
