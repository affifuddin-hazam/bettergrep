//! SQLite persistence: the single source of truth for lineage and events.
//!
//! The schema is versioned with `PRAGMA user_version` (currently 1) and migrated
//! forward on open.  See `docs/schema.md`.

use std::cell::Cell;
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde_json::Value;

use crate::fileinfo::FileInfo;
use crate::paths::{descendant_range, file_name, key, norm, parent_key};

pub const SCHEMA_VERSION: i64 = 1;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS files (
    id          INTEGER PRIMARY KEY,
    path        TEXT    NOT NULL,
    path_key    TEXT    NOT NULL,
    parent_key  TEXT    NOT NULL,
    name        TEXT    NOT NULL,
    is_dir      INTEGER NOT NULL DEFAULT 0,
    size        INTEGER,
    mtime       REAL,
    volume      INTEGER,
    inode       INTEGER,
    sha256      TEXT,
    status      TEXT    NOT NULL DEFAULT 'present'
                CHECK (status IN ('present', 'deleted', 'missing')),
    copied_from INTEGER REFERENCES files(id) ON DELETE SET NULL,
    first_seen  REAL    NOT NULL,
    last_seen   REAL    NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_files_present_path ON files(path_key) WHERE status = 'present';
CREATE INDEX IF NOT EXISTS ix_files_path     ON files(path_key);
CREATE INDEX IF NOT EXISTS ix_files_parent   ON files(parent_key, status);
CREATE INDEX IF NOT EXISTS ix_files_identity ON files(volume, inode) WHERE status = 'present';
CREATE INDEX IF NOT EXISTS ix_files_size     ON files(size) WHERE status = 'present';
CREATE INDEX IF NOT EXISTS ix_files_sha      ON files(sha256);
CREATE INDEX IF NOT EXISTS ix_files_name     ON files(name COLLATE NOCASE);

CREATE TABLE IF NOT EXISTS operations (
    id          TEXT PRIMARY KEY,
    ts          REAL NOT NULL,
    kind        TEXT NOT NULL,
    actor       TEXT,
    description TEXT,
    undone_by   TEXT
);

CREATE TABLE IF NOT EXISTS events (
    id              INTEGER PRIMARY KEY,
    ts              REAL    NOT NULL,
    kind            TEXT    NOT NULL,
    source          TEXT    NOT NULL,
    file_id         INTEGER REFERENCES files(id) ON DELETE SET NULL,
    path            TEXT,
    path_key        TEXT,
    folder_key      TEXT,
    dest_path       TEXT,
    dest_key        TEXT,
    dest_folder_key TEXT,
    related_id      INTEGER,
    op_id           TEXT REFERENCES operations(id) ON DELETE SET NULL,
    actor           TEXT,
    reason          TEXT,
    details         TEXT
);
CREATE INDEX IF NOT EXISTS ix_events_ts          ON events(ts);
CREATE INDEX IF NOT EXISTS ix_events_file        ON events(file_id, ts);
CREATE INDEX IF NOT EXISTS ix_events_folder      ON events(folder_key, ts);
CREATE INDEX IF NOT EXISTS ix_events_dest_folder ON events(dest_folder_key, ts);
CREATE INDEX IF NOT EXISTS ix_events_path        ON events(path_key);
CREATE INDEX IF NOT EXISTS ix_events_dest        ON events(dest_key);
CREATE INDEX IF NOT EXISTS ix_events_op          ON events(op_id);
CREATE INDEX IF NOT EXISTS ix_events_kind        ON events(kind, ts);

CREATE TABLE IF NOT EXISTS sessions (
    id        INTEGER PRIMARY KEY,
    label     TEXT,
    command   TEXT,
    started   REAL NOT NULL,
    ended     REAL,
    status    TEXT NOT NULL DEFAULT 'running'
              CHECK (status IN ('running', 'stopping', 'finished', 'failed')),
    pid       INTEGER,
    exit_code INTEGER,
    roots     TEXT,
    summary   TEXT
);

CREATE TABLE IF NOT EXISTS session_events (
    id         INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ts         REAL NOT NULL,
    source     TEXT NOT NULL,
    kind       TEXT NOT NULL,
    path       TEXT NOT NULL,
    dest_path  TEXT,
    details    TEXT
);
CREATE INDEX IF NOT EXISTS ix_session_events ON session_events(session_id, source, kind);
"#;

pub const FILE_EVENT_KINDS: [&str; 8] =
    ["discovered", "created", "modified", "renamed", "moved", "copied", "deleted", "missing"];
pub const REGISTRY_EVENT_KINDS: [&str; 5] =
    ["reg_key_created", "reg_key_deleted", "reg_value_created", "reg_value_changed", "reg_value_deleted"];

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or_default()
}

/// A short random-enough identifier for operations.
pub fn new_id() -> String {
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seed = format!("{}-{}-{}", std::process::id(), now().to_bits(), COUNTER.fetch_add(1, Ordering::Relaxed));
    hex::encode(Sha256::digest(seed.as_bytes()))[..12].to_string()
}

#[derive(Clone, Debug)]
pub struct FileRow {
    pub id: i64,
    pub path: String,
    pub path_key: String,
    pub parent_key: String,
    pub name: String,
    pub is_dir: bool,
    pub size: Option<i64>,
    pub mtime: Option<f64>,
    pub volume: Option<i64>,
    pub inode: Option<i64>,
    pub sha256: Option<String>,
    pub status: String,
    pub copied_from: Option<i64>,
    pub first_seen: f64,
    pub last_seen: f64,
}

impl FileRow {
    pub fn from_row(r: &Row) -> rusqlite::Result<Self> {
        Ok(FileRow {
            id: r.get("id")?,
            path: r.get("path")?,
            path_key: r.get("path_key")?,
            parent_key: r.get("parent_key")?,
            name: r.get("name")?,
            is_dir: r.get::<_, i64>("is_dir")? != 0,
            size: r.get("size")?,
            mtime: r.get("mtime")?,
            volume: r.get("volume")?,
            inode: r.get("inode")?,
            sha256: r.get("sha256")?,
            status: r.get("status")?,
            copied_from: r.get("copied_from")?,
            first_seen: r.get("first_seen")?,
            last_seen: r.get("last_seen")?,
        })
    }

    pub fn present(&self) -> bool {
        self.status == "present"
    }

    pub fn info(&self) -> FileInfo {
        FileInfo {
            path: self.path.clone(),
            is_dir: self.is_dir,
            size: self.size.unwrap_or(0) as u64,
            mtime: self.mtime.unwrap_or(0.0),
            volume: self.volume.unwrap_or(0) as u64,
            inode: self.inode.unwrap_or(0) as u64,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EventRow {
    pub id: i64,
    pub ts: f64,
    pub kind: String,
    pub source: String,
    pub file_id: Option<i64>,
    pub path: Option<String>,
    pub path_key: Option<String>,
    pub folder_key: Option<String>,
    pub dest_path: Option<String>,
    pub dest_key: Option<String>,
    pub dest_folder_key: Option<String>,
    pub related_id: Option<i64>,
    pub op_id: Option<String>,
    pub actor: Option<String>,
    pub reason: Option<String>,
    pub details: Option<String>,
}

impl EventRow {
    pub fn from_row(r: &Row) -> rusqlite::Result<Self> {
        Ok(EventRow {
            id: r.get("id")?,
            ts: r.get("ts")?,
            kind: r.get("kind")?,
            source: r.get("source")?,
            file_id: r.get("file_id")?,
            path: r.get("path")?,
            path_key: r.get("path_key")?,
            folder_key: r.get("folder_key")?,
            dest_path: r.get("dest_path")?,
            dest_key: r.get("dest_key")?,
            dest_folder_key: r.get("dest_folder_key")?,
            related_id: r.get("related_id")?,
            op_id: r.get("op_id")?,
            actor: r.get("actor")?,
            reason: r.get("reason")?,
            details: r.get("details")?,
        })
    }
}

#[derive(Clone, Debug)]
pub struct SessionRow {
    pub id: i64,
    pub label: Option<String>,
    pub command: Option<String>,
    pub started: f64,
    pub ended: Option<f64>,
    pub status: String,
    pub pid: Option<i64>,
    pub exit_code: Option<i64>,
    pub roots: Option<String>,
    pub summary: Option<String>,
}

impl SessionRow {
    fn from_row(r: &Row) -> rusqlite::Result<Self> {
        Ok(SessionRow {
            id: r.get("id")?,
            label: r.get("label")?,
            command: r.get("command")?,
            started: r.get("started")?,
            ended: r.get("ended")?,
            status: r.get("status")?,
            pid: r.get("pid")?,
            exit_code: r.get("exit_code")?,
            roots: r.get("roots")?,
            summary: r.get("summary")?,
        })
    }
}

/// An event to record.  Build with struct-update syntax:
/// `NewEvent { kind: "moved", source, path: Some(old), ..Default::default() }`.
#[derive(Default)]
pub struct NewEvent<'a> {
    pub kind: &'a str,
    pub source: &'a str,
    pub ts: Option<f64>,
    pub file_id: Option<i64>,
    pub path: Option<&'a str>,
    pub dest_path: Option<&'a str>,
    pub related_id: Option<i64>,
    pub op_id: Option<&'a str>,
    pub actor: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub details: Option<Value>,
}

/// Dynamic SQL parameters.
pub fn sql_params(values: Vec<SqlValue>) -> rusqlite::ParamsFromIter<Vec<SqlValue>> {
    params_from_iter(values)
}

pub struct Store {
    pub conn: Connection,
    pub path: String,
    depth: Cell<u32>,
}

impl Store {
    pub fn open(path: &str) -> Result<Store> {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(30))?;
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
        let store = Store { conn, path: path.to_string(), depth: Cell::new(0) };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            bail!(
                "database {} has schema v{version}; this bettergrep understands up to v{SCHEMA_VERSION}. Upgrade bettergrep.",
                self.path
            );
        }
        if version < SCHEMA_VERSION {
            self.conn.execute_batch(SCHEMA)?;
            self.conn.execute_batch(&format!("PRAGMA user_version={SCHEMA_VERSION}"))?;
        }
        Ok(())
    }

    /// Start (or join) a write transaction.  Pair every call with [`Store::end`].
    pub fn begin(&self) -> Result<()> {
        if self.depth.get() == 0 {
            self.conn.execute_batch("BEGIN IMMEDIATE")?;
        }
        self.depth.set(self.depth.get() + 1);
        Ok(())
    }

    /// Leave a transaction; the outermost `end` commits (or rolls back if `!ok`).
    pub fn end(&self, ok: bool) -> Result<()> {
        let depth = self.depth.get().saturating_sub(1);
        self.depth.set(depth);
        if depth == 0 {
            self.conn.execute_batch(if ok { "COMMIT" } else { "ROLLBACK" })?;
        }
        Ok(())
    }

    /// Run `f` atomically (`BEGIN IMMEDIATE`).  Nested calls join the outer transaction.
    pub fn tx<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.begin()?;
        match f() {
            Ok(v) => {
                self.end(true)?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.end(false);
                Err(e)
            }
        }
    }

    // -- files ------------------------------------------------------------

    pub fn files_where(&self, clause: &str, values: Vec<SqlValue>) -> Result<Vec<FileRow>> {
        let mut stmt = self.conn.prepare_cached(&format!("SELECT * FROM files WHERE {clause}"))?;
        let rows = stmt.query_map(sql_params(values), FileRow::from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn one_file(&self, clause: &str, values: Vec<SqlValue>) -> Result<Option<FileRow>> {
        Ok(self.files_where(clause, values)?.into_iter().next())
    }

    pub fn get_file(&self, id: i64) -> Result<Option<FileRow>> {
        self.one_file("id = ?", vec![id.into()])
    }

    pub fn get_present(&self, path: &str) -> Result<Option<FileRow>> {
        self.one_file("path_key = ? AND status = 'present'", vec![key(path).into()])
    }

    /// Most recent file (any status) recorded at `path`.
    pub fn get_latest_at(&self, path: &str) -> Result<Option<FileRow>> {
        self.one_file("path_key = ? ORDER BY status = 'present' DESC, last_seen DESC LIMIT 1", vec![key(path).into()])
    }

    pub fn find_present_by_identity(&self, volume: u64, inode: u64) -> Result<Option<FileRow>> {
        self.one_file(
            "volume = ? AND inode = ? AND status = 'present' LIMIT 1",
            vec![(volume as i64).into(), (inode as i64).into()],
        )
    }

    pub fn find_present_by_size(&self, size: u64, limit: i64) -> Result<Vec<FileRow>> {
        self.files_where(
            "size = ? AND status = 'present' AND is_dir = 0 ORDER BY last_seen DESC LIMIT ?",
            vec![(size as i64).into(), limit.into()],
        )
    }

    pub fn present_descendants(&self, dir: &str) -> Result<Vec<FileRow>> {
        let (low, high) = descendant_range(&key(dir));
        self.files_where(
            "path_key >= ? AND path_key < ? AND status = 'present' ORDER BY path_key",
            vec![low.into(), high.into()],
        )
    }

    pub fn insert_file(&self, info: &FileInfo, ts: f64, sha256: Option<&str>, copied_from: Option<i64>) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO files (path, path_key, parent_key, name, is_dir, size, mtime, volume, inode, sha256, status,
                                copied_from, first_seen, last_seen)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'present', ?, ?, ?)",
            params![
                info.path,
                key(&info.path),
                parent_key(&info.path),
                file_name(&info.path),
                info.is_dir as i64,
                info.size as i64,
                info.mtime,
                info.volume as i64,
                info.inode as i64,
                sha256,
                copied_from,
                ts,
                ts
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Point a record at a new path (refreshing stat data if `info` is given).
    pub fn relocate(&self, id: i64, new_path: &str, ts: f64, info: Option<&FileInfo>) -> Result<()> {
        let new_path = norm(new_path);
        self.conn.execute(
            "UPDATE files SET path = ?, path_key = ?, parent_key = ?, name = ?, status = 'present', last_seen = ?
             WHERE id = ?",
            params![new_path, key(&new_path), parent_key(&new_path), file_name(&new_path), ts, id],
        )?;
        if let Some(info) = info {
            self.conn.execute(
                "UPDATE files SET size = ?, mtime = ?, volume = ?, inode = ? WHERE id = ?",
                params![info.size as i64, info.mtime, info.volume as i64, info.inode as i64, id],
            )?;
        }
        Ok(())
    }

    pub fn update_content(&self, id: i64, info: &FileInfo, ts: f64, sha256: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET size = ?, mtime = ?, volume = ?, inode = ?, sha256 = ?, last_seen = ? WHERE id = ?",
            params![info.size as i64, info.mtime, info.volume as i64, info.inode as i64, sha256, ts, id],
        )?;
        Ok(())
    }

    pub fn set_sha(&self, id: i64, sha256: Option<&str>) -> Result<()> {
        self.conn.execute("UPDATE files SET sha256 = ? WHERE id = ?", params![sha256, id])?;
        Ok(())
    }

    pub fn set_status(&self, id: i64, status: &str, ts: f64) -> Result<()> {
        self.conn.execute("UPDATE files SET status = ?, last_seen = ? WHERE id = ?", params![status, ts, id])?;
        Ok(())
    }

    /// Fold record `drop` into `keep` (both describe the same logical file).
    pub fn merge_into(&self, keep: i64, drop: i64) -> Result<()> {
        self.conn.execute("UPDATE events SET file_id = ? WHERE file_id = ?", params![keep, drop])?;
        self.conn
            .execute("UPDATE events SET related_id = NULL WHERE related_id = ? AND file_id = ?", params![keep, keep])?;
        self.conn.execute("UPDATE files SET copied_from = ? WHERE copied_from = ?", params![keep, drop])?;
        self.conn.execute("DELETE FROM files WHERE id = ?", params![drop])?;
        Ok(())
    }

    // -- events -----------------------------------------------------------

    pub fn add_event(&self, ev: NewEvent) -> Result<i64> {
        let ts = ev.ts.unwrap_or_else(now);
        let (path, path_key, folder_key, dest, dest_key, dest_folder_key) = if ev.kind.starts_with("reg_") {
            // Registry keys are case-insensitive but are not filesystem paths.
            (ev.path.map(str::to_string), ev.path.map(str::to_lowercase), None, None, None, None)
        } else {
            let path = ev.path.map(norm);
            let dest = ev.dest_path.map(norm);
            (
                path.clone(),
                path.as_deref().map(key),
                path.as_deref().map(parent_key),
                dest.clone(),
                dest.as_deref().map(key),
                dest.as_deref().map(parent_key),
            )
        };
        self.conn.execute(
            "INSERT INTO events (ts, kind, source, file_id, path, path_key, folder_key, dest_path, dest_key,
                                 dest_folder_key, related_id, op_id, actor, reason, details)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                ts,
                ev.kind,
                ev.source,
                ev.file_id,
                path,
                path_key,
                folder_key,
                dest,
                dest_key,
                dest_folder_key,
                ev.related_id,
                ev.op_id,
                ev.actor,
                ev.reason,
                ev.details.map(|d| d.to_string())
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn events_where(&self, clause: &str, values: Vec<SqlValue>) -> Result<Vec<EventRow>> {
        let mut stmt = self.conn.prepare_cached(&format!("SELECT * FROM events WHERE {clause}"))?;
        let rows = stmt.query_map(sql_params(values), EventRow::from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // -- operations -------------------------------------------------------

    pub fn new_operation(&self, kind: &str, actor: Option<&str>, description: &str) -> Result<String> {
        let id = new_id();
        self.conn.execute(
            "INSERT INTO operations (id, ts, kind, actor, description) VALUES (?, ?, ?, ?, ?)",
            params![id, now(), kind, actor, description],
        )?;
        Ok(id)
    }

    // -- sessions ---------------------------------------------------------

    pub fn create_session(&self, label: &str, command: Option<&str>, roots: &[String]) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO sessions (label, command, started, roots) VALUES (?, ?, ?, ?)",
            params![label, command, now(), serde_json::to_string(roots)?],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn get_session(&self, id: i64) -> Result<Option<SessionRow>> {
        Ok(self.conn.query_row("SELECT * FROM sessions WHERE id = ?", params![id], SessionRow::from_row).optional()?)
    }

    pub fn sessions_where(&self, clause: &str, values: Vec<SqlValue>) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(&format!("SELECT * FROM sessions WHERE {clause}"))?;
        let rows = stmt.query_map(sql_params(values), SessionRow::from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn add_session_events<'a>(
        &self,
        session_id: i64,
        rows: impl IntoIterator<Item = (f64, &'a str, &'a str, &'a str, Option<&'a str>, Option<Value>)>,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO session_events (session_id, ts, source, kind, path, dest_path, details)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?;
        for (ts, source, kind, path, dest, details) in rows {
            stmt.execute(params![session_id, ts, source, kind, path, dest, details.map(|d| d.to_string())])?;
        }
        Ok(())
    }

    pub fn scalar_i64(&self, sql: &str, values: Vec<SqlValue>) -> Result<i64> {
        Ok(self.conn.query_row(sql, sql_params(values), |r| r.get::<_, Option<i64>>(0))?.unwrap_or(0))
    }
}
