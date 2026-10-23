//! User configuration (`config.json` in the data directory).
//!
//! The JSON layout is versioned and stable, so data directories survive
//! upgrades.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::{IgnoreRules, data_dir, is_within, key, norm, parent};

pub const CONFIG_FILENAME: &str = "config.json";
pub const DB_FILENAME: &str = "bettergrep.db";

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

pub fn default_ignore() -> Vec<String> {
    strings(&[
        "*.crdownload",
        "*.part",
        "*.partial",
        "*.download",
        "*.opdownload",
        "*.tmp",
        "~$*",
        ".~lock.*",
        "*.swp",
        "desktop.ini",
        "thumbs.db",
        ".ds_store",
        ".git",
        "node_modules",
        "__pycache__",
        ".venv",
    ])
}

pub fn default_registry_keys() -> Vec<String> {
    strings(&[
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce",
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall",
        r"HKCU\Environment",
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Run",
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\RunOnce",
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Run",
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\RunOnce",
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
        r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon",
        r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options",
        r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Schedule\TaskCache\Tree",
        r"HKLM\SYSTEM\CurrentControlSet\Services",
        r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
    ])
}

pub fn default_trace_registry_keys() -> Vec<String> {
    strings(&[r"HKCU\Software", r"HKLM\SOFTWARE", r"HKLM\SYSTEM\CurrentControlSet\Services"])
}

pub fn default_trace_registry_exclude() -> Vec<String> {
    strings(&[
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Installer\UserData\*\Components",
        r"HKLM\SOFTWARE\Microsoft\Office\ClickToRun",
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\SideBySide",
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing",
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModel\StateChange",
        r"HKLM\SOFTWARE\Microsoft\.NETFramework\*\NGenService",
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\CloudStore",
        r"HKCU\Software\Microsoft\Windows\Shell\BagMRU",
        r"HKCU\Software\Microsoft\Windows\Shell\Bags",
        r"HKCU\Software\Classes\Local Settings\Software\Microsoft\Windows\Shell",
    ])
}

pub fn default_trace_ignore() -> Vec<String> {
    strings(&[
        "*/windows/prefetch/*",
        "*/windows/logs/*",
        "*/windows/softwaredistribution/*",
        "*/windows/system32/winevt/*",
        "*/windows/system32/sru/*",
        "*/windows/system32/config/*",
        "*/windows/servicing/*",
        "*/system volume information/*",
        "*/programdata/microsoft/windows defender/*",
        "*/programdata/microsoft/search/*",
        "*/appdata/local/microsoft/windows/inetcache/*",
        "*/appdata/local/microsoft/windows/explorer/*",
        "*/appdata/local/microsoft/edge/user data/*",
        "*/appdata/local/google/chrome/user data/*",
        "*/appdata/local/bravesoftware/*",
        "*/appdata/roaming/mozilla/firefox/profiles/*",
        "*/appdata/local/packages/*/ac/*",
        "*/appdata/local/microsoft/windows/notifications/*",
        "*/ntuser.dat*",
        "*/usrclass.dat*",
        "*/pagefile.sys",
        "*/swapfile.sys",
        "*/hiberfil.sys",
        "*.etl",
    ])
}

pub fn default_categories() -> BTreeMap<String, Vec<String>> {
    let table: [(&str, &[&str]); 9] = [
        (
            "Images",
            &[
                "jpg", "jpeg", "png", "gif", "bmp", "webp", "svg", "heic", "heif", "tif", "tiff", "ico", "avif", "raw",
                "psd",
            ],
        ),
        (
            "Documents",
            &[
                "pdf", "doc", "docx", "odt", "rtf", "txt", "md", "xls", "xlsx", "ods", "csv", "ppt", "pptx", "odp",
                "epub", "pages", "key", "numbers",
            ],
        ),
        ("Archives", &["zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "iso", "img", "cab"]),
        (
            "Installers",
            &["exe", "msi", "msix", "msixbundle", "appx", "appxbundle", "dmg", "pkg", "deb", "rpm", "apk", "xapk"],
        ),
        ("Audio", &["mp3", "wav", "flac", "aac", "ogg", "m4a", "wma", "opus", "mid", "midi"]),
        ("Video", &["mp4", "mkv", "avi", "mov", "wmv", "webm", "flv", "m4v", "mpg", "mpeg", "3gp"]),
        (
            "Code",
            &[
                "py", "js", "ts", "json", "xml", "html", "css", "java", "c", "cpp", "h", "cs", "go", "rs", "sh", "ps1",
                "bat", "cmd", "yml", "yaml", "toml", "sql", "ipynb",
            ],
        ),
        ("Fonts", &["ttf", "otf", "woff", "woff2"]),
        ("Torrents", &["torrent"]),
    ];
    table.iter().map(|(cat, exts)| (cat.to_string(), strings(exts))).collect()
}

fn default_trace_roots() -> Vec<String> {
    let drive = std::env::var("SYSTEMDRIVE").unwrap_or_else(|_| "C:".into());
    vec![format!("{drive}\\")]
}

fn yes() -> bool {
    true
}

/// A folder tree bettergrep keeps a lineage index for.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Root {
    pub path: String,
    #[serde(default = "yes")]
    pub folder_index: bool,
    #[serde(default = "yes")]
    pub recursive: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RegistryConfig {
    pub enabled: bool,
    pub keys: Vec<String>,
    pub debounce_seconds: f64,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        RegistryConfig { enabled: true, keys: default_registry_keys(), debounce_seconds: 1.0 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TraceConfig {
    pub roots: Vec<String>,
    pub ignore: Vec<String>,
    pub registry_keys: Vec<String>,
    pub registry_exclude: Vec<String>,
    /// Seconds to keep recording after the traced program exits.
    pub settle_seconds: f64,
    /// Minimum time spent learning which folders are busy before launch.
    pub baseline_seconds: f64,
}

impl Default for TraceConfig {
    fn default() -> Self {
        TraceConfig {
            roots: default_trace_roots(),
            ignore: default_trace_ignore(),
            registry_keys: default_trace_registry_keys(),
            registry_exclude: default_trace_registry_exclude(),
            settle_seconds: 5.0,
            baseline_seconds: 5.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub roots: Vec<Root>,
    pub ignore: Vec<String>,
    /// Files larger than this are tracked by name/size/file ID only, never hashed.
    pub hash_max_bytes: u64,
    /// Quiet period after the last write before a new or changed file is recorded.
    pub settle_seconds: f64,
    /// Upper bound on settling for files written continuously.
    pub max_settle_seconds: f64,
    /// How long a deletion is held back so a matching arrival can claim it as a move.
    pub move_window_seconds: f64,
    /// Delay before a changed folder's index is rewritten.
    pub index_debounce_seconds: f64,
    /// Cap on each list in a folder index.
    pub index_max_entries: usize,
    /// Full reconcile of every root this often (0 = never).
    pub rescan_hours: f64,
    pub registry: RegistryConfig,
    pub trace: TraceConfig,
    pub categories: BTreeMap<String, Vec<String>>,
    /// Drives indexed in full-drive mode (e.g. "C:\\"); needs administrator rights.
    pub drives: Vec<String>,
    /// Folders whose changes are left out of the activity feed (still recorded and searchable).
    pub activity_mute: Vec<String>,
    #[serde(skip)]
    pub home: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            roots: Vec::new(),
            ignore: default_ignore(),
            hash_max_bytes: 256 * 1024 * 1024,
            settle_seconds: 1.5,
            max_settle_seconds: 30.0,
            move_window_seconds: 10.0,
            index_debounce_seconds: 3.0,
            index_max_entries: 500,
            rescan_hours: 6.0,
            registry: RegistryConfig::default(),
            trace: TraceConfig::default(),
            categories: default_categories(),
            drives: Vec::new(),
            activity_mute: Vec::new(),
            home: data_dir(),
        }
    }
}

impl Config {
    pub fn with_home(home: &str) -> Self {
        Config { home: norm(home), ..Config::default() }
    }

    pub fn config_path(&self) -> String {
        Path::new(&self.home).join(CONFIG_FILENAME).to_string_lossy().into_owned()
    }

    pub fn db_path(&self) -> String {
        Path::new(&self.home).join(DB_FILENAME).to_string_lossy().into_owned()
    }

    pub fn ignore_rules(&self) -> IgnoreRules {
        IgnoreRules::new(&self.ignore, std::slice::from_ref(&self.home))
    }

    pub fn trace_ignore_rules(&self) -> IgnoreRules {
        let all: Vec<String> = self.ignore.iter().chain(&self.trace.ignore).cloned().collect();
        IgnoreRules::new(&all, std::slice::from_ref(&self.home))
    }

    /// The most specific configured root containing `path`.
    pub fn root_for(&self, path: &str) -> Option<&Root> {
        let k = key(path);
        self.roots
            .iter()
            .filter(|root| {
                let rk = key(&root.path);
                is_within(&k, &rk) && (root.recursive || k == rk || parent(&k) == rk)
            })
            .max_by_key(|root| root.path.len())
    }

    pub fn folder_index_enabled(&self, folder: &str) -> bool {
        self.root_for(folder).is_some_and(|r| r.folder_index)
    }

    /// Whether changes at `path` are hidden from the activity feed.
    pub fn activity_muted(&self, path: &str) -> bool {
        let k = key(path);
        self.activity_mute.iter().any(|m| is_within(&k, &key(m)))
    }

    /// Mute (or unmute) a folder in the activity feed.  Returns false when
    /// nothing changed.
    pub fn set_activity_muted(&mut self, folder: &str, muted: bool) -> bool {
        let folder = norm(folder);
        let before = self.activity_mute.len();
        self.activity_mute.retain(|m| key(m) != key(&folder));
        if muted {
            self.activity_mute.push(folder);
            self.activity_mute.sort_by_key(|m| key(m));
        }
        muted || self.activity_mute.len() != before
    }

    pub fn add_root(&mut self, path: &str, folder_index: bool, recursive: bool) {
        let path = norm(path);
        if let Some(root) = self.roots.iter_mut().find(|r| key(&r.path) == key(&path)) {
            root.folder_index = folder_index;
            root.recursive = recursive;
        } else {
            self.roots.push(Root { path, folder_index, recursive });
        }
    }

    pub fn remove_root(&mut self, path: &str) -> bool {
        let before = self.roots.len();
        self.roots.retain(|r| key(&r.path) != key(path));
        before != self.roots.len()
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.home)?;
        let tmp = format!("{}.tmp", self.config_path());
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")?;
        std::fs::rename(&tmp, self.config_path())?;
        Ok(())
    }

    pub fn load(home: Option<&str>) -> Result<Config> {
        let home = home.map(norm).unwrap_or_else(data_dir);
        let path = Path::new(&home).join(CONFIG_FILENAME);
        let mut cfg: Config = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("invalid config {}", path.display()))?,
            Err(_) => Config::default(),
        };
        cfg.home = home;
        for root in &mut cfg.roots {
            root.path = norm(&root.path);
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_string_lossy().into_owned();
        let mut cfg = Config::with_home(&home);
        cfg.add_root(&format!("{home}\\Downloads"), false, true);
        cfg.rescan_hours = 1.5;
        cfg.save().unwrap();
        let loaded = Config::load(Some(&home)).unwrap();
        assert_eq!(loaded, cfg);

        std::fs::write(Path::new(&home).join(CONFIG_FILENAME), r#"{"roots": [{"path": "C:\\x"}]}"#).unwrap();
        let partial = Config::load(Some(&home)).unwrap();
        assert!(partial.roots[0].folder_index && partial.registry.enabled);
        assert_eq!(partial.settle_seconds, 1.5);
    }

    #[test]
    fn most_specific_root() {
        let mut cfg = Config::with_home(r"C:\wm-home");
        cfg.add_root(r"C:\data", true, true);
        cfg.add_root(r"C:\data\inbox", true, false);
        assert_eq!(cfg.root_for(r"C:\data\inbox\a.txt").unwrap().path, r"C:\data\inbox");
        assert_eq!(cfg.root_for(r"C:\data\inbox\sub\a.txt").unwrap().path, r"C:\data");
        assert!(cfg.root_for(r"C:\other\a.txt").is_none());
    }
}
