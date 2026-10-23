//! Per-folder index files (`.bettergrep.json`, schema `bettergrep.folder-index/1`).
//!
//! Answers, without any tool: what is here and where did it come from, what used
//! to be here and where is it now, and what was deleted.  The database stays the
//! source of truth; these files are regenerated from it.  Format: `docs/folder-index.md`.

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::paths::{INDEX_FILENAME, INDEX_TMP_SUFFIX, file_name, key, norm};
use crate::query::{compact, event_json, iso, iso_opt};
use crate::store::Store;
use crate::win;

pub const SCHEMA_ID: &str = "bettergrep.folder-index/1";

pub fn build(store: &Store, folder: &str, limit: usize) -> Result<Value> {
    let folder = norm(folder);
    let fk = key(&folder);

    let rows = store.files_where(
        "parent_key = ? AND status = 'present' ORDER BY is_dir DESC, name COLLATE NOCASE",
        vec![fk.clone().into()],
    )?;
    let mut contents = Vec::new();
    for row in rows.iter().take(limit) {
        let arrival = store
            .events_where(
                "file_id = ? AND dest_folder_key = ? AND folder_key != ? AND kind IN ('moved', 'copied')
                 ORDER BY ts DESC LIMIT 1",
                vec![row.id.into(), fk.clone().into(), fk.clone().into()],
            )?
            .into_iter()
            .next();
        let first = store.events_where("file_id = ? ORDER BY ts, id LIMIT 1", vec![row.id.into()])?.into_iter().next();
        let renamed = store
            .events_where(
                "file_id = ? AND kind = 'renamed' AND dest_folder_key = ? ORDER BY ts DESC LIMIT 1",
                vec![row.id.into(), fk.clone().into()],
            )?
            .into_iter()
            .next();
        let mut entry = json!({
            "id": row.id,
            "name": row.name,
            "type": if row.is_dir { "folder" } else { "file" },
            "size": if row.is_dir { None } else { row.size },
            "sha256": row.sha256,
            "modified": iso_opt(row.mtime),
        });
        if let Some(a) = &arrival {
            entry["arrived"] = iso(a.ts).into();
            entry["how"] = a.kind.clone().into();
            entry["came_from"] = json!(a.path);
            entry["by"] = json!(a.actor);
            entry["reason"] = json!(a.reason);
        } else if let Some(f) = &first {
            entry["arrived"] = iso(f.ts).into();
            entry["how"] = f.kind.clone().into();
        }
        if let Some(r) = &renamed {
            entry["previous_name"] = file_name(r.path.as_deref().unwrap_or_default()).into();
        }
        contents.push(compact(entry));
    }

    let mut departed = Vec::new();
    let mut stmt = store.conn.prepare(
        "SELECT file_id, path, dest_path, actor, reason, MAX(ts) AS last_ts FROM events
         WHERE folder_key = ? AND kind = 'moved' AND dest_folder_key != ? AND file_id IS NOT NULL
         GROUP BY file_id ORDER BY last_ts DESC LIMIT ?",
    )?;
    let moves = stmt
        .query_map(rusqlite::params![fk, fk, (limit * 2) as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, f64>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (file_id, path, dest, actor, reason, ts) in moves {
        let Some(current) = store.get_file(file_id)? else { continue };
        if current.parent_key == fk && current.present() {
            continue; // it came back
        }
        departed.push(compact(json!({
            "id": file_id,
            "name": file_name(path.as_deref().unwrap_or_default()),
            "left": iso(ts),
            "moved_to": dest,
            "now_at": if current.present() { Some(current.path.clone()) } else { None },
            "now_status": current.status,
            "by": actor,
            "reason": reason,
        })));
        if departed.len() >= limit {
            break;
        }
    }

    let copied_out: Vec<Value> = store
        .events_where(
            "folder_key = ? AND kind = 'copied' AND dest_folder_key != ? ORDER BY ts DESC LIMIT ?",
            vec![fk.clone().into(), fk.clone().into(), (limit as i64).into()],
        )?
        .iter()
        .map(|e| {
            compact(json!({
                "id": e.related_id,
                "name": file_name(e.path.as_deref().unwrap_or_default()),
                "copied": iso(e.ts),
                "copy_id": e.file_id,
                "copy_path": e.dest_path,
            }))
        })
        .collect();

    let mut deleted = Vec::new();
    for row in store.files_where(
        "parent_key = ? AND status IN ('deleted', 'missing') ORDER BY last_seen DESC LIMIT ?",
        vec![fk.clone().into(), (limit as i64).into()],
    )? {
        let recycled = store
            .events_where("file_id = ? AND kind = 'deleted' ORDER BY ts DESC LIMIT 1", vec![row.id.into()])?
            .into_iter()
            .next()
            .and_then(|e| e.details)
            .and_then(|d| serde_json::from_str::<Value>(&d).ok())
            .and_then(|d| d.get("recycle_bin_path").cloned());
        deleted.push(compact(json!({
            "id": row.id,
            "name": row.name,
            "type": if row.is_dir { "folder" } else { "file" },
            "status": row.status,
            "when": iso(row.last_seen),
            "size": if row.is_dir { None } else { row.size },
            "sha256": row.sha256,
            "recycle_bin_path": recycled,
        })));
    }

    let recent: Vec<Value> = store
        .events_where(
            "id IN (SELECT id FROM events WHERE folder_key = ?1 UNION SELECT id FROM events WHERE dest_folder_key = ?1)
             ORDER BY ts DESC, id DESC LIMIT 50",
            vec![fk.clone().into()],
        )?
        .iter()
        .map(event_json)
        .collect();

    Ok(json!({
        "schema": SCHEMA_ID,
        "generator": format!("bettergrep {} (rust)", crate::VERSION),
        "folder": folder,
        "summary": {
            "files_here": rows.iter().filter(|r| !r.is_dir).count(),
            "folders_here": rows.iter().filter(|r| r.is_dir).count(),
            "departed": departed.len(),
            "copied_out": copied_out.len(),
            "deleted": deleted.len(),
            "truncated": rows.len() > limit,
        },
        "contents": contents,
        "departed": departed,
        "copied_out": copied_out,
        "deleted": deleted,
        "recent_events": recent,
        "help": "Generated by bettergrep. 'contents' = files here now (came_from = previous folder); \
                 'departed' = files that were moved out (now_at = where they are today); \
                 'deleted' = files removed from this folder (recycle_bin_path if it is in the Recycle Bin). \
                 Ask with `bettergrep where <name>` or the bettergrep MCP server.",
    }))
}

/// (Re)write `folder/.bettergrep.json`.  Returns the path, or `None` if nothing changed.
pub fn write(store: &Store, folder: &str, limit: usize) -> Result<Option<String>> {
    let folder = norm(folder);
    if !Path::new(&folder).is_dir() {
        return Ok(None);
    }
    let data = build(store, &folder, limit)?;
    let target = Path::new(&folder).join(INDEX_FILENAME).to_string_lossy().into_owned();
    if let Ok(existing) = std::fs::read_to_string(&target)
        && serde_json::from_str::<Value>(&existing).ok().as_ref() == Some(&data)
    {
        return Ok(None);
    }
    let tmp = format!("{target}{INDEX_TMP_SUFFIX}");
    std::fs::write(&tmp, serde_json::to_string_pretty(&data)? + "\n")?;
    if Path::new(&target).exists() {
        win::set_hidden(&target, false); // Windows refuses to replace hidden files
    }
    std::fs::rename(&tmp, &target)?;
    win::set_hidden(&target, true);
    Ok(Some(target))
}
