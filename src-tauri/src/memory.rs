//! The persistent memory bank.
//!
//! SQLite holds everything we need to recognize a file across scans:
//!   - `folders` — every root we've ever scanned, plus per-scan stats.
//!   - `files` — one row per (hash, normalized_path) we've ever observed,
//!     including size, modified-time, source root, and last-seen scan id.
//!   - `scans` — one row per scan (dry or real), with start/end timestamps,
//!     roots scanned, and high-level counts.
//!
//! On every scan we look up each candidate file by `(normalized_path, size,
//! modified_ns)` first; on hit we trust the cached full hash and don't reread
//! bytes. On a stale-path hit we fall back to `(file_name, size, modified_ns)`
//! only when the previous path no longer exists, catching pure moves without
//! trusting same-name copies at different existing paths.

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::error::{AppError, AppResult};
use crate::reports::{self, FileChange, ScanFileSnapshot};

const SCHEMA_VERSION: i32 = 3;

#[derive(Clone)]
pub struct MemoryBank {
    inner: Arc<Mutex<Connection>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FolderRow {
    pub id: i64,
    pub path: String,
    pub normalized_path: String,
    pub first_seen_ts: String,
    pub last_scan_ts: Option<String>,
    pub last_file_count: i64,
    pub last_total_bytes: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileRow {
    pub hash: String,
    pub path: String,
    pub normalized_path: String,
    pub source_root: String,
    pub size: i64,
    pub modified_ns: i64,
    pub first_seen_ts: String,
    pub last_seen_ts: String,
    pub last_scan_id: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScanRow {
    pub id: i64,
    pub started_ts: String,
    pub ended_ts: Option<String>,
    pub mode: String,
    pub roots: Vec<String>,
    pub files_seen: i64,
    pub bytes_seen: i64,
    pub hashes_reused: i64,
    pub hashes_computed: i64,
    pub duplicate_groups: i64,
    pub wasted_bytes: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct MemoryStats {
    pub folders: i64,
    pub files: i64,
    pub distinct_hashes: i64,
    pub duplicate_hashes: i64,
    pub last_scan_ts: Option<String>,
    pub db_path: String,
}

/// Used by the scanner to ask "have I seen this exact file before?" without
/// rereading bytes.
#[derive(Clone, Debug)]
pub enum CacheHit {
    /// `(normalized_path, size, modified_ns)` matched. Strongest signal.
    Primary {
        hash: String,
        source_root: String,
    },
    /// `(file_name, size, modified_ns)` matched, path differs — probably a move.
    /// Caller should record the new path against the same hash.
    Filename {
        hash: String,
        source_root: String,
        previous_path: String,
        previous_normalized_path: String,
    },
    Miss,
}

impl MemoryBank {
    pub fn open(db_path: &Path) -> AppResult<Self> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;
        let bank = Self {
            inner: Arc::new(Mutex::new(conn)),
        };
        bank.migrate()?;
        Ok(bank)
    }

    fn migrate(&self) -> AppResult<()> {
        let conn = self.inner.lock();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS folders (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 path TEXT NOT NULL,
                 normalized_path TEXT NOT NULL UNIQUE,
                 first_seen_ts TEXT NOT NULL,
                 last_scan_ts TEXT,
                 last_file_count INTEGER NOT NULL DEFAULT 0,
                 last_total_bytes INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS scans (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 started_ts TEXT NOT NULL,
                 ended_ts TEXT,
                 mode TEXT NOT NULL,
                 roots_json TEXT NOT NULL,
                 files_seen INTEGER NOT NULL DEFAULT 0,
                 bytes_seen INTEGER NOT NULL DEFAULT 0,
                 hashes_reused INTEGER NOT NULL DEFAULT 0,
                 hashes_computed INTEGER NOT NULL DEFAULT 0,
                 duplicate_groups INTEGER NOT NULL DEFAULT 0,
                 wasted_bytes INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS files (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 hash TEXT NOT NULL,
                 path TEXT NOT NULL,
                 normalized_path TEXT NOT NULL,
                 file_name TEXT NOT NULL,
                 source_root TEXT NOT NULL,
                 size INTEGER NOT NULL,
                 modified_ns INTEGER NOT NULL,
                 first_seen_ts TEXT NOT NULL,
                 last_seen_ts TEXT NOT NULL,
                 last_scan_id INTEGER REFERENCES scans(id),
                 UNIQUE(normalized_path)
             );
             CREATE INDEX IF NOT EXISTS files_hash_idx ON files(hash);
             CREATE INDEX IF NOT EXISTS files_filename_idx ON files(file_name, size, modified_ns);
             CREATE INDEX IF NOT EXISTS files_lookup_idx ON files(normalized_path, size, modified_ns);
             CREATE TABLE IF NOT EXISTS scan_files (
                 scan_id INTEGER NOT NULL REFERENCES scans(id) ON DELETE CASCADE,
                 hash TEXT NOT NULL,
                 path TEXT NOT NULL,
                 normalized_path TEXT NOT NULL,
                 file_name TEXT NOT NULL,
                 source_root TEXT NOT NULL,
                 size INTEGER NOT NULL,
                 modified_ns INTEGER NOT NULL,
                 PRIMARY KEY(scan_id, normalized_path)
             );
             CREATE INDEX IF NOT EXISTS scan_files_scan_idx ON scan_files(scan_id);
             CREATE INDEX IF NOT EXISTS scan_files_hash_idx ON scan_files(scan_id, hash);
             ",
        )?;
        let current: Option<i32> = conn
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
                r.get(0)
            })
            .optional()?;
        match current {
            None => {
                conn.execute(
                    "INSERT INTO schema_version(version) VALUES(?1)",
                    params![SCHEMA_VERSION],
                )?;
            }
            Some(v) if v == SCHEMA_VERSION => {}
            Some(1) => {
                conn.execute("UPDATE schema_version SET version = ?1", params![2])?;
                Self::migrate_v2_to_v3(&conn)?;
            }
            Some(2) => {
                Self::migrate_v2_to_v3(&conn)?;
            }
            Some(other) => {
                return Err(AppError::BadInput(format!(
                    "memory bank schema version {other} not supported (expected {SCHEMA_VERSION}); \
                     delete the file or run a migration."
                )));
            }
        }
        Ok(())
    }

    fn migrate_v2_to_v3(conn: &Connection) -> AppResult<()> {
        let legacy_rows: i64 = conn.query_row(
            "SELECT
                (SELECT COUNT(*) FROM files WHERE hash LIKE 'p:%') +
                (SELECT COUNT(*) FROM scan_files WHERE hash LIKE 'p:%')",
            [],
            |r| r.get(0),
        )?;
        if legacy_rows > 0 {
            conn.execute("DELETE FROM scan_files", [])?;
            conn.execute("UPDATE files SET last_scan_id = NULL", [])?;
            conn.execute("DELETE FROM scans", [])?;
            conn.execute("DELETE FROM files WHERE hash LIKE 'p:%'", [])?;
        }
        conn.execute(
            "UPDATE schema_version SET version = ?1",
            params![SCHEMA_VERSION],
        )?;
        Ok(())
    }

    pub fn remember_folder(&self, raw_path: &str, normalized: &str) -> AppResult<i64> {
        let conn = self.inner.lock();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO folders(path, normalized_path, first_seen_ts) VALUES(?1, ?2, ?3)
             ON CONFLICT(normalized_path) DO UPDATE SET path = excluded.path",
            params![raw_path, normalized, now],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM folders WHERE normalized_path = ?1",
            params![normalized],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn update_folder_stats(
        &self,
        normalized: &str,
        file_count: i64,
        total_bytes: i64,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.inner.lock();
        conn.execute(
            "UPDATE folders SET last_scan_ts = ?1, last_file_count = ?2, last_total_bytes = ?3
             WHERE normalized_path = ?4",
            params![now, file_count, total_bytes, normalized],
        )?;
        Ok(())
    }

    /// Single-folder lookup by normalized path. Returns None if we've never
    /// scanned this folder before.
    pub fn peek_folder(&self, normalized: &str) -> AppResult<Option<FolderRow>> {
        let conn = self.inner.lock();
        let row = conn
            .query_row(
                "SELECT id, path, normalized_path, first_seen_ts, last_scan_ts,
                        last_file_count, last_total_bytes
                 FROM folders WHERE normalized_path = ?1",
                rusqlite::params![normalized],
                |r| {
                    Ok(FolderRow {
                        id: r.get(0)?,
                        path: r.get(1)?,
                        normalized_path: r.get(2)?,
                        first_seen_ts: r.get(3)?,
                        last_scan_ts: r.get(4)?,
                        last_file_count: r.get(5)?,
                        last_total_bytes: r.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_folders(&self) -> AppResult<Vec<FolderRow>> {
        let conn = self.inner.lock();
        let mut stmt = conn.prepare(
            "SELECT id, path, normalized_path, first_seen_ts, last_scan_ts,
                    last_file_count, last_total_bytes
             FROM folders ORDER BY last_scan_ts DESC NULLS LAST, id DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(FolderRow {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    normalized_path: r.get(2)?,
                    first_seen_ts: r.get(3)?,
                    last_scan_ts: r.get(4)?,
                    last_file_count: r.get(5)?,
                    last_total_bytes: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn forget_folder(&self, normalized: &str) -> AppResult<()> {
        let conn = self.inner.lock();
        conn.execute(
            "DELETE FROM folders WHERE normalized_path = ?1",
            params![normalized],
        )?;
        Ok(())
    }

    /// Return up to `limit` known records with this exact content hash. Used
    /// after a brand-new hash to decide whether a file moved or was renamed.
    pub fn records_for_hash(&self, hash: &str, limit: i64) -> AppResult<Vec<FileRow>> {
        let conn = self.inner.lock();
        let mut stmt = conn.prepare(
            "SELECT hash, path, normalized_path, source_root, size, modified_ns,
                    first_seen_ts, last_seen_ts, last_scan_id
             FROM files WHERE hash = ?1 LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![hash, limit], |r| {
                Ok(FileRow {
                    hash: r.get(0)?,
                    path: r.get(1)?,
                    normalized_path: r.get(2)?,
                    source_root: r.get(3)?,
                    size: r.get(4)?,
                    modified_ns: r.get(5)?,
                    first_seen_ts: r.get(6)?,
                    last_seen_ts: r.get(7)?,
                    last_scan_id: r.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn lookup(
        &self,
        normalized_path: &str,
        file_name: &str,
        size: i64,
        modified_ns: i64,
    ) -> AppResult<CacheHit> {
        let conn = self.inner.lock();
        let primary: Option<(String, String)> = conn
            .query_row(
                "SELECT hash, source_root FROM files
                 WHERE normalized_path = ?1 AND size = ?2 AND modified_ns = ?3 LIMIT 1",
                params![normalized_path, size, modified_ns],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((hash, source_root)) = primary {
            return Ok(CacheHit::Primary { hash, source_root });
        }
        let secondary: Option<(String, String, String, String)> = conn
            .query_row(
                "SELECT hash, source_root, path, normalized_path FROM files
                 WHERE file_name = ?1 AND size = ?2 AND modified_ns = ?3 LIMIT 1",
                params![file_name, size, modified_ns],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        if let Some((hash, source_root, previous_path, previous_normalized_path)) = secondary {
            return Ok(CacheHit::Filename {
                hash,
                source_root,
                previous_path,
                previous_normalized_path,
            });
        }
        Ok(CacheHit::Miss)
    }

    pub fn upsert_file(
        &self,
        hash: &str,
        path: &str,
        normalized_path: &str,
        file_name: &str,
        source_root: &str,
        size: i64,
        modified_ns: i64,
        scan_id: i64,
    ) -> AppResult<()> {
        let conn = self.inner.lock();
        let now = Utc::now().to_rfc3339();
        // Both writes share a transaction so a crash between them never leaves
        // `files` updated but the corresponding `scan_files` row missing (which
        // would produce phantom Gone entries on the next diff).
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO files(hash, path, normalized_path, file_name, source_root,
                               size, modified_ns, first_seen_ts, last_seen_ts, last_scan_id)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)
             ON CONFLICT(normalized_path) DO UPDATE SET
                 hash = excluded.hash,
                 path = excluded.path,
                 file_name = excluded.file_name,
                 source_root = excluded.source_root,
                 size = excluded.size,
                 modified_ns = excluded.modified_ns,
                 last_seen_ts = excluded.last_seen_ts,
                 last_scan_id = excluded.last_scan_id",
            params![
                hash,
                path,
                normalized_path,
                file_name,
                source_root,
                size,
                modified_ns,
                now,
                scan_id
            ],
        )?;
        tx.execute(
            "INSERT INTO scan_files(scan_id, hash, path, normalized_path, file_name,
                                    source_root, size, modified_ns)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(scan_id, normalized_path) DO UPDATE SET
                 hash = excluded.hash,
                 path = excluded.path,
                 file_name = excluded.file_name,
                 source_root = excluded.source_root,
                 size = excluded.size,
                 modified_ns = excluded.modified_ns",
            params![
                scan_id,
                hash,
                path,
                normalized_path,
                file_name,
                source_root,
                size,
                modified_ns
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Re-tag a file when only its path changed (move/rename). Keeps `first_seen_ts`.
    pub fn relocate_file(
        &self,
        previous_normalized: &str,
        new_path: &str,
        new_normalized: &str,
        new_file_name: &str,
        new_source_root: &str,
        scan_id: i64,
    ) -> AppResult<()> {
        let conn = self.inner.lock();
        let now = Utc::now().to_rfc3339();
        // Delete-then-insert under transaction so the UNIQUE(normalized_path)
        // constraint never conflicts when path didn't actually change.
        let tx = conn.unchecked_transaction()?;
        // Pull the existing record to preserve hash + first_seen.
        let existing: Option<(String, String, i64, i64)> = tx
            .query_row(
                "SELECT hash, first_seen_ts, size, modified_ns FROM files WHERE normalized_path = ?1",
                params![previous_normalized],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        if let Some((hash, first_seen, size, modified_ns)) = existing {
            tx.execute(
                "DELETE FROM files WHERE normalized_path = ?1",
                params![previous_normalized],
            )?;
            tx.execute(
                "INSERT INTO files(hash, path, normalized_path, file_name, source_root,
                                   size, modified_ns, first_seen_ts, last_seen_ts, last_scan_id)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(normalized_path) DO UPDATE SET
                     hash = excluded.hash,
                     path = excluded.path,
                     file_name = excluded.file_name,
                     source_root = excluded.source_root,
                     last_seen_ts = excluded.last_seen_ts,
                     last_scan_id = excluded.last_scan_id",
                params![
                    hash,
                    new_path,
                    new_normalized,
                    new_file_name,
                    new_source_root,
                    size,
                    modified_ns,
                    first_seen,
                    now,
                    scan_id
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_file_by_path(&self, normalized_path: &str) -> AppResult<()> {
        let conn = self.inner.lock();
        // Remove from scan_files first so the next diff emits GONE instead of
        // treating the quarantine copy as a MOVED version of this file.
        conn.execute(
            "DELETE FROM scan_files WHERE normalized_path = ?1",
            params![normalized_path],
        )?;
        conn.execute(
            "DELETE FROM files WHERE normalized_path = ?1",
            params![normalized_path],
        )?;
        Ok(())
    }

    pub fn start_scan(&self, mode: &str, roots: &[String]) -> AppResult<i64> {
        let conn = self.inner.lock();
        let now = Utc::now().to_rfc3339();
        let roots_json = serde_json::to_string(roots)?;
        conn.execute(
            "INSERT INTO scans(started_ts, mode, roots_json) VALUES(?1, ?2, ?3)",
            params![now, mode, roots_json],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn finish_scan(
        &self,
        scan_id: i64,
        files_seen: i64,
        bytes_seen: i64,
        hashes_reused: i64,
        hashes_computed: i64,
        duplicate_groups: i64,
        wasted_bytes: i64,
    ) -> AppResult<()> {
        let conn = self.inner.lock();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE scans SET ended_ts = ?1, files_seen = ?2, bytes_seen = ?3,
                              hashes_reused = ?4, hashes_computed = ?5,
                              duplicate_groups = ?6, wasted_bytes = ?7
             WHERE id = ?8",
            params![
                now,
                files_seen,
                bytes_seen,
                hashes_reused,
                hashes_computed,
                duplicate_groups,
                wasted_bytes,
                scan_id
            ],
        )?;
        Ok(())
    }

    pub fn recent_scans(&self, limit: i64) -> AppResult<Vec<ScanRow>> {
        let conn = self.inner.lock();
        let mut stmt = conn.prepare(
            "SELECT id, started_ts, ended_ts, mode, roots_json,
                    files_seen, bytes_seen, hashes_reused, hashes_computed,
                    duplicate_groups, wasted_bytes
             FROM scans ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], |r| {
                let roots_json: String = r.get(4)?;
                let roots: Vec<String> = serde_json::from_str(&roots_json).unwrap_or_default();
                Ok(ScanRow {
                    id: r.get(0)?,
                    started_ts: r.get(1)?,
                    ended_ts: r.get(2)?,
                    mode: r.get(3)?,
                    roots,
                    files_seen: r.get(5)?,
                    bytes_seen: r.get(6)?,
                    hashes_reused: r.get(7)?,
                    hashes_computed: r.get(8)?,
                    duplicate_groups: r.get(9)?,
                    wasted_bytes: r.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn scan_files(&self, scan_id: i64) -> AppResult<Vec<ScanFileSnapshot>> {
        let conn = self.inner.lock();
        let mut stmt = conn.prepare(
            "SELECT hash, path, normalized_path, file_name, source_root, size, modified_ns
             FROM scan_files WHERE scan_id = ?1 ORDER BY normalized_path",
        )?;
        let rows = stmt
            .query_map(params![scan_id], |r| {
                Ok(ScanFileSnapshot {
                    hash: r.get(0)?,
                    path: r.get(1)?,
                    normalized_path: r.get(2)?,
                    file_name: r.get(3)?,
                    source_root: r.get(4)?,
                    size: r.get(5)?,
                    modified_ns: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn scan_changes(&self, scan_id: i64) -> AppResult<Vec<FileChange>> {
        let prev_id = {
            let conn = self.inner.lock();
            conn.query_row(
                "SELECT id FROM scans
                 WHERE id < ?1 AND ended_ts IS NOT NULL
                 ORDER BY id DESC LIMIT 1",
                params![scan_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        };
        let Some(prev_id) = prev_id else {
            return Ok(Vec::new());
        };
        let prev = self.scan_files(prev_id)?;
        let curr = self.scan_files(scan_id)?;
        Ok(reports::compute_changes(&prev, &curr))
    }

    pub fn stats(&self, db_path: &Path) -> AppResult<MemoryStats> {
        let conn = self.inner.lock();
        let folders: i64 = conn.query_row("SELECT COUNT(*) FROM folders", [], |r| r.get(0))?;
        let files: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        let distinct_hashes: i64 =
            conn.query_row("SELECT COUNT(DISTINCT hash) FROM files", [], |r| r.get(0))?;
        let duplicate_hashes: i64 = conn.query_row(
            "SELECT COUNT(*) FROM (
                SELECT hash FROM files
                GROUP BY hash
                HAVING COUNT(*) > 1
             )",
            [],
            |r| r.get(0),
        )?;
        let last_scan_ts: Option<String> = conn
            .query_row(
                "SELECT ended_ts FROM scans WHERE ended_ts IS NOT NULL ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(MemoryStats {
            folders,
            files,
            distinct_hashes,
            duplicate_hashes,
            last_scan_ts,
            db_path: db_path.to_string_lossy().into_owned(),
        })
    }
}
