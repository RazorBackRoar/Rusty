//! Perceptual near-duplicate image detection (review-only).
//!
//! Computes a 64-bit dHash per image and clusters images whose Hamming distance
//! is within a threshold. These are *visually similar*, NOT byte-identical — so
//! the results are advisory only and never feed the quarantine plan. Nothing in
//! this module moves, deletes, or plans anything; it only reports.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use jwalk::WalkDir;
use rayon::prelude::*;
use serde::Serialize;

use crate::logs::LogSink;
use crate::paths;

// dHash compares each pixel to its right neighbor over a (W-1)xH grid → 64 bits.
const HASH_W: u32 = 9;
const HASH_H: u32 = 8;

#[derive(Serialize)]
pub struct SimilarImage {
    pub path: String,
    pub size: i64,
    pub width: u32,
    pub height: u32,
}

#[derive(Serialize)]
pub struct SimilarCluster {
    pub images: Vec<SimilarImage>,
    pub max_distance: u32,
}

#[derive(Serialize)]
pub struct SimilarImagesResult {
    pub clusters: Vec<SimilarCluster>,
    pub images_scanned: i64,
    pub errors: i64,
}

/// True for file names with a still-image extension we can decode.
pub fn is_image_ext(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        ".jpg", ".jpeg", ".png", ".gif", ".bmp", ".tif", ".tiff", ".webp",
    ]
    .iter()
    .any(|e| lower.ends_with(e))
}

struct Fingerprint {
    path: PathBuf,
    size: i64,
    width: u32,
    height: u32,
    hash: u64,
}

/// Difference hash: downscale to grayscale and record, per row, whether each
/// pixel is brighter than the one to its right.
fn dhash(path: &Path) -> image::ImageResult<(u64, u32, u32)> {
    let img = image::open(path)?;
    let (w, h) = (img.width(), img.height());
    let small = img
        .resize_exact(HASH_W, HASH_H, image::imageops::FilterType::Triangle)
        .to_luma8();
    let mut hash: u64 = 0;
    let mut bit = 0u32;
    for y in 0..HASH_H {
        for x in 0..(HASH_W - 1) {
            let left = small.get_pixel(x, y)[0];
            let right = small.get_pixel(x + 1, y)[0];
            if left > right {
                hash |= 1u64 << bit;
            }
            bit += 1;
        }
    }
    Ok((hash, w, h))
}

fn uf_find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

/// Walk the roots for image files, fingerprint them in parallel, and cluster by
/// Hamming distance `<= threshold`.
pub fn find_similar(
    roots: &[PathBuf],
    threshold: u32,
    skip_dev_dirs: bool,
    logs: &LogSink,
) -> SimilarImagesResult {
    // Collect candidate image paths.
    let mut image_paths: Vec<(PathBuf, i64)> = Vec::new();
    for root in roots {
        let skip_dev = skip_dev_dirs;
        for dirent in
            WalkDir::new(root)
                .skip_hidden(false)
                .process_read_dir(move |_, _, _, children| {
                    children.iter_mut().for_each(|res| {
                        if let Ok(child) = res {
                            let name = child.file_name.to_string_lossy();
                            let prune = paths::is_macos_metadata_dir(&name)
                                || (skip_dev && paths::is_dev_or_cache_dir(&name));
                            if child.file_type.is_dir() && prune {
                                child.read_children_path = None;
                            }
                        }
                    });
                })
        {
            let Ok(d) = dirent else { continue };
            if !d.file_type.is_file() {
                continue;
            }
            let name = d.file_name.to_string_lossy();
            if !is_image_ext(&name) || crate::paths::is_macos_metadata(&name) {
                continue;
            }
            let size = d.metadata().map(|m| m.len() as i64).unwrap_or(0);
            image_paths.push((d.path(), size));
        }
    }

    logs.info(format!(
        "perceptual: fingerprinting {} image(s)",
        image_paths.len()
    ));

    let errors = Arc::new(AtomicUsize::new(0));
    let fps: Vec<Fingerprint> = image_paths
        .par_iter()
        .filter_map(|(p, size)| match dhash(p) {
            Ok((hash, width, height)) => Some(Fingerprint {
                path: p.clone(),
                size: *size,
                width,
                height,
                hash,
            }),
            Err(_) => {
                errors.fetch_add(1, Ordering::Relaxed);
                None
            }
        })
        .collect();

    // Union-find: connect every pair within the distance threshold.
    // If there are too many images, limit comparisons to prevent O(n^2) freeze.
    let n = fps.len();
    if n > 20_000 {
        logs.info("Too many images for full clustering; limiting to first 20,000 for similarity search.");
    }
    let n_limit = n.min(20_000);
    let mut parent: Vec<usize> = (0..n).collect();
    for i in 0..n_limit {
        for j in (i + 1)..n_limit {
            if (fps[i].hash ^ fps[j].hash).count_ones() <= threshold {
                let a = uf_find(&mut parent, i);
                let b = uf_find(&mut parent, j);
                if a != b {
                    parent[a] = b;
                }
            }
        }
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let r = uf_find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }

    let mut clusters: Vec<SimilarCluster> = groups
        .into_values()
        .filter(|idxs| idxs.len() > 1)
        .map(|idxs| {
            let mut max_distance = 0u32;
            for a in 0..idxs.len() {
                for b in (a + 1)..idxs.len() {
                    let d = (fps[idxs[a]].hash ^ fps[idxs[b]].hash).count_ones();
                    if d > max_distance {
                        max_distance = d;
                    }
                }
            }
            let mut images: Vec<SimilarImage> = idxs
                .iter()
                .map(|&i| SimilarImage {
                    path: fps[i].path.to_string_lossy().into_owned(),
                    size: fps[i].size,
                    width: fps[i].width,
                    height: fps[i].height,
                })
                .collect();
            // Largest (usually highest quality) first — handy for review.
            images.sort_by(|a, b| {
                (b.width as i64 * b.height as i64)
                    .cmp(&(a.width as i64 * a.height as i64))
                    .then_with(|| b.size.cmp(&a.size))
            });
            SimilarCluster {
                images,
                max_distance,
            }
        })
        .collect();

    clusters.sort_by_key(|b| std::cmp::Reverse(b.images.len()));

    SimilarImagesResult {
        clusters,
        images_scanned: n as i64,
        errors: errors.load(Ordering::Relaxed) as i64,
    }
}
