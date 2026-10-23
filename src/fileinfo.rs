//! Filesystem probing and content hashing.

use std::io::Read;

use sha2::{Digest, Sha256};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;

use crate::paths::norm;
use crate::win;

/// A point-in-time view of a filesystem entry.
///
/// `volume`/`inode` are the volume serial number and the NTFS file ID, which
/// survive renames and moves on the same volume (the values a POSIX-style
/// `stat` reports as `st_dev`/`st_ino`).
#[derive(Clone, Debug, PartialEq)]
pub struct FileInfo {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: f64,
    pub volume: u64,
    pub inode: u64,
}

impl FileInfo {
    pub fn identity(&self) -> Option<(u64, u64)> {
        (self.inode != 0).then_some((self.volume, self.inode))
    }

    pub fn with_path(&self, path: &str) -> FileInfo {
        FileInfo { path: norm(path), ..self.clone() }
    }
}

/// Stat `path` without following reparse points; `None` if it does not exist.
pub fn probe(path: &str) -> Option<FileInfo> {
    let path = norm(path);
    let handle = win::open_for_attributes(&path)?;
    let raw = win::handle_info(&handle)?;
    let is_dir = raw.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    Some(FileInfo {
        path,
        is_dir,
        size: if is_dir { 0 } else { raw.size },
        mtime: raw.mtime,
        volume: raw.volume as u64,
        inode: raw.file_id,
    })
}

/// SHA-256 of `path`, or `None` if it is larger than `max_bytes` or unreadable.
pub fn sha256_file(path: &str, max_bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > max_bytes {
        return None;
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hex::encode(hasher.finalize()))
}
