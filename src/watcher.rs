//! Directory watching with `ReadDirectoryChangesExW`.
//!
//! The extended notification format reports, for every change, whether the
//! entry is a folder (even for deletions).  When Windows drops notifications
//! because its buffer overflowed, the watcher reports [`FsEvent::Overflow`] so the
//! daemon can rescan that tree.  Volumes that do not support the extended call
//! (FAT, some network shares) fall back to `ReadDirectoryChangesW`.

use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{Result, anyhow};
use crossbeam_channel::Sender;
use windows_sys::Win32::Foundation::{ERROR_OPERATION_ABORTED, GetLastError, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME,
    FILE_ACTION_RENAMED_OLD_NAME, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_EXTENDED_INFORMATION, FILE_NOTIFY_INFORMATION,
    OPEN_EXISTING, ReadDirectoryChangesExW, ReadDirectoryChangesW, ReadDirectoryNotifyExtendedInformation,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects};

use crate::paths::norm;
use crate::win::{OwnedHandle, SHARE_ALL, wide};

#[derive(Clone, Debug, PartialEq)]
pub enum FsEvent {
    Created {
        path: String,
        is_dir: bool,
    },
    Modified {
        path: String,
        is_dir: bool,
    },
    Deleted {
        path: String,
        is_dir: bool,
    },
    Moved {
        src: String,
        dest: String,
        is_dir: bool,
    },
    /// Notifications were lost; `root` must be rescanned.
    Overflow {
        root: String,
    },
}

const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_CREATION;

/// Watches a set of folders until [`Watcher::stop`] (or drop).
pub struct Watcher {
    stop: Arc<OwnedHandle>,
    threads: Vec<JoinHandle<()>>,
}

impl Watcher {
    /// Start one watcher thread per root.  Events are sent to `tx`.
    pub fn start(roots: &[(String, bool)], tx: Sender<FsEvent>) -> Result<Watcher> {
        let stop = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        let stop = Arc::new(OwnedHandle::checked(stop).ok_or_else(|| anyhow!("CreateEvent failed"))?);
        let mut threads = Vec::new();
        for (root, recursive) in roots {
            let dir = open_dir(root)?;
            let (root, recursive, tx, stop) = (norm(root), *recursive, tx.clone(), stop.clone());
            threads.push(
                std::thread::Builder::new()
                    .name(format!("bettergrep-watch {root}"))
                    .spawn(move || watch_loop(dir, &root, recursive, &tx, stop.0))?,
            );
        }
        Ok(Watcher { stop, threads })
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        unsafe { SetEvent(self.stop.0) };
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn open_dir(path: &str) -> Result<OwnedHandle> {
    let w = wide(path);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            FILE_LIST_DIRECTORY,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    OwnedHandle::checked(h).ok_or_else(|| anyhow!("cannot watch {path}: {}", std::io::Error::last_os_error()))
}

fn watch_loop(dir: OwnedHandle, root: &str, recursive: bool, tx: &Sender<FsEvent>, stop: HANDLE) {
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    let Some(event) = OwnedHandle::checked(event) else { return };
    let mut buf = vec![0u64; 8 * 1024]; // 64 KiB: the maximum for network shares
    let mut extended = true;
    let mut pending_old: Option<(String, bool)> = None;
    loop {
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = event.0;
        unsafe { ResetEvent(event.0) };
        let len = (buf.len() * 8) as u32;
        let ok = unsafe {
            if extended {
                ReadDirectoryChangesExW(
                    dir.0,
                    buf.as_mut_ptr().cast(),
                    len,
                    recursive as i32,
                    FILTER,
                    std::ptr::null_mut(),
                    &mut ov,
                    None,
                    ReadDirectoryNotifyExtendedInformation,
                )
            } else {
                ReadDirectoryChangesW(
                    dir.0,
                    buf.as_mut_ptr().cast(),
                    len,
                    recursive as i32,
                    FILTER,
                    std::ptr::null_mut(),
                    &mut ov,
                    None,
                )
            }
        };
        if ok == 0 {
            if extended {
                extended = false; // not supported on this volume: use the classic call
                continue;
            }
            eprintln!("bettergrep: stopped watching {root}: {}", std::io::Error::last_os_error());
            return;
        }
        let handles = [event.0, stop];
        let which = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
        let mut bytes = 0u32;
        if which != WAIT_OBJECT_0 {
            unsafe {
                CancelIoEx(dir.0, &ov);
                GetOverlappedResult(dir.0, &ov, &mut bytes, 1);
            }
            return;
        }
        if unsafe { GetOverlappedResult(dir.0, &ov, &mut bytes, 0) } == 0 {
            if unsafe { GetLastError() } == ERROR_OPERATION_ABORTED {
                return;
            }
            let _ = tx.send(FsEvent::Overflow { root: root.to_string() });
            continue;
        }
        if bytes == 0 {
            // The kernel buffer overflowed: individual changes were lost.
            let _ = tx.send(FsEvent::Overflow { root: root.to_string() });
            continue;
        }
        for (action, name, attrs) in parse(&buf, bytes as usize, extended) {
            let path = format!("{}\\{}", root.trim_end_matches('\\'), name);
            let is_dir = match attrs {
                Some(a) => a & FILE_ATTRIBUTE_DIRECTORY != 0,
                None => Path::new(&path).is_dir(),
            };
            if let Some((old, old_dir)) = pending_old.take()
                && action != FILE_ACTION_RENAMED_NEW_NAME
            {
                let _ = tx.send(FsEvent::Deleted { path: old, is_dir: old_dir }); // renamed out of the tree
            }
            let ev = match action {
                FILE_ACTION_ADDED => FsEvent::Created { path, is_dir },
                FILE_ACTION_REMOVED => FsEvent::Deleted { path, is_dir },
                FILE_ACTION_MODIFIED => FsEvent::Modified { path, is_dir },
                FILE_ACTION_RENAMED_OLD_NAME => {
                    pending_old = Some((path, is_dir));
                    continue;
                }
                FILE_ACTION_RENAMED_NEW_NAME => match pending_old.take() {
                    Some((src, _)) => FsEvent::Moved { src, dest: path, is_dir },
                    None => FsEvent::Created { path, is_dir },
                },
                _ => continue,
            };
            if tx.send(ev).is_err() {
                return;
            }
        }
    }
}

/// Decode a notification buffer into `(action, relative name, attributes)`.
fn parse(buf: &[u64], bytes: usize, extended: bool) -> Vec<(u32, String, Option<u32>)> {
    let base = buf.as_ptr().cast::<u8>();
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset < bytes {
        // SAFETY: the kernel wrote a chain of records linked by NextEntryOffset.  Records are only
        // 4-byte aligned while the struct contains 8-byte fields, so headers are copied out with
        // read_unaligned; file names (u16) are always 2-byte aligned.
        let (next, action, name, attrs) = unsafe {
            if extended {
                let rec = std::ptr::read_unaligned(base.add(offset).cast::<FILE_NOTIFY_EXTENDED_INFORMATION>());
                let name_ptr =
                    base.add(offset + std::mem::offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, FileName)).cast::<u16>();
                let name = std::slice::from_raw_parts(name_ptr, rec.FileNameLength as usize / 2);
                (rec.NextEntryOffset, rec.Action, String::from_utf16_lossy(name), Some(rec.FileAttributes))
            } else {
                let rec = std::ptr::read_unaligned(base.add(offset).cast::<FILE_NOTIFY_INFORMATION>());
                let name_ptr = base.add(offset + std::mem::offset_of!(FILE_NOTIFY_INFORMATION, FileName)).cast::<u16>();
                let name = std::slice::from_raw_parts(name_ptr, rec.FileNameLength as usize / 2);
                (rec.NextEntryOffset, rec.Action, String::from_utf16_lossy(name), None)
            }
        };
        out.push((action, name, attrs));
        if next == 0 {
            break;
        }
        offset += next as usize;
    }
    out
}
