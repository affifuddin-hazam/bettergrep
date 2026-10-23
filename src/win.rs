//! Small safe wrappers over the Win32 calls bettergrep needs.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_HIDDEN, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdType, GetFileAttributesW, GetFileInformationByHandle,
    GetFinalPathNameByHandleW, GetVolumePathNameW, INVALID_FILE_ATTRIBUTES, OPEN_EXISTING, OpenFileById,
    SetFileAttributesW,
};

use crate::paths::strip_verbatim;

pub const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const UNIX_EPOCH_AS_FILETIME: i64 = 116_444_736_000_000_000;

/// NUL-terminated UTF-16 for Win32 `W` functions.
pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// A Win32 handle closed on drop.
pub struct OwnedHandle(pub HANDLE);

impl OwnedHandle {
    pub fn checked(h: HANDLE) -> Option<Self> {
        if h == INVALID_HANDLE_VALUE || h.is_null() { None } else { Some(OwnedHandle(h)) }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

// SAFETY: kernel handles are process-wide; the APIs used on them are thread-safe.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

pub fn filetime_to_unix(ft: FILETIME) -> f64 {
    let ticks = ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64;
    (ticks - UNIX_EPOCH_AS_FILETIME) as f64 / 10_000_000.0
}

pub fn filetime_now() -> i64 {
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    UNIX_EPOCH_AS_FILETIME + (unix.as_nanos() / 100) as i64
}

/// Open any file or folder (without following reparse points) just to read its metadata.
/// Never conflicts with other processes' sharing modes.
pub fn open_for_attributes(path: &str) -> Option<OwnedHandle> {
    let w = wide(path);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            FILE_READ_ATTRIBUTES,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    OwnedHandle::checked(h)
}

pub struct RawInfo {
    pub attributes: u32,
    pub size: u64,
    pub mtime: f64,
    pub volume: u32,
    pub file_id: u64,
}

pub fn handle_info(h: &OwnedHandle) -> Option<RawInfo> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(h.0, &mut info) } == 0 {
        return None;
    }
    Some(RawInfo {
        attributes: info.dwFileAttributes,
        size: ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64,
        mtime: filetime_to_unix(info.ftLastWriteTime),
        volume: info.dwVolumeSerialNumber,
        file_id: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    })
}

/// The canonical DOS path of an open handle.
pub fn final_path(h: &OwnedHandle) -> Option<String> {
    let mut buf = vec![0u16; 1024];
    loop {
        let n = unsafe { GetFinalPathNameByHandleW(h.0, buf.as_mut_ptr(), buf.len() as u32, 0) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(strip_verbatim(&String::from_utf16_lossy(&buf[..n])));
        }
        buf.resize(n + 1, 0);
    }
}

/// Mount point of the volume containing `path` (e.g. `C:\`).
pub fn volume_root(path: &str) -> Option<String> {
    let w = wide(path);
    let mut buf = vec![0u16; 512];
    if unsafe { GetVolumePathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Where does the file with NTFS ID `file_id` live now?  `hint_path` is any path on
/// the same volume.  Works without administrator rights; returns `None` if the file
/// no longer exists or is not accessible.
pub fn locate_by_id(hint_path: &str, file_id: u64) -> Option<String> {
    let root = volume_root(hint_path)?;
    let hint = open_for_attributes(&root)?;
    let desc = FILE_ID_DESCRIPTOR {
        dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: FileIdType,
        Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: file_id as i64 },
    };
    let h = unsafe {
        OpenFileById(
            hint.0,
            &desc,
            FILE_READ_ATTRIBUTES,
            SHARE_ALL,
            std::ptr::null(),
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
    };
    final_path(&OwnedHandle::checked(h)?)
}

/// One entry of a directory listing, with its NTFS file ID.
#[derive(Clone, Debug)]
pub struct DirEntryInfo {
    pub name: String,
    pub is_dir: bool,
    pub is_reparse_point: bool,
    pub size: u64,
    pub mtime: f64,
    pub file_id: u64,
}

/// List a directory in bulk (names, sizes, times and file IDs) with one handle,
/// instead of opening every entry.  This is what makes full scans fast.
pub fn list_dir(path: &str) -> std::io::Result<Vec<DirEntryInfo>> {
    use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, GetLastError};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY,
        FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, GetFileInformationByHandleEx,
    };

    let w = wide(path);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            FILE_LIST_DIRECTORY,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    let h = OwnedHandle::checked(h).ok_or_else(std::io::Error::last_os_error)?;
    let mut buf = vec![0u64; 8 * 1024]; // 64 KiB, 8-byte aligned
    let mut out = Vec::new();
    let mut class = FileIdBothDirectoryRestartInfo;
    loop {
        let ok = unsafe { GetFileInformationByHandleEx(h.0, class, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32) };
        class = FileIdBothDirectoryInfo;
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_NO_MORE_FILES {
                break;
            }
            return Err(std::io::Error::from_raw_os_error(err as i32));
        }
        let base = buf.as_ptr().cast::<u8>();
        let mut offset = 0usize;
        loop {
            // SAFETY: the kernel filled `buf` with a chain of FILE_ID_BOTH_DIR_INFO records linked by
            // NextEntryOffset; headers are read unaligned, names are 2-byte aligned.
            let (entry, name) = unsafe {
                let entry_ptr = base.add(offset).cast::<FILE_ID_BOTH_DIR_INFO>();
                let entry = std::ptr::read_unaligned(entry_ptr);
                let name_ptr = base.add(offset + std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName)).cast::<u16>();
                let name = std::slice::from_raw_parts(name_ptr, entry.FileNameLength as usize / 2);
                (entry, String::from_utf16_lossy(name))
            };
            if name != "." && name != ".." {
                let is_dir = entry.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                let ticks = entry.LastWriteTime;
                out.push(DirEntryInfo {
                    name,
                    is_dir,
                    is_reparse_point: entry.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
                    size: if is_dir { 0 } else { entry.EndOfFile as u64 },
                    mtime: (ticks - UNIX_EPOCH_AS_FILETIME) as f64 / 10_000_000.0,
                    file_id: entry.FileId as u64,
                });
            }
            if entry.NextEntryOffset == 0 {
                break;
            }
            offset += entry.NextEntryOffset as usize;
        }
    }
    Ok(out)
}

/// Set or clear the *hidden* attribute.
pub fn set_hidden(path: &str, hidden: bool) {
    let w = wide(path);
    unsafe {
        let attrs = GetFileAttributesW(w.as_ptr());
        if attrs == INVALID_FILE_ATTRIBUTES {
            return;
        }
        let new = if hidden { attrs | FILE_ATTRIBUTE_HIDDEN } else { attrs & !FILE_ATTRIBUTE_HIDDEN };
        if new != attrs {
            SetFileAttributesW(w.as_ptr(), new);
        }
    }
}

pub fn is_hidden(path: &str) -> bool {
    let w = wide(path);
    let attrs = unsafe { GetFileAttributesW(w.as_ptr()) };
    attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_HIDDEN != 0
}
