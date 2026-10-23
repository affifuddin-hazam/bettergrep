//! Shared test harness: a tracker on a temp folder, driven by a fake clock.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bettergrep::config::Config;
use bettergrep::store::Store;
use bettergrep::tracker::{Clock, Tracker};

pub struct Env {
    pub dir: tempfile::TempDir,
    pub area: PathBuf,
    pub cfg: Config,
    pub tracker: Tracker,
    time: Arc<Mutex<f64>>,
}

impl Env {
    pub fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let area = dir.path().join("area");
        std::fs::create_dir_all(&area).unwrap();
        let cfg = test_config(dir.path());
        let time = Arc::new(Mutex::new(1_700_000_000.0));
        let t = time.clone();
        let clock: Clock = Arc::new(move || *t.lock().unwrap());
        let tracker = Tracker::new(Store::open(&cfg.db_path()).unwrap(), cfg.clone(), "watcher", clock);
        Env { dir, area, cfg, tracker, time }
    }

    pub fn p(&self, rel: &str) -> String {
        bettergrep::paths::norm(self.area.join(rel))
    }

    pub fn write(&self, rel: &str, content: &str) -> String {
        let path = self.area.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    pub fn advance(&self, seconds: f64) {
        *self.time.lock().unwrap() += seconds;
    }

    pub fn settle(&mut self, seconds: f64) {
        self.advance(seconds);
        self.tracker.tick().unwrap();
    }

    pub fn store(&self) -> &Store {
        &self.tracker.store
    }

    /// Create a file and let it settle; returns its record id.
    pub fn created(&mut self, rel: &str, content: &str) -> i64 {
        let path = self.write(rel, content);
        self.tracker.created(&path, false).unwrap();
        self.settle(2.0);
        self.store().get_present(&path).unwrap().expect("recorded").id
    }

    pub fn kinds(&self, file_id: Option<i64>) -> Vec<String> {
        let (sql, params): (&str, Vec<rusqlite::types::Value>) = match file_id {
            Some(id) => ("SELECT kind FROM events WHERE file_id = ? ORDER BY id", vec![id.into()]),
            None => ("SELECT kind FROM events ORDER BY id", vec![]),
        };
        let mut stmt = self.store().conn.prepare(sql).unwrap();
        stmt.query_map(rusqlite::params_from_iter(params), |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    }
}

pub fn test_config(dir: &Path) -> Config {
    let mut cfg = Config::with_home(&dir.join("home").to_string_lossy());
    cfg.settle_seconds = 1.0;
    cfg.max_settle_seconds = 10.0;
    cfg.move_window_seconds = 5.0;
    cfg.index_debounce_seconds = 0.0;
    cfg.registry.enabled = false;
    cfg
}
