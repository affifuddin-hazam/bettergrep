//! The background service: watches roots and registry keys and keeps the index current.
//!
//! Watcher and registry threads only send messages; the main thread owns the
//! database, the [`Tracker`] and the folder index writes.  State, stop requests
//! and the single-instance lock live in the data directory
//! (`daemon.json`, `stop.request`, `daemon.lock`), so two daemons never run at once.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError, unbounded};
use serde_json::{Value, json};

use crate::config::Config;
use crate::paths::{is_within, key};
use crate::registry::{Change, RegistryWatcher};
use crate::store::{NewEvent, Store, now};
use crate::tracker::{Tracker, system_clock};
use crate::watcher::{FsEvent, Watcher};
use crate::{VERSION, drive, folder_index, log};

pub const STATE_FILE: &str = "daemon.json";
pub const STOP_FILE: &str = "stop.request";
pub const LOCK_FILE: &str = "daemon.lock";

enum Msg {
    Fs(FsEvent),
    Reg(Vec<Change>),
}

fn home_file(cfg: &Config, name: &str) -> String {
    Path::new(&cfg.home).join(name).to_string_lossy().into_owned()
}

/// Exclusive lock released by the OS when the process exits.
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    pub fn acquire(cfg: &Config) -> Option<InstanceLock> {
        std::fs::create_dir_all(&cfg.home).ok()?;
        let file =
            File::options().create(true).truncate(false).read(true).write(true).open(home_file(cfg, LOCK_FILE)).ok()?;
        file.try_lock().ok()?;
        Some(InstanceLock { _file: file })
    }
}

/// The running daemon's self-reported state, or `None` if it is not running.
pub fn read_state(cfg: &Config) -> Option<Value> {
    let text = std::fs::read_to_string(home_file(cfg, STATE_FILE)).ok()?;
    let mut state: Value = serde_json::from_str(&text).ok()?;
    if InstanceLock::acquire(cfg).is_some() {
        return None; // nobody holds the lock
    }
    let age = now() - state["heartbeat"].as_f64().unwrap_or(0.0);
    state["heartbeat_age_seconds"] = json!((age * 10.0).round() / 10.0);
    Some(state)
}

pub fn request_stop(cfg: &Config) -> Result<()> {
    std::fs::write(home_file(cfg, STOP_FILE), now().to_string())?;
    Ok(())
}

/// How often the drive keeper reads the change journals.
const DRIVE_POLL: Duration = Duration::from_secs(2);
/// How often it re-reads `config.json` for drives added or removed.
const DRIVE_CONFIG_POLL: Duration = Duration::from_secs(10);
/// How long it waits before retrying a drive that could not be opened.
const DRIVE_RETRY: Duration = Duration::from_secs(300);

/// Keeps the full-drive indexes (`bettergrep drives`) current from each
/// volume's USN change journal.  Reading a journal needs administrator rights,
/// so this only works when the daemon runs elevated (`bettergrep autostart
/// enable` from an administrator terminal sets that up).  Readers without
/// those rights, such as the CLI and the MCP server, see the changes through
/// the side file each index writes every few minutes.
fn keep_drives(home: &str, stop: &AtomicBool) {
    struct Slot {
        root: String,
        volume: drive::Volume,
        index: drive::DriveIndex,
    }
    let save = |ix: &mut drive::DriveIndex| {
        if let Err(e) = ix.save(&drive::DriveIndex::snapshot_path(home, ix.serial)) {
            log::warn(&format!("saving drive index failed: {e}"));
        }
    };
    let mut slots: Vec<Slot> = Vec::new();
    let mut retry_at: HashMap<String, Instant> = HashMap::new();
    let mut next_config = Instant::now();
    let mut warned_admin = false;
    while !stop.load(Ordering::Relaxed) {
        if Instant::now() >= next_config {
            next_config = Instant::now() + DRIVE_CONFIG_POLL;
            let wanted = Config::load(Some(home)).map(|c| c.drives).unwrap_or_default();
            if !wanted.is_empty() && !drive::is_elevated() {
                if !warned_admin {
                    log::info("not running as administrator: full-drive indexes are not kept current");
                    warned_admin = true;
                }
            } else {
                for slot in slots.iter_mut().filter(|s| !wanted.contains(&s.root)) {
                    save(&mut slot.index);
                }
                slots.retain(|s| wanted.contains(&s.root));
                for root in &wanted {
                    if slots.iter().any(|s| &s.root == root) || retry_at.get(root).is_some_and(|t| Instant::now() < *t)
                    {
                        continue;
                    }
                    let opened = drive::Volume::open(root).and_then(|volume| {
                        let (index, outcome) = drive::DriveIndex::open(home, root, Some(&volume))?;
                        Ok((volume, index, outcome))
                    });
                    match opened {
                        Ok((volume, index, outcome)) => {
                            log::info(&format!("keeping {root} current ({} entries, {outcome:?})", index.len()));
                            retry_at.remove(root);
                            slots.push(Slot { root: root.clone(), volume, index });
                        }
                        Err(e) => {
                            log::warn(&format!("cannot keep {root} current: {e}; retrying in 5 minutes"));
                            retry_at.insert(root.clone(), Instant::now() + DRIVE_RETRY);
                        }
                    }
                }
            }
        }
        for slot in &mut slots {
            match slot.index.catch_up(&slot.volume) {
                Ok(_) => {}
                Err(drive::DriveError::JournalReset) => {
                    log::warn(&format!("the change journal of {} was reset; reading the drive again", slot.root));
                    match drive::DriveIndex::open(home, &slot.root, Some(&slot.volume)) {
                        Ok((index, _)) => slot.index = index,
                        Err(e) => log::warn(&format!("re-reading {} failed: {e}", slot.root)),
                    }
                }
                Err(e) => log::warn(&format!("reading the change journal of {} failed: {e}", slot.root)),
            }
            // Nothing reads the live feed here; drop it so it stays small.
            let _ = slot.index.take_updates();
            if slot.index.should_save() {
                save(&mut slot.index);
            }
        }
        let wake = Instant::now() + DRIVE_POLL;
        while Instant::now() < wake && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    for slot in slots.iter_mut().filter(|s| s.index.is_dirty()) {
        save(&mut slot.index);
    }
}

/// Roots to watch, dropping any already covered by a recursive ancestor.
pub fn effective_roots(cfg: &Config) -> Vec<(String, bool)> {
    let mut roots = cfg.roots.clone();
    roots.sort_by_key(|r| r.path.len());
    let mut chosen: Vec<(String, bool)> = Vec::new();
    for root in roots {
        if !Path::new(&root.path).is_dir() {
            log::warn(&format!("root {} does not exist; skipping", root.path));
            continue;
        }
        if chosen.iter().any(|(p, rec)| *rec && is_within(&key(&root.path), &key(p))) {
            continue;
        }
        chosen.push((root.path, root.recursive));
    }
    chosen
}

pub struct Daemon {
    cfg: Config,
    tracker: Tracker,
    scan_on_start: bool,
    started: f64,
    fs_events: u64,
    registry_changes: u64,
    indexes_written: u64,
    halt: Arc<AtomicBool>,
}

impl Daemon {
    pub fn new(cfg: Config, scan_on_start: bool) -> Result<Daemon> {
        let store = Store::open(&cfg.db_path())?;
        let tracker = Tracker::new(store, cfg.clone(), "watcher", system_clock());
        Ok(Daemon {
            cfg,
            tracker,
            scan_on_start,
            started: now(),
            fs_events: 0,
            registry_changes: 0,
            indexes_written: 0,
            halt: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Flag that stops [`Daemon::run`] (e.g. from a Ctrl+C handler or a test).
    pub fn halt_flag(&self) -> Arc<AtomicBool> {
        self.halt.clone()
    }

    pub fn run(mut self) -> Result<()> {
        let Some(_lock) = InstanceLock::acquire(&self.cfg) else {
            bail!("another bettergrep daemon is already running for {}", self.cfg.home);
        };
        let _ = std::fs::remove_file(home_file(&self.cfg, STOP_FILE));
        log::info(&format!("bettergrep {VERSION} (rust) starting; data dir {}", self.cfg.home));

        let (tx, rx) = unbounded::<Msg>();
        let (fs_tx, fs_rx) = unbounded::<FsEvent>();
        let mut watcher = Some(self.start_watcher(fs_tx.clone())?);
        let registry = self.start_registry(tx.clone())?;
        let drives_stop = Arc::new(AtomicBool::new(false));
        let drives = {
            let (home, stop) = (self.cfg.home.clone(), drives_stop.clone());
            std::thread::spawn(move || keep_drives(&home, &stop))
        };
        // Forward filesystem events into the main queue.
        let forward = {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for ev in fs_rx {
                    if tx.send(Msg::Fs(ev)).is_err() {
                        break;
                    }
                }
            })
        };
        if self.scan_on_start {
            self.reconcile_all();
        }
        let mut last_scan = Instant::now();
        let result = self.event_loop(&rx, &mut watcher, &fs_tx, &mut last_scan);

        log::info("shutting down");
        drives_stop.store(true, Ordering::Relaxed);
        let _ = drives.join();
        drop(watcher.take());
        if let Some(r) = registry {
            r.stop();
        }
        drop(fs_tx);
        drop(tx);
        let _ = forward.join();
        while let Ok(msg) = rx.try_recv() {
            self.dispatch(msg);
        }
        if let Err(e) = self.tracker.flush() {
            log::error(&format!("flush failed: {e:#}"));
        }
        self.write_indexes(true);
        let _ = std::fs::remove_file(home_file(&self.cfg, STATE_FILE));
        result
    }

    fn start_watcher(&self, fs_tx: crossbeam_channel::Sender<FsEvent>) -> Result<Watcher> {
        let roots = effective_roots(&self.cfg);
        for (path, recursive) in &roots {
            log::info(&format!("watching {path}{}", if *recursive { "" } else { " (top level only)" }));
        }
        Watcher::start(&roots, fs_tx)
    }

    fn start_registry(&self, tx: crossbeam_channel::Sender<Msg>) -> Result<Option<RegistryWatcher>> {
        if !self.cfg.registry.enabled || self.cfg.registry.keys.is_empty() {
            return Ok(None);
        }
        let sink = Box::new(move |changes: Vec<Change>| {
            let _ = tx.send(Msg::Reg(changes));
        });
        let debounce = Duration::from_secs_f64(self.cfg.registry.debounce_seconds);
        log::info(&format!("watching {} registry keys", self.cfg.registry.keys.len()));
        Ok(Some(RegistryWatcher::start(self.cfg.registry.keys.clone(), debounce, sink)?))
    }

    fn event_loop(
        &mut self,
        rx: &Receiver<Msg>,
        watcher: &mut Option<Watcher>,
        fs_tx: &crossbeam_channel::Sender<FsEvent>,
        last_scan: &mut Instant,
    ) -> Result<()> {
        let stop_path = home_file(&self.cfg, STOP_FILE);
        let mut config_mtime = mtime(&self.cfg.config_path());
        let (mut next_tick, mut next_beat) = (Instant::now(), Instant::now());
        while !self.halt.load(Ordering::Relaxed) {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(msg) => {
                    self.dispatch(msg);
                    for msg in rx.try_iter().take(500) {
                        self.dispatch(msg); // drain bursts without ticking in between
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if Instant::now() >= next_tick {
                next_tick = Instant::now() + Duration::from_millis(500);
                if let Err(e) = self.tracker.tick() {
                    log::error(&format!("tick failed: {e:#}"));
                }
                self.write_indexes(false);
            }
            if Instant::now() >= next_beat {
                next_beat = Instant::now() + Duration::from_secs(5);
                self.heartbeat(rx.len());
                if Path::new(&stop_path).exists() {
                    log::info("stop requested");
                    let _ = std::fs::remove_file(&stop_path);
                    break;
                }
                let m = mtime(&self.cfg.config_path());
                if m != config_mtime {
                    config_mtime = m;
                    self.reload_config(watcher, fs_tx);
                }
                if self.cfg.rescan_hours > 0.0 && last_scan.elapsed().as_secs_f64() > self.cfg.rescan_hours * 3600.0 {
                    self.reconcile_all();
                    *last_scan = Instant::now();
                }
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, msg: Msg) {
        let result = match msg {
            Msg::Fs(ev) => {
                self.fs_events += 1;
                match ev {
                    FsEvent::Created { path, is_dir } => self.tracker.created(&path, is_dir),
                    FsEvent::Modified { path, is_dir } => self.tracker.modified(&path, is_dir),
                    FsEvent::Deleted { path, .. } => self.tracker.deleted(&path),
                    FsEvent::Moved { src, dest, is_dir } => self.tracker.moved(&src, &dest, is_dir),
                    FsEvent::Overflow { root } => {
                        log::warn(&format!("notifications for {root} overflowed; rescanning"));
                        let recursive = self.cfg.root_for(&root).is_none_or(|r| r.recursive);
                        self.tracker.reconcile(&root, recursive).map(|_| ())
                    }
                }
            }
            Msg::Reg(changes) => {
                self.registry_changes += changes.len() as u64;
                let store = &self.tracker.store;
                let ts = now();
                store.tx(|| {
                    for c in &changes {
                        store.add_event(NewEvent {
                            kind: c.kind,
                            source: "registry",
                            ts: Some(ts),
                            path: Some(&c.key),
                            details: Some(c.details()),
                            ..Default::default()
                        })?;
                    }
                    Ok(())
                })
            }
        };
        if let Err(e) = result {
            log::error(&format!("failed to process event: {e:#}"));
        }
    }

    fn reconcile_all(&mut self) {
        for root in self.cfg.roots.clone() {
            if !Path::new(&root.path).is_dir() {
                continue;
            }
            let t = Instant::now();
            match self.tracker.reconcile(&root.path, root.recursive) {
                Ok(stats) => log::info(&format!("scanned {} in {:.1?}: {}", root.path, t.elapsed(), json!(stats))),
                Err(e) => log::error(&format!("scan of {} failed: {e:#}", root.path)),
            }
        }
    }

    fn write_indexes(&mut self, force: bool) {
        let folders = self.tracker.take_dirty_folders(force.then_some(0.0));
        let mut seen = HashSet::new();
        for folder in folders {
            if !seen.insert(key(&folder)) || !self.cfg.folder_index_enabled(&folder) {
                continue;
            }
            match folder_index::write(&self.tracker.store, &folder, self.cfg.index_max_entries) {
                Ok(Some(_)) => self.indexes_written += 1,
                Ok(None) => {}
                Err(e) => log::warn(&format!("cannot write index for {folder}: {e:#}")),
            }
        }
    }

    fn reload_config(&mut self, watcher: &mut Option<Watcher>, fs_tx: &crossbeam_channel::Sender<FsEvent>) {
        let Ok(new) = Config::load(Some(&self.cfg.home)) else {
            log::warn("configuration is invalid; keeping the previous one");
            return;
        };
        log::info("configuration changed; reloading");
        let roots_changed = effective_roots(&new) != effective_roots(&self.cfg);
        self.cfg = new.clone();
        self.tracker.set_config(new);
        if roots_changed {
            drop(watcher.take());
            match self.start_watcher(fs_tx.clone()) {
                Ok(w) => *watcher = Some(w),
                Err(e) => log::error(&format!("cannot restart watchers: {e:#}")),
            }
            self.reconcile_all();
        }
    }

    fn heartbeat(&self, queue: usize) {
        let state = json!({
            "pid": std::process::id(),
            "version": VERSION,
            "implementation": "rust",
            "elevated": drive::is_elevated(),
            "started": self.started,
            "heartbeat": now(),
            "roots": self.cfg.roots.iter().map(|r| r.path.clone()).collect::<Vec<_>>(),
            "registry_keys": if self.cfg.registry.enabled { self.cfg.registry.keys.len() } else { 0 },
            "queue": queue,
            "fs_events": self.fs_events,
            "registry_changes": self.registry_changes,
            "indexes_written": self.indexes_written,
        });
        let tmp = home_file(&self.cfg, &format!("{STATE_FILE}.tmp"));
        let ok = File::create(&tmp).and_then(|mut f| f.write_all(state.to_string().as_bytes()));
        if ok.is_ok() {
            let _ = std::fs::rename(&tmp, home_file(&self.cfg, STATE_FILE));
        }
    }
}

fn mtime(path: &str) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}
