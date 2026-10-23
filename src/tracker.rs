//! The lineage engine.
//!
//! Raw notifications are ambiguous: a move between two watched trees or across
//! volumes arrives as an unrelated delete and create, Office "saves" by deleting
//! and re-creating a file, downloads appear under a temporary name first.
//! [`Tracker`] turns that stream into the logical history of each file:
//!
//! * new and modified files *settle* for a quiet period before they are hashed;
//! * deletions are held back for `move_window_seconds` so a matching arrival
//!   (same NTFS file ID, or same name/size/hash) can claim them as a **move**;
//! * a deletion nobody claimed is checked with `OpenFileById`: if the file still
//!   exists somewhere on the volume it was **moved** out of the tracked folders,
//!   if it is in the Recycle Bin it was **recycled**, otherwise it was deleted;
//! * new content matching an existing file is a **copy**, and a copy whose
//!   original disappears moments later becomes a **move**.
//!
//! All methods must be called from one thread; call [`Tracker::tick`] often.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::Config;
use crate::fileinfo::{FileInfo, probe, sha256_file};
use crate::paths::{IgnoreRules, file_name, key, norm, parent};
use crate::store::{FileRow, NewEvent, Store};
use crate::win;

pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(crate::store::now)
}

/// Options for [`record_move`].
#[derive(Default)]
pub struct MoveOpts<'a> {
    pub source: &'a str,
    pub info: Option<FileInfo>,
    pub details: Option<Value>,
    pub op_id: Option<&'a str>,
    pub actor: Option<&'a str>,
    pub reason: Option<&'a str>,
}

fn is_recycle_bin(path_key: &str) -> bool {
    path_key.contains("\\$recycle.bin\\")
}

/// Record that `row` now lives at `dest`; for folders, every tracked descendant
/// moves too.  Returns the folders whose history changed.  Call inside a transaction.
pub fn record_move(store: &Store, row: &FileRow, dest: &str, ts: f64, opts: MoveOpts) -> Result<HashSet<String>> {
    let dest = norm(dest);
    let old_path = row.path.clone();
    let mut touched: HashSet<String> = [parent(&old_path), parent(&dest)].into();

    if let Some(clobbered) = store.get_present(&dest)?
        && clobbered.id != row.id
    {
        store.set_status(clobbered.id, "deleted", ts)?;
        store.add_event(NewEvent {
            kind: "deleted",
            source: opts.source,
            ts: Some(ts),
            file_id: Some(clobbered.id),
            path: Some(&dest),
            related_id: Some(row.id),
            op_id: opts.op_id,
            actor: opts.actor,
            reason: opts.reason,
            details: Some(json!({"overwritten": true})),
            ..Default::default()
        })?;
    }

    let info = opts.info.clone().or_else(|| probe(&dest));
    store.relocate(row.id, &dest, ts, info.as_ref())?;
    let kind = if key(parent(&old_path)) == key(parent(&dest)) { "renamed" } else { "moved" };
    store.add_event(NewEvent {
        kind,
        source: opts.source,
        ts: Some(ts),
        file_id: Some(row.id),
        path: Some(&old_path),
        dest_path: Some(&dest),
        op_id: opts.op_id,
        actor: opts.actor,
        reason: opts.reason,
        details: opts.details.clone(),
        ..Default::default()
    })?;

    if row.is_dir {
        let prefix_len = norm(&old_path).len();
        for child in store.present_descendants(&old_path)? {
            let new_child = format!("{dest}{}", &child.path[prefix_len..]);
            store.relocate(child.id, &new_child, ts, None)?;
            store.add_event(NewEvent {
                kind: "moved",
                source: opts.source,
                ts: Some(ts),
                file_id: Some(child.id),
                path: Some(&child.path),
                dest_path: Some(&new_child),
                op_id: opts.op_id,
                actor: opts.actor,
                reason: opts.reason,
                details: Some(json!({"with_folder": dest})),
                ..Default::default()
            })?;
            touched.insert(parent(&new_child));
        }
    }
    Ok(touched)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum SettleKind {
    Created,
    Modified,
}

struct Settle {
    path: String,
    first: f64,
    last: f64,
    kind: SettleKind,
}

#[derive(Clone, Debug)]
struct PendingDelete {
    file_id: i64,
    path: String,
    ts: f64,
    name_key: String,
    size: Option<i64>,
    sha256: Option<String>,
    is_dir: bool,
    with_folder: Option<String>,
}

/// What happened to an entry seen by [`Tracker::reconcile`] or a live arrival.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Outcome {
    Unchanged,
    Modified,
    Moved,
    Copied,
    New,
}

#[derive(Default, Debug, Serialize, Clone)]
pub struct ScanStats {
    pub seen: u64,
    pub discovered: u64,
    pub modified: u64,
    pub moved: u64,
    pub missing: u64,
    /// Vanished files found again elsewhere on the volume by their file ID.
    pub followed: u64,
    pub recycled: u64,
}

pub struct Tracker {
    pub store: Store,
    pub cfg: Config,
    pub source: String,
    clock: Clock,
    ignored: IgnoreRules,
    settling: HashMap<String, Settle>,
    pending: HashMap<i64, PendingDelete>,
    dirty: HashMap<String, (String, f64)>,
}

impl Tracker {
    pub fn new(store: Store, cfg: Config, source: &str, clock: Clock) -> Self {
        let ignored = cfg.ignore_rules();
        Tracker {
            store,
            cfg,
            source: source.to_string(),
            clock,
            ignored,
            settling: HashMap::new(),
            pending: HashMap::new(),
            dirty: HashMap::new(),
        }
    }

    pub fn set_config(&mut self, cfg: Config) {
        self.ignored = cfg.ignore_rules();
        self.cfg = cfg;
    }

    pub fn is_ignored(&self, path: &str) -> bool {
        self.ignored.is_ignored(path)
    }

    fn now(&self) -> f64 {
        (self.clock)()
    }

    // -- notifications ------------------------------------------------------

    pub fn created(&mut self, path: &str, is_dir: bool) -> Result<()> {
        let path = norm(path);
        if self.ignored.is_ignored(&path) {
            return Ok(());
        }
        if is_dir {
            // Folders have no content to settle: record them now so a folder moved in
            // is matched before its children, then pick up whatever is inside it.
            self.finalize(&path)?;
            self.adopt_children(&path)?;
            Ok(())
        } else {
            self.settle(&path, SettleKind::Created);
            Ok(())
        }
    }

    pub fn modified(&mut self, path: &str, is_dir: bool) -> Result<()> {
        if is_dir {
            return Ok(()); // a folder's "modified" just means its listing changed
        }
        let path = norm(path);
        if self.ignored.is_ignored(&path) {
            return Ok(());
        }
        let kind = if self.store.get_present(&path)?.is_some() { SettleKind::Modified } else { SettleKind::Created };
        self.settle(&path, kind);
        Ok(())
    }

    pub fn deleted(&mut self, path: &str) -> Result<()> {
        let path = norm(path);
        if self.ignored.is_ignored(&path) {
            return Ok(());
        }
        let now = self.now();
        let pk = key(&path);
        let was_settling = self.settling.remove(&pk).is_some();
        let child_prefix = format!("{pk}\\");
        self.settling.retain(|k, _| !k.starts_with(&child_prefix));

        let Some(row) = self.store.get_present(&path)? else {
            if was_settling {
                return Ok(()); // came and went before it settled: noise
            }
            for child in self.store.present_descendants(&path)? {
                self.queue_delete(&child, now, Some(&path));
            }
            return Ok(());
        };
        let became_move = self.store.tx(|| self.copy_became_move(&row, now))?;
        if became_move {
            return Ok(());
        }
        self.queue_delete(&row, now, None);
        for child in self.store.present_descendants(&path)? {
            self.queue_delete(&child, now, Some(&path));
        }
        Ok(())
    }

    pub fn moved(&mut self, src: &str, dest: &str, is_dir: bool) -> Result<()> {
        let (src, dest) = (norm(src), norm(dest));
        let (src_ignored, dest_ignored) = (self.ignored.is_ignored(&src), self.ignored.is_ignored(&dest));
        if src_ignored && dest_ignored {
            return Ok(());
        }
        if src_ignored {
            // e.g. "report.pdf.crdownload" -> "report.pdf"
            return self.created(&dest, is_dir || Path::new(&dest).is_dir());
        }
        if dest_ignored {
            return self.deleted(&src);
        }
        let now = self.now();
        let settling = self.settling.remove(&key(&src));
        let Some(row) = self.store.get_present(&src)? else {
            match settling {
                Some(mut s) => {
                    s.path = dest.clone();
                    s.last = now;
                    self.settling.insert(key(&dest), s);
                }
                // Unknown source, or a child of a folder we already moved: treat as an arrival.
                None => self.created(&dest, is_dir || Path::new(&dest).is_dir())?,
            }
            return Ok(());
        };
        self.pending.remove(&row.id);
        let source = self.source.clone();
        let touched = self
            .store
            .tx(|| record_move(&self.store, &row, &dest, now, MoveOpts { source: &source, ..Default::default() }))?;
        self.mark_dirty(touched, now);
        if settling.is_some() && !row.is_dir {
            self.settle(&dest, SettleKind::Modified);
        }
        Ok(())
    }

    // -- housekeeping -------------------------------------------------------

    /// Record files that have settled and commit deletions nobody claimed.
    pub fn tick(&mut self) -> Result<()> {
        let now = self.now();
        let mut due: Vec<(f64, String)> = self
            .settling
            .values()
            .filter(|s| now - s.last >= self.cfg.settle_seconds || now - s.first >= self.cfg.max_settle_seconds)
            .map(|s| (s.first, s.path.clone()))
            .collect();
        due.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (_, path) in due {
            self.settling.remove(&key(&path));
            if let Err(err) = self.finalize(&path) {
                crate::log::error(&format!("failed to record {path}: {err:#}"));
            }
        }
        let window = self.cfg.move_window_seconds;
        let expired: Vec<PendingDelete> = self.pending.values().filter(|p| now - p.ts >= window).cloned().collect();
        self.commit_deletes(expired)
    }

    /// Process everything outstanding now (shutdown, end of a scan, tests).
    pub fn flush(&mut self) -> Result<()> {
        let mut all: Vec<(f64, String)> = self.settling.values().map(|s| (s.first, s.path.clone())).collect();
        all.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (_, path) in all {
            self.settling.remove(&key(&path));
            if let Err(err) = self.finalize(&path) {
                crate::log::error(&format!("failed to record {path}: {err:#}"));
            }
        }
        let pending: Vec<PendingDelete> = self.pending.values().cloned().collect();
        self.commit_deletes(pending)
    }

    pub fn busy(&self) -> bool {
        !self.settling.is_empty() || !self.pending.is_empty()
    }

    /// Folders whose history changed at least `debounce` seconds ago (and forget them).
    pub fn take_dirty_folders(&mut self, debounce: Option<f64>) -> Vec<String> {
        let now = self.now();
        let debounce = debounce.unwrap_or(self.cfg.index_debounce_seconds);
        let ready: Vec<String> =
            self.dirty.iter().filter(|(_, (_, ts))| now - ts >= debounce).map(|(k, _)| k.clone()).collect();
        ready.into_iter().filter_map(|k| self.dirty.remove(&k).map(|(path, _)| path)).collect()
    }

    // -- reconcile ----------------------------------------------------------

    /// Bring the database in line with the disk under `root`.
    ///
    /// Used at start-up and by `bettergrep scan`.  Unknown entries are `discovered`;
    /// tracked entries that are gone are matched by file ID or content against
    /// new arrivals, then looked up by file ID anywhere on the volume, and only
    /// then marked `missing`.
    pub fn reconcile(&mut self, root: &str, recursive: bool) -> Result<ScanStats> {
        let root = norm(root);
        let now = self.now();
        let mut stats = ScanStats::default();
        let volume = probe(&root).map(|i| i.volume).unwrap_or(0);

        let mut on_disk: Vec<FileInfo> = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = win::list_dir(&dir) else { continue };
            for e in entries {
                let path = format!("{}\\{}", dir.trim_end_matches('\\'), e.name);
                if self.ignored.is_ignored(&path) {
                    continue;
                }
                if e.is_dir && recursive && !e.is_reparse_point {
                    stack.push(path.clone());
                }
                on_disk.push(FileInfo {
                    path,
                    is_dir: e.is_dir,
                    size: e.size,
                    mtime: e.mtime,
                    volume,
                    inode: e.file_id,
                });
            }
        }
        stats.seen = on_disk.len() as u64;
        on_disk.sort_by_key(|i| key(&i.path)); // parents before children

        let root_key = key(&root);
        let known: HashMap<String, FileRow> = self
            .store
            .present_descendants(&root)?
            .into_iter()
            .filter(|r| recursive || r.parent_key == root_key)
            .map(|r| (r.path_key.clone(), r))
            .collect();
        let disk_keys: HashSet<String> = on_disk.iter().map(|i| key(&i.path)).collect();
        let vanished: Vec<FileRow> = known
            .values()
            .filter(|r| !disk_keys.contains(&r.path_key) && !self.ignored.is_ignored(&r.path))
            .cloned()
            .collect();
        for row in &vanished {
            self.queue_delete(row, now, None);
        }

        for chunk in on_disk.chunks(500) {
            let mut touched = HashSet::new();
            self.with_tx(|t| {
                for info in chunk {
                    if let Some(row) = known.get(&key(&info.path)) {
                        if t.refresh(row, info, now, false)? {
                            stats.modified += 1;
                            touched.insert(parent(&info.path));
                        }
                        continue;
                    }
                    if t.store.get_present(&info.path)?.is_some() {
                        continue; // placed by an earlier folder move in this pass
                    }
                    let (outcome, changed) = t.record_arrival(info, now, "discovered", false)?;
                    touched.extend(changed);
                    match outcome {
                        Outcome::New => stats.discovered += 1,
                        Outcome::Moved => stats.moved += 1,
                        _ => {}
                    }
                }
                Ok(())
            })?;
            self.mark_dirty(touched, now);
        }

        let source = self.source.clone();
        self.with_tx(|t| {
            for row in &vanished {
                let claimed = t.pending.remove(&row.id).is_none();
                let Some(current) = t.store.get_file(row.id)? else { continue };
                if claimed || !current.present() || current.path_key != row.path_key {
                    continue;
                }
                match t.follow(&current, now, &source)? {
                    Followed::Moved(touched) => {
                        stats.followed += 1;
                        t.mark_dirty(touched, now);
                    }
                    Followed::Recycled => {
                        stats.recycled += 1;
                        t.mark_dirty([parent(&row.path)], now);
                    }
                    Followed::StillThere => {}
                    Followed::Gone => {
                        t.store.set_status(row.id, "missing", now)?;
                        t.store.add_event(NewEvent {
                            kind: "missing",
                            source: &source,
                            ts: Some(now),
                            file_id: Some(row.id),
                            path: Some(&row.path),
                            details: Some(
                                json!({"note": "not found during scan; deleted while bettergrep was not running"}),
                            ),
                            ..Default::default()
                        })?;
                        t.mark_dirty([parent(&row.path)], now);
                        stats.missing += 1;
                    }
                }
            }
            Ok(())
        })?;
        Ok(stats)
    }

    // -- internals ----------------------------------------------------------

    fn settle(&mut self, path: &str, kind: SettleKind) {
        let now = self.now();
        let entry =
            self.settling.entry(key(path)).or_insert(Settle { path: path.to_string(), first: now, last: now, kind });
        entry.last = now;
        if kind == SettleKind::Created {
            entry.kind = SettleKind::Created;
        }
    }

    fn mark_dirty(&mut self, folders: impl IntoIterator<Item = String>, now: f64) {
        for folder in folders {
            self.dirty.insert(key(&folder), (norm(&folder), now));
        }
    }

    fn queue_delete(&mut self, row: &FileRow, now: f64, with_folder: Option<&str>) {
        self.pending.insert(
            row.id,
            PendingDelete {
                file_id: row.id,
                path: row.path.clone(),
                ts: now,
                name_key: row.name.to_lowercase(),
                size: row.size,
                sha256: row.sha256.clone(),
                is_dir: row.is_dir,
                with_folder: with_folder.map(str::to_string),
            },
        );
    }

    fn commit_deletes(&mut self, mut list: Vec<PendingDelete>) -> Result<()> {
        if list.is_empty() {
            return Ok(());
        }
        // Folders first (shallowest first): following a folder relocates its children.
        list.sort_by(|a, b| a.ts.total_cmp(&b.ts).then(b.is_dir.cmp(&a.is_dir)).then(a.path.len().cmp(&b.path.len())));
        let source = self.source.clone();
        self.with_tx(|t| {
            for pending in list {
                t.pending.remove(&pending.file_id);
                let Some(row) = t.store.get_file(pending.file_id)? else { continue };
                if !row.present() || row.path_key != key(&pending.path) {
                    continue; // claimed by a move in the meantime
                }
                match t.follow(&row, pending.ts, &source)? {
                    Followed::Moved(touched) => t.mark_dirty(touched, pending.ts),
                    Followed::Recycled | Followed::StillThere => t.mark_dirty([parent(&row.path)], pending.ts),
                    Followed::Gone => {
                        t.store.set_status(row.id, "deleted", pending.ts)?;
                        t.store.add_event(NewEvent {
                            kind: "deleted",
                            source: &source,
                            ts: Some(pending.ts),
                            file_id: Some(row.id),
                            path: Some(&row.path),
                            details: pending.with_folder.as_ref().map(|f| json!({"with_folder": f})),
                            ..Default::default()
                        })?;
                        t.mark_dirty([parent(&row.path)], pending.ts);
                    }
                }
            }
            Ok(())
        })
    }

    /// A tracked file vanished from its path: find it again by its NTFS file ID.
    fn follow(&self, row: &FileRow, ts: f64, source: &str) -> Result<Followed> {
        let Some(inode) = row.inode.filter(|&i| i != 0) else { return Ok(Followed::Gone) };
        let Some(found) = win::locate_by_id(&row.path, inode as u64) else { return Ok(Followed::Gone) };
        let found_key = key(&found);
        if found_key == row.path_key {
            return Ok(Followed::StillThere);
        }
        if is_recycle_bin(&found_key) {
            self.store.set_status(row.id, "deleted", ts)?;
            self.store.add_event(NewEvent {
                kind: "deleted",
                source,
                ts: Some(ts),
                file_id: Some(row.id),
                path: Some(&row.path),
                details: Some(json!({"recycled": true, "recycle_bin_path": found})),
                ..Default::default()
            })?;
            return Ok(Followed::Recycled);
        }
        let touched = record_move(
            &self.store,
            row,
            &found,
            ts,
            MoveOpts {
                source,
                info: probe(&found),
                details: Some(
                    json!({"matched": "file_id_lookup", "outside_tracked_folders": self.cfg.root_for(&found).is_none()}),
                ),
                ..Default::default()
            },
        )?;
        Ok(Followed::Moved(touched))
    }

    /// Explorer moves across volumes by copying and then deleting: if a copy of
    /// `row` appeared moments ago and `row` is now deleted, that was a move.
    fn copy_became_move(&self, row: &FileRow, now: f64) -> Result<bool> {
        let events = self.store.events_where(
            "kind = 'copied' AND related_id = ? AND ts >= ? AND file_id IN (SELECT id FROM files WHERE status = 'present')
             ORDER BY ts DESC LIMIT 1",
            vec![row.id.into(), (now - self.cfg.move_window_seconds).into()],
        )?;
        let Some(event) = events.into_iter().next() else { return Ok(false) };
        let Some(copy) = self.store.get_file(event.file_id.unwrap_or_default())? else { return Ok(false) };
        let info = copy.info();
        self.store.merge_into(row.id, copy.id)?;
        self.store.relocate(row.id, &copy.path, now, Some(&info))?;
        let kind = if key(parent(&row.path)) == key(parent(&copy.path)) { "renamed" } else { "moved" };
        self.store.conn.execute(
            "UPDATE events SET kind = ?, related_id = NULL, details = ? WHERE id = ?",
            rusqlite::params![kind, r#"{"matched": "copy_then_delete"}"#, event.id],
        )?;
        Ok(true)
    }

    fn finalize(&mut self, path: &str) -> Result<()> {
        let Some(info) = probe(path) else { return Ok(()) }; // vanished before it settled
        let now = self.now();
        let (_, touched) = self.with_tx(|t| t.record_arrival(&info, now, "created", true))?;
        self.mark_dirty(touched, now);
        Ok(())
    }

    /// Run `f` in one write transaction while still allowing `&mut self`.
    fn with_tx<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.store.begin()?;
        match f(self) {
            Ok(v) => {
                self.store.end(true)?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.store.end(false);
                Err(e)
            }
        }
    }

    /// Children of a folder that just arrived (moved in, or created by an unzip):
    /// sub-folders are recorded now, files settle like any new file.
    fn adopt_children(&mut self, dir: &str) -> Result<()> {
        let mut stack = vec![dir.to_string()];
        while let Some(current) = stack.pop() {
            let Ok(entries) = win::list_dir(&current) else { continue };
            for e in entries {
                let path = format!("{}\\{}", current.trim_end_matches('\\'), e.name);
                if self.ignored.is_ignored(&path) {
                    continue;
                }
                if e.is_dir {
                    self.finalize(&path)?;
                    if !e.is_reparse_point {
                        stack.push(path);
                    }
                } else if self.store.get_present(&path)?.is_none() {
                    self.settle(&path, SettleKind::Created);
                }
            }
        }
        Ok(())
    }

    /// Record an entry that appeared at `info.path`.
    fn record_arrival(
        &mut self,
        info: &FileInfo,
        now: f64,
        new_kind: &str,
        hash_new: bool,
    ) -> Result<(Outcome, HashSet<String>)> {
        let path = info.path.clone();
        let here: HashSet<String> = [parent(&path)].into();

        if let Some(existing) = self.store.get_present(&path)? {
            let replaced = self.pending.remove(&existing.id).is_some();
            return Ok(if self.refresh(&existing, info, now, replaced)? {
                (Outcome::Modified, here)
            } else {
                (Outcome::Unchanged, HashSet::new())
            });
        }

        // 1. Same volume: the NTFS file ID survives moves, so this is the same file.
        if let Some((volume, inode)) = info.identity()
            && let Some(m) = self.store.find_present_by_identity(volume, inode)?
            && m.is_dir == info.is_dir
        {
            let still_at_old = probe(&m.path).and_then(|i| i.identity()) == Some((volume, inode));
            if !still_at_old {
                self.claim_pending(&m)?;
                let touched = record_move(
                    &self.store,
                    &m,
                    &path,
                    now,
                    MoveOpts {
                        source: &self.source,
                        info: Some(info.clone()),
                        details: Some(json!({"matched": "file_id"})),
                        ..Default::default()
                    },
                )?;
                return Ok((Outcome::Moved, touched));
            }
            // Same ID at both paths: a hard link.  Track it separately.
        }

        let sha = if info.is_dir || !hash_new { None } else { sha256_file(&path, self.cfg.hash_max_bytes) };

        // 2. Across volumes or trees: a recently deleted file with the same content.
        if let Some(pending_id) = self.match_pending(info, sha.as_deref())
            && let Some(row) = self.store.get_file(pending_id)?
            && row.present()
        {
            self.claim_pending(&row)?;
            let touched = record_move(
                &self.store,
                &row,
                &path,
                now,
                MoveOpts {
                    source: &self.source,
                    info: Some(info.clone()),
                    details: Some(json!({"matched": "content"})),
                    ..Default::default()
                },
            )?;
            if sha.is_some() {
                self.store.set_sha(row.id, sha.as_deref())?;
            }
            return Ok((Outcome::Moved, touched));
        }

        // 3. Same content as a file that still exists: a copy.
        let origin = match &sha {
            Some(sha) if info.size > 0 => self.find_copy_source(info, sha)?,
            _ => None,
        };
        let file_id = self.store.insert_file(info, now, sha.as_deref(), origin.as_ref().map(|o| o.id))?;
        if let Some(origin) = origin {
            self.store.add_event(NewEvent {
                kind: "copied",
                source: &self.source,
                ts: Some(now),
                file_id: Some(file_id),
                path: Some(&origin.path),
                dest_path: Some(&path),
                related_id: Some(origin.id),
                ..Default::default()
            })?;
            let mut touched = here;
            touched.insert(parent(&origin.path));
            return Ok((Outcome::Copied, touched));
        }
        self.store.add_event(NewEvent {
            kind: new_kind,
            source: &self.source,
            ts: Some(now),
            file_id: Some(file_id),
            path: Some(&path),
            details: info.is_dir.then(|| json!({"is_dir": true})),
            ..Default::default()
        })?;
        Ok((Outcome::New, here))
    }

    /// Cancel the pending deletion of `row` (and of its descendants, for folders).
    fn claim_pending(&mut self, row: &FileRow) -> Result<()> {
        self.pending.remove(&row.id);
        if row.is_dir {
            for child in self.store.present_descendants(&row.path)? {
                self.pending.remove(&child.id);
            }
        }
        Ok(())
    }

    /// Update a known file's metadata; record `modified` only if its content changed.
    fn refresh(&self, row: &FileRow, info: &FileInfo, now: f64, replaced: bool) -> Result<bool> {
        let unchanged = info.size as i64 == row.size.unwrap_or(-1) && Some(info.mtime) == row.mtime;
        if row.is_dir || (!replaced && unchanged) {
            let identity_changed = info.identity().is_some()
                && (Some(info.volume as i64), Some(info.inode as i64)) != (row.volume, row.inode);
            if identity_changed {
                self.store.update_content(row.id, info, now, row.sha256.as_deref())?;
            }
            return Ok(false);
        }
        let sha = sha256_file(&info.path, self.cfg.hash_max_bytes);
        let changed = replaced || sha.is_none() || sha != row.sha256 || Some(info.size as i64) != row.size;
        self.store.update_content(row.id, info, now, sha.as_deref())?;
        if changed {
            let mut details = json!({"size": info.size, "previous_size": row.size});
            if replaced {
                details["replaced"] = json!(true);
            }
            self.store.add_event(NewEvent {
                kind: "modified",
                source: &self.source,
                ts: Some(now),
                file_id: Some(row.id),
                path: Some(&info.path),
                details: Some(details),
                ..Default::default()
            })?;
        }
        Ok(changed)
    }

    fn match_pending(&self, info: &FileInfo, sha: Option<&str>) -> Option<i64> {
        let name_key = file_name(&info.path).to_lowercase();
        let mut best: Option<(u8, f64, i64)> = None;
        for p in self.pending.values() {
            if p.is_dir != info.is_dir {
                continue;
            }
            let same_name = p.name_key == name_key;
            let score = if info.is_dir {
                same_name as u8
            } else if p.size != Some(info.size as i64) {
                continue;
            } else if let (Some(a), Some(b)) = (sha, p.sha256.as_deref()) {
                if a != b {
                    continue;
                }
                if same_name { 2 } else { 1 }
            } else {
                same_name as u8
            };
            if score > 0 && best.is_none_or(|(s, t, _)| (score, p.ts) > (s, t)) {
                best = Some((score, p.ts, p.file_id));
            }
        }
        best.map(|(_, _, id)| id)
    }

    fn find_copy_source(&self, info: &FileInfo, sha: &str) -> Result<Option<FileRow>> {
        let name = file_name(&info.path).to_lowercase();
        let mut matches = Vec::new();
        for cand in self.store.find_present_by_size(info.size, 25)? {
            if self.pending.contains_key(&cand.id) {
                continue;
            }
            let mut cand_sha = cand.sha256.clone();
            if cand_sha.is_none() && Path::new(&cand.path).exists() {
                cand_sha = sha256_file(&cand.path, self.cfg.hash_max_bytes);
                self.store.set_sha(cand.id, cand_sha.as_deref())?;
            }
            if cand_sha.as_deref() == Some(sha) {
                matches.push(cand);
            }
        }
        Ok(matches.into_iter().max_by(|a, b| {
            (a.name.to_lowercase() == name, a.last_seen)
                .partial_cmp(&(b.name.to_lowercase() == name, b.last_seen))
                .unwrap()
        }))
    }
}

enum Followed {
    Moved(HashSet<String>),
    Recycled,
    StillThere,
    Gone,
}
