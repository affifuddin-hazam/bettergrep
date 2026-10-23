//! Command-line interface.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::config::Config;
use crate::operations::{self, Actor};
use crate::paths::{file_name, norm};
use crate::query::{self, RecentOpts, SearchOpts};
use crate::store::Store;
use crate::trace::{self, RegistryMode, SessionOpts};
use crate::tracker::{ScanStats, Tracker, system_clock};
use crate::{VERSION, autostart, daemon, drive, folder_index, grep, log, mcp};

#[derive(Parser)]
#[command(
    name = "bettergrep",
    version,
    about = "Search inside and find any file on Windows, fast. Also remembers where every file went and what installers changed.",
    after_help = "Shorthand: `bettergrep PATTERN [PATHS]` is `bettergrep grep PATTERN [PATHS]`."
)]
struct Cli {
    /// Data directory (default: %LOCALAPPDATA%\bettergrep or %BETTERGREP_HOME%)
    #[arg(long, global = true)]
    home: Option<String>,
    /// Machine-readable JSON output
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the config, add folders to track (default: Downloads) and index them
    Init {
        paths: Vec<String>,
        /// Do not write .bettergrep.json files in these folders
        #[arg(long)]
        no_index: bool,
        /// Disable background registry monitoring
        #[arg(long)]
        no_registry: bool,
        #[arg(long)]
        no_scan: bool,
    },
    /// List, add or remove tracked folders: roots [list|add|remove] <folders>
    Roots {
        #[arg(value_parser = ["list", "add", "remove"], default_value = "list")]
        action: String,
        paths: Vec<String>,
        /// Track history but do not write .bettergrep.json files
        #[arg(long)]
        no_index: bool,
        /// Do not descend into subfolders
        #[arg(long)]
        top_level: bool,
        #[arg(long)]
        no_scan: bool,
    },
    /// Run the background monitor in the foreground
    Daemon {
        /// Skip the catch-up scan at start-up
        #[arg(long)]
        no_scan: bool,
        /// Log to file only
        #[arg(long)]
        quiet: bool,
        #[arg(short, long)]
        verbose: bool,
    },
    /// Stop the background monitor
    Stop,
    /// Monitor state, tracked folders and database statistics
    Status,
    /// Reconcile the database with disk (catches changes made while the monitor was off)
    Scan { paths: Vec<String> },
    /// Search tracked files by name or path
    Search {
        #[arg(required = true)]
        terms: Vec<String>,
        #[arg(long, default_value = "present", value_parser = ["present", "deleted", "missing", "any"])]
        status: String,
        #[arg(long)]
        under: Option<String>,
        #[arg(long = "type", value_parser = ["file", "folder"])]
        kind: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Where is this file now? (id, current or former path, or name)
    Where { target: String },
    /// Full history of one file
    History { target: String },
    /// Timeline of recent file and registry changes
    Changes {
        /// 30m, 2h, 7d, today, or an ISO date
        #[arg(long, default_value = "24h")]
        since: String,
        /// Event kind, or 'files' / 'registry' (repeatable)
        #[arg(long)]
        kind: Vec<String>,
        #[arg(long)]
        under: Option<String>,
        #[arg(long)]
        source: Option<String>,
        #[arg(long, default_value_t = 200)]
        limit: i64,
    },
    /// Recent registry changes seen by the monitor
    Registry {
        #[arg(long, default_value = "24h")]
        since: String,
        /// Key prefix, e.g. HKLM\SYSTEM\CurrentControlSet\Services
        #[arg(long)]
        key: Option<String>,
        #[arg(long, default_value_t = 200)]
        limit: i64,
    },
    /// Print (or --write) a folder's index
    Index {
        folder: String,
        #[arg(long)]
        write: bool,
    },
    /// Sort a folder's loose files into category subfolders (dry run unless --apply)
    Organize {
        folder: String,
        #[arg(long)]
        apply: bool,
    },
    /// Move or rename a file and record why
    Mv {
        src: String,
        dest: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Revert a move/organize operation
    Undo { op_id: String },
    /// Recent operations (for undo)
    Ops {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Record every file and registry change while a program runs: trace -- setup.exe /S
    Trace {
        #[arg(long)]
        label: Option<String>,
        /// Folder to watch (default: the system drive; repeatable)
        #[arg(long)]
        root: Vec<String>,
        /// full: HKCU+HKLM software and services; quick: persistence keys only
        #[arg(long, default_value = "full", value_parser = ["full", "quick", "off"])]
        registry: String,
        /// Seconds to keep recording after the program exits
        #[arg(long)]
        settle: Option<f64>,
        /// Also write the report to this file (.md or .json)
        #[arg(long)]
        report: Option<String>,
        #[arg(long, hide = true)]
        attach: Option<i64>,
        /// Program and arguments
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// List trace sessions
    Sessions {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Start or stop a background trace session, or show one
    Session {
        #[arg(value_parser = ["start", "stop", "show"])]
        action: String,
        id: Option<i64>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long, default_value = "full", value_parser = ["full", "quick", "off"])]
        registry: String,
        /// Include every recorded event (JSON)
        #[arg(long)]
        events: bool,
    },
    /// Run the MCP server on stdio (for Claude, Cursor, VS Code, ...)
    Mcp {
        /// Expose only read tools
        #[arg(long)]
        read_only: bool,
    },
    /// Start the monitor at logon
    Autostart {
        #[arg(value_parser = ["enable", "disable", "status"], default_value = "status")]
        action: String,
    },
    /// Full-drive mode: drives [list|add|remove|scan] <C: ...> (scan needs administrator rights)
    Drives {
        #[arg(value_parser = ["list", "add", "remove", "scan"], default_value = "list")]
        action: String,
        drives: Vec<String>,
    },
    /// Find files by name on every full-drive-mode drive, instantly
    Find {
        #[arg(required = true)]
        terms: Vec<String>,
        /// Show at most this many matches, best first (0 = no limit)
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Include hidden and system files
        #[arg(long)]
        hidden: bool,
    },
    /// Search inside files, like ripgrep. Folders are walked (respecting
    /// .gitignore); a drive root in full-drive mode (C:\) is listed from the
    /// index instead. Prints path:line:text; exits 1 when nothing matched.
    Grep {
        /// Regular expression (or plain text with -F)
        pattern: String,
        /// Files or folders to search (default: the current folder)
        paths: Vec<String>,
        /// Search every drive in full-drive mode
        #[arg(long)]
        drives: bool,
        #[arg(short = 'i', long)]
        ignore_case: bool,
        /// Treat the pattern as plain text
        #[arg(short = 'F', long)]
        fixed_strings: bool,
        /// Only files matching this glob, e.g. -g "*.rs" (repeatable). Case-insensitive,
        /// like Windows file names (ripgrep needs --iglob for that)
        #[arg(short = 'g', long = "glob")]
        globs: Vec<String>,
        /// Print only the paths of matching files
        #[arg(short = 'l', long)]
        files_with_matches: bool,
        /// Include hidden and system files and folders
        #[arg(long)]
        hidden: bool,
        /// Do not respect .gitignore and similar files
        #[arg(long)]
        no_ignore: bool,
        /// List folders on full-drive-mode drives from the index instead of walking them
        #[arg(long)]
        index: bool,
        /// Also open files with binary extensions (.exe, .zip, ...)
        #[arg(long)]
        binary: bool,
        /// Stop after this many results (0 = no limit)
        #[arg(long, default_value_t = 0)]
        limit: usize,
        /// Stop after this many seconds
        #[arg(long)]
        timeout: Option<f64>,
    },
    /// Dump files and events as JSON Lines
    Export {
        #[arg(long)]
        out: Option<String>,
    },
    /// Show the configuration or its path
    Config {
        #[arg(value_parser = ["show", "path"], default_value = "show")]
        action: String,
    },
}

// -- output helpers ------------------------------------------------------------

/// Status line for long `grep` runs on stderr, so a search reading a cold disk
/// for minutes does not look hung.  On a console it is one line rewritten in
/// place; when stderr is captured (scripts, AI tools) it is a plain line every
/// ten seconds.
struct Progress {
    tty: bool,
    next: Duration,
    /// Width of the line currently on the console, to blank it out.
    shown: usize,
}

impl Progress {
    fn new() -> Progress {
        let tty = std::io::stderr().is_terminal();
        Progress { tty, next: Duration::from_secs(if tty { 2 } else { 5 }), shown: 0 }
    }

    fn update(&mut self, p: &grep::GrepProgress) {
        if p.elapsed < self.next {
            return;
        }
        self.next = p.elapsed + Duration::from_secs(if self.tty { 1 } else { 10 });
        let line = format!(
            "searching... {} files, {} matches, {:.0} s",
            thousands(p.files_searched),
            thousands(p.matches),
            p.elapsed.as_secs_f64()
        );
        if self.tty {
            // Pad over a longer previous line; plain \r works on every console.
            eprint!("\r{line:<width$}", width = self.shown);
            self.shown = line.len().max(self.shown);
        } else {
            eprintln!("{line}");
        }
        let _ = std::io::stderr().flush();
    }

    fn clear(&mut self) {
        if self.shown > 0 {
            eprint!("\r{:width$}\r", "", width = self.shown);
            let _ = std::io::stderr().flush();
            self.shown = 0;
        }
    }
}

/// `count(1, "file", "files")` -> "1 file"; `count(29752, ...)` -> "29,752 files".
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", thousands(n), if n == 1 { one } else { many })
}

/// 12345678 -> "12,345,678".
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn emit(value: &Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
}

fn short_time(t: &Value) -> String {
    t.as_str().unwrap_or("").chars().take(19).collect::<String>().replace('T', " ")
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn print_events(events: &[Value]) {
    if events.is_empty() {
        println!("No events.");
    }
    for e in events {
        let mut target = s(&e["path"]).to_string();
        if let Some(dest) = e["dest_path"].as_str() {
            target.push_str(&format!("  ->  {dest}"));
        }
        if s(&e["kind"]).starts_with("reg_") {
            let d = &e["details"];
            if let Some(v) = d["value"].as_str() {
                target.push_str(&format!("  [{v}]"));
                if let Some(new) = d["new"].as_str() {
                    let short: String = new.chars().take(80).collect();
                    target.push_str(&format!(" = {short:?}"));
                }
            }
        }
        let extra: Vec<&str> = [e["actor"].as_str(), e["reason"].as_str()].into_iter().flatten().collect();
        let extra = if extra.is_empty() { String::new() } else { format!("  ({})", extra.join(", ")) };
        println!("{}  {:<18} {target}{extra}", short_time(&e["time"]), s(&e["kind"]));
    }
}

fn print_files(files: &[Value]) {
    if files.is_empty() {
        println!("No matching files.");
    }
    for f in files {
        let size = f["size"].as_i64().map(|n| format!("{n:>12}")).unwrap_or_else(|| " ".repeat(12));
        println!("#{:<7} {:<8} {size}  {}", f["id"], s(&f["status"]), s(&f["path"]));
    }
}

fn print_trail(trail: &Value) {
    for stop in trail.as_array().into_iter().flatten() {
        let mut note = s(&stop["how"]).to_string();
        if let Some(from) = stop["copied_from"].as_str() {
            note.push_str(&format!(" from {from}"));
        }
        if let Some(reason) = stop["reason"].as_str() {
            note.push_str(&format!(" ({reason})"));
        }
        if let Some(ended) = stop["ended"].as_str() {
            note.push_str(&format!(", then {ended} {}", short_time(&stop["until"])));
        }
        let since = short_time(&stop["since"]);
        println!("    {:<19}  {}  [{note}]", if since.is_empty() { "?".into() } else { since }, s(&stop["path"]));
    }
}

// -- commands ---------------------------------------------------------------------

fn scan(cfg: &Config, paths: &[String]) -> Result<Vec<(String, f64, ScanStats, u64)>> {
    let store = Store::open(&cfg.db_path())?;
    let mut tracker = Tracker::new(store, cfg.clone(), "scan", system_clock());
    let mut results = Vec::new();
    for path in paths {
        let recursive = cfg.root_for(path).is_none_or(|r| r.recursive);
        let t = Instant::now();
        let stats = tracker.reconcile(path, recursive)?;
        results.push((path.clone(), t.elapsed().as_secs_f64(), stats, 0));
    }
    tracker.flush()?;
    let mut written = 0;
    for folder in tracker.take_dirty_folders(Some(0.0)) {
        if cfg.folder_index_enabled(&folder)
            && folder_index::write(&tracker.store, &folder, cfg.index_max_entries)?.is_some()
        {
            written += 1;
        }
    }
    if let Some(last) = results.last_mut() {
        last.3 = written;
    }
    Ok(results)
}

fn report_scan(results: &[(String, f64, ScanStats, u64)], json_out: bool) {
    if json_out {
        emit(&json!(
            results
                .iter()
                .map(|(p, secs, st, w)| json!({"path": p, "seconds": secs, "stats": st, "indexes_written": w}))
                .collect::<Vec<_>>()
        ));
        return;
    }
    for (path, secs, st, _) in results {
        println!(
            "{path}: {} seen, {} new, {} modified, {} moved, {} followed elsewhere, {} recycled, {} missing ({secs:.1}s)",
            st.seen, st.discovered, st.modified, st.moved, st.followed, st.recycled, st.missing
        );
    }
}

fn spawn_detached(args: &[String]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let exe = std::env::current_exe()?;
    let windowless = exe.with_file_name("bettergrepw.exe");
    let program = if windowless.exists() { windowless } else { exe };
    // Windows hands every inheritable handle to the child, not only its standard
    // ones. If our output is a pipe (e.g. `$(bettergrep autostart enable)`), the
    // long-lived daemon would hold it open and the caller would wait forever.
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    for std_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe { SetHandleInformation(GetStdHandle(std_handle), HANDLE_FLAG_INHERIT, 0) };
    }
    std::process::Command::new(program)
        .args(args)
        .creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000) // detached, new group, no window
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

fn run(cli: Cli) -> Result<i32> {
    let mut cfg = Config::load(cli.home.as_deref())?;
    let json_out = cli.json;
    match cli.cmd {
        Cmd::Init { paths, no_index, no_registry, no_scan } => {
            let paths = if paths.is_empty() && cfg.roots.is_empty() {
                vec![format!("{}\\Downloads", std::env::var("USERPROFILE").unwrap_or_default())]
            } else {
                paths
            };
            let mut added = Vec::new();
            for p in &paths {
                if !Path::new(p).is_dir() {
                    eprintln!("warning: {p} is not a folder; skipping");
                    continue;
                }
                cfg.add_root(p, !no_index, true);
                added.push(norm(p));
            }
            if no_registry {
                cfg.registry.enabled = false;
            }
            cfg.save()?;
            Store::open(&cfg.db_path())?;
            println!("Config:   {}\nDatabase: {}", cfg.config_path(), cfg.db_path());
            for r in &cfg.roots {
                println!("Root:     {}{}", r.path, if r.folder_index { "" } else { "  (no folder index files)" });
            }
            if !no_scan && !added.is_empty() {
                println!("Indexing...");
                for (path, secs, st, _) in scan(&cfg, &added)? {
                    println!("  {path}: {} entries ({secs:.1}s)", st.seen);
                }
            }
            println!(
                "\nNext: run `bettergrep autostart enable` to keep tracking in the background (it also starts at every logon)."
            );
        }
        Cmd::Roots { action, paths, no_index, top_level, no_scan } => {
            match action.as_str() {
                "add" => {
                    for p in &paths {
                        if !Path::new(p).is_dir() {
                            bail!("{p} is not a folder");
                        }
                        cfg.add_root(p, !no_index, !top_level);
                    }
                    cfg.save()?;
                    if !no_scan && !paths.is_empty() {
                        let paths: Vec<String> = paths.iter().map(norm).collect();
                        for (path, _, st, _) in scan(&cfg, &paths)? {
                            println!("Indexed {path}: {} entries", st.seen);
                        }
                    }
                }
                "remove" => {
                    for p in &paths {
                        if !cfg.remove_root(p) {
                            eprintln!("warning: {p} was not a root");
                        }
                    }
                    cfg.save()?;
                }
                _ => {}
            }
            if json_out {
                emit(&json!(cfg.roots));
            } else if cfg.roots.is_empty() {
                println!("No roots. Add one with `bettergrep roots add <folder>`.");
            } else {
                for r in &cfg.roots {
                    let flags: Vec<&str> = [("folder-index", r.folder_index), ("recursive", r.recursive)]
                        .iter()
                        .filter(|f| f.1)
                        .map(|f| f.0)
                        .collect();
                    println!("{}  [{}]", r.path, flags.join(", "));
                }
            }
        }
        Cmd::Daemon { no_scan, quiet, verbose } => {
            log::init(&cfg.home, quiet, verbose);
            if cfg.roots.is_empty() && !cfg.registry.enabled {
                bail!("nothing to watch: add a folder with `bettergrep roots add <folder>`");
            }
            let d = daemon::Daemon::new(cfg, !no_scan)?;
            let halt = d.halt_flag();
            let _ = ctrlc::set_handler(move || halt.store(true, Ordering::Relaxed));
            d.run()?;
        }
        Cmd::Stop => {
            if daemon::read_state(&cfg).is_none() {
                println!("The daemon is not running.");
                return Ok(1);
            }
            daemon::request_stop(&cfg)?;
            for _ in 0..60 {
                std::thread::sleep(Duration::from_millis(500));
                if daemon::read_state(&cfg).is_none() {
                    println!("Stopped.");
                    return Ok(0);
                }
            }
            println!("Stop requested; the daemon is still finishing.");
        }
        Cmd::Status => {
            let store = Store::open(&cfg.db_path())?;
            let data = json!({
                "version": VERSION,
                "implementation": "rust",
                "data_dir": cfg.home,
                "roots": cfg.roots.iter().map(|r| r.path.clone()).collect::<Vec<_>>(),
                "registry_monitoring": cfg.registry.enabled,
                "daemon": daemon::read_state(&cfg),
                "autostart": autostart::status(),
                "database": query::stats(&store)?,
            });
            if json_out {
                emit(&data);
                return Ok(0);
            }
            println!("bettergrep {VERSION} (rust)  ({})", cfg.home);
            let d = &data["daemon"];
            if d.is_null() {
                println!("Daemon:    not running");
            } else {
                println!(
                    "Daemon:    running ({}), pid {}, {} fs events, {} registry changes",
                    d["implementation"].as_str().unwrap_or("python"),
                    d["pid"],
                    d["fs_events"],
                    d["registry_changes"]
                );
            }
            println!("Autostart: {}", if data["autostart"].is_null() { "disabled" } else { "enabled" });
            println!(
                "Registry:  {}",
                if cfg.registry.enabled {
                    format!("monitoring {} keys", cfg.registry.keys.len())
                } else {
                    "off".into()
                }
            );
            let roots: Vec<String> = cfg.roots.iter().map(|r| r.path.clone()).collect();
            println!("Roots:     {}", if roots.is_empty() { "(none)".into() } else { roots.join("\n           ") });
            let db = &data["database"];
            let files: Vec<String> =
                db["files"].as_object().into_iter().flatten().map(|(k, v)| format!("{v} {k}")).collect();
            println!(
                "Tracked:   {}; {} events; {} trace sessions",
                if files.is_empty() { "nothing yet".into() } else { files.join(", ") },
                db["events"],
                db["sessions"]
            );
        }
        Cmd::Scan { paths } => {
            let paths: Vec<String> = if paths.is_empty() {
                cfg.roots.iter().map(|r| r.path.clone()).collect()
            } else {
                paths.iter().map(norm).collect()
            };
            if paths.is_empty() {
                bail!("no roots configured");
            }
            report_scan(&scan(&cfg, &paths)?, json_out);
        }
        Cmd::Search { terms, status, under, kind, limit } => {
            let store = Store::open(&cfg.db_path())?;
            let under = under.map(norm);
            let files = query::search(
                &store,
                &terms.join(" "),
                SearchOpts { status: &status, under: under.as_deref(), kind: kind.as_deref(), limit },
            )?;
            if json_out { emit(&json!(files)) } else { print_files(&files) }
        }
        Cmd::Where { target } => {
            let store = Store::open(&cfg.db_path())?;
            let data = query::locate(&store, &target)?;
            let matches = data["matches"].as_array().cloned().unwrap_or_default();
            if json_out {
                emit(&data);
            } else if matches.is_empty() {
                println!("No tracked file matches {target:?}.");
            } else {
                for m in &matches {
                    let f = &m["file"];
                    println!("#{} {}  [{}]", f["id"], s(&f["name"]), s(&f["status"]));
                    match m["current_path"].as_str() {
                        Some(p) => println!("  now: {p}"),
                        None => println!("  now: (no longer exists: {})", s(&f["status"])),
                    }
                    if m["trail"].as_array().is_some_and(|t| t.len() > 1) {
                        println!("  trail:");
                        print_trail(&m["trail"]);
                    }
                }
            }
            return Ok(if matches.is_empty() { 1 } else { 0 });
        }
        Cmd::History { target } => {
            let store = Store::open(&cfg.db_path())?;
            let rows = query::resolve(&store, &target)?;
            if rows.is_empty() {
                println!("No tracked file matches {target:?}.");
                return Ok(1);
            }
            if rows.len() > 1 && target.trim_start_matches('#').parse::<i64>().is_err() {
                println!("Several files match; pick one by id:");
                print_files(&rows.iter().map(query::file_json).collect::<Vec<_>>());
                return Ok(1);
            }
            let data = query::history(&store, rows[0].id, 200)?.unwrap_or(Value::Null);
            if json_out {
                emit(&data);
                return Ok(0);
            }
            let f = &data["file"];
            println!("#{} {}  [{}]", f["id"], s(&f["path"]), s(&f["status"]));
            if let Some(o) = data.get("copied_from") {
                println!("  copied from #{} {}", o["id"], s(&o["path"]));
            }
            for c in data["copies"].as_array().into_iter().flatten() {
                println!("  copy: #{} {} [{}]", c["id"], s(&c["path"]), s(&c["status"]));
            }
            println!("  trail:");
            print_trail(&data["trail"]);
            println!("  events:");
            print_events(data["events"].as_array().map(Vec::as_slice).unwrap_or(&[]));
        }
        Cmd::Changes { since, kind, under, source, limit } => {
            let store = Store::open(&cfg.db_path())?;
            let under = under.map(norm);
            let mut events = query::recent(
                &store,
                RecentOpts {
                    since: Some(&since),
                    kinds: kind,
                    under: under.as_deref(),
                    source: source.as_deref(),
                    limit,
                },
            )?;
            if json_out {
                emit(&json!(events));
            } else {
                events.reverse();
                print_events(&events);
            }
        }
        Cmd::Registry { since, key, limit } => {
            let store = Store::open(&cfg.db_path())?;
            let mut events = query::registry_changes(&store, Some(&since), key.as_deref(), limit)?;
            if json_out {
                emit(&json!(events));
            } else {
                events.reverse();
                print_events(&events);
            }
        }
        Cmd::Index { folder, write } => {
            let store = Store::open(&cfg.db_path())?;
            if write {
                match folder_index::write(&store, &folder, cfg.index_max_entries)? {
                    Some(p) => println!("{p}"),
                    None => println!("Index already up to date."),
                }
            } else {
                emit(&folder_index::build(&store, &folder, cfg.index_max_entries)?);
            }
        }
        Cmd::Organize { folder, apply } => {
            let store = Store::open(&cfg.db_path())?;
            let result = operations::organize(&store, &cfg, &folder, apply, "cli", "bettergrep organize")?;
            if json_out {
                emit(&result);
                return Ok(0);
            }
            let root = s(&result["folder"]).to_string();
            let rel = |p: &str| p.strip_prefix(&format!("{root}\\")).unwrap_or(p).to_string();
            let planned = result["planned"].as_array().cloned().unwrap_or_default();
            if planned.is_empty() {
                println!("Nothing to organize.");
            } else if !apply {
                for p in &planned {
                    println!("{:<11} {}  ->  {}", s(&p["category"]), file_name(s(&p["src"])), rel(s(&p["dest"])));
                }
                println!("\n{} files would move. Run again with --apply to do it.", planned.len());
            } else {
                for m in result["moved"].as_array().into_iter().flatten() {
                    println!("moved  {}  ->  {}", file_name(s(&m["from"])), rel(s(&m["to"])));
                }
                let failed = result["failed"].as_array().cloned().unwrap_or_default();
                for f in &failed {
                    eprintln!("FAILED {}: {}", file_name(s(&f["from"])), s(&f["error"]));
                }
                let op = s(&result["op_id"]);
                println!(
                    "\nOperation {op}: {} moved, {} failed. Undo with `bettergrep undo {op}`.",
                    result["moved"].as_array().map_or(0, Vec::len),
                    failed.len()
                );
                return Ok(if failed.is_empty() { 0 } else { 1 });
            }
        }
        Cmd::Mv { src, dest, reason } => {
            let store = Store::open(&cfg.db_path())?;
            let out = operations::move_file(
                &store,
                &src,
                &dest,
                Actor { source: "cli", actor: Some("bettergrep mv"), reason: reason.as_deref(), op_id: None },
                false,
            )?;
            for folder in &out.touched {
                if cfg.folder_index_enabled(folder) {
                    folder_index::write(&store, folder, cfg.index_max_entries)?;
                }
            }
            if json_out { emit(&json!(out)) } else { println!("{}  ->  {}  (op {})", out.from, out.to, out.op_id) }
        }
        Cmd::Undo { op_id } => {
            let store = Store::open(&cfg.db_path())?;
            let out = operations::undo(&store, &cfg, &op_id, "cli", "bettergrep undo")?;
            if json_out {
                emit(&out);
                return Ok(0);
            }
            for r in out["restored"].as_array().into_iter().flatten() {
                println!("restored #{} -> {}", r["file_id"], s(&r["to"]));
            }
            for d in out["removed_folders"].as_array().into_iter().flatten() {
                println!("removed empty folder {}", s(d));
            }
            let skipped = out["skipped"].as_array().cloned().unwrap_or_default();
            for x in &skipped {
                eprintln!("skipped #{} {}: {}", x["file_id"], s(&x["path"]), s(&x["why"]));
            }
            return Ok(if skipped.is_empty() { 0 } else { 1 });
        }
        Cmd::Ops { limit } => {
            let store = Store::open(&cfg.db_path())?;
            let ops = query::operations(&store, limit)?;
            if json_out {
                emit(&json!(ops));
            } else {
                for o in &ops {
                    let undone = o["undone_by"].as_str().map(|u| format!("  (undone by {u})")).unwrap_or_default();
                    println!(
                        "{}  {}  {:<9} {}{undone}",
                        s(&o["id"]),
                        short_time(&o["time"]),
                        s(&o["kind"]),
                        s(&o["description"])
                    );
                }
            }
        }
        Cmd::Trace { label, root, registry, settle, report, attach, command } => {
            let store = Store::open(&cfg.db_path())?;
            let quiet = attach.is_some();
            let status: trace::Status = if quiet { Arc::new(|_| {}) } else { Arc::new(|msg: &str| eprintln!("{msg}")) };
            let keep_going = Arc::new(AtomicBool::new(true));
            {
                let k = keep_going.clone();
                let _ = ctrlc::set_handler(move || k.store(false, Ordering::Relaxed));
            }
            if command.is_empty() && attach.is_none() {
                eprintln!("Recording. Do what you want to trace, then press Enter (or Ctrl+C) to stop.");
                let k = keep_going.clone();
                std::thread::spawn(move || {
                    let _ = std::io::stdin().lock().read_line(&mut String::new());
                    k.store(false, Ordering::Relaxed);
                });
            }
            let id = trace::run_session(
                &store,
                &cfg,
                SessionOpts {
                    command,
                    label,
                    roots: root,
                    registry: RegistryMode::parse(&registry),
                    settle_seconds: settle,
                    keep_going: Some(keep_going),
                    status,
                    attach,
                },
            )?;
            let session = query::session_json(&store.get_session(id)?.ok_or_else(|| anyhow!("session vanished"))?);
            let markdown = trace::render_markdown(&session);
            if let Some(path) = report {
                let body = if path.to_lowercase().ends_with(".json") {
                    serde_json::to_string_pretty(&session)?
                } else {
                    markdown.clone()
                };
                std::fs::write(&path, body)?;
                eprintln!("Report written to {path}");
            }
            if !quiet {
                if json_out { emit(&session) } else { println!("{markdown}") }
            }
        }
        Cmd::Sessions { limit } => {
            let store = Store::open(&cfg.db_path())?;
            let rows = query::sessions(&store, limit)?;
            if json_out {
                emit(&json!(rows));
            } else {
                for r in &rows {
                    let total: i64 =
                        r["counts"].as_object().into_iter().flatten().filter_map(|(_, v)| v.as_i64()).sum();
                    println!(
                        "#{:<4} {}  {:<9} {total:>7} changes  {}",
                        r["id"],
                        short_time(&r["started"]),
                        s(&r["status"]),
                        s(&r["label"])
                    );
                }
            }
        }
        Cmd::Session { action, id, label, registry, events } => match action.as_str() {
            "start" => {
                let id = trace::start_background_session(&cfg, label.as_deref(), RegistryMode::parse(&registry), &[])?;
                println!("Session {id} starting (registry baseline first). Stop it with `bettergrep session stop`.");
            }
            "stop" => match trace::stop_session(&cfg, id, Duration::from_secs(180))? {
                None => {
                    println!("No running session.");
                    return Ok(1);
                }
                Some(session) => {
                    if json_out {
                        emit(&session)
                    } else {
                        println!("{}", trace::render_markdown(&session))
                    }
                }
            },
            _ => {
                let store = Store::open(&cfg.db_path())?;
                let id = id.ok_or_else(|| anyhow!("which session? `bettergrep session show <id>`"))?;
                let row = store.get_session(id)?.ok_or_else(|| anyhow!("unknown session {id}"))?;
                let mut session = query::session_json(&row);
                if events {
                    session["events"] = json!(query::session_events(&store, id, None, None, None, i64::MAX)?);
                }
                if json_out || events { emit(&session) } else { println!("{}", trace::render_markdown(&session)) }
            }
        },
        Cmd::Mcp { read_only } => mcp::run(cfg, read_only)?,
        Cmd::Autostart { action } => match action.as_str() {
            "enable" => {
                let (cmd, mode) = autostart::enable(&cfg)?;
                if mode == autostart::Mode::ElevatedTask {
                    println!(
                        "bettergrep will start at logon with administrator rights (scheduled task), keeping drive indexes current:\n  {cmd}"
                    );
                } else {
                    println!("bettergrep will start at logon:\n  {cmd}");
                    if !cfg.drives.is_empty() {
                        println!(
                            "note: drive indexes stay current only when the monitor has administrator rights; run this from an administrator terminal to set that up."
                        );
                    }
                }
                // A monitor started without administrator rights cannot follow the
                // drives' change journals: restart it with ours.
                let unelevated = daemon::read_state(&cfg).is_some_and(|s| !s["elevated"].as_bool().unwrap_or(false));
                if mode == autostart::Mode::ElevatedTask && unelevated {
                    daemon::request_stop(&cfg)?;
                    for _ in 0..60 {
                        if daemon::read_state(&cfg).is_none() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
                if daemon::read_state(&cfg).is_none() {
                    spawn_detached(&["--home".into(), cfg.home.clone(), "daemon".into(), "--quiet".into()])?;
                    println!("Started the daemon in the background.");
                }
            }
            "disable" => {
                println!("{}", if autostart::disable()? { "Autostart disabled." } else { "Autostart was not enabled." })
            }
            _ => println!(
                "{}",
                autostart::status().map(|c| format!("enabled: {c}")).unwrap_or_else(|| "disabled".into())
            ),
        },
        Cmd::Drives { action, drives } => {
            let requested: Vec<String> = drives.iter().filter_map(|d| drive::normalize_root(d)).collect();
            match action.as_str() {
                "add" => {
                    for d in &requested {
                        if !cfg.drives.contains(d) {
                            cfg.drives.push(d.clone());
                        }
                    }
                    cfg.save()?;
                    if drive::is_elevated() {
                        for d in &requested {
                            let t = Instant::now();
                            let volume = drive::Volume::open(d).map_err(|e| anyhow!("{d} {e}"))?;
                            let (index, outcome) =
                                drive::DriveIndex::open(&cfg.home, d, Some(&volume)).map_err(|e| anyhow!("{d} {e}"))?;
                            let how = match outcome {
                                drive::OpenOutcome::Scanned => "read from the drive",
                                _ => "already indexed, brought up to date",
                            };
                            println!("{d}  {} entries, {how} in {:.1?}", thousands(index.len()), t.elapsed());
                        }
                        if !autostart::status().is_some_and(|s| s.starts_with("scheduled task")) {
                            println!(
                                "To keep it current, run `bettergrep autostart enable` from this administrator terminal."
                            );
                        }
                    } else {
                        println!(
                            "Added. Indexing reads the drive's master file table, which needs administrator rights: run `bettergrep drives add {}` from an administrator terminal.",
                            requested.join(" ")
                        );
                    }
                }
                "remove" => {
                    cfg.drives.retain(|d| !requested.contains(d));
                    cfg.save()?;
                }
                "scan" => {
                    let targets = if requested.is_empty() { cfg.drives.clone() } else { requested };
                    if targets.is_empty() {
                        bail!("no drives: add one with `bettergrep drives add C:`");
                    }
                    for d in &targets {
                        let t = Instant::now();
                        let volume = drive::Volume::open(d).map_err(|e| anyhow!("{d} {e}"))?;
                        let mut index = drive::DriveIndex::scan(&volume).map_err(|e| anyhow!("{d} {e}"))?;
                        let scanned = t.elapsed();
                        index.save(&drive::DriveIndex::snapshot_path(&cfg.home, volume.serial))?;
                        println!(
                            "{d}  {} entries read in {scanned:.1?}, saved in {:.1?}",
                            index.len(),
                            t.elapsed() - scanned
                        );
                    }
                    return Ok(0);
                }
                _ => {}
            }
            let rows: Vec<Value> = cfg
                .drives
                .iter()
                .map(|d| {
                    let info = drive::volume_info(d).ok();
                    let snapshot = info.as_ref().map(|(serial, _)| drive::DriveIndex::snapshot_path(&cfg.home, *serial));
                    // The side file holds the latest changes, so it is newer than the snapshot.
                    let indexed = snapshot.as_deref().and_then(|p| {
                        [p.to_string(), format!("{p}.delta")]
                            .iter()
                            .filter_map(|f| std::fs::metadata(f).ok()?.modified().ok())
                            .max()
                    });
                    json!({
                        "drive": d,
                        "filesystem": info.map(|i| i.1),
                        "indexed": indexed.map(|t| query::iso(t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64())),
                    })
                })
                .collect();
            if json_out {
                emit(&json!(rows));
            } else if rows.is_empty() {
                println!(
                    "No drives in full-drive mode. Add one with `bettergrep drives add C:` (needs administrator rights to index)."
                );
            } else {
                for r in &rows {
                    println!(
                        "{}  {:<6} {}",
                        s(&r["drive"]),
                        s(&r["filesystem"]),
                        r["indexed"]
                            .as_str()
                            .map(|t| format!("indexed {}", short_time(&json!(t))))
                            .unwrap_or_else(|| "not indexed yet".into())
                    );
                }
            }
        }
        Cmd::Find { terms, limit, hidden } => {
            if cfg.drives.is_empty() {
                bail!("no drives in full-drive mode: add one with `bettergrep drives add C:`");
            }
            let mut indexes = Vec::new();
            let mut stale = false;
            for d in &cfg.drives {
                let volume = drive::Volume::open(d).ok();
                stale |= volume.is_none();
                match drive::DriveIndex::open(&cfg.home, d, volume.as_ref()) {
                    Ok((mut index, outcome)) => {
                        if index.should_save() {
                            let path = drive::DriveIndex::snapshot_path(&cfg.home, index.serial);
                            let _ = index.save(&path);
                        }
                        if outcome == drive::OpenOutcome::Scanned {
                            eprintln!("{d}: indexed {} entries", index.len());
                        }
                        indexes.push(index);
                    }
                    Err(e) => eprintln!("{d}: {e}"),
                }
            }
            let refs: Vec<&drive::DriveIndex> = indexes.iter().collect();
            // 0 means no limit, as for `grep`.
            let limit = if limit == 0 { usize::MAX } else { limit };
            let result = drive::search(&refs, &terms.join(" "), limit, hidden);
            if json_out {
                emit(&json!(result));
            } else {
                for h in &result.hits {
                    println!("{}{}", h.path, if h.is_dir { "\\" } else { "" });
                }
                println!(
                    "\n{} of {} in {:.0} ms",
                    thousands(result.hits.len()),
                    count(result.total, "match", "matches"),
                    result.millis
                );
                if stale {
                    eprintln!("note: not running as administrator, so results are from the last saved index");
                }
            }
        }
        Cmd::Grep {
            pattern,
            mut paths,
            drives,
            ignore_case,
            fixed_strings,
            globs,
            files_with_matches,
            hidden,
            no_ignore,
            index,
            binary,
            limit,
            timeout,
        } => {
            if drives {
                if cfg.drives.is_empty() {
                    bail!("no drives in full-drive mode: add one with `bettergrep drives add C:`");
                }
                paths.extend(cfg.drives.iter().cloned());
            }
            if paths.is_empty() {
                paths.push(".".into());
            }
            let opts = grep::GrepOptions {
                pattern,
                paths,
                ignore_case,
                fixed: fixed_strings,
                globs,
                files_only: files_with_matches,
                hidden,
                no_ignore,
                use_index: index,
                binary,
                limit,
                timeout: timeout.map(Duration::from_secs_f64),
            };
            let stdout = std::io::stdout();
            let out_tty = stdout.is_terminal();
            let mut out = std::io::BufWriter::new(stdout.lock());
            let progress = std::cell::RefCell::new(Progress::new());
            let report = grep::grep_with_progress(
                &cfg,
                &opts,
                |m| {
                    if json_out {
                        return;
                    }
                    progress.borrow_mut().clear();
                    // A closed pipe (e.g. `| head`) is not an error worth reporting.
                    let _ = if files_with_matches {
                        writeln!(out, "{}", m.path)
                    } else {
                        writeln!(out, "{}:{}:{}", m.path, m.line, m.text)
                    };
                    // Keep matches and the status line from interleaving on a console.
                    if out_tty {
                        let _ = out.flush();
                    }
                },
                |p| {
                    if !json_out {
                        progress.borrow_mut().update(p);
                    }
                },
            )?;
            progress.borrow_mut().clear();
            let _ = out.flush();
            drop(out);
            if json_out {
                emit(&json!(report));
            } else {
                for s in report.sources.iter().filter(|s| s.note.is_some()) {
                    eprintln!("note: {}: {}", s.path, s.note.as_deref().unwrap_or_default());
                }
                eprintln!(
                    "{} in {} ({} searched in {:.0} ms){}",
                    count(report.matches.len(), "match", "matches"),
                    count(report.files_matched, "file", "files"),
                    count(report.files_searched, "file", "files"),
                    report.millis,
                    match report.stopped_by {
                        Some("limit") => ", stopped at --limit",
                        Some(_) => ", stopped at --timeout",
                        None => "",
                    }
                );
            }
            return Ok(if report.matches.is_empty() { 1 } else { 0 });
        }
        Cmd::Export { out } => {
            let store = Store::open(&cfg.db_path())?;
            let mut writer: Box<dyn Write> = match &out {
                Some(path) => Box::new(std::io::BufWriter::new(std::fs::File::create(path)?)),
                None => Box::new(std::io::stdout().lock()),
            };
            for row in store.files_where("1 ORDER BY id", vec![])? {
                let mut v = json!({"type": "file"});
                v.as_object_mut().unwrap().extend(query::file_json(&row).as_object().cloned().unwrap_or_default());
                writeln!(writer, "{v}")?;
            }
            for row in store.events_where("1 ORDER BY id", vec![])? {
                let mut v = json!({"type": "event"});
                v.as_object_mut().unwrap().extend(query::event_json(&row).as_object().cloned().unwrap_or_default());
                writeln!(writer, "{v}")?;
            }
        }
        Cmd::Config { action } => {
            if action == "path" {
                println!("{}", cfg.config_path());
            } else {
                if !Path::new(&cfg.config_path()).exists() {
                    cfg.save()?;
                }
                emit(&serde_json::to_value(&cfg)?);
            }
        }
    }
    Ok(0)
}

/// `bettergrep PATTERN [PATHS]` is short for `bettergrep grep PATTERN [PATHS]`:
/// arguments that don't start with a command are tried as a grep before
/// giving up.  A pattern that is itself a command name needs the long form.
fn parse_args(args: Vec<std::ffi::OsString>) -> Result<Cli, clap::Error> {
    use clap::error::ErrorKind;
    match Cli::try_parse_from(&args) {
        Err(e) if args.len() > 1 && matches!(e.kind(), ErrorKind::InvalidSubcommand | ErrorKind::UnknownArgument) => {
            let mut grep = args;
            grep.insert(1, "grep".into());
            Cli::try_parse_from(grep).map_err(|_| e)
        }
        parsed => parsed,
    }
}

/// Entry point shared by `bettergrep.exe` and the windowless `bettergrepw.exe`.
pub fn main() -> i32 {
    let cli = parse_args(std::env::args_os().collect()).unwrap_or_else(|e| e.exit());
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Cmd, count, parse_args, thousands};

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(40965), "40,965");
        assert_eq!(thousands(12345678), "12,345,678");
    }

    #[test]
    fn counts_are_pluralized() {
        assert_eq!(count(0, "match", "matches"), "0 matches");
        assert_eq!(count(1, "file", "files"), "1 file");
        assert_eq!(count(29752, "file", "files"), "29,752 files");
    }

    fn parse(args: &[&str]) -> Option<Cmd> {
        let args = std::iter::once("bettergrep").chain(args.iter().copied()).map(Into::into).collect();
        parse_args(args).ok().map(|c| c.cmd)
    }

    #[test]
    fn bare_pattern_means_grep() {
        let Some(Cmd::Grep { pattern, paths, ignore_case, .. }) = parse(&["-i", "TODO", "src"]) else {
            panic!("not grep")
        };
        assert_eq!((pattern.as_str(), paths, ignore_case), ("TODO", vec!["src".to_string()], true));
        assert!(matches!(parse(&["TODO"]), Some(Cmd::Grep { .. })));
        assert!(matches!(parse(&["--json", "TODO"]), Some(Cmd::Grep { .. })));
        // Real commands still win, and the long form still works.
        assert!(matches!(parse(&["find", "invoice"]), Some(Cmd::Find { .. })));
        assert!(matches!(parse(&["grep", "find"]), Some(Cmd::Grep { .. })));
        assert!(parse(&["--bogus-flag"]).is_none());
    }
}
