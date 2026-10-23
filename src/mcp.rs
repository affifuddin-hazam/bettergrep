//! Model Context Protocol server over stdio (newline-delimited JSON-RPC 2.0).
//!
//! Implements what tool servers need: `initialize`, `tools/list`, `tools/call`
//! and `ping`.  `find_anywhere` and `grep_files` use the full-drive index when
//! one is available.

use std::io::{BufRead, Write};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};

use crate::config::Config;
use crate::drive::{self, DriveIndex, KindFilter};
use crate::operations::{self, Actor};
use crate::paths::norm;
use crate::query::{self, RecentOpts, SearchOpts};
use crate::store::Store;
use crate::trace::{self, RegistryMode};
use crate::{VERSION, daemon, folder_index, grep};

pub const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "bettergrep records the history of files and folders on this computer: where every \
file came from, every place it has moved to, copies, renames and deletions (including files sent to the \
Recycle Bin), plus Windows registry changes and full \"trace sessions\" of what an installer did.\n\
- To find where a file is now (even if it was moved or renamed), use locate_file with its name or any path it used to have.\n\
- To search inside files (like grep or ripgrep), use grep_files, with a folder path when you know one.\n\
- To find any file or folder anywhere on the computer by name, use find_anywhere (every file on the indexed drives, \
not only the tracked folders, but without history).\n\
- To see what is in a folder and where its former files went, use folder_index.\n\
- For \"what changed recently\", use recent_changes (files and registry).\n\
- To see what an installer did, use start_trace_session before it runs and stop_trace_session afterwards, \
or read past sessions with trace_session_report.\n\
- When you move or organize files for the user, prefer move_file / organize_folder so the move is recorded \
with a reason and can be undone with undo_operation.\n\
Times are local ISO-8601. File ids are stable across moves.";

struct Tool {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    properties: Value,
    required: &'static [&'static str],
    read_only: bool,
}

fn s(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}

fn tools() -> Vec<Tool> {
    let int = json!({"type": "integer"});
    let since = s("How far back: '30m', '2h', '7d', 'today', or an ISO date. Default 24h.");
    vec![
        Tool {
            name: "search_files",
            title: "Search files",
            description: "Search tracked files by name or path. Terms are ANDed; plain terms match anywhere in the name, '*'/'?' globs match the whole name, terms containing a slash match the full path.",
            properties: json!({"query": {"type": "string"}, "status": {"type": "string", "enum": ["present", "deleted", "missing", "any"], "description": "Default 'present'."}, "under": s("Only inside this folder."), "type": {"type": "string", "enum": ["file", "folder"]}, "limit": int}),
            required: &["query"],
            read_only: true,
        },
        Tool {
            name: "find_anywhere",
            title: "Find anywhere",
            description: "Find files and folders anywhere on the computer by name: every file on the drives in full-drive mode, not only the tracked folders (but without history). Terms are ANDed, case-insensitive parts of the name. Results are ranked: exact name, then prefix, then word start. 'as_of' says when each drive's index was last saved.",
            properties: json!({"query": {"type": "string"}, "type": {"type": "string", "enum": ["file", "folder"]}, "hidden": {"type": "boolean", "description": "Include hidden and system files. Default false."}, "limit": {"type": "integer", "description": "Default 50, at most 500."}}),
            required: &["query"],
            read_only: true,
        },
        Tool {
            name: "grep_files",
            title: "Search inside files",
            description: "Search the contents of files for a regular expression (ripgrep engine), returning path, line number and line text. Give a folder path when you can: folders are walked like ripgrep (respecting .gitignore). Without a path, or with a drive root like C:\\, every drive in full-drive mode is searched, listing files from the index; that reads a lot of data, so narrow it with glob (e.g. [\"*.log\"]). Binary, hidden and system files are skipped. 'complete' is false when the limit or time budget stopped the search early.",
            properties: json!({"pattern": s("Regular expression, or plain text with fixed=true."), "path": s("File or folder to search. Default: every full-drive-mode drive."), "glob": {"type": "array", "items": {"type": "string"}, "description": "Only files whose name matches, e.g. [\"*.rs\", \"*.toml\"]."}, "ignore_case": {"type": "boolean"}, "fixed": {"type": "boolean"}, "files_only": {"type": "boolean", "description": "One result per matching file."}, "hidden": {"type": "boolean"}, "limit": {"type": "integer", "description": "Default 100, at most 1000."}, "timeout_seconds": {"type": "integer", "description": "Default 20, at most 120."}}),
            required: &["pattern"],
            read_only: true,
        },
        Tool {
            name: "locate_file",
            title: "Locate a file",
            description: "Where is a file now, and every place it has been. Accepts a file id, a current or FORMER path (files that were moved away are still found), or a file name.",
            properties: json!({"target": {"type": "string"}}),
            required: &["target"],
            read_only: true,
        },
        Tool {
            name: "file_history",
            title: "File history",
            description: "Full history of one file: its trail of locations, copies made from it, what it was copied from, and every event (created, modified, renamed, moved, copied, deleted).",
            properties: json!({"target": s("File id, path or name."), "limit": int}),
            required: &["target"],
            read_only: true,
        },
        Tool {
            name: "folder_index",
            title: "Folder index",
            description: "The index of a folder: files here now (and where each came from), files that left (and where they are now), copies made elsewhere, deleted files, and recent events. Same content as the folder's .bettergrep.json, computed live.",
            properties: json!({"folder": {"type": "string"}, "limit": int}),
            required: &["folder"],
            read_only: true,
        },
        Tool {
            name: "recent_changes",
            title: "Recent changes",
            description: "Timeline of file and registry changes, newest first.",
            properties: json!({"since": since, "kinds": {"type": "array", "items": {"type": "string"}, "description": "Event kinds (created, modified, renamed, moved, copied, deleted, missing, discovered, reg_*) or the groups 'files' / 'registry'."}, "under": s("Only changes inside this folder."), "source": s("watcher, scan, organizer, cli, mcp or registry."), "limit": int}),
            required: &[],
            read_only: true,
        },
        Tool {
            name: "registry_changes",
            title: "Registry changes",
            description: "Changes the background monitor saw in watched registry keys (autostart, services, uninstall entries, environment, ...).",
            properties: json!({"since": since, "key_prefix": s("e.g. HKLM\\SYSTEM\\CurrentControlSet\\Services"), "limit": int}),
            required: &[],
            read_only: true,
        },
        Tool {
            name: "list_trace_sessions",
            title: "List trace sessions",
            description: "Recorded trace sessions (e.g. installer runs) with per-kind change counts.",
            properties: json!({"limit": int}),
            required: &[],
            read_only: true,
        },
        Tool {
            name: "trace_session_report",
            title: "Trace session report",
            description: "What a trace session found: summary of files added/modified/removed/extracted-then-removed, new folders, registry keys/values and notable changes (autostart, services, uninstall entries). Set include_events to page through raw events.",
            properties: json!({"session_id": int, "include_events": {"type": "boolean"}, "source": {"type": "string", "enum": ["fs", "registry"]}, "kind": {"type": "string"}, "under": s("Path or registry key prefix."), "limit": int}),
            required: &["session_id"],
            read_only: true,
        },
        Tool {
            name: "start_trace_session",
            title: "Start trace session",
            description: "Start recording every file and registry change system-wide (e.g. before the user runs an installer). Capturing the registry baseline takes ~10s with registry='full'. Returns the session id.",
            properties: json!({"label": {"type": "string"}, "registry": {"type": "string", "enum": ["full", "quick", "off"]}, "roots": {"type": "array", "items": {"type": "string"}}}),
            required: &[],
            read_only: false,
        },
        Tool {
            name: "stop_trace_session",
            title: "Stop trace session",
            description: "Stop a running trace session (the latest one if no id) and return its summary.",
            properties: json!({"session_id": int}),
            required: &[],
            read_only: false,
        },
        Tool {
            name: "organize_folder",
            title: "Organize folder",
            description: "Sort a folder's loose files into category subfolders (Images, Documents, Archives, Installers, ...). Dry run by default: call with apply=false, show the plan to the user, then apply=true. Every move is recorded and the folder indexes are updated; undo with undo_operation(op_id).",
            properties: json!({"folder": {"type": "string"}, "apply": {"type": "boolean"}}),
            required: &["folder"],
            read_only: false,
        },
        Tool {
            name: "move_file",
            title: "Move file",
            description: "Move or rename a file/folder and record why. dest may be a new path or an existing folder.",
            properties: json!({"src": {"type": "string"}, "dest": {"type": "string"}, "reason": {"type": "string"}}),
            required: &["src", "dest"],
            read_only: false,
        },
        Tool {
            name: "undo_operation",
            title: "Undo operation",
            description: "Revert a move/organize operation by its op_id, putting files back where they were.",
            properties: json!({"op_id": {"type": "string"}}),
            required: &["op_id"],
            read_only: false,
        },
        Tool {
            name: "list_operations",
            title: "List operations",
            description: "Recent move/organize/undo operations with their op_id.",
            properties: json!({"limit": int}),
            required: &[],
            read_only: true,
        },
        Tool {
            name: "bettergrep_status",
            title: "bettergrep status",
            description: "Monitored folders, whether the background monitor is running, and database statistics.",
            properties: json!({}),
            required: &[],
            read_only: true,
        },
    ]
}

impl Tool {
    fn describe(&self) -> Value {
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": {"type": "object", "properties": self.properties, "required": self.required, "additionalProperties": false},
            "annotations": {"title": self.title, "readOnlyHint": self.read_only, "destructiveHint": false, "openWorldHint": false},
        })
    }

    fn check_args(&self, args: &Map<String, Value>) -> Result<()> {
        let known = self.properties.as_object().map(|p| p.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
        if let Some(extra) = args.keys().find(|k| !known.contains(k)) {
            return Err(anyhow!("unexpected argument {extra:?}"));
        }
        if let Some(missing) = self.required.iter().find(|r| !args.contains_key(**r)) {
            return Err(anyhow!("missing required argument {missing:?}"));
        }
        Ok(())
    }
}

/// Expand `%VAR%` references; unknown variables are left as written.
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%').map(|end| (&after[..end], &after[end + 1..])) {
            Some((name, tail)) if !name.is_empty() && std::env::var(name).is_ok() => {
                out.push_str(&std::env::var(name).unwrap_or_default());
                rest = tail;
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A path argument from a client: `~` and `%VAR%` expanded, then normalised.
fn path_arg(value: &str) -> String {
    let value = match value.strip_prefix('~') {
        Some(rest) => format!("{}{rest}", std::env::var("USERPROFILE").unwrap_or_default()),
        None => value.to_string(),
    };
    norm(expand_env(&value))
}

/// Search every file on the drives in full-drive mode.  With administrator
/// rights the indexes are brought up to date first; otherwise they are read as
/// last saved by the bettergrep app.
fn find_anywhere(cfg: &Config, query: &str, kind: Option<&str>, hidden: bool, limit: usize) -> Result<Value> {
    if cfg.drives.is_empty() {
        return Err(anyhow!(
            "full-drive mode is off: add a drive with `bettergrep drives add C:` or in the bettergrep app's settings"
        ));
    }
    let kind = match kind {
        Some("file") => KindFilter::Files,
        Some("folder") => KindFilter::Folders,
        _ => KindFilter::Any,
    };
    let mut indexes = Vec::new();
    let mut drives = Vec::new();
    for d in &cfg.drives {
        let volume = drive::Volume::open(d).ok();
        match DriveIndex::open(&cfg.home, d, volume.as_ref()) {
            Ok((mut index, _)) => {
                let path = DriveIndex::snapshot_path(&cfg.home, index.serial);
                if index.should_save() {
                    let _ = index.save(&path);
                }
                let saved = [path.clone(), format!("{path}.delta")]
                    .iter()
                    .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
                    .max();
                drives.push(json!({
                    "drive": d,
                    "entries": index.len(),
                    "as_of": if volume.is_some() { "now".to_string() } else {
                        saved.map(|t| query::iso(t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64())).unwrap_or_default()
                    },
                }));
                indexes.push(index);
            }
            Err(e) => drives.push(json!({"drive": d, "error": e.to_string()})),
        }
    }
    let refs: Vec<&DriveIndex> = indexes.iter().collect();
    let r = drive::search_kind(&refs, query, limit, hidden, kind);
    Ok(
        json!({"total": r.total, "returned": r.hits.len(), "results": r.hits, "millis": r.millis.round(), "drives": drives}),
    )
}

pub struct Server {
    cfg: Config,
    store: Store,
    read_only: bool,
    tools: Vec<Tool>,
}

impl Server {
    pub fn new(cfg: Config, read_only: bool) -> Result<Server> {
        let store = Store::open(&cfg.db_path())?;
        let tools = tools().into_iter().filter(|t| !read_only || t.read_only).collect();
        Ok(Server { cfg, store, read_only, tools })
    }

    pub fn tool_names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.name).collect()
    }

    fn call(&self, name: &str, args: &Map<String, Value>) -> Result<Value> {
        let st = &self.store;
        let get = |k: &str| args.get(k).and_then(Value::as_str);
        let int = |k: &str, d: i64| args.get(k).and_then(Value::as_i64).unwrap_or(d);
        let flag = |k: &str| args.get(k).and_then(Value::as_bool).unwrap_or(false);
        let req = |k: &str| get(k).ok_or_else(|| anyhow!("argument {k:?} must be a string"));
        Ok(match name {
            "search_files" => json!(query::search(
                st,
                req("query")?,
                SearchOpts {
                    status: get("status").unwrap_or("present"),
                    under: get("under").map(path_arg).as_deref(),
                    kind: get("type"),
                    limit: int("limit", 50),
                }
            )?),
            "find_anywhere" => find_anywhere(
                &self.cfg,
                req("query")?,
                get("type"),
                flag("hidden"),
                int("limit", 50).clamp(1, 500) as usize,
            )?,
            "grep_files" => {
                let paths = match get("path") {
                    Some(p) => vec![path_arg(p)],
                    None if self.cfg.drives.is_empty() => {
                        return Err(anyhow!("give a path: no drives are in full-drive mode"));
                    }
                    None => self.cfg.drives.clone(),
                };
                let globs = args
                    .get("glob")
                    .and_then(Value::as_array)
                    .map_or_else(Vec::new, |a| a.iter().filter_map(Value::as_str).map(str::to_string).collect());
                let opts = grep::GrepOptions {
                    pattern: req("pattern")?.to_string(),
                    paths,
                    ignore_case: flag("ignore_case"),
                    fixed: flag("fixed"),
                    globs,
                    files_only: flag("files_only"),
                    hidden: flag("hidden"),
                    limit: int("limit", 100).clamp(1, 1000) as usize,
                    timeout: Some(Duration::from_secs(int("timeout_seconds", 20).clamp(1, 120) as u64)),
                    ..Default::default()
                };
                json!(grep::grep(&self.cfg, &opts, |_| {})?)
            }
            "locate_file" => query::locate(st, req("target")?)?,
            "file_history" => {
                let target = req("target")?;
                let rows = query::resolve(st, target)?;
                match rows.as_slice() {
                    [] => json!({"error": format!("no tracked file matches {target:?}")}),
                    [one] => query::history(st, one.id, int("limit", 200))?.unwrap_or(Value::Null),
                    _ if target.trim_start_matches('#').parse::<i64>().is_ok() => {
                        query::history(st, rows[0].id, int("limit", 200))?.unwrap_or(Value::Null)
                    }
                    many => {
                        json!({"ambiguous": true, "matches": many.iter().map(query::file_json).collect::<Vec<_>>(), "hint": "call again with one of the ids"})
                    }
                }
            }
            "folder_index" => folder_index::build(st, &path_arg(req("folder")?), int("limit", 200) as usize)?,
            "recent_changes" => {
                let kinds = args
                    .get("kinds")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let under = get("under").map(path_arg);
                json!(query::recent(
                    st,
                    RecentOpts {
                        since: Some(get("since").unwrap_or("24h")),
                        kinds,
                        under: under.as_deref(),
                        source: get("source"),
                        limit: int("limit", 100)
                    }
                )?)
            }
            "registry_changes" => json!(query::registry_changes(
                st,
                Some(get("since").unwrap_or("24h")),
                get("key_prefix"),
                int("limit", 200)
            )?),
            "list_trace_sessions" => json!(query::sessions(st, int("limit", 20))?),
            "trace_session_report" => {
                let id = args
                    .get("session_id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| anyhow!("session_id must be an integer"))?;
                match st.get_session(id)? {
                    None => json!({"error": format!("no session {id}")}),
                    Some(s) => {
                        let mut out = query::session_json(&s);
                        if flag("include_events") {
                            out["events"] = json!(query::session_events(
                                st,
                                id,
                                get("source"),
                                get("kind"),
                                get("under"),
                                int("limit", 200)
                            )?);
                        }
                        out
                    }
                }
            }
            "start_trace_session" => {
                let roots: Vec<String> = args
                    .get("roots")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(path_arg)).collect())
                    .unwrap_or_default();
                let id = trace::start_background_session(
                    &self.cfg,
                    get("label"),
                    RegistryMode::parse(get("registry").unwrap_or("full")),
                    &roots,
                )?;
                json!({"session_id": id, "status": "starting", "note": "Recording begins once the registry baseline is captured; call stop_trace_session when done."})
            }
            "stop_trace_session" => trace::stop_session(
                &self.cfg,
                args.get("session_id").and_then(Value::as_i64),
                Duration::from_secs(180),
            )?
            .unwrap_or_else(|| json!({"error": "no running session"})),
            "organize_folder" => operations::organize(
                st,
                &self.cfg,
                &path_arg(req("folder")?),
                flag("apply"),
                "mcp",
                "mcp: organize_folder",
            )?,
            "move_file" => {
                let who = Actor { source: "mcp", actor: Some("mcp: move_file"), reason: get("reason"), op_id: None };
                let out = operations::move_file(st, &path_arg(req("src")?), &path_arg(req("dest")?), who, false)?;
                for folder in &out.touched {
                    if self.cfg.folder_index_enabled(folder) {
                        folder_index::write(st, folder, self.cfg.index_max_entries)?;
                    }
                }
                json!(out)
            }
            "undo_operation" => operations::undo(st, &self.cfg, req("op_id")?, "mcp", "mcp: undo_operation")?,
            "list_operations" => json!(query::operations(st, int("limit", 20))?),
            "bettergrep_status" => json!({
                "version": VERSION,
                "implementation": "rust",
                "data_dir": self.cfg.home,
                "roots": self.cfg.roots,
                "daemon": daemon::read_state(&self.cfg).unwrap_or_else(|| json!({"running": false, "hint": "start it with `bettergrep daemon` or `bettergrep autostart enable`"})),
                "database": query::stats(st)?,
                "read_only": self.read_only,
            }),
            other => return Err(anyhow!("unknown tool {other}")),
        })
    }

    /// Handle one JSON-RPC message; `None` for notifications.
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let Some(obj) = msg.as_object().filter(|o| o.get("jsonrpc") == Some(&json!("2.0"))) else {
            return Some(error(Value::Null, -32600, "invalid request"));
        };
        let id = obj.get("id").cloned();
        let method = obj.get("method")?.as_str().unwrap_or_default();
        let params = obj.get("params").cloned().unwrap_or_else(|| json!({}));
        let result: std::result::Result<Value, (i64, String)> = match method {
            "initialize" => {
                let requested = params["protocolVersion"].as_str().unwrap_or_default();
                let version = if PROTOCOL_VERSIONS.contains(&requested) { requested } else { PROTOCOL_VERSIONS[0] };
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "bettergrep", "title": "bettergrep", "version": VERSION},
                    "instructions": INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.tools.iter().map(Tool::describe).collect::<Vec<_>>()})),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                match self.tools.iter().find(|t| t.name == name) {
                    None => Err((-32602, format!("unknown tool: {name}"))),
                    Some(tool) => {
                        let args = params["arguments"].as_object().cloned().unwrap_or_default();
                        Ok(match tool.check_args(&args).and_then(|_| self.call(name, &args)) {
                            Ok(data) => {
                                let is_error =
                                    data.as_object().is_some_and(|o| o.contains_key("error") && o.len() <= 2);
                                tool_result(data, is_error)
                            }
                            Err(e) => tool_result(json!({"error": format!("{e:#}")}), true),
                        })
                    }
                }
            }
            "resources/list" => Ok(json!({"resources": []})),
            "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
            "prompts/list" => Ok(json!({"prompts": []})),
            m if m.starts_with("notifications/") => return None,
            m => Err((-32601, format!("method not found: {m}"))),
        };
        let id = id?; // notifications get no reply
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => error(id, code, &message),
        })
    }

    pub fn serve(&self, input: impl BufRead, mut output: impl Write) -> Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Err(_) => Some(error(Value::Null, -32700, "parse error")),
                Ok(Value::Array(batch)) => {
                    let replies: Vec<Value> = batch.iter().filter_map(|m| self.handle(m)).collect();
                    (!replies.is_empty()).then_some(Value::Array(replies))
                }
                Ok(msg) => self.handle(&msg),
            };
            if let Some(reply) = reply {
                writeln!(output, "{reply}")?;
                output.flush()?;
            }
        }
        Ok(())
    }
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn tool_result(data: Value, is_error: bool) -> Value {
    let structured = if data.is_object() { data.clone() } else { json!({"result": data}) };
    json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&data).unwrap_or_default()}],
        "isError": is_error,
        "structuredContent": structured,
    })
}

pub fn run(cfg: Config, read_only: bool) -> Result<()> {
    let server = Server::new(cfg, read_only)?;
    let stdin = std::io::stdin();
    server.serve(stdin.lock(), std::io::stdout().lock())
}
