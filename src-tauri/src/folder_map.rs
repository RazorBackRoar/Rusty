//! Build a folder-only tree with subtree file counts for the Map tab.
//!
//! Leaves never list filenames — each node shows how many files live in that
//! directory and all of its descendants.

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;

use crate::paths;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FolderMapNode {
    pub name: String,
    pub path: String,
    /// Total files in this directory and all subdirectories.
    pub file_count: u64,
    pub children: Vec<FolderMapNode>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DirFileCount {
    pub depth: usize,
    pub files: u64,
}

/// Build one tree rooted at `root` from per-directory direct file counts.
pub fn build_folder_map(root: &Path, dir_counts: &HashMap<String, DirFileCount>) -> FolderMapNode {
    let root_norm = paths::normalize_for_storage(root);
    let root_name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| root_norm.clone());

    // Ensure the root is present even when the walk recorded no files under it.
    let mut counts = dir_counts.clone();
    counts.entry(root_norm.clone()).or_insert(DirFileCount {
        depth: 0,
        files: 0,
    });

    let mut children_of: HashMap<String, Vec<String>> = HashMap::new();
    for path in counts.keys() {
        if path == &root_norm {
            continue;
        }
        if !path.starts_with(&root_norm) {
            continue;
        }
        let parent = match path.rsplit_once('/') {
            Some((p, _)) if !p.is_empty() => p.to_string(),
            _ => root_norm.clone(),
        };
        // Only attach if the parent is in the set (or is the root).
        if counts.contains_key(&parent) || parent == root_norm {
            children_of.entry(parent).or_default().push(path.clone());
        }
    }
    for kids in children_of.values_mut() {
        kids.sort();
    }

    fn build_node(
        path: &str,
        name: &str,
        counts: &HashMap<String, DirFileCount>,
        children_of: &HashMap<String, Vec<String>>,
    ) -> FolderMapNode {
        let mut children = Vec::new();
        if let Some(kids) = children_of.get(path) {
            for child_path in kids {
                let child_name = child_path
                    .rsplit_once('/')
                    .map(|(_, n)| n.to_string())
                    .unwrap_or_else(|| child_path.clone());
                children.push(build_node(child_path, &child_name, counts, children_of));
            }
        }
        let direct = counts.get(path).map(|c| c.files).unwrap_or(0);
        let file_count = direct + children.iter().map(|c| c.file_count).sum::<u64>();
        FolderMapNode {
            name: name.to_string(),
            path: path.to_string(),
            file_count,
            children,
        }
    }

    build_node(&root_norm, &root_name, &counts, &children_of)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn rolls_up_subtree_file_counts() {
        let root = PathBuf::from("/Photos");
        let mut counts = HashMap::new();
        counts.insert(
            "/Photos".into(),
            DirFileCount {
                depth: 0,
                files: 2,
            },
        );
        counts.insert(
            "/Photos/A".into(),
            DirFileCount {
                depth: 1,
                files: 3,
            },
        );
        counts.insert(
            "/Photos/A/B".into(),
            DirFileCount {
                depth: 2,
                files: 5,
            },
        );
        counts.insert(
            "/Photos/C".into(),
            DirFileCount {
                depth: 1,
                files: 1,
            },
        );

        let tree = build_folder_map(&root, &counts);
        assert_eq!(tree.name, "Photos");
        assert_eq!(tree.file_count, 2 + 3 + 5 + 1);
        assert_eq!(tree.children.len(), 2);
        let a = tree.children.iter().find(|c| c.name == "A").unwrap();
        assert_eq!(a.file_count, 3 + 5);
        let b = a.children.iter().find(|c| c.name == "B").unwrap();
        assert_eq!(b.file_count, 5);
        let c = tree.children.iter().find(|c| c.name == "C").unwrap();
        assert_eq!(c.file_count, 1);
        assert!(b.children.is_empty());
    }
}
