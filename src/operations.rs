//! File operations that record *why* they happened: organize, move and undo.
//!
//! Moves made here carry an operation id, an actor and a reason, so an index can
//! say "moved to Documents by bettergrep organize" and the batch can be undone.  The
//! file is moved first and recorded right after; if the daemon noticed the move
//! first, its event is annotated instead of duplicated.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::Config;
use crate::fileinfo::probe;
use crate::folder_index;
use crate::paths::{INDEX_FILENAME, file_name, key, norm, parent};
use crate::store::{FileRow, NewEvent, Store, now};
use crate::tracker::{MoveOpts, record_move};

#[derive(Clone, Debug, Serialize)]
pub struct PlannedMove {
    pub src: String,
    pub dest: String,
    pub category: String,
}

/// `report.pdf` -> `report (1).pdf` until the name is free (on disk and in `taken`).
pub fn unique_path(path: &str, taken: &HashSet<String>) -> String {
    let free = |p: &str| !Path::new(p).exists() && !taken.contains(&key(p));
    if free(path) {
        return path.to_string();
    }
    let p = Path::new(path);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let dir = parent(path);
    (1..).map(|n| format!("{dir}\\{stem} ({n}){ext}")).find(|candidate| free(candidate)).unwrap()
}

/// Plan moving each top-level file of `folder` into a `<Category>` subfolder.
/// Unknown extensions, ignored files and files written in the last
/// `min_age_seconds` (probably still downloading) stay put.
pub fn plan_organize(cfg: &Config, folder: &str, min_age_seconds: f64) -> Result<Vec<PlannedMove>> {
    let folder = norm(folder);
    let by_ext: std::collections::HashMap<String, String> = cfg
        .categories
        .iter()
        .flat_map(|(cat, exts)| exts.iter().map(move |e| (e.trim_start_matches('.').to_lowercase(), cat.clone())))
        .collect();
    let ignored = cfg.ignore_rules();
    let mut entries: Vec<_> =
        std::fs::read_dir(&folder).with_context(|| format!("cannot read {folder}"))?.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name().to_string_lossy().to_lowercase());
    let mut plan = Vec::new();
    let mut taken = HashSet::new();
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path().to_string_lossy().into_owned();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || name.starts_with('.') || ignored.is_ignored(&path) {
            continue;
        }
        let ext = Path::new(&name).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let Some(category) = by_ext.get(&ext) else { continue };
        let age = meta.modified().ok().and_then(|m| m.elapsed().ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        if age < min_age_seconds {
            continue;
        }
        let dest = unique_path(&format!("{folder}\\{category}\\{name}"), &taken);
        taken.insert(key(&dest));
        plan.push(PlannedMove { src: path, dest, category: category.clone() });
    }
    Ok(plan)
}

/// The present record for `path`, registering it if bettergrep has never seen it.
fn register(
    store: &Store,
    path: &str,
    source: &str,
    kind: &str,
    op_id: Option<&str>,
    actor: Option<&str>,
    reason: Option<&str>,
) -> Result<FileRow> {
    if let Some(row) = store.get_present(path)? {
        return Ok(row);
    }
    let info = probe(path).ok_or_else(|| anyhow!("no such file or folder: {path}"))?;
    let inserted = store.tx(|| {
        let id = store.insert_file(&info, now(), None, None)?;
        store.add_event(NewEvent {
            kind,
            source,
            file_id: Some(id),
            path: Some(&info.path),
            op_id,
            actor,
            reason,
            details: info.is_dir.then(|| json!({"is_dir": true})),
            ..Default::default()
        })?;
        Ok(())
    });
    if let Err(err) = inserted {
        // The daemon may have registered it a moment ago (unique present-path index).
        if store.get_present(path)?.is_none() {
            return Err(err);
        }
    }
    let row = store.get_present(path)?.ok_or_else(|| anyhow!("failed to register {path}"))?;
    if op_id.is_some() && kind == "created" {
        // Claim the daemon's event so undo knows this folder is ours to remove.
        store.conn.execute(
            "UPDATE events SET op_id = ?, actor = ?, reason = ? WHERE file_id = ? AND kind = 'created' AND op_id IS NULL",
            rusqlite::params![op_id, actor, reason, row.id],
        )?;
    }
    Ok(row)
}

fn make_dirs(
    store: &Store,
    folder: &str,
    source: &str,
    op_id: Option<&str>,
    actor: Option<&str>,
    reason: Option<&str>,
) -> Result<()> {
    let mut missing = Vec::new();
    let mut current = norm(folder);
    while !Path::new(&current).is_dir() {
        missing.push(current.clone());
        let up = parent(&current);
        if up == current {
            break;
        }
        current = up;
    }
    for dir in missing.into_iter().rev() {
        std::fs::create_dir(&dir).with_context(|| format!("cannot create {dir}"))?;
        register(store, &dir, source, "created", op_id, actor, reason)?;
    }
    Ok(())
}

fn move_on_disk(src: &str, dest: &str) -> Result<()> {
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(err) if err.raw_os_error() == Some(17) => {
            // ERROR_NOT_SAME_DEVICE: copy, then delete (files only).
            if Path::new(src).is_dir() {
                bail!("moving folders between drives is not supported yet: {src}");
            }
            std::fs::copy(src, dest)?;
            std::fs::remove_file(src)?;
            Ok(())
        }
        Err(err) => Err(err).with_context(|| format!("cannot move {src} to {dest}")),
    }
}

#[derive(Default)]
pub struct Actor<'a> {
    pub source: &'a str,
    pub actor: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub op_id: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub struct MoveResult {
    pub op_id: String,
    pub file_id: i64,
    pub from: String,
    pub to: String,
    pub touched: BTreeSet<String>,
}

/// Move `src` to `dest` (a path, or an existing folder to move into) and record it.
pub fn move_file(store: &Store, src: &str, dest: &str, who: Actor, overwrite: bool) -> Result<MoveResult> {
    let src = norm(src);
    let mut dest = norm(dest);
    if !Path::new(&src).exists() {
        bail!("no such file or folder: {src}");
    }
    if Path::new(&dest).is_dir() && key(&dest) != key(&src) {
        dest = format!("{dest}\\{}", file_name(&src));
    }
    if src == dest {
        bail!("source and destination are the same");
    }
    if Path::new(&dest).exists() && key(&src) != key(&dest) && !overwrite {
        bail!("destination exists: {dest}");
    }
    let op_id = match who.op_id {
        Some(id) => id.to_string(),
        None => store
            .tx(|| store.new_operation("move", who.actor, who.reason.unwrap_or(&format!("move {src} -> {dest}"))))?,
    };
    let row = register(store, &src, who.source, "discovered", None, None, None)?;
    make_dirs(store, &parent(&dest), who.source, Some(&op_id), who.actor, who.reason)?;
    move_on_disk(&src, &dest)?;
    let info = probe(&dest);

    let touched = store.tx(|| {
        let current = store.get_file(row.id)?;
        if let Some(current) = current.as_ref().filter(|c| c.present() && c.path_key == key(&dest)) {
            // The daemon saw the move first; attach our context to its event.
            store.conn.execute(
                "UPDATE events SET source = ?, op_id = ?, actor = ?, reason = ? WHERE id = (
                     SELECT id FROM events WHERE file_id = ? AND kind IN ('moved', 'renamed') ORDER BY ts DESC, id DESC LIMIT 1)",
                rusqlite::params![who.source, op_id, who.actor, who.reason, current.id],
            )?;
            return Ok([parent(&src), parent(&dest)].into_iter().collect::<HashSet<_>>());
        }
        record_move(
            store,
            current.as_ref().unwrap_or(&row),
            &dest,
            now(),
            MoveOpts { source: who.source, info, op_id: Some(&op_id), actor: who.actor, reason: who.reason, ..Default::default() },
        )
    })?;
    Ok(MoveResult { op_id, file_id: row.id, from: src, to: dest, touched: touched.into_iter().collect() })
}

/// Sort a folder's loose files into category subfolders (dry run unless `apply`).
pub fn organize(store: &Store, cfg: &Config, folder: &str, apply: bool, source: &str, actor: &str) -> Result<Value> {
    let folder = norm(folder);
    let plan = plan_organize(cfg, &folder, 30.0)?;
    let mut result = json!({"folder": folder, "applied": apply, "planned": plan});
    if !apply || plan.is_empty() {
        return Ok(result);
    }
    let op_id = store
        .tx(|| store.new_operation("organize", Some(actor), &format!("organize {folder} ({} files)", plan.len())))?;
    let (mut moved, mut failed) = (Vec::new(), Vec::new());
    let mut touched: BTreeSet<String> = [folder.clone()].into();
    for item in &plan {
        let reason = format!("category: {}", item.category);
        let dest = unique_path(&item.dest, &HashSet::new());
        let who = Actor { source, actor: Some(actor), reason: Some(&reason), op_id: Some(&op_id) };
        match move_file(store, &item.src, &dest, who, false) {
            Ok(out) => {
                moved.push(json!({"from": out.from, "to": out.to, "file_id": out.file_id}));
                touched.extend(out.touched);
            }
            Err(err) => failed.push(json!({"from": item.src, "error": format!("{err:#}")})),
        }
    }
    for path in &touched {
        folder_index::write(store, path, cfg.index_max_entries)?;
    }
    result["op_id"] = op_id.into();
    result["moved"] = moved.into();
    result["failed"] = failed.into();
    Ok(result)
}

/// Revert the moves of operation `op_id` and remove the folders it created (if empty).
pub fn undo(store: &Store, cfg: &Config, op_id: &str, source: &str, actor: &str) -> Result<Value> {
    let (description, undone_by): (Option<String>, Option<String>) = store
        .conn
        .query_row("SELECT description, undone_by FROM operations WHERE id = ?", [op_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(|_| anyhow!("unknown operation {op_id}"))?;
    if let Some(by) = undone_by {
        bail!("operation {op_id} was already undone by {by}");
    }
    let undo_id = store.tx(|| {
        store.new_operation("undo", Some(actor), &format!("undo {op_id}: {}", description.unwrap_or_default()))
    })?;
    let reason = format!("undo {op_id}");
    let (mut restored, mut skipped, mut removed) = (Vec::new(), Vec::new(), Vec::new());
    let mut touched: BTreeSet<String> = BTreeSet::new();
    let events = store.events_where("op_id = ? ORDER BY id DESC", vec![op_id.to_string().into()])?;
    for ev in events.iter().filter(|e| e.kind == "moved" || e.kind == "renamed") {
        let row = match ev.file_id {
            Some(id) => store.get_file(id)?,
            None => None,
        };
        let (Some(row), Some(original)) = (row, ev.path.as_deref()) else { continue };
        if !row.present() || Some(&row.path_key) != ev.dest_key.as_ref() {
            skipped.push(json!({"file_id": row.id, "path": ev.dest_path, "why": "moved or deleted since"}));
            continue;
        }
        if Path::new(original).exists() {
            skipped.push(json!({"file_id": row.id, "path": original, "why": "original location is occupied"}));
            continue;
        }
        let who = Actor { source, actor: Some(actor), reason: Some(&reason), op_id: Some(&undo_id) };
        match move_file(store, &row.path, original, who, false) {
            Ok(out) => {
                restored.push(json!({"file_id": row.id, "to": original}));
                touched.extend(out.touched);
            }
            Err(err) => skipped.push(json!({"file_id": row.id, "path": row.path, "why": format!("{err:#}")})),
        }
    }
    for ev in events.iter().filter(|e| e.kind == "created") {
        let Some(row) = ev.file_id.map(|id| store.get_file(id)).transpose()?.flatten() else { continue };
        if !row.is_dir || !row.present() || !Path::new(&row.path).is_dir() {
            continue;
        }
        let leftovers = std::fs::read_dir(&row.path)?
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy() != INDEX_FILENAME);
        if leftovers {
            continue;
        }
        let index = Path::new(&row.path).join(INDEX_FILENAME);
        let _ = std::fs::remove_file(&index);
        if std::fs::remove_dir(&row.path).is_err() {
            continue;
        }
        store.tx(|| {
            store.set_status(row.id, "deleted", now())?;
            store.add_event(NewEvent {
                kind: "deleted",
                source,
                file_id: Some(row.id),
                path: Some(&row.path),
                op_id: Some(&undo_id),
                actor: Some(actor),
                reason: Some(&reason),
                ..Default::default()
            })?;
            Ok(())
        })?;
        removed.push(row.path.clone());
        touched.insert(parent(&row.path));
    }
    store.conn.execute("UPDATE operations SET undone_by = ? WHERE id = ?", [&undo_id, op_id])?;
    for path in &touched {
        if Path::new(path).is_dir() {
            folder_index::write(store, path, cfg.index_max_entries)?;
        }
    }
    Ok(json!({"op_id": undo_id, "undid": op_id, "restored": restored, "skipped": skipped, "removed_folders": removed}))
}
