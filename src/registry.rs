//! Registry snapshot, diff and change notification.
//!
//! `RegNotifyChangeKeyValue` only says *that* something below a key changed.
//! bettergrep keeps a snapshot of each watched subtree and diffs it on notification.
//! Re-snapshots reuse a key's cached values when its last-write time is unchanged
//! *and* was well before the cached read (last-write times only move on clock
//! ticks, so a write in the same tick as the previous read would be missed).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS, FILETIME, HANDLE, WAIT_TIMEOUT};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_CONFIG, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, HKEY_USERS, KEY_NOTIFY,
    KEY_READ, KEY_WOW64_64KEY, REG_DWORD, REG_DWORD_BIG_ENDIAN, REG_EXPAND_SZ, REG_LINK, REG_MULTI_SZ,
    REG_NOTIFY_CHANGE_LAST_SET, REG_NOTIFY_CHANGE_NAME, REG_NOTIFY_THREAD_AGNOSTIC, REG_QWORD, REG_SZ, RegCloseKey,
    RegEnumKeyExW, RegEnumValueW, RegNotifyChangeKeyValue, RegOpenKeyExW, RegQueryInfoKeyW,
};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};

use crate::paths::fnmatch;
use crate::win::{OwnedHandle, filetime_now, wide};

const MAX_TEXT: usize = 512;
const MAX_BINARY: usize = 64;
const REUSE_MARGIN: i64 = 2 * 10_000_000; // 2 s in FILETIME units

fn hive(name: &str) -> Option<HKEY> {
    Some(match name {
        "HKLM" => HKEY_LOCAL_MACHINE,
        "HKCU" => HKEY_CURRENT_USER,
        "HKCR" => HKEY_CLASSES_ROOT,
        "HKU" => HKEY_USERS,
        "HKCC" => HKEY_CURRENT_CONFIG,
        _ => return None,
    })
}

/// `HKEY_LOCAL_MACHINE\Software\X` -> `("HKLM", "Software\X")`.
pub fn canonical(path: &str) -> Result<(String, String)> {
    // Key names may contain '/' (e.g. MIME types like "application/pdf"): not a separator.
    let path = path.trim().trim_matches('\\');
    let (h, sub) = path.split_once('\\').unwrap_or((path, ""));
    let h = match h.to_uppercase().as_str() {
        "HKEY_LOCAL_MACHINE" => "HKLM".to_string(),
        "HKEY_CURRENT_USER" => "HKCU".to_string(),
        "HKEY_CLASSES_ROOT" => "HKCR".to_string(),
        "HKEY_USERS" => "HKU".to_string(),
        "HKEY_CURRENT_CONFIG" => "HKCC".to_string(),
        other => other.to_string(),
    };
    if hive(&h).is_none() {
        bail!("unknown registry hive in {path:?}");
    }
    Ok((h, sub.to_string()))
}

pub fn type_name(t: u32) -> String {
    match t {
        0 => "REG_NONE",
        1 => "REG_SZ",
        2 => "REG_EXPAND_SZ",
        3 => "REG_BINARY",
        4 => "REG_DWORD",
        5 => "REG_DWORD_BIG_ENDIAN",
        6 => "REG_LINK",
        7 => "REG_MULTI_SZ",
        8 => "REG_RESOURCE_LIST",
        11 => "REG_QWORD",
        _ => return t.to_string(),
    }
    .to_string()
}

fn short_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))[..12].to_string()
}

fn bounded(text: String) -> String {
    let count = text.chars().count();
    if count <= MAX_TEXT {
        return text;
    }
    let head: String = text.chars().take(MAX_TEXT).collect();
    format!("{head} ... ({count} chars, sha256 {})", short_hash(text.as_bytes()))
}

fn utf16(data: &[u8]) -> Vec<u16> {
    data.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect()
}

/// A stable, bounded text form of a registry value (used for diffing and display).
pub fn render_value(t: u32, data: &[u8]) -> String {
    match t {
        REG_SZ | REG_EXPAND_SZ | REG_LINK => {
            let s = String::from_utf16_lossy(&utf16(data));
            bounded(s.trim_end_matches('\0').to_string())
        }
        REG_MULTI_SZ => {
            let s = String::from_utf16_lossy(&utf16(data));
            bounded(s.split('\0').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n"))
        }
        REG_DWORD if data.len() >= 4 => u32::from_le_bytes(data[..4].try_into().unwrap()).to_string(),
        REG_DWORD_BIG_ENDIAN if data.len() >= 4 => u32::from_be_bytes(data[..4].try_into().unwrap()).to_string(),
        REG_QWORD if data.len() >= 8 => u64::from_le_bytes(data[..8].try_into().unwrap()).to_string(),
        _ => {
            let head = data.iter().take(MAX_BINARY).map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
            if data.len() > MAX_BINARY {
                format!("{head} ... ({} bytes, sha256 {})", data.len(), short_hash(data))
            } else {
                head
            }
        }
    }
}

#[derive(Debug)]
pub struct KeyState {
    pub last_write: i64,
    /// (name, type, rendered value), in enumeration order.
    pub values: Vec<(String, u32, String)>,
    pub subkeys: Vec<String>,
    pub read_at: i64,
}

impl KeyState {
    fn empty() -> Arc<KeyState> {
        Arc::new(KeyState { last_write: 0, values: Vec::new(), subkeys: Vec::new(), read_at: 0 })
    }

    fn reusable(&self, last_write: i64) -> bool {
        last_write != 0 && last_write == self.last_write && self.read_at - last_write > REUSE_MARGIN
    }
}

pub type Snapshot = HashMap<String, Arc<KeyState>>;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Change {
    pub kind: &'static str,
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub value_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
}

impl Change {
    fn key(kind: &'static str, key: &str) -> Change {
        Change { kind, key: key.to_string(), value: None, value_type: None, old: None, new: None }
    }

    /// The `details` object stored with the event.
    pub fn details(&self) -> Value {
        let mut d = serde_json::Map::new();
        for (k, v) in [("value", &self.value), ("type", &self.value_type), ("old", &self.old), ("new", &self.new)] {
            if let Some(v) = v {
                d.insert(k.into(), json!(v));
            }
        }
        Value::Object(d)
    }
}

struct RegKey(HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn open_key(root: HKEY, sub: &str, access: u32) -> std::result::Result<RegKey, u32> {
    let w = wide(sub);
    let mut h: HKEY = std::ptr::null_mut();
    let rc = unsafe { RegOpenKeyExW(root, w.as_ptr(), 0, access | KEY_WOW64_64KEY, &mut h) };
    if rc == ERROR_SUCCESS { Ok(RegKey(h)) } else { Err(rc) }
}

fn read_key(h: &RegKey) -> Option<KeyState> {
    let (mut n_sub, mut max_sub, mut n_val, mut max_name, mut max_data) = (0u32, 0u32, 0u32, 0u32, 0u32);
    let mut ft = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let rc = unsafe {
        RegQueryInfoKeyW(
            h.0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
            &mut n_sub,
            &mut max_sub,
            std::ptr::null_mut(),
            &mut n_val,
            &mut max_name,
            &mut max_data,
            std::ptr::null_mut(),
            &mut ft,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let last_write = ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64;
    let mut values = Vec::with_capacity(n_val as usize);
    let mut name = vec![0u16; max_name as usize + 2];
    let mut data = vec![0u8; max_data as usize + 2];
    for i in 0..n_val {
        loop {
            let mut name_len = name.len() as u32;
            let mut data_len = data.len() as u32;
            let mut t = 0u32;
            let rc = unsafe {
                RegEnumValueW(
                    h.0,
                    i,
                    name.as_mut_ptr(),
                    &mut name_len,
                    std::ptr::null(),
                    &mut t,
                    data.as_mut_ptr(),
                    &mut data_len,
                )
            };
            if rc == ERROR_MORE_DATA {
                // A value grew since RegQueryInfoKey; retry with room to spare.
                name.resize(name.len().max(name_len as usize + 2) * 2, 0);
                data.resize(data.len().max(data_len as usize + 2) * 2, 0);
                continue;
            }
            if rc == ERROR_SUCCESS {
                let n = String::from_utf16_lossy(&name[..name_len as usize]);
                values.push((n, t, render_value(t, &data[..data_len as usize])));
            }
            break;
        }
    }
    let mut subkeys = Vec::with_capacity(n_sub as usize);
    let mut buf = vec![0u16; max_sub as usize + 2];
    for i in 0..n_sub {
        let mut len = buf.len() as u32;
        let rc = unsafe {
            RegEnumKeyExW(
                h.0,
                i,
                buf.as_mut_ptr(),
                &mut len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != ERROR_SUCCESS {
            break;
        }
        subkeys.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    Some(KeyState { last_write, values, subkeys, read_at: filetime_now() })
}

/// Case-insensitive glob patterns over full key paths (`*` also matches `\`).
#[derive(Clone, Debug, Default)]
pub struct Exclusions(Vec<String>);

impl Exclusions {
    pub fn new(patterns: &[String]) -> Self {
        Exclusions(patterns.iter().map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty()).collect())
    }

    pub fn matches(&self, key_path: &str) -> bool {
        let k = key_path.to_lowercase();
        self.0.iter().any(|p| fnmatch(p, &k))
    }
}

/// Capture every key and value below `path` (64-bit view).  Keys that cannot be
/// opened are recorded empty so they are not reported as deleted.
pub fn snapshot(path: &str, previous: Option<&Snapshot>, exclude: &Exclusions) -> Result<Snapshot> {
    let (hive_name, sub) = canonical(path)?;
    let root = hive(&hive_name).ok_or_else(|| anyhow!("bad hive"))?;
    let full = if sub.is_empty() { hive_name.clone() } else { format!("{hive_name}\\{sub}") };
    let mut result: Snapshot = HashMap::new();
    match open_key(root, &sub, KEY_READ) {
        Ok(h) => walk(&h, full, previous, exclude, &mut result),
        Err(2) => {} // ERROR_FILE_NOT_FOUND
        Err(_) => {
            result.insert(full, KeyState::empty());
        }
    }
    Ok(result)
}

/// Depth-first walk that opens each child relative to its open parent, which is
/// far cheaper than re-resolving the full path from the hive root every time.
fn walk(h: &RegKey, full: String, previous: Option<&Snapshot>, exclude: &Exclusions, out: &mut Snapshot) {
    let state = match read_key(h) {
        None => KeyState::empty(),
        Some(fresh) => match previous.and_then(|p| p.get(&full)) {
            Some(prior) if prior.reusable(fresh.last_write) => prior.clone(),
            _ => Arc::new(fresh),
        },
    };
    for name in &state.subkeys {
        let child_full = format!("{full}\\{name}");
        if exclude.matches(&child_full) {
            continue;
        }
        match open_key(h.0, name, KEY_READ) {
            Ok(child) => walk(&child, child_full, previous, exclude, out),
            Err(2) => {}
            Err(_) => {
                out.insert(child_full, KeyState::empty());
            }
        }
    }
    out.insert(full, state);
}

/// Compare two snapshots of the same subtree (case-insensitively).
pub fn diff(old: &Snapshot, new: &Snapshot) -> Vec<Change> {
    let lower =
        |s: &Snapshot| -> BTreeMap<String, String> { s.keys().map(|k| (k.to_lowercase(), k.clone())).collect() };
    let (old_keys, new_keys) = (lower(old), lower(new));
    let mut changes = Vec::new();
    for (lk, k) in &new_keys {
        if old_keys.contains_key(lk) {
            continue;
        }
        changes.push(Change::key("reg_key_created", k));
        let mut values = new[k].values.clone();
        values.sort();
        for (name, t, text) in values {
            changes.push(Change {
                value: Some(display_name(&name)),
                value_type: Some(type_name(t)),
                new: Some(text),
                ..Change::key("reg_value_created", k)
            });
        }
    }
    for (lk, k) in &old_keys {
        if !new_keys.contains_key(lk) {
            changes.push(Change::key("reg_key_deleted", k));
        }
    }
    for (lk, k_old) in &old_keys {
        let Some(k_new) = new_keys.get(lk) else { continue };
        let (before, after) = (&old[k_old], &new[k_new]);
        if Arc::ptr_eq(before, after) {
            continue;
        }
        let by_name = |s: &KeyState| -> BTreeMap<String, (String, u32, String)> {
            s.values.iter().map(|(n, t, v)| (n.to_lowercase(), (n.clone(), *t, v.clone()))).collect()
        };
        let (bv, av) = (by_name(before), by_name(after));
        for (ln, (name, t, text)) in &av {
            match bv.get(ln) {
                None => changes.push(Change {
                    value: Some(display_name(name)),
                    value_type: Some(type_name(*t)),
                    new: Some(text.clone()),
                    ..Change::key("reg_value_created", k_new)
                }),
                Some((_, ot, otext)) if (ot, otext) != (t, text) => changes.push(Change {
                    value: Some(display_name(name)),
                    value_type: Some(type_name(*t)),
                    old: Some(otext.clone()),
                    new: Some(text.clone()),
                    ..Change::key("reg_value_changed", k_new)
                }),
                _ => {}
            }
        }
        for (ln, (name, t, text)) in &bv {
            if !av.contains_key(ln) {
                changes.push(Change {
                    value: Some(display_name(name)),
                    value_type: Some(type_name(*t)),
                    old: Some(text.clone()),
                    ..Change::key("reg_value_deleted", k_new)
                });
            }
        }
    }
    changes
}

fn display_name(name: &str) -> String {
    if name.is_empty() { "(Default)".to_string() } else { name.to_string() }
}

/// Read a single key (no children); `None` if it does not exist.
fn read_one(full: &str, previous: Option<&Snapshot>) -> Option<Arc<KeyState>> {
    let (hive_name, sub) = canonical(full).ok()?;
    match open_key(hive(&hive_name)?, &sub, KEY_READ) {
        Ok(h) => Some(match read_key(&h) {
            None => KeyState::empty(),
            Some(fresh) => match previous.and_then(|p| p.get(full)) {
                Some(prior) if prior.reusable(fresh.last_write) => prior.clone(),
                _ => Arc::new(fresh),
            },
        }),
        Err(2) => None,
        Err(_) => Some(KeyState::empty()),
    }
}

/// Snapshot several large subtrees using `threads` workers.
///
/// Registry reads scale across threads, so the top two levels are read first and
/// the subtrees below them are spread over a work queue.  With 8 threads this
/// roughly halves a full `HKLM\SOFTWARE` baseline compared with one thread.
pub fn snapshot_parallel(
    paths: &[String],
    previous: Option<&HashMap<String, Snapshot>>,
    exclude: &Exclusions,
    threads: usize,
) -> HashMap<String, Snapshot> {
    let mut out = HashMap::new();
    for path in paths {
        let prev = previous.and_then(|m| m.get(path));
        let full = match canonical(path) {
            Ok((h, s)) if s.is_empty() => h,
            Ok((h, s)) => format!("{h}\\{s}"),
            Err(e) => {
                eprintln!("bettergrep: cannot snapshot {path}: {e:#}");
                out.insert(path.clone(), Snapshot::new());
                continue;
            }
        };
        let mut result = Snapshot::new();
        let mut frontier = vec![full];
        for _ in 0..2 {
            let mut next = Vec::new();
            for key_path in frontier {
                let Some(state) = read_one(&key_path, prev) else { continue };
                for name in &state.subkeys {
                    let child = format!("{key_path}\\{name}");
                    if !exclude.matches(&child) {
                        next.push(child);
                    }
                }
                result.insert(key_path, state);
            }
            frontier = next;
        }
        let queue = std::sync::Mutex::new(frontier);
        let merged = std::sync::Mutex::new(result);
        std::thread::scope(|scope| {
            for _ in 0..threads.max(1) {
                scope.spawn(|| {
                    loop {
                        let Some(item) = queue.lock().unwrap().pop() else { break };
                        if let Ok(part) = snapshot(&item, prev, exclude) {
                            merged.lock().unwrap().extend(part);
                        }
                    }
                });
            }
        });
        out.insert(path.clone(), merged.into_inner().unwrap());
    }
    out
}

pub fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get().min(8)).unwrap_or(4)
}

/// Reports registry changes below a set of keys from a background thread.
pub struct RegistryWatcher {
    stop: Arc<OwnedHandle>,
    thread: Option<JoinHandle<()>>,
}

pub type ChangeSink = Box<dyn Fn(Vec<Change>) + Send>;

struct Watch {
    path: String,
    key: Option<RegKey>,
    event: Option<OwnedHandle>,
    snap: Snapshot,
    dirty_since: Option<Instant>,
}

impl RegistryWatcher {
    pub fn start(keys: Vec<String>, debounce: Duration, sink: ChangeSink) -> Result<RegistryWatcher> {
        let stop = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        let stop = Arc::new(OwnedHandle::checked(stop).ok_or_else(|| anyhow!("CreateEvent failed"))?);
        let stop_handle = stop.clone();
        let thread = std::thread::Builder::new()
            .name("bettergrep-registry".into())
            .spawn(move || registry_loop(keys, debounce, sink, stop_handle.0))?;
        Ok(RegistryWatcher { stop, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        unsafe { SetEvent(self.stop.0) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for RegistryWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

const NOTIFY_FILTER: u32 = REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_THREAD_AGNOSTIC;

fn arm(w: &Watch) -> bool {
    match (&w.key, &w.event) {
        (Some(k), Some(e)) => unsafe { RegNotifyChangeKeyValue(k.0, 1, NOTIFY_FILTER, e.0, 1) == ERROR_SUCCESS },
        _ => false,
    }
}

fn open_watch(w: &mut Watch, none: &Exclusions) -> bool {
    let Ok((hive_name, sub)) = canonical(&w.path) else { return false };
    let Ok(key) = open_key(hive(&hive_name).unwrap(), &sub, KEY_READ | KEY_NOTIFY) else { return false };
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
    w.key = Some(key);
    w.event = OwnedHandle::checked(event);
    w.snap = snapshot(&w.path, None, none).unwrap_or_default();
    arm(w)
}

fn registry_loop(keys: Vec<String>, debounce: Duration, sink: ChangeSink, stop: HANDLE) {
    let none = Exclusions::default();
    let mut active: Vec<Watch> = Vec::new();
    let mut missing: Vec<Watch> = Vec::new();
    for path in keys.into_iter().take(60) {
        let mut w = Watch { path, key: None, event: None, snap: HashMap::new(), dirty_since: None };
        if open_watch(&mut w, &none) { active.push(w) } else { missing.push(w) }
    }
    let mut last_retry = Instant::now();
    loop {
        let mut handles: Vec<HANDLE> = active.iter().filter_map(|w| w.event.as_ref().map(|e| e.0)).collect();
        handles.push(stop);
        let rc = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, 250) };
        if rc as usize == handles.len() - 1 {
            return; // stop requested
        }
        if rc != WAIT_TIMEOUT && (rc as usize) < active.len() {
            let w = &mut active[rc as usize];
            w.dirty_since.get_or_insert_with(Instant::now);
            arm(w);
        }
        for w in &mut active {
            if w.dirty_since.is_some_and(|t| t.elapsed() >= debounce) {
                w.dirty_since = None;
                match snapshot(&w.path, Some(&w.snap), &none) {
                    Ok(new) => {
                        let changes = diff(&w.snap, &new);
                        w.snap = new;
                        if !changes.is_empty() {
                            sink(changes);
                        }
                    }
                    Err(err) => eprintln!("bettergrep: registry snapshot failed for {}: {err:#}", w.path),
                }
            }
        }
        // Keys like RunOnce come and go: keep trying to open the missing ones.
        if !missing.is_empty() && last_retry.elapsed() > Duration::from_secs(30) {
            last_retry = Instant::now();
            let mut still = Vec::new();
            for mut w in missing.drain(..) {
                if open_watch(&mut w, &none) {
                    let created: Vec<Change> = w.snap.keys().map(|k| Change::key("reg_key_created", k)).collect();
                    if !created.is_empty() {
                        sink(created);
                    }
                    active.push(w);
                } else {
                    still.push(w);
                }
            }
            missing = still;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(values: &[(&str, u32, &str)], subkeys: &[&str]) -> Arc<KeyState> {
        Arc::new(KeyState {
            last_write: 1,
            values: values.iter().map(|(n, t, v)| (n.to_string(), *t, v.to_string())).collect(),
            subkeys: subkeys.iter().map(|s| s.to_string()).collect(),
            read_at: 0,
        })
    }

    #[test]
    fn diff_reports_every_kind_of_change() {
        let old: Snapshot = [
            (r"HKCU\Software\App".to_string(), state(&[("Version", 1, "1.0"), ("Gone", 4, "1")], &["Old"])),
            (r"HKCU\Software\App\Old".to_string(), state(&[], &[])),
        ]
        .into();
        let new: Snapshot = [
            (r"HKCU\Software\App".to_string(), state(&[("Version", 1, "2.0"), ("New", 1, "x")], &["Fresh"])),
            (r"HKCU\Software\App\Fresh".to_string(), state(&[("", 1, "default")], &[])),
        ]
        .into();
        let mut got: Vec<(&str, String, Option<String>)> = diff(&old, &new)
            .into_iter()
            .map(|c| (c.kind, c.key.rsplit('\\').next().unwrap().to_string(), c.value))
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("reg_key_created", "Fresh".into(), None),
                ("reg_key_deleted", "Old".into(), None),
                ("reg_value_changed", "App".into(), Some("Version".into())),
                ("reg_value_created", "App".into(), Some("New".into())),
                ("reg_value_created", "Fresh".into(), Some("(Default)".into())),
                ("reg_value_deleted", "App".into(), Some("Gone".into())),
            ]
        );
    }

    #[test]
    fn diff_is_case_insensitive() {
        let old: Snapshot = [(r"HKLM\SOFTWARE\X".to_string(), state(&[("Path", 1, "a")], &[]))].into();
        let new: Snapshot = [(r"HKLM\Software\x".to_string(), state(&[("PATH", 1, "a")], &[]))].into();
        assert!(diff(&old, &new).is_empty());
    }

    #[test]
    fn values_render_bounded() {
        let text = render_value(3, &[0u8; 1000]);
        assert!(text.contains("1000 bytes") && text.len() < 300);
        let multi: Vec<u8> = "a\0b\0\0".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(render_value(REG_MULTI_SZ, &multi), "a\nb");
        assert_eq!(render_value(REG_DWORD, &7u32.to_le_bytes()), "7");
        assert_eq!(canonical(r"HKEY_LOCAL_MACHINE\Software\X").unwrap(), ("HKLM".into(), r"Software\X".into()));
        assert!(canonical(r"HKXX\Foo").is_err());
    }
}
