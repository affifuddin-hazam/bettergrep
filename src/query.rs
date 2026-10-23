//! Read-only queries shared by the CLI, the MCP server and folder index files.
//! Every function returns JSON with local ISO-8601 times; `null` fields are omitted.

use anyhow::{Result, bail};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone};
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value, json};

use crate::paths::{descendant_range, key, norm};
use crate::store::{EventRow, FILE_EVENT_KINDS, FileRow, REGISTRY_EVENT_KINDS, SessionRow, Store, now};

pub fn iso(ts: f64) -> String {
    let secs = ts.floor() as i64;
    let nanos = ((ts - ts.floor()) * 1e9) as u32;
    match Local.timestamp_opt(secs, nanos).single() {
        Some(dt) => dt.to_rfc3339_opts(SecondsFormat::Secs, false),
        None => ts.to_string(),
    }
}

pub fn iso_opt(ts: Option<f64>) -> Value {
    ts.map(|t| Value::String(iso(t))).unwrap_or(Value::Null)
}

/// Parse `"15m"`, `"2h"`, `"7d"`, `"1w"`, `"today"`, `"yesterday"` or an ISO date/time.
pub fn parse_since(value: Option<&str>) -> Result<Option<f64>> {
    let Some(text) = value.map(str::trim).filter(|t| !t.is_empty()) else { return Ok(None) };
    let lower = text.to_lowercase();
    let digits: String = lower.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    if !digits.is_empty() {
        let unit = lower[digits.len()..].trim().trim_end_matches('s');
        let seconds = match unit {
            "s" | "sec" => Some(1.0),
            "m" | "min" => Some(60.0),
            "h" | "hr" | "hour" => Some(3600.0),
            "d" | "day" => Some(86400.0),
            "w" | "wk" | "week" => Some(604800.0),
            _ => None,
        };
        if let (Some(unit_secs), Ok(n)) = (seconds, digits.parse::<f64>()) {
            return Ok(Some(now() - n * unit_secs));
        }
    }
    let midnight = |days_back: i64| -> f64 {
        let date = Local::now().date_naive() - chrono::Duration::days(days_back);
        local_ts(date.and_hms_opt(0, 0, 0).unwrap())
    };
    match lower.as_str() {
        "today" => return Ok(Some(midnight(0))),
        "yesterday" => return Ok(Some(midnight(1))),
        _ => {}
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return Ok(Some(dt.timestamp() as f64));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(text, fmt) {
            return Ok(Some(local_ts(dt)));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Ok(Some(local_ts(date.and_hms_opt(0, 0, 0).unwrap())));
    }
    bail!("cannot understand time {text:?}; use e.g. 30m, 2h, 7d, today or 2026-01-31")
}

fn local_ts(dt: NaiveDateTime) -> f64 {
    Local.from_local_datetime(&dt).earliest().map(|d| d.timestamp() as f64).unwrap_or(0.0)
}

/// Drop `null` members of an object.
pub fn compact(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(map.into_iter().filter(|(_, v)| !v.is_null()).collect::<Map<_, _>>()),
        other => other,
    }
}

pub fn file_json(r: &FileRow) -> Value {
    compact(json!({
        "id": r.id,
        "path": r.path,
        "name": r.name,
        "type": if r.is_dir { "folder" } else { "file" },
        "status": r.status,
        "size": if r.is_dir { None } else { r.size },
        "sha256": r.sha256,
        "modified": iso_opt(r.mtime),
        "first_seen": iso(r.first_seen),
        "last_seen": iso(r.last_seen),
        "copied_from": r.copied_from,
    }))
}

pub fn event_json(e: &EventRow) -> Value {
    let details: Value = e.details.as_deref().and_then(|d| serde_json::from_str(d).ok()).unwrap_or(Value::Null);
    compact(json!({
        "id": e.id,
        "time": iso(e.ts),
        "kind": e.kind,
        "source": e.source,
        "file_id": e.file_id,
        "path": e.path,
        "dest_path": e.dest_path,
        "related_id": e.related_id,
        "op_id": e.op_id,
        "actor": e.actor,
        "reason": e.reason,
        "details": details,
    }))
}

fn like_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn looks_like_path(text: &str) -> bool {
    text.contains('\\') || text.contains('/') || (text.len() > 1 && text.as_bytes()[1] == b':')
}

/// Find the file(s) meant by `target`: an id (`42`, `#42`), a current or *former*
/// path (files that have moved away are still found), or a file name.
pub fn resolve(store: &Store, target: &str) -> Result<Vec<FileRow>> {
    let text = target.trim();
    if let Ok(id) = text.trim_start_matches('#').parse::<i64>() {
        return Ok(store.get_file(id)?.into_iter().collect());
    }
    if looks_like_path(text) {
        let at = store.get_latest_at(text)?;
        if let Some(row) = &at
            && row.present()
        {
            return Ok(vec![row.clone()]);
        }
        let k = key(text);
        let mut stmt = store.conn.prepare(
            "SELECT file_id FROM events WHERE (path_key = ?1 OR dest_key = ?1) AND file_id IS NOT NULL
             GROUP BY file_id ORDER BY MAX(ts) DESC LIMIT 20",
        )?;
        let ids: Vec<i64> = stmt.query_map([&k], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let mut rows: Vec<FileRow> = ids.into_iter().filter_map(|id| store.get_file(id).ok().flatten()).collect();
        if let Some(row) = at
            && rows.iter().all(|r| r.id != row.id)
        {
            rows.insert(0, row);
        }
        return Ok(rows);
    }
    let exact = store.files_where(
        "name = ? COLLATE NOCASE ORDER BY status = 'present' DESC, last_seen DESC LIMIT 20",
        vec![text.to_string().into()],
    )?;
    if !exact.is_empty() {
        return Ok(exact);
    }
    let fuzzy = store.files_where(
        "name LIKE ? ESCAPE '\\' ORDER BY status = 'present' DESC, last_seen DESC LIMIT 20",
        vec![format!("%{}%", like_escape(text)).into()],
    )?;
    if !fuzzy.is_empty() {
        return Ok(fuzzy);
    }
    // A name the file used to have (it was renamed since).
    let former = format!("%\\{}", like_escape(&text.to_lowercase()));
    let mut stmt = store.conn.prepare(
        "SELECT file_id FROM events WHERE (path_key LIKE ?1 ESCAPE '\\' OR dest_key LIKE ?1 ESCAPE '\\')
         AND file_id IS NOT NULL GROUP BY file_id ORDER BY MAX(ts) DESC LIMIT 20",
    )?;
    let ids: Vec<i64> = stmt.query_map([&former], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(ids.into_iter().filter_map(|id| store.get_file(id).ok().flatten()).collect())
}

pub struct SearchOpts<'a> {
    pub status: &'a str,
    pub under: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub limit: i64,
}

impl Default for SearchOpts<'_> {
    fn default() -> Self {
        SearchOpts { status: "present", under: None, kind: None, limit: 50 }
    }
}

/// Every whitespace-separated term must match the name (plain terms anywhere,
/// `*`/`?` globs the whole name) or, if it contains a separator, the full path.
pub fn search(store: &Store, query: &str, opts: SearchOpts) -> Result<Vec<Value>> {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<SqlValue> = Vec::new();
    for term in query.split_whitespace() {
        let is_path = term.contains('\\') || term.contains('/');
        let (column, term) =
            if is_path { ("path_key", term.replace('/', "\\").to_lowercase()) } else { ("name", term.to_string()) };
        let pattern = if term.contains('*') || term.contains('?') {
            like_escape(&term).replace('*', "%").replace('?', "_")
        } else {
            format!("%{}%", like_escape(&term))
        };
        clauses.push(format!("{column} LIKE ? ESCAPE '\\'"));
        params.push(pattern.into());
    }
    if opts.status != "any" {
        clauses.push("status = ?".into());
        params.push(opts.status.to_string().into());
    }
    if let Some(under) = opts.under {
        let (low, high) = descendant_range(&key(under));
        clauses.push("path_key >= ? AND path_key < ?".into());
        params.extend([low.into(), high.into()]);
    }
    if let Some(kind) = opts.kind {
        clauses.push("is_dir = ?".into());
        params.push(i64::from(kind == "folder").into());
    }
    let mut clause = if clauses.is_empty() { "1".to_string() } else { clauses.join(" AND ") };
    clause.push_str(" ORDER BY status = 'present' DESC, last_seen DESC LIMIT ?");
    params.push(opts.limit.into());
    Ok(store.files_where(&clause, params)?.iter().map(file_json).collect())
}

/// Every location a file has occupied, oldest first.
pub fn trail(store: &Store, file_id: i64) -> Result<Vec<Value>> {
    let mut stops: Vec<Map<String, Value>> = Vec::new();
    for e in store.events_where("file_id = ? ORDER BY ts, id", vec![file_id.into()])? {
        match e.kind.as_str() {
            "created" | "discovered" => {
                stops.push(obj(json!({"path": e.path, "since": iso(e.ts), "how": e.kind})));
            }
            "copied" => {
                stops.push(obj(
                    json!({"path": e.dest_path, "since": iso(e.ts), "how": "copied", "copied_from": e.path}),
                ));
            }
            "moved" | "renamed" => {
                if stops.is_empty() {
                    stops.push(obj(json!({"path": e.path, "since": null, "how": "unknown"})));
                }
                stops.last_mut().unwrap().insert("until".into(), iso(e.ts).into());
                stops.push(obj(compact(json!({
                    "path": e.dest_path, "since": iso(e.ts), "how": e.kind, "actor": e.actor, "reason": e.reason,
                }))));
            }
            "deleted" | "missing" => {
                if let Some(last) = stops.last_mut() {
                    last.insert("until".into(), iso(e.ts).into());
                    let recycled = e.details.as_deref().is_some_and(|d| d.contains("\"recycled\":true"));
                    last.insert("ended".into(), if recycled { "recycled" } else { e.kind.as_str() }.into());
                }
            }
            _ => {}
        }
    }
    Ok(stops.into_iter().map(Value::Object).collect())
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

pub fn history(store: &Store, file_id: i64, limit: i64) -> Result<Option<Value>> {
    let Some(row) = store.get_file(file_id)? else { return Ok(None) };
    let origin = match row.copied_from {
        Some(id) => store.get_file(id)?,
        None => None,
    };
    let copies: Vec<Value> =
        store.files_where("copied_from = ? ORDER BY first_seen", vec![file_id.into()])?.iter().map(file_json).collect();
    let mut events =
        store.events_where("file_id = ? ORDER BY ts DESC, id DESC LIMIT ?", vec![file_id.into(), limit.into()])?;
    events.reverse();
    Ok(Some(compact(json!({
        "file": file_json(&row),
        "trail": trail(store, file_id)?,
        "copied_from": origin.as_ref().map(file_json),
        "copies": if copies.is_empty() { Value::Null } else { Value::Array(copies) },
        "events": events.iter().map(event_json).collect::<Vec<_>>(),
    }))))
}

/// Where is this file now, and how did it get there?
pub fn locate(store: &Store, target: &str) -> Result<Value> {
    let mut matches = Vec::new();
    for row in resolve(store, target)? {
        matches.push(json!({
            "file": file_json(&row),
            "current_path": if row.present() { Value::String(row.path.clone()) } else { Value::Null },
            "trail": trail(store, row.id)?,
        }));
    }
    Ok(json!({"query": target, "matches": matches}))
}

#[derive(Default)]
pub struct RecentOpts<'a> {
    pub since: Option<&'a str>,
    pub kinds: Vec<String>,
    pub under: Option<&'a str>,
    pub source: Option<&'a str>,
    pub limit: i64,
}

/// Timeline of changes, newest first.  `kinds` may contain the groups `files` and `registry`.
pub fn recent(store: &Store, opts: RecentOpts) -> Result<Vec<Value>> {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(start) = parse_since(opts.since)? {
        clauses.push("ts >= ?".into());
        params.push(start.into());
    }
    if !opts.kinds.is_empty() {
        let mut expanded: Vec<String> = Vec::new();
        for kind in &opts.kinds {
            match kind.as_str() {
                "files" => expanded.extend(FILE_EVENT_KINDS.iter().map(|s| s.to_string())),
                "registry" => expanded.extend(REGISTRY_EVENT_KINDS.iter().map(|s| s.to_string())),
                other => expanded.push(other.to_string()),
            }
        }
        clauses.push(format!("kind IN ({})", vec!["?"; expanded.len()].join(",")));
        params.extend(expanded.into_iter().map(SqlValue::from));
    }
    if let Some(under) = opts.under {
        let (low, high) = descendant_range(&key(under));
        clauses.push("((path_key >= ? AND path_key < ?) OR (dest_key >= ? AND dest_key < ?))".into());
        params.extend([low.clone().into(), high.clone().into(), low.into(), high.into()]);
    }
    if let Some(source) = opts.source {
        clauses.push("source = ?".into());
        params.push(source.to_string().into());
    }
    let mut clause = if clauses.is_empty() { "1".to_string() } else { clauses.join(" AND ") };
    clause.push_str(" ORDER BY ts DESC, id DESC LIMIT ?");
    params.push(opts.limit.max(1).into());
    Ok(store.events_where(&clause, params)?.iter().map(event_json).collect())
}

pub fn registry_changes(
    store: &Store,
    since: Option<&str>,
    key_prefix: Option<&str>,
    limit: i64,
) -> Result<Vec<Value>> {
    let mut clause =
        format!("kind IN ({})", REGISTRY_EVENT_KINDS.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(","));
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(start) = parse_since(since)? {
        clause.push_str(" AND ts >= ?");
        params.push(start.into());
    }
    if let Some(prefix) = key_prefix {
        clause.push_str(" AND path_key LIKE ? ESCAPE '\\'");
        params.push(format!("{}%", like_escape(&prefix.to_lowercase())).into());
    }
    clause.push_str(" ORDER BY ts DESC, id DESC LIMIT ?");
    params.push(limit.into());
    Ok(store.events_where(&clause, params)?.iter().map(event_json).collect())
}

pub fn operations(store: &Store, limit: i64) -> Result<Vec<Value>> {
    let mut stmt = store.conn.prepare(
        "SELECT o.id, o.ts, o.kind, o.actor, o.description, o.undone_by,
                (SELECT COUNT(*) FROM events e WHERE e.op_id = o.id)
         FROM operations o ORDER BY ts DESC LIMIT ?",
    )?;
    let rows = stmt.query_map([limit], |r| {
        Ok(compact(json!({
            "id": r.get::<_, String>(0)?,
            "time": iso(r.get(1)?),
            "kind": r.get::<_, String>(2)?,
            "actor": r.get::<_, Option<String>>(3)?,
            "description": r.get::<_, Option<String>>(4)?,
            "events": r.get::<_, i64>(6)?,
            "undone_by": r.get::<_, Option<String>>(5)?,
        })))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn session_json(s: &SessionRow) -> Value {
    let parse =
        |t: &Option<String>| t.as_deref().and_then(|t| serde_json::from_str::<Value>(t).ok()).unwrap_or(Value::Null);
    compact(json!({
        "id": s.id,
        "label": s.label,
        "command": s.command,
        "status": s.status,
        "started": iso(s.started),
        "ended": iso_opt(s.ended),
        "pid": s.pid,
        "exit_code": s.exit_code,
        "roots": parse(&s.roots),
        "summary": parse(&s.summary),
    }))
}

pub fn sessions(store: &Store, limit: i64) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for s in store.sessions_where("1 ORDER BY started DESC LIMIT ?", vec![limit.into()])? {
        let mut v = session_json(&s);
        if let Value::Object(m) = &mut v {
            m.shift_remove("summary");
            let mut stmt = store.conn.prepare_cached(
                "SELECT source, kind, COUNT(*) FROM session_events WHERE session_id = ? GROUP BY source, kind",
            )?;
            let counts: Map<String, Value> = stmt
                .query_map([s.id], |r| {
                    Ok((format!("{}.{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?), json!(r.get::<_, i64>(2)?)))
                })?
                .collect::<rusqlite::Result<_>>()?;
            m.insert("counts".into(), Value::Object(counts));
        }
        out.push(v);
    }
    Ok(out)
}

pub fn session_events(
    store: &Store,
    session_id: i64,
    source: Option<&str>,
    kind: Option<&str>,
    under: Option<&str>,
    limit: i64,
) -> Result<Vec<Value>> {
    let mut clause = "session_id = ?".to_string();
    let mut params: Vec<SqlValue> = vec![session_id.into()];
    for (column, value) in [("source", source), ("kind", kind)] {
        if let Some(v) = value {
            clause.push_str(&format!(" AND {column} = ?"));
            params.push(v.to_string().into());
        }
    }
    if let Some(under) = under {
        clause.push_str(" AND path LIKE ? ESCAPE '\\'");
        params.push(format!("{}%", like_escape(under)).into());
    }
    clause.push_str(" ORDER BY ts, id LIMIT ?");
    params.push(limit.into());
    let mut stmt = store
        .conn
        .prepare(&format!("SELECT ts, source, kind, path, dest_path, details FROM session_events WHERE {clause}"))?;
    let rows = stmt.query_map(crate::store::sql_params(params), |r| {
        let details: Option<String> = r.get(5)?;
        Ok(compact(json!({
            "time": iso(r.get(0)?),
            "source": r.get::<_, String>(1)?,
            "kind": r.get::<_, String>(2)?,
            "path": r.get::<_, String>(3)?,
            "dest_path": r.get::<_, Option<String>>(4)?,
            "details": details.and_then(|d| serde_json::from_str::<Value>(&d).ok()),
        })))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn stats(store: &Store) -> Result<Value> {
    let mut stmt = store.conn.prepare("SELECT status, COUNT(*) FROM files GROUP BY status")?;
    let files: Map<String, Value> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, json!(r.get::<_, i64>(1)?))))?
        .collect::<rusqlite::Result<_>>()?;
    let (first, last, total): (Option<f64>, Option<f64>, i64) =
        store.conn.query_row("SELECT MIN(ts), MAX(ts), COUNT(*) FROM events", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
    Ok(json!({
        "files": files,
        "events": total,
        "first_event": iso_opt(first),
        "last_event": iso_opt(last),
        "sessions": store.scalar_i64("SELECT COUNT(*) FROM sessions", vec![])?,
        "database": norm(&store.path),
    }))
}
