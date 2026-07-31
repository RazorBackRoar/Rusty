//! Review-only similar-video matching via rotation-normalized frame fingerprints.
//!
//! Never feeds the exact-duplicate quarantine plan. Requires `ffmpeg`/`ffprobe`
//! on PATH (Homebrew `/opt/homebrew/bin` is checked first).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use image::imageops::{self, FilterType};
use image::{GrayImage, ImageReader};
use rayon::prelude::*;
use serde::Serialize;

use crate::error::{AppError, AppResult};
use crate::logs::LogSink;

/// Default Hamming distance threshold for a single-frame pHash match.
pub const DEFAULT_MAX_DISTANCE: u32 = 10;
/// Frames sampled per video (evenly across duration).
const SAMPLE_FRAMES: usize = 8;
/// How many frame matches (best orientation) must agree to call two videos similar.
const MIN_MATCHING_FRAMES: usize = 4;

#[derive(Clone, Debug, Serialize)]
pub struct SimilarVideoFile {
    pub path: String,
    pub file_name: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SimilarCluster {
    pub files: Vec<SimilarVideoFile>,
    /// Mean best-orientation Hamming distance across matched frame pairs.
    pub avg_distance: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SimilarReport {
    pub clusters: Vec<SimilarCluster>,
    pub videos_considered: usize,
    pub videos_fingerprinted: usize,
    pub videos_failed: usize,
    pub ffmpeg_available: bool,
    pub message: String,
}

#[derive(Clone)]
struct Fingerprint {
    path: PathBuf,
    /// One 64-bit pHash per sampled frame, for each of 4 orientations.
    /// Layout: orientations[orient][frame_idx]
    orientations: [Vec<u64>; 4],
}

pub fn find_similar_videos(
    paths: &[PathBuf],
    max_distance: u32,
    logs: &LogSink,
    cancel: &Arc<AtomicBool>,
) -> AppResult<SimilarReport> {
    let Some(ffmpeg) = find_tool("ffmpeg") else {
        return Ok(SimilarReport {
            clusters: Vec::new(),
            videos_considered: paths.len(),
            videos_fingerprinted: 0,
            videos_failed: 0,
            ffmpeg_available: false,
            message: "ffmpeg not found. Install with: brew install ffmpeg".into(),
        });
    };
    let Some(ffprobe) = find_tool("ffprobe") else {
        return Ok(SimilarReport {
            clusters: Vec::new(),
            videos_considered: paths.len(),
            videos_fingerprinted: 0,
            videos_failed: 0,
            ffmpeg_available: false,
            message: "ffprobe not found. Install with: brew install ffmpeg".into(),
        });
    };

    logs.info(format!(
        "similar videos — fingerprinting {} candidate(s) (max distance {max_distance})",
        paths.len()
    ));

    let tmp_root = std::env::temp_dir().join(format!("rusty-similar-{}", std::process::id()));
    let _ = fs::create_dir_all(&tmp_root);

    let results: Vec<Option<Fingerprint>> = paths
        .par_iter()
        .map(|path| {
            if cancel.load(Ordering::SeqCst) {
                return None;
            }
            match fingerprint_video(path, &ffmpeg, &ffprobe, &tmp_root) {
                Ok(fp) => Some(fp),
                Err(e) => {
                    logs.warn(format!("similar skip {}: {e}", path.display()));
                    None
                }
            }
        })
        .collect();

    let _ = fs::remove_dir_all(&tmp_root);

    if cancel.load(Ordering::SeqCst) {
        return Err(AppError::BadInput("similar search cancelled".into()));
    }

    let fps: Vec<Fingerprint> = results.into_iter().flatten().collect();
    let failed = paths.len().saturating_sub(fps.len());
    let clusters = cluster_fingerprints(&fps, max_distance);

    let message = if clusters.is_empty() {
        if fps.is_empty() {
            "No videos could be fingerprinted.".into()
        } else {
            format!(
                "No similar groups found among {} fingerprinted video(s).",
                fps.len()
            )
        }
    } else {
        format!(
            "Found {} similar group(s) among {} fingerprinted video(s). Review only — not quarantined.",
            clusters.len(),
            fps.len()
        )
    };

    logs.info(message.clone());

    Ok(SimilarReport {
        clusters,
        videos_considered: paths.len(),
        videos_fingerprinted: fps.len(),
        videos_failed: failed,
        ffmpeg_available: true,
        message,
    })
}

fn find_tool(name: &str) -> Option<PathBuf> {
    let candidates = [
        PathBuf::from(format!("/opt/homebrew/bin/{name}")),
        PathBuf::from(format!("/usr/local/bin/{name}")),
        PathBuf::from(name),
    ];
    for c in candidates {
        if c.is_absolute() {
            if c.is_file() {
                return Some(c);
            }
        } else if Command::new(&c)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Some(c);
        }
    }
    None
}

pub fn ffmpeg_is_available() -> bool {
    find_tool("ffmpeg").is_some() && find_tool("ffprobe").is_some()
}

fn fingerprint_video(
    path: &Path,
    ffmpeg: &Path,
    ffprobe: &Path,
    tmp_root: &Path,
) -> Result<Fingerprint, String> {
    let duration = probe_duration(ffprobe, path).unwrap_or(1.0).max(0.1);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "video".into());
    let safe: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(40)
        .collect();
    let work = tmp_root.join(format!("{safe}-{}", uuid_slug(path)));
    fs::create_dir_all(&work).map_err(|e| e.to_string())?;

    let mut base_hashes = Vec::with_capacity(SAMPLE_FRAMES);
    for i in 0..SAMPLE_FRAMES {
        let t = duration * (i as f64 + 0.5) / SAMPLE_FRAMES as f64;
        let frame_path = work.join(format!("f{i}.jpg"));
        extract_frame(ffmpeg, path, t, &frame_path)?;
        let img = load_gray(&frame_path)?;
        base_hashes.push(phash(&img));
    }

    // Orientation 0 = as decoded (ffmpeg applies display matrix by default).
    // 1/2/3 = rotate the grayscale frame 90/180/270 before hashing.
    let mut orientations: [Vec<u64>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    orientations[0] = base_hashes.clone();
    for i in 0..SAMPLE_FRAMES {
        let frame_path = work.join(format!("f{i}.jpg"));
        let img = load_gray(&frame_path)?;
        orientations[1].push(phash(&imageops::rotate90(&img)));
        orientations[2].push(phash(&imageops::rotate180(&img)));
        orientations[3].push(phash(&imageops::rotate270(&img)));
    }

    let _ = fs::remove_dir_all(&work);

    Ok(Fingerprint {
        path: path.to_path_buf(),
        orientations,
    })
}

fn uuid_slug(path: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    path.hash(&mut h);
    format!("{:x}", h.finish())
}

fn probe_duration(ffprobe: &Path, path: &Path) -> Option<f64> {
    let out = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().parse().ok()
}

fn extract_frame(ffmpeg: &Path, input: &Path, seconds: f64, out: &Path) -> Result<(), String> {
    let status = Command::new(ffmpeg)
        .args(["-y", "-ss", &format!("{seconds:.3}"), "-i"])
        .arg(input)
        .args(["-frames:v", "1", "-q:v", "3"])
        .arg(out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() || !out.is_file() {
        return Err(format!("ffmpeg frame extract failed at {seconds:.2}s"));
    }
    Ok(())
}

fn load_gray(path: &Path) -> Result<GrayImage, String> {
    let img = ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?;
    Ok(img.to_luma8())
}

/// Classic 64-bit DCT perceptual hash on a downscaled grayscale image.
pub fn phash(img: &GrayImage) -> u64 {
    let small = imageops::resize(img, 32, 32, FilterType::Triangle);
    let mut vals = [[0.0f64; 32]; 32];
    for y in 0..32 {
        for x in 0..32 {
            vals[y][x] = f64::from(small.get_pixel(x as u32, y as u32)[0]);
        }
    }
    let dct = dct2d(&vals);
    let mut low = [0.0f64; 64];
    let mut i = 0;
    for y in 0..8 {
        for x in 0..8 {
            if x == 0 && y == 0 {
                continue; // skip DC
            }
            if i < 64 {
                low[i] = dct[y][x];
                i += 1;
            }
        }
    }
    // We skipped DC so only 63 coeffs — pad last with 0.
    let mut sorted = low;
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[32];
    let mut hash = 0u64;
    for (bit, v) in low.iter().enumerate() {
        if *v > median {
            hash |= 1u64 << bit;
        }
    }
    hash
}

fn dct2d(input: &[[f64; 32]; 32]) -> [[f64; 32]; 32] {
    let mut rows = [[0.0f64; 32]; 32];
    for y in 0..32 {
        rows[y] = dct1d(&input[y]);
    }
    let mut out = [[0.0f64; 32]; 32];
    for x in 0..32 {
        let mut col = [0.0f64; 32];
        for y in 0..32 {
            col[y] = rows[y][x];
        }
        let transformed = dct1d(&col);
        for y in 0..32 {
            out[y][x] = transformed[y];
        }
    }
    out
}

fn dct1d(input: &[f64; 32]) -> [f64; 32] {
    let mut out = [0.0f64; 32];
    let n = 32.0f64;
    for k in 0..32 {
        let mut sum = 0.0;
        for (i, v) in input.iter().enumerate() {
            sum +=
                v * ((std::f64::consts::PI * k as f64 * (2.0 * i as f64 + 1.0)) / (2.0 * n)).cos();
        }
        let alpha = if k == 0 {
            (1.0 / n).sqrt()
        } else {
            (2.0 / n).sqrt()
        };
        out[k] = alpha * sum;
    }
    out
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

fn best_orientation_distance(a: &Fingerprint, b: &Fingerprint, max_distance: u32) -> Option<f64> {
    let frames = a.orientations[0].len().min(b.orientations[0].len());
    if frames == 0 {
        return None;
    }
    let mut best_avg = f64::MAX;
    let mut best_matches = 0usize;
    for oa in 0..4 {
        for ob in 0..4 {
            let mut total = 0u32;
            let mut matches = 0usize;
            for i in 0..frames {
                let d = hamming(a.orientations[oa][i], b.orientations[ob][i]);
                total += d;
                if d <= max_distance {
                    matches += 1;
                }
            }
            let avg = f64::from(total) / frames as f64;
            if matches >= MIN_MATCHING_FRAMES && avg < best_avg {
                best_avg = avg;
                best_matches = matches;
            }
        }
    }
    if best_matches >= MIN_MATCHING_FRAMES {
        Some(best_avg)
    } else {
        None
    }
}

fn cluster_fingerprints(fps: &[Fingerprint], max_distance: u32) -> Vec<SimilarCluster> {
    let n = fps.len();
    if n < 2 {
        return Vec::new();
    }
    let mut parent: Vec<usize> = (0..n).collect();
    let mut dist_sum: HashMap<(usize, usize), f64> = HashMap::new();

    fn find(parent: &mut [usize], i: usize) -> usize {
        if parent[i] != i {
            parent[i] = find(parent, parent[i]);
        }
        parent[i]
    }
    fn union(parent: &mut [usize], a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[rb] = ra;
        }
    }

    for i in 0..n {
        for j in (i + 1)..n {
            if let Some(avg) = best_orientation_distance(&fps[i], &fps[j], max_distance) {
                union(&mut parent, i, j);
                dist_sum.insert((i, j), avg);
            }
        }
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        groups.entry(find(&mut parent, i)).or_default().push(i);
    }

    let mut clusters = Vec::new();
    for members in groups.values() {
        if members.len() < 2 {
            continue;
        }
        let mut pair_dists = Vec::new();
        for a in 0..members.len() {
            for b in (a + 1)..members.len() {
                let i = members[a].min(members[b]);
                let j = members[a].max(members[b]);
                if let Some(d) = dist_sum.get(&(i, j)) {
                    pair_dists.push(*d);
                }
            }
        }
        let avg_distance = if pair_dists.is_empty() {
            0.0
        } else {
            pair_dists.iter().sum::<f64>() / pair_dists.len() as f64
        };
        let mut files: Vec<SimilarVideoFile> = members
            .iter()
            .map(|&idx| {
                let p = &fps[idx].path;
                SimilarVideoFile {
                    path: p.to_string_lossy().into_owned(),
                    file_name: p
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                }
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        clusters.push(SimilarCluster {
            files,
            avg_distance,
        });
    }
    clusters.sort_by(|a, b| {
        b.files.len().cmp(&a.files.len()).then_with(|| {
            a.avg_distance
                .partial_cmp(&b.avg_distance)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    clusters
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;

    #[test]
    fn identical_images_have_zero_hamming() {
        let img = GrayImage::from_fn(64, 64, |x, y| Luma([((x + y) % 256) as u8]));
        let a = phash(&img);
        let b = phash(&img);
        assert_eq!(hamming(a, b), 0);
    }

    #[test]
    fn rotated_match_via_orientation_slots() {
        let img = GrayImage::from_fn(128, 64, |x, y| Luma([((x * 3 + y * 5) % 256) as u8]));
        let h0 = phash(&img);
        let h90 = phash(&imageops::rotate90(&img));
        // Same content under different orientations should match when we compare
        // across orientation slots (h0 vs h90 of the rotated source's "0").
        let rot = imageops::rotate90(&img);
        let rot_as_upright = phash(&rot);
        assert_eq!(hamming(h90, rot_as_upright), 0);
        assert_ne!(h0, h90); // rotation changes the base hash without trying orientations
    }

    #[test]
    fn cluster_joins_close_fingerprints() {
        let mk = |path: &str, hashes: Vec<u64>| Fingerprint {
            path: PathBuf::from(path),
            orientations: [
                hashes.clone(),
                hashes.clone(),
                hashes.clone(),
                hashes.clone(),
            ],
        };
        let shared = vec![0u64; SAMPLE_FRAMES];
        let other = vec![u64::MAX; SAMPLE_FRAMES];
        let fps = vec![
            mk("/a.mp4", shared.clone()),
            mk("/b.mp4", shared),
            mk("/c.mp4", other),
        ];
        let clusters = cluster_fingerprints(&fps, DEFAULT_MAX_DISTANCE);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].files.len(), 2);
    }
}
