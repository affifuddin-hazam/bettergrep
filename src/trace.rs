//! Trace sessions: record everything that changes on disk and in the registry
//! while an installer (or anything else) runs.
//!
//! The recorder starts watching first, captures a registry baseline (in
//! parallel), and treats every folder that changes before launch as background
//! activity.  It then launches the program (through UAC if it needs elevation),
//! records until it exits plus a settle period, diffs the registry and
//! summarises what was added, modified, removed, and extracted-then-removed.
//! Attribution is by time window, not by process; see `docs/tracing.md`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, unbounded};
use serde_json::{Value, json};

use crate::config::Config;
use crate::paths::{IgnoreRules, key, norm, parent};
use crate::query::session_json;
use crate::registry::{self, Change, Exclusions, Snapshot};
use crate::store::{Store, now};
use crate::watcher::{FsEvent, Watcher};
use crate::win::OwnedHandle;

const NOTABLE: &[(&str, &[&str])] = &[
    ("autostart", &["\\currentversion\\run", "\\currentversion\\runonce", "\\winlogon", "\\policies\\explorer\\run"]),
    ("service", &["\\currentcontrolset\\services\\"]),
    ("uninstall entry", &["\\currentversion\\uninstall\\"]),
    ("scheduled task", &["\\schedule\\taskcache\\"]),
    ("debugger hijack", &["\\image file execution options\\"]),
    ("environment", &["\\environment"]),
    ("shell extension", &["\\shellex\\", "\\shellexecutehooks", "\\shell extensions\\"]),
    ("file association", &["\\classes\\."]),
    ("COM class", &["\\classes\\clsid\\", "\\wow6432node\\clsid\\"]),
];

fn classify(key_path: &str) -> Option<&'static str> {
    let lower = key_path.to_lowercase();
    NOTABLE.iter().find(|(_, needles)| needles.iter().any(|n| lower.contains(n))).map(|(label, _)| *label)
}

#[derive(Clone, Debug)]
pub struct FsRow {
    pub ts: f64,
    pub kind: String,
    pub path: String,
    pub dest: Option<String>,
    pub is_dir: bool,
}

pub type Status = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RegistryMode {
    Full,
    Quick,
    Off,
}

impl RegistryMode {
    pub fn parse(s: &str) -> RegistryMode {
        match s {
            "quick" => RegistryMode::Quick,
            "off" => RegistryMode::Off,
            _ => RegistryMode::Full,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RegistryMode::Full => "full",
            RegistryMode::Quick => "quick",
            RegistryMode::Off => "off",
        }
    }
}

struct Recorder<'a> {
    store: &'a Store,
    cfg: &'a Config,
    session_id: i64,
    roots: Vec<String>,
    mode: RegistryMode,
    status: Status,
    ignored: IgnoreRules,
    watcher: Option<Watcher>,
    rx: Receiver<FsEvent>,
    rows: Vec<FsRow>,
    modified_seen: HashSet<String>,
    overflowed: Vec<String>,
    baseline: HashMap<String, Snapshot>,
    background_dirs: HashSet<String>,
}

impl<'a> Recorder<'a> {
    fn registry_keys(&self) -> Vec<String> {
        match self.mode {
            RegistryMode::Full => self.cfg.trace.registry_keys.clone(),
            RegistryMode::Quick => self.cfg.registry.keys.clone(),
            RegistryMode::Off => Vec::new(),
        }
    }

    fn start(
        store: &'a Store,
        cfg: &'a Config,
        session_id: i64,
        roots: Vec<String>,
        mode: RegistryMode,
        status: Status,
    ) -> Result<Self> {
        let (tx, rx) = unbounded();
        let watch: Vec<(String, bool)> =
            roots.iter().filter(|r| Path::new(r).is_dir()).map(|r| (r.clone(), true)).collect();
        // Watch first: whatever changes before launch (including during the
        // registry baseline) is background activity, not the program.
        let watcher = Watcher::start(&watch, tx)?;
        let learning = Instant::now();
        status(&format!("Watching {}", roots.join(", ")));
        let mut rec = Recorder {
            store,
            cfg,
            session_id,
            roots,
            mode,
            status: status.clone(),
            ignored: cfg.trace_ignore_rules(),
            watcher: Some(watcher),
            rx,
            rows: Vec::new(),
            modified_seen: HashSet::new(),
            overflowed: Vec::new(),
            baseline: HashMap::new(),
            background_dirs: HashSet::new(),
        };
        let keys = rec.registry_keys();
        if !keys.is_empty() {
            status(&format!("Capturing registry baseline ({} keys, mode={})...", keys.len(), mode.as_str()));
            let exclude = Exclusions::new(&cfg.trace.registry_exclude);
            rec.baseline = registry::snapshot_parallel(&keys, None, &exclude, registry::default_threads());
            let count: usize = rec.baseline.values().map(|s| s.len()).sum();
            status(&format!("Registry baseline: {count} keys in {:.1}s", learning.elapsed().as_secs_f64()));
        }
        let remaining = cfg.trace.baseline_seconds - learning.elapsed().as_secs_f64();
        if remaining > 0.0 {
            status(&format!("Sampling background activity for {remaining:.0}s..."));
            std::thread::sleep(Duration::from_secs_f64(remaining));
        }
        for ev in rec.rx.try_iter() {
            match ev {
                FsEvent::Created { path, .. } | FsEvent::Modified { path, .. } | FsEvent::Deleted { path, .. } => {
                    rec.background_dirs.insert(key(parent(&path)));
                }
                FsEvent::Moved { src, dest, .. } => {
                    rec.background_dirs.insert(key(parent(&src)));
                    rec.background_dirs.insert(key(parent(&dest)));
                }
                FsEvent::Overflow { .. } => {}
            }
        }
        if !rec.background_dirs.is_empty() {
            status(&format!(
                "{} folders were busy before launch; changes there are reported as background activity",
                rec.background_dirs.len()
            ));
        }
        Ok(rec)
    }

    /// Move collected notifications into the database.
    fn flush(&mut self) -> Result<()> {
        let mut batch = Vec::new();
        for ev in self.rx.try_iter() {
            let (kind, path, dest, is_dir) = match ev {
                FsEvent::Created { path, is_dir } => ("created", path, None, is_dir),
                FsEvent::Deleted { path, is_dir } => ("deleted", path, None, is_dir),
                FsEvent::Modified { path, is_dir } => {
                    if is_dir || !self.modified_seen.insert(key(&path)) {
                        continue; // one 'modified' per file is enough for a report
                    }
                    ("modified", path, None, false)
                }
                FsEvent::Moved { src, dest, is_dir } => ("moved", src, Some(dest), is_dir),
                FsEvent::Overflow { root } => {
                    self.overflowed.push(root.clone());
                    ("overflow", root, None, true)
                }
            };
            let ignored = self.ignored.is_ignored(&path) && dest.as_deref().is_none_or(|d| self.ignored.is_ignored(d));
            if ignored && kind != "overflow" {
                continue;
            }
            batch.push(FsRow { ts: now(), kind: kind.to_string(), path, dest, is_dir });
        }
        if batch.is_empty() {
            return Ok(());
        }
        self.store.tx(|| {
            self.store.add_session_events(
                self.session_id,
                batch.iter().map(|r| {
                    (
                        r.ts,
                        "fs",
                        r.kind.as_str(),
                        r.path.as_str(),
                        r.dest.as_deref(),
                        r.is_dir.then(|| json!({"is_dir": true})),
                    )
                }),
            )
        })?;
        self.rows.extend(batch);
        Ok(())
    }

    fn stop(mut self) -> Result<Value> {
        if let Some(w) = self.watcher.take() {
            w.stop();
        }
        self.flush()?;
        let mut changes: Vec<Change> = Vec::new();
        if !self.baseline.is_empty() {
            (self.status)("Diffing registry...");
            let keys = self.registry_keys();
            let exclude = Exclusions::new(&self.cfg.trace.registry_exclude);
            let after = registry::snapshot_parallel(&keys, Some(&self.baseline), &exclude, registry::default_threads());
            for k in &keys {
                if let (Some(before), Some(after)) = (self.baseline.get(k), after.get(k)) {
                    changes.extend(registry::diff(before, after));
                }
            }
            let ts = now();
            self.store.tx(|| {
                self.store.add_session_events(
                    self.session_id,
                    changes.iter().map(|c| (ts, "registry", c.kind, c.key.as_str(), None, Some(c.details()))),
                )
            })?;
        }
        let mut summary = summarize(&self.rows, &changes, &self.background_dirs);
        if !self.overflowed.is_empty() {
            summary["warnings"] = json!([format!(
                "Windows dropped notifications for {} during the session; some short-lived files may be missing.",
                self.overflowed.join(", ")
            )]);
        }
        let _ = &self.roots;
        Ok(summary)
    }
}

struct Entry {
    path: String,
    created: bool,
    modified: bool,
    deleted: bool,
    is_dir: bool,
}

/// Replay a session's events into the final picture of what changed.  Files in
/// folders that were busy before launch are counted as background activity;
/// folders never are, so a new install folder in a busy parent is still reported.
pub fn summarize(rows: &[FsRow], changes: &[Change], background_dirs: &HashSet<String>) -> Value {
    let mut state: BTreeMap<String, Entry> = BTreeMap::new();
    let entry = |path: &str, state: &mut BTreeMap<String, Entry>| {
        state.entry(key(path)).or_insert_with(|| Entry {
            path: norm(path),
            created: false,
            modified: false,
            deleted: false,
            is_dir: false,
        });
        key(path)
    };
    for r in rows {
        match r.kind.as_str() {
            "created" => {
                let k = entry(&r.path, &mut state);
                let e = state.get_mut(&k).unwrap();
                e.created = true;
                e.deleted = false;
                e.is_dir |= r.is_dir;
            }
            "modified" => {
                let k = entry(&r.path, &mut state);
                state.get_mut(&k).unwrap().modified = true;
            }
            "deleted" => {
                let k = entry(&r.path, &mut state);
                state.get_mut(&k).unwrap().deleted = true;
            }
            "moved" => {
                let k = entry(&r.path, &mut state);
                state.get_mut(&k).unwrap().deleted = true;
                if let Some(dest) = &r.dest {
                    let k = entry(dest, &mut state);
                    let e = state.get_mut(&k).unwrap();
                    e.created = true;
                    e.deleted = false;
                    e.is_dir |= r.is_dir;
                }
            }
            _ => {}
        }
    }

    let (mut added, mut modified, mut removed, mut transient, mut background) =
        (vec![], vec![], vec![], vec![], vec![]);
    for e in state.values_mut() {
        let exists = Path::new(&e.path).exists();
        if exists && Path::new(&e.path).is_dir() {
            e.is_dir = true;
        }
        if !e.is_dir && background_dirs.contains(&key(parent(&e.path))) {
            background.push(&*e);
        } else if e.created && exists {
            added.push(&*e);
        } else if e.created {
            transient.push(&*e);
        } else if exists && e.modified && !e.is_dir {
            modified.push(&*e);
        } else if !exists && e.deleted {
            removed.push(&*e);
        }
    }

    // Folders created during the session whose parent was not, with file counts.
    fn top_level(entries: &[&Entry]) -> Vec<(String, usize, u64)> {
        let dirs: HashSet<String> = entries.iter().filter(|e| e.is_dir).map(|e| key(&e.path)).collect();
        let mut tops: Vec<&&Entry> =
            entries.iter().filter(|e| e.is_dir && !dirs.contains(&key(parent(&e.path)))).collect();
        tops.sort_by_key(|e| key(&e.path));
        tops.iter()
            .map(|top| {
                let prefix = format!("{}\\", key(&top.path));
                let files: Vec<&&Entry> =
                    entries.iter().filter(|e| !e.is_dir && key(&e.path).starts_with(&prefix)).collect();
                let bytes = files.iter().map(|e| std::fs::metadata(&e.path).map(|m| m.len()).unwrap_or(0)).sum();
                (top.path.clone(), files.len(), bytes)
            })
            .collect()
    }
    let loose = |entries: &[&Entry], tops: &[(String, usize, u64)]| -> Vec<String> {
        let covered: Vec<String> = tops.iter().map(|(p, _, _)| format!("{}\\", key(p))).collect();
        entries
            .iter()
            .filter(|e| !e.is_dir && !covered.iter().any(|c| key(&e.path).starts_with(c)))
            .map(|e| e.path.clone())
            .collect()
    };
    let new_folders = top_level(&added);
    let temp_folders = top_level(&transient);
    let cap = 200;

    let count = |k: &str| changes.iter().filter(|c| c.kind == k).count();
    let notable: Vec<Value> = changes
        .iter()
        .filter_map(|c| classify(&c.key).map(|label| (label, c)))
        .take(cap)
        .map(|(label, c)| {
            let mut v = json!({"category": label, "kind": c.kind, "key": c.key});
            if let (Value::Object(m), Value::Object(d)) = (&mut v, c.details()) {
                m.extend(d);
            }
            v
        })
        .collect();
    let uninstall: Vec<Value> = changes
        .iter()
        .filter(|c| {
            c.kind == "reg_value_created"
                && c.value.as_deref() == Some("DisplayName")
                && c.key.to_lowercase().contains("\\uninstall\\")
        })
        .map(|c| json!({"name": c.new, "key": c.key}))
        .collect();
    let created_keys: Vec<&String> = changes.iter().filter(|c| c.kind == "reg_key_created").map(|c| &c.key).collect();
    let created_set: HashSet<String> = created_keys.iter().map(|k| k.to_lowercase()).collect();
    let mut top_keys: Vec<&String> =
        created_keys.into_iter().filter(|k| !created_set.contains(&parent_key_of(k))).collect();
    top_keys.sort();

    let mut bg_folders: Vec<String> =
        background.iter().map(|e| parent(&e.path)).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    bg_folders.truncate(cap);
    let sorted = |v: &[&Entry]| -> Vec<String> {
        let mut out: Vec<String> = v.iter().map(|e| e.path.clone()).collect();
        out.sort();
        out.truncate(cap);
        out
    };

    json!({
        "files": {
            "added": added.iter().filter(|e| !e.is_dir).count(),
            "folders_added": added.iter().filter(|e| e.is_dir).count(),
            "modified": modified.len(),
            "removed": removed.len(),
            "transient": transient.iter().filter(|e| !e.is_dir).count(),
        },
        "new_folders": new_folders.iter().take(cap).map(|(p, n, b)| json!({"path": p, "files": n, "bytes": b})).collect::<Vec<_>>(),
        "loose_files_added": loose(&added, &new_folders).into_iter().take(cap).collect::<Vec<_>>(),
        "modified_files": sorted(&modified),
        "removed_files": sorted(&removed),
        "extracted_then_removed": {
            "folders": temp_folders.iter().take(cap).map(|(p, n, _)| json!({"path": p, "files": n})).collect::<Vec<_>>(),
            "files": loose(&transient, &temp_folders).into_iter().take(cap).collect::<Vec<_>>(),
        },
        "background": {"files": background.len(), "folders": bg_folders},
        "registry": {
            "keys_created": count("reg_key_created"),
            "keys_deleted": count("reg_key_deleted"),
            "values_created": count("reg_value_created"),
            "values_changed": count("reg_value_changed"),
            "values_deleted": count("reg_value_deleted"),
            "new_keys": top_keys.into_iter().take(cap).collect::<Vec<_>>(),
            "notable": notable,
            "uninstall_entries": uninstall,
        },
    })
}

fn parent_key_of(k: &str) -> String {
    k.rsplit_once('\\').map(|(p, _)| p.to_lowercase()).unwrap_or_default()
}

// -- launching --------------------------------------------------------------

pub enum Proc {
    Std(std::process::Child),
    Elevated { handle: OwnedHandle, pid: u32, code: Option<i32> },
}

impl Proc {
    pub fn pid(&self) -> u32 {
        match self {
            Proc::Std(c) => c.id(),
            Proc::Elevated { pid, .. } => *pid,
        }
    }

    pub fn poll(&mut self) -> Option<i32> {
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
        match self {
            Proc::Std(c) => c.try_wait().ok().flatten().map(|s| s.code().unwrap_or(-1)),
            Proc::Elevated { handle, code, .. } => {
                if code.is_none() && unsafe { WaitForSingleObject(handle.0, 0) } == 0 {
                    let mut c = 0u32;
                    unsafe { GetExitCodeProcess(handle.0, &mut c) };
                    *code = Some(c as i32);
                }
                *code
            }
        }
    }
}

/// Find a program like a shell would: relative paths and PATH, trying PATHEXT.
fn resolve_program(name: &str) -> Option<String> {
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(|e| e.to_lowercase())
        .collect();
    let candidates = |base: &Path| -> Vec<std::path::PathBuf> {
        let mut out = vec![base.to_path_buf()];
        if base.extension().is_none() {
            out.extend(exts.iter().map(|e| base.with_extension(e.trim_start_matches('.'))));
        }
        out
    };
    let p = Path::new(name);
    let has_dir = name.contains('\\') || name.contains('/');
    let dirs: Vec<std::path::PathBuf> = if has_dir || p.is_absolute() {
        vec![std::path::PathBuf::new()]
    } else {
        std::iter::once(std::path::PathBuf::from("."))
            .chain(std::env::var_os("PATH").map(|v| std::env::split_paths(&v).collect::<Vec<_>>()).unwrap_or_default())
            .collect()
    };
    dirs.iter().flat_map(|d| candidates(&d.join(p))).find(|c| c.is_file()).map(norm)
}

fn shell_execute_runas(program: &str, args: &[String]) -> Result<Proc> {
    use windows_sys::Win32::System::Threading::GetProcessId;
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let verb = crate::win::wide("runas");
    let file = crate::win::wide(program);
    let params = crate::win::wide(&args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" "));
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_SHOWNORMAL;
    if unsafe { ShellExecuteExW(&mut info) } == 0 || info.hProcess.is_null() {
        return Err(anyhow!("elevation was refused or failed"));
    }
    let handle = OwnedHandle(info.hProcess);
    let pid = unsafe { GetProcessId(handle.0) };
    Ok(Proc::Elevated { handle, pid, code: None })
}

fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    format!("\"{}\"", a.replace('"', "\\\""))
}

/// Start `command`; if Windows says it needs elevation, ask through UAC.
pub fn launch(command: &[String]) -> Result<Proc> {
    let program = resolve_program(&command[0]).ok_or_else(|| anyhow!("cannot find program {:?}", command[0]))?;
    match std::process::Command::new(&program).args(&command[1..]).spawn() {
        Ok(child) => Ok(Proc::Std(child)),
        Err(e) if e.raw_os_error() == Some(740) => shell_execute_runas(&program, &command[1..]),
        Err(e) => Err(e).with_context(|| format!("cannot start {program}")),
    }
}

// -- sessions -----------------------------------------------------------------

pub struct SessionOpts {
    pub command: Vec<String>,
    pub label: Option<String>,
    pub roots: Vec<String>,
    pub registry: RegistryMode,
    pub settle_seconds: Option<f64>,
    /// Interactive/manual sessions: keep recording while this returns true.
    pub keep_going: Option<Arc<AtomicBool>>,
    pub status: Status,
    pub attach: Option<i64>,
}

/// Run a trace session and return its id.
pub fn run_session(store: &Store, cfg: &Config, opts: SessionOpts) -> Result<i64> {
    let roots: Vec<String> = if opts.roots.is_empty() { cfg.trace.roots.clone() } else { opts.roots.clone() };
    let roots: Vec<String> = roots.iter().map(norm).collect();
    let settle = opts.settle_seconds.unwrap_or(cfg.trace.settle_seconds);
    let cmdline =
        (!opts.command.is_empty()).then(|| opts.command.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" "));
    let session_id = match opts.attach {
        Some(id) => id,
        None => {
            let label = opts.label.clone().unwrap_or_else(|| match opts.command.first() {
                Some(p) => crate::paths::file_name(p),
                None => "manual session".into(),
            });
            store.tx(|| store.create_session(&label, cmdline.as_deref(), &roots))?
        }
    };
    let status = opts.status.clone();
    let mut exit_code: Option<i32> = None;
    let outcome = (|| -> Result<Value> {
        let mut rec = Recorder::start(store, cfg, session_id, roots.clone(), opts.registry, status.clone())?;
        store.conn.execute(
            "UPDATE sessions SET started = ?, pid = ? WHERE id = ?",
            rusqlite::params![now(), std::process::id(), session_id],
        )?;
        let mut proc = None;
        if let Some(cmd) = &cmdline {
            status(&format!("Launching {cmd}"));
            let p = launch(&opts.command)?;
            store.conn.execute("UPDATE sessions SET pid = ? WHERE id = ?", rusqlite::params![p.pid(), session_id])?;
            proc = Some(p);
        }
        let mut last_check = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(500));
            rec.flush()?;
            if let Some(p) = proc.as_mut()
                && let Some(code) = p.poll()
            {
                exit_code = Some(code);
                status(&format!("Program exited with code {code}; recording {settle:.0}s more..."));
                let deadline = Instant::now() + Duration::from_secs_f64(settle);
                while Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(500));
                    rec.flush()?;
                }
                break;
            }
            if last_check.elapsed() >= Duration::from_secs(1) {
                last_check = Instant::now();
                let stopping = store.get_session(session_id)?.is_some_and(|s| s.status == "stopping");
                let interrupted = opts.keep_going.as_ref().is_some_and(|k| !k.load(Ordering::Relaxed));
                if stopping || interrupted {
                    break;
                }
            }
        }
        rec.stop()
    })();
    let (status_text, summary) = match &outcome {
        Ok(summary) => ("finished", summary.clone()),
        Err(e) => ("failed", json!({"error": format!("{e:#}")})),
    };
    store.conn.execute(
        "UPDATE sessions SET ended = ?, status = ?, exit_code = ?, summary = ? WHERE id = ?",
        rusqlite::params![now(), status_text, exit_code, summary.to_string(), session_id],
    )?;
    outcome.map(|_| session_id)
}

/// Start a manual session in a detached process; returns the session id.
pub fn start_background_session(
    cfg: &Config,
    label: Option<&str>,
    mode: RegistryMode,
    roots: &[String],
) -> Result<i64> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let store = Store::open(&cfg.db_path())?;
    let roots: Vec<String> = if roots.is_empty() { cfg.trace.roots.clone() } else { roots.iter().map(norm).collect() };
    let session_id = store.tx(|| store.create_session(label.unwrap_or("manual session"), None, &roots))?;
    let mut args = vec![
        "--home".to_string(),
        cfg.home.clone(),
        "trace".into(),
        "--attach".into(),
        session_id.to_string(),
        "--registry".into(),
        mode.as_str().into(),
    ];
    for r in &roots {
        args.extend(["--root".to_string(), r.clone()]);
    }
    std::process::Command::new(std::env::current_exe()?)
        .args(&args)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("cannot start the background recorder")?;
    Ok(session_id)
}

/// Ask a running session to stop (the latest if `id` is `None`) and wait for its summary.
pub fn stop_session(cfg: &Config, id: Option<i64>, timeout: Duration) -> Result<Option<Value>> {
    let store = Store::open(&cfg.db_path())?;
    let id = match id {
        Some(id) => id,
        None => match store.sessions_where("status = 'running' ORDER BY started DESC LIMIT 1", vec![])?.first() {
            Some(s) => s.id,
            None => return Ok(None),
        },
    };
    store.conn.execute("UPDATE sessions SET status = 'stopping' WHERE id = ? AND status = 'running'", [id])?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if store.get_session(id)?.is_none_or(|s| s.status == "finished" || s.status == "failed") {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(store.get_session(id)?.map(|s| session_json(&s)))
}

/// Human-readable report for a session (as returned by `query::session_json`).
pub fn render_markdown(session: &Value) -> String {
    let s = &session["summary"];
    let f = &s["files"];
    let r = &s["registry"];
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let text = |v: &Value| v.as_str().unwrap_or("").to_string();
    let mut out = vec![
        format!("# Trace: {}", text(&session["label"])),
        String::new(),
        format!("- Command: `{}`", session["command"].as_str().unwrap_or("(manual session)")),
        format!(
            "- Started: {}  Ended: {}  Exit code: {}",
            text(&session["started"]),
            text(&session["ended"]),
            session.get("exit_code").map(|v| v.to_string()).unwrap_or_else(|| "-".into())
        ),
        format!(
            "- Watched: {}",
            session["roots"].as_array().map(|a| a.iter().map(text).collect::<Vec<_>>().join(", ")).unwrap_or_default()
        ),
        String::new(),
        "## Files".into(),
        format!(
            "{} files added (plus {} new folders), {} modified, {} removed, {} extracted then removed.",
            n(&f["added"]),
            n(&f["folders_added"]),
            n(&f["modified"]),
            n(&f["removed"]),
            n(&f["transient"])
        ),
        String::new(),
    ];
    let list = |out: &mut Vec<String>, title: &str, items: &Value| {
        if let Some(items) = items.as_array().filter(|a| !a.is_empty()) {
            out.push(format!("### {title}"));
            out.push(String::new());
            out.extend(items.iter().map(|p| format!("- `{}`", text(p))));
            out.push(String::new());
        }
    };
    if let Some(folders) = s["new_folders"].as_array().filter(|a| !a.is_empty()) {
        out.push("### New folders".into());
        out.push(String::new());
        for x in folders {
            out.push(format!("- `{}` ({} files, {} bytes)", text(&x["path"]), n(&x["files"]), n(&x["bytes"])));
        }
        out.push(String::new());
    }
    list(&mut out, "Files added elsewhere", &s["loose_files_added"]);
    list(&mut out, "Modified files", &s["modified_files"]);
    list(&mut out, "Removed files", &s["removed_files"]);
    let temp = &s["extracted_then_removed"];
    let temp_folders = temp["folders"].as_array().cloned().unwrap_or_default();
    let temp_files = temp["files"].as_array().cloned().unwrap_or_default();
    if !temp_folders.is_empty() || !temp_files.is_empty() {
        out.push("### Extracted, then cleaned up".into());
        out.push(String::new());
        out.extend(temp_folders.iter().map(|x| format!("- `{}` ({} files)", text(&x["path"]), n(&x["files"]))));
        out.extend(temp_files.iter().map(|p| format!("- `{}`", text(p))));
        out.push(String::new());
    }
    out.push("## Registry".into());
    out.push(format!(
        "{} keys created, {} deleted; {} values created, {} changed, {} deleted.",
        n(&r["keys_created"]),
        n(&r["keys_deleted"]),
        n(&r["values_created"]),
        n(&r["values_changed"]),
        n(&r["values_deleted"])
    ));
    out.push(String::new());
    if let Some(entries) = r["uninstall_entries"].as_array().filter(|a| !a.is_empty()) {
        out.push("### Uninstall entries".into());
        out.push(String::new());
        out.extend(entries.iter().map(|u| format!("- **{}** - `{}`", text(&u["name"]), text(&u["key"]))));
        out.push(String::new());
    }
    if let Some(notable) = r["notable"].as_array().filter(|a| !a.is_empty()) {
        out.push("### Notable changes".into());
        out.push(String::new());
        out.push("| Category | Change | Key | Value | Data |".into());
        out.push("|---|---|---|---|---|".into());
        for x in notable {
            let data = x.get("new").or_else(|| x.get("old")).map(text).unwrap_or_default();
            let data: String = data.replace('|', "\\|").replace('\n', " ").chars().take(120).collect();
            out.push(format!(
                "| {} | {} | `{}` | {} | {} |",
                text(&x["category"]),
                text(&x["kind"]).trim_start_matches("reg_"),
                text(&x["key"]),
                text(&x["value"]),
                data
            ));
        }
        out.push(String::new());
    }
    list(&mut out, "New keys", &r["new_keys"]);
    let bg = &s["background"];
    if n(&bg["files"]) > 0 {
        out.push("## Background activity (not attributed to the program)".into());
        out.push(String::new());
        out.push(format!("{} file changes in folders that were already busy before launch:", n(&bg["files"])));
        out.push(String::new());
        out.extend(bg["folders"].as_array().into_iter().flatten().map(|d| format!("- `{}`", text(d))));
        out.push(String::new());
    }
    for w in s["warnings"].as_array().into_iter().flatten() {
        out.push(format!("> **Warning:** {}", text(w)));
        out.push(String::new());
    }
    if let Some(err) = s["error"].as_str() {
        out.push("## Error".into());
        out.push(String::new());
        out.push(format!("`{err}`"));
        out.push(String::new());
    }
    out.join("\n")
}
