//! Full-drive mode: every file on an NTFS volume, found instantly.
//!
//! The initial index is read from the volume's master file table in one sweep
//! (`FSCTL_ENUM_USN_DATA`), which lists every file and folder with its parent
//! without opening a single directory.  The index is then kept current from the
//! volume's USN change journal (`FSCTL_READ_USN_JOURNAL`), starting from the
//! last position seen, so changes made while bettergrep was closed are caught up
//! on the next start.  Both need administrator rights (a raw volume handle).
//!
//! Each volume's index is a snapshot file that is mapped into memory and
//! searched in place, plus a small in-memory overlay of the changes since it
//! was written.  See [`DriveIndex`].

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use memmap2::Mmap;
use windows_sys::Win32::Foundation::{GENERIC_READ, GetLastError};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, GetVolumeInformationW, OPEN_EXISTING};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::win::{OwnedHandle, SHARE_ALL, wide};

const FSCTL_ENUM_USN_DATA: u32 = 0x000900b3;
const FSCTL_READ_USN_JOURNAL: u32 = 0x000900bb;
const FSCTL_QUERY_USN_JOURNAL: u32 = 0x000900f4;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_HANDLE_EOF: u32 = 38;
const ERROR_JOURNAL_DELETE_IN_PROGRESS: u32 = 0x49a;
const ERROR_JOURNAL_NOT_ACTIVE: u32 = 0x49b;
const ERROR_JOURNAL_ENTRY_DELETED: u32 = 0x49d;

const REASON_FILE_CREATE: u32 = 0x0000_0100;
const REASON_FILE_DELETE: u32 = 0x0000_0200;
const REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_HIDDEN: u32 = 0x2;
const ATTR_SYSTEM: u32 = 0x4;
/// MFT record number of a volume's root directory.
const ROOT_RECORD: u64 = 5;
const RECORD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const SNAPSHOT_MAGIC_V1: &[u8; 8] = b"WMDRIVE1";
const SNAPSHOT_MAGIC: &[u8; 8] = b"WMDRIVE2";
const DELTA_MAGIC: &[u8; 8] = b"WMDELTA1";
const HEADER_LEN: usize = 64;
const RECENT_CAPACITY: usize = 5000;
/// Repeated changes to one path within this many seconds share a feed entry...
const FOLD_WINDOW: f64 = 60.0;
/// ...if that entry is among the last few.
const FOLD_LOOKBACK: usize = 32;
/// Fold the in-memory changes into a new snapshot once there are this many
/// (roughly 10 MB of memory)...
const OVERLAY_LIMIT: usize = 100_000;
/// ...or once the snapshot is this old.
const SAVE_EVERY: Duration = Duration::from_secs(6 * 3600);
/// How often to write the in-memory changes to the side file, which keeps the
/// saved index current for readers without administrator rights (CLI, MCP).
const CHECKPOINT_EVERY: Duration = Duration::from_secs(300);

#[cfg(target_endian = "big")]
compile_error!("drive snapshots are read in place and assume a little-endian CPU");

#[derive(Debug)]
pub enum DriveError {
    /// Reading a volume directly requires running as administrator.
    AdminRequired,
    /// Full-drive mode needs NTFS (found e.g. FAT32/exFAT).
    NotNtfs(String),
    /// The journal was reset or wrapped past our checkpoint: a full rescan is needed.
    JournalReset,
    Io(std::io::Error),
}

impl fmt::Display for DriveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriveError::AdminRequired => {
                write!(f, "full-drive mode needs administrator rights (run bettergrep as administrator)")
            }
            DriveError::NotNtfs(fs) => write!(f, "full-drive mode needs an NTFS drive (this one is {fs})"),
            DriveError::JournalReset => write!(f, "the drive's change journal was reset; a full rescan is needed"),
            DriveError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DriveError {}

impl From<std::io::Error> for DriveError {
    fn from(e: std::io::Error) -> Self {
        DriveError::Io(e)
    }
}

fn last_error() -> u32 {
    unsafe { GetLastError() }
}

/// `"c"`, `"C:"`, `"C:\"` -> `"C:\"`.
pub fn normalize_root(drive: &str) -> Option<String> {
    let letter = drive.trim().chars().next()?.to_ascii_uppercase();
    letter.is_ascii_alphabetic().then(|| format!("{letter}:\\"))
}

/// One record from the master file table or the change journal.
#[derive(Clone, Debug)]
pub struct UsnRecord {
    pub frn: u64,
    pub parent: u64,
    pub usn: i64,
    /// Unix seconds (0 for master-file-table enumeration records).
    pub ts: f64,
    pub reason: u32,
    pub attrs: u32,
    pub name: String,
}

/// Parse a buffer of `USN_RECORD_V2` records (every field read unaligned).
fn parse_records(buf: &[u8], mut f: impl FnMut(UsnRecord)) {
    let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u64_at = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let mut off = 0usize;
    while off + 60 <= buf.len() {
        let rec = &buf[off..];
        let len = u32_at(rec, 0) as usize;
        if len < 60 || len > rec.len() {
            break;
        }
        if u16_at(rec, 4) == 2 {
            let name_len = u16_at(rec, 56) as usize;
            let name_off = u16_at(rec, 58) as usize;
            if name_off + name_len <= len {
                let units: Vec<u16> = rec[name_off..name_off + name_len]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect();
                let ticks = u64_at(rec, 32) as i64;
                f(UsnRecord {
                    frn: u64_at(rec, 8),
                    parent: u64_at(rec, 16),
                    usn: u64_at(rec, 24) as i64,
                    ts: if ticks > 0 { (ticks - 116_444_736_000_000_000) as f64 / 1e7 } else { 0.0 },
                    reason: u32_at(rec, 40),
                    attrs: u32_at(rec, 52),
                    name: String::from_utf16_lossy(&units),
                });
            }
        }
        off += len;
    }
}

#[derive(Clone, Copy, Debug)]
pub struct JournalInfo {
    pub id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
}

/// An open NTFS volume (read-only).
pub struct Volume {
    pub root: String,
    pub serial: u32,
    pub filesystem: String,
    handle: OwnedHandle,
}

impl Volume {
    pub fn open(drive: &str) -> Result<Volume, DriveError> {
        let root = normalize_root(drive)
            .ok_or_else(|| DriveError::Io(std::io::Error::other(format!("not a drive: {drive}"))))?;
        let (serial, filesystem) = volume_info(&root)?;
        if !filesystem.eq_ignore_ascii_case("NTFS") {
            return Err(DriveError::NotNtfs(filesystem));
        }
        let device = wide(&format!("\\\\.\\{}", &root[..2]));
        let h = unsafe {
            CreateFileW(
                device.as_ptr(),
                GENERIC_READ,
                SHARE_ALL,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        match OwnedHandle::checked(h) {
            Some(handle) => Ok(Volume { root, serial, filesystem, handle }),
            None if last_error() == ERROR_ACCESS_DENIED => Err(DriveError::AdminRequired),
            None => Err(std::io::Error::last_os_error().into()),
        }
    }

    fn ioctl(&self, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, u32> {
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.handle.0,
                code,
                input.as_ptr().cast(),
                input.len() as u32,
                output.as_mut_ptr().cast(),
                output.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 { Err(last_error()) } else { Ok(returned as usize) }
    }

    pub fn journal(&self) -> Result<JournalInfo, DriveError> {
        let mut out = [0u8; 80];
        match self.ioctl(FSCTL_QUERY_USN_JOURNAL, &[], &mut out) {
            Ok(_) => Ok(JournalInfo {
                id: u64::from_le_bytes(out[0..8].try_into().unwrap()),
                first_usn: i64::from_le_bytes(out[8..16].try_into().unwrap()),
                next_usn: i64::from_le_bytes(out[16..24].try_into().unwrap()),
            }),
            Err(ERROR_JOURNAL_NOT_ACTIVE | ERROR_JOURNAL_DELETE_IN_PROGRESS) => Err(DriveError::JournalReset),
            Err(code) => Err(std::io::Error::from_raw_os_error(code as i32).into()),
        }
    }

    /// Every file and folder on the volume, straight from the master file table.
    pub fn enumerate(&self, mut f: impl FnMut(UsnRecord)) -> Result<(), DriveError> {
        let mut buf = vec![0u8; 1 << 20];
        let mut start: u64 = 0;
        loop {
            // MFT_ENUM_DATA_V0: StartFileReferenceNumber, LowUsn, HighUsn.
            let mut input = [0u8; 24];
            input[0..8].copy_from_slice(&start.to_le_bytes());
            input[16..24].copy_from_slice(&i64::MAX.to_le_bytes());
            match self.ioctl(FSCTL_ENUM_USN_DATA, &input, &mut buf) {
                Ok(n) if n > 8 => {
                    start = u64::from_le_bytes(buf[0..8].try_into().unwrap());
                    parse_records(&buf[8..n], &mut f);
                }
                Ok(_) | Err(ERROR_HANDLE_EOF) => return Ok(()),
                Err(code) => return Err(std::io::Error::from_raw_os_error(code as i32).into()),
            }
        }
    }

    /// Journal records from `start_usn` up to now (only final "file closed"
    /// records, which carry every reason accumulated while the file was open).
    /// Returns the position to continue from next time.
    pub fn read_changes(
        &self,
        journal_id: u64,
        start_usn: i64,
        mut f: impl FnMut(UsnRecord),
    ) -> Result<i64, DriveError> {
        let mut buf = vec![0u8; 1 << 16];
        let mut usn = start_usn;
        loop {
            // READ_USN_JOURNAL_DATA_V0: StartUsn, ReasonMask, ReturnOnlyOnClose, Timeout, BytesToWaitFor, UsnJournalID.
            let mut input = [0u8; 40];
            input[0..8].copy_from_slice(&usn.to_le_bytes());
            input[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
            input[12..16].copy_from_slice(&1u32.to_le_bytes());
            input[32..40].copy_from_slice(&journal_id.to_le_bytes());
            match self.ioctl(FSCTL_READ_USN_JOURNAL, &input, &mut buf) {
                Ok(n) if n >= 8 => {
                    let next = i64::from_le_bytes(buf[0..8].try_into().unwrap());
                    parse_records(&buf[8..n], &mut f);
                    if next == usn || n <= 8 {
                        return Ok(next);
                    }
                    usn = next;
                }
                Ok(_) => return Ok(usn),
                Err(ERROR_JOURNAL_ENTRY_DELETED | ERROR_JOURNAL_NOT_ACTIVE | ERROR_JOURNAL_DELETE_IN_PROGRESS) => {
                    return Err(DriveError::JournalReset);
                }
                Err(code) => return Err(std::io::Error::from_raw_os_error(code as i32).into()),
            }
        }
    }
}

/// Volume serial number and filesystem name of a drive root.
pub fn volume_info(root: &str) -> std::io::Result<(u32, String)> {
    let w = wide(root);
    let mut serial = 0u32;
    let mut fs = [0u16; 64];
    let ok = unsafe {
        GetVolumeInformationW(
            w.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let len = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
    Ok((serial, String::from_utf16_lossy(&fs[..len])))
}

/// Whether this process runs with administrator rights.
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let token = OwnedHandle(token);
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    ok != 0 && elevation.TokenIsElevated != 0
}

const FLAG_DIR: u8 = 1;
/// Hidden or system.
const FLAG_HIDDEN: u8 = 2;
/// Record numbers above this are ignored (they would need a huge lookup table).
const MAX_RECORD: usize = 0x7fff_ffff;

fn flags_from_attrs(attrs: u32) -> u8 {
    let mut f = 0;
    if attrs & ATTR_DIRECTORY != 0 {
        f |= FLAG_DIR;
    }
    if attrs & (ATTR_HIDDEN | ATTR_SYSTEM) != 0 {
        f |= FLAG_HIDDEN;
    }
    f
}

/// One file or folder, laid out exactly as in a snapshot file (24 bytes).
/// Every bit pattern is valid, so mapped snapshot bytes can be read as entries.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Entry {
    frn: u64,
    parent: u64,
    name_off: u32,
    name_len: u16,
    flags: u8,
    _pad: u8,
}

impl Entry {
    fn new(frn: u64, parent: u64, name_len: usize, attrs: u32) -> Entry {
        Entry {
            frn,
            parent,
            name_off: 0,
            name_len: name_len.min(u16::MAX as usize) as u16,
            flags: flags_from_attrs(attrs),
            _pad: 0,
        }
    }

    fn is_dir(&self) -> bool {
        self.flags & FLAG_DIR != 0
    }

    fn hidden(&self) -> bool {
        self.flags & FLAG_HIDDEN != 0
    }
}

fn record_of(frn: u64) -> usize {
    (frn & RECORD_MASK) as usize
}

/// What a journal record did, for the activity feed.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Created,
    Deleted,
    Renamed,
    Moved,
    Modified,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DriveChange {
    /// Stable while repeats are folded in, so a feed can update the entry in place.
    pub id: u64,
    pub ts: f64,
    pub kind: ChangeKind,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub is_dir: bool,
    /// How many changes this entry stands for (see [`DriveIndex::push_recent`]).
    pub count: u32,
    /// When the first of those changes happened, if more than one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<f64>,
}

impl DriveChange {
    fn new(ts: f64, kind: ChangeKind, path: String, from: Option<String>, is_dir: bool) -> DriveChange {
        DriveChange { id: 0, ts, kind, path, from, is_dir, count: 1, since: None }
    }

    /// Creations, deletions and edits can fold together; renames and moves
    /// carry a `from` and always stand alone.
    fn foldable(&self) -> bool {
        matches!(self.kind, ChangeKind::Created | ChangeKind::Deleted | ChangeKind::Modified)
    }
}

// -- snapshot files ---------------------------------------------------------------
//
// Layout (little-endian), designed to be searched in place once mapped:
//
//   0   magic "WMDRIVE2"          32  entry count
//   8   volume serial (u32)       40  record table length
//   16  journal id                48  name bytes
//   24  next USN                  56  stamp (ties the side file of changes to this snapshot)
//   64  names (UTF-8, back to back), padded to 8 bytes
//       entries (24 bytes each, pointing into the names)
//       record table (u32 per MFT record number: entry index + 1, 0 = none)

/// Where each part of a snapshot file lives.
#[derive(Clone, Copy, Debug)]
struct Layout {
    names: usize,
    entries_at: usize,
    count: usize,
    records_at: usize,
    records: usize,
    end: usize,
}

impl Layout {
    fn new(names: usize, count: usize, records: usize) -> Option<Layout> {
        let entries_at = HEADER_LEN.checked_add(names)?.checked_next_multiple_of(8)?;
        let records_at = entries_at.checked_add(count.checked_mul(std::mem::size_of::<Entry>())?)?;
        let end = records_at.checked_add(records.checked_mul(4)?)?;
        Some(Layout { names, entries_at, count, records_at, records, end })
    }
}

/// The bulk of an index: a snapshot file mapped into memory, or (after a sweep,
/// until it is saved) the same arrays held in memory.
enum Base {
    Owned { entries: Vec<Entry>, by_record: Vec<u32>, names: Vec<u8> },
    Mapped { map: Mmap, path: String, layout: Layout },
}

struct Mapped {
    base: Base,
    serial: u32,
    journal_id: u64,
    next_usn: i64,
    stamp: u64,
}

impl Base {
    fn empty() -> Base {
        Base::Owned { entries: Vec::new(), by_record: Vec::new(), names: Vec::new() }
    }

    fn entries(&self) -> &[Entry] {
        match self {
            Base::Owned { entries, .. } => entries,
            // SAFETY: `map` checked that this range lies inside the mapping and starts
            // 8-byte aligned (mappings start on a page boundary), and `Entry` is plain data.
            Base::Mapped { map, layout, .. } => unsafe {
                std::slice::from_raw_parts(map.as_ptr().add(layout.entries_at).cast::<Entry>(), layout.count)
            },
        }
    }

    fn by_record(&self) -> &[u32] {
        match self {
            Base::Owned { by_record, .. } => by_record,
            // SAFETY: as for `entries`; the table follows the entries, so it is 8-byte aligned too.
            Base::Mapped { map, layout, .. } => unsafe {
                std::slice::from_raw_parts(map.as_ptr().add(layout.records_at).cast::<u32>(), layout.records)
            },
        }
    }

    fn names(&self) -> &[u8] {
        match self {
            Base::Owned { names, .. } => names,
            Base::Mapped { map, layout, .. } => &map[HEADER_LEN..HEADER_LEN + layout.names],
        }
    }

    fn map(path: &str) -> std::io::Result<Mapped> {
        let file = File::open(path)?;
        // SAFETY: snapshots are replaced by renaming a new file over them, never
        // rewritten in place.
        let map = unsafe { Mmap::map(&file)? };
        let bad = || std::io::Error::other("damaged bettergrep drive snapshot");
        if map.len() < HEADER_LEN || &map[..8] != SNAPSHOT_MAGIC {
            return Err(bad());
        }
        let u64_at = |o: usize| u64::from_le_bytes(map[o..o + 8].try_into().unwrap());
        let size = |o: usize| usize::try_from(u64_at(o)).map_err(|_| bad());
        let layout = Layout::new(size(48)?, size(32)?, size(40)?).ok_or_else(bad)?;
        if layout.end > map.len() || !(map.as_ptr() as usize).is_multiple_of(8) {
            return Err(bad());
        }
        Ok(Mapped {
            serial: u32::from_le_bytes(map[8..12].try_into().unwrap()),
            journal_id: u64_at(16),
            next_usn: u64_at(24) as i64,
            stamp: u64_at(56),
            base: Base::Mapped { map, path: path.to_string(), layout },
        })
    }
}

/// Collects a sweep (or an old-format snapshot) into in-memory arrays.
#[derive(Default)]
struct Builder {
    entries: Vec<Entry>,
    by_record: Vec<u32>,
    names: Vec<u8>,
}

impl Builder {
    fn add(&mut self, frn: u64, parent: u64, name: &str, attrs: u32) {
        let record = record_of(frn);
        if record > MAX_RECORD {
            return;
        }
        if record >= self.by_record.len() {
            self.by_record.resize(record + 1 + record / 8, 0);
        }
        let mut entry = Entry::new(frn, parent, name.len(), attrs);
        entry.name_off = self.names.len() as u32;
        self.names.extend_from_slice(&name.as_bytes()[..entry.name_len as usize]);
        match self.by_record[record] {
            0 => {
                self.entries.push(entry);
                self.by_record[record] = self.entries.len() as u32;
            }
            i => self.entries[i as usize - 1] = entry,
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn finish(mut self, root: &str, serial: u32) -> DriveIndex {
        let used = self.by_record.iter().rposition(|&i| i != 0).map_or(0, |p| p + 1);
        self.by_record.truncate(used);
        self.by_record.shrink_to_fit();
        self.entries.shrink_to_fit();
        self.names.shrink_to_fit();
        let base = Base::Owned { entries: self.entries, by_record: self.by_record, names: self.names };
        DriveIndex::from_base(root, serial, base)
    }
}

/// A file created, renamed or moved since the snapshot was written.
struct Added {
    entry: Entry,
    name: Box<str>,
}

/// Index of one volume: a mapped snapshot plus the changes made since.
///
/// The snapshot is searched where it lies, so its pages belong to Windows' file
/// cache rather than to bettergrep: they are read on demand, dropped when memory
/// is short, and never pushed to the page file.  Journal changes go to a small
/// overlay (`added`, plus a bit per snapshot entry that has gone), saved every
/// few minutes to a side file and folded into a new snapshot now and then.
pub struct DriveIndex {
    pub root: String,
    pub serial: u32,
    pub journal_id: u64,
    pub next_usn: i64,
    base: Base,
    stamp: u64,
    /// One bit per snapshot entry deleted, renamed or moved since it was written.
    dead: Vec<u64>,
    dead_count: usize,
    /// By MFT record number.
    added: HashMap<usize, Added>,
    live: usize,
    pub recent: VecDeque<DriveChange>,
    next_change_id: u64,
    /// Ids of entries in `recent` added or updated since the last `take_updates`.
    updated: Vec<u64>,
    dirty: bool,
    saved_at: Instant,
    merged_at: Instant,
}

impl DriveIndex {
    fn from_base(root: &str, serial: u32, base: Base) -> DriveIndex {
        DriveIndex {
            root: root.to_string(),
            serial,
            journal_id: 0,
            next_usn: 0,
            live: base.entries().len(),
            base,
            stamp: 0,
            dead: Vec::new(),
            dead_count: 0,
            added: HashMap::new(),
            recent: VecDeque::new(),
            next_change_id: 1,
            updated: Vec::new(),
            dirty: false,
            saved_at: Instant::now(),
            merged_at: Instant::now(),
        }
    }

    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Memory this index holds privately, in bytes (the mapped snapshot is not
    /// counted: see [`DriveIndex::mapped_bytes`]).
    pub fn memory_bytes(&self) -> usize {
        let owned = match &self.base {
            Base::Owned { entries, by_record, names } => {
                entries.capacity() * std::mem::size_of::<Entry>() + by_record.capacity() * 4 + names.capacity()
            }
            Base::Mapped { .. } => 0,
        };
        owned
            + self.dead.capacity() * 8
            + self.added.capacity() * (std::mem::size_of::<(usize, Added)>() + 1)
            + self.added.values().map(|a| a.name.len()).sum::<usize>()
    }

    /// Size of the mapped snapshot file, in bytes.
    pub fn mapped_bytes(&self) -> usize {
        match &self.base {
            Base::Mapped { layout, .. } => layout.end,
            Base::Owned { .. } => 0,
        }
    }

    /// Whether [`DriveIndex::save`] has work to do now: an unsaved sweep, many
    /// changes held in memory, an old snapshot, or changes not written to the
    /// side file for a few minutes.
    pub fn should_save(&self) -> bool {
        self.dirty
            && (matches!(self.base, Base::Owned { .. })
                || self.added.len() + self.dead_count >= OVERLAY_LIMIT
                || self.merged_at.elapsed() >= SAVE_EVERY
                || self.saved_at.elapsed() >= CHECKPOINT_EVERY)
    }

    fn is_dead(&self, i: usize) -> bool {
        self.dead.get(i / 64).is_some_and(|w| (w >> (i % 64)) & 1 != 0)
    }

    fn kill(&mut self, i: usize) {
        if self.dead.is_empty() {
            self.dead = vec![0; self.base.entries().len().div_ceil(64)];
        }
        self.dead[i / 64] |= 1 << (i % 64);
        self.dead_count += 1;
        self.live -= 1;
    }

    fn base_name(&self, e: &Entry) -> &[u8] {
        let off = e.name_off as usize;
        self.base.names().get(off..off + e.name_len as usize).unwrap_or_default()
    }

    /// The live snapshot entry for an MFT record number.
    fn base_slot(&self, record: usize) -> Option<usize> {
        let i = *self.base.by_record().get(record)? as usize;
        (i != 0 && i <= self.base.entries().len() && !self.is_dead(i - 1)).then_some(i - 1)
    }

    fn find(&self, frn: u64) -> Option<(Entry, &[u8])> {
        let record = record_of(frn);
        if let Some(a) = self.added.get(&record) {
            return (a.entry.frn == frn).then_some((a.entry, a.name.as_bytes()));
        }
        let e = self.base.entries()[self.base_slot(record)?];
        (e.frn == frn).then(|| (e, self.base_name(&e)))
    }

    /// Every live entry with its name.
    fn live_entries(&self) -> impl Iterator<Item = (Entry, &[u8])> {
        let base = self.base.entries().iter().enumerate().filter(|(i, _)| !self.is_dead(*i));
        let base = base.map(|(_, e)| (*e, self.base_name(e)));
        base.chain(self.added.values().map(|a| (a.entry, a.name.as_bytes())))
    }

    /// MFT record number of a folder given by path (`C:\Users\Me`, or the root).
    fn folder_record(&self, dir: &str) -> Option<usize> {
        let dir = dir.trim_end_matches('\\');
        if dir.eq_ignore_ascii_case(self.root.trim_end_matches('\\')) {
            return Some(ROOT_RECORD as usize);
        }
        let last = dir.rsplit('\\').next()?.as_bytes();
        self.live_entries()
            .filter(|(e, name)| e.is_dir() && name.eq_ignore_ascii_case(last))
            .find(|(e, name)| self.path_of(*e, name).is_some_and(|p| p.eq_ignore_ascii_case(dir)))
            .map(|(e, _)| record_of(e.frn))
    }

    /// Call `f` with the path of every file under `dir` (the root or a folder
    /// on this drive) whose name passes `keep`, without opening any folder.
    /// Hidden and system files and folders are left out unless
    /// `include_hidden`.  Stops early when `f` returns false.  Returns false
    /// when `dir` is not in the index.
    pub fn for_each_file_under(
        &self,
        dir: &str,
        include_hidden: bool,
        keep: impl Fn(&[u8]) -> bool,
        mut f: impl FnMut(String) -> bool,
    ) -> bool {
        let Some(target) = self.folder_record(dir) else { return false };
        // Whether each folder seen so far is inside `dir` (and visible).
        let mut inside: HashMap<usize, bool> = HashMap::new();
        for (e, name) in self.live_entries() {
            if e.is_dir() || (!include_hidden && e.hidden()) || !keep(name) {
                continue;
            }
            if self.folder_inside(e.parent, target, include_hidden, &mut inside)
                && let Some(path) = self.path_of(e, name)
                && !f(path)
            {
                break;
            }
        }
        true
    }

    fn folder_inside(&self, folder: u64, target: usize, include_hidden: bool, memo: &mut HashMap<usize, bool>) -> bool {
        let mut chain = Vec::new();
        let mut cur = folder;
        let inside = loop {
            let record = record_of(cur);
            if record == target {
                break true;
            }
            if record == ROOT_RECORD as usize || chain.len() > 512 {
                break false;
            }
            if let Some(&known) = memo.get(&record) {
                break known;
            }
            chain.push(record);
            match self.find(cur) {
                Some((e, _)) if include_hidden || !e.hidden() => cur = e.parent,
                _ => break false,
            }
        };
        for record in chain {
            memo.insert(record, inside);
        }
        inside
    }

    /// Full sweep of the master file table.
    pub fn scan(volume: &Volume) -> Result<DriveIndex, DriveError> {
        Self::scan_with_progress(volume, |_| {})
    }

    /// Full sweep, calling `progress` with the running entry count every 50,000
    /// entries.  The result is held in memory until it is saved.
    pub fn scan_with_progress(volume: &Volume, mut progress: impl FnMut(usize)) -> Result<DriveIndex, DriveError> {
        let journal = volume.journal()?;
        let mut builder = Builder::default();
        builder.entries.reserve(1 << 20);
        volume.enumerate(|r| {
            builder.add(r.frn, r.parent, &r.name, r.attrs);
            if builder.len().is_multiple_of(50_000) {
                progress(builder.len());
            }
        })?;
        let mut index = builder.finish(&volume.root, volume.serial);
        progress(index.live);
        index.journal_id = journal.id;
        index.next_usn = journal.next_usn;
        index.dirty = true;
        Ok(index)
    }

    fn upsert(&mut self, frn: u64, parent: u64, name: &str, attrs: u32) {
        self.put(Entry::new(frn, parent, name.len(), attrs), name);
    }

    fn put(&mut self, entry: Entry, name: &str) {
        let record = record_of(entry.frn);
        if record > MAX_RECORD {
            return;
        }
        // A rename or move of this file, or its record reused for a new one.
        if let Some(i) = self.base_slot(record) {
            self.kill(i);
        }
        if self.added.insert(record, Added { entry, name: name.into() }).is_none() {
            self.live += 1;
        }
    }

    fn remove(&mut self, frn: u64) {
        let record = record_of(frn);
        if self.added.get(&record).is_some_and(|a| a.entry.frn == frn) {
            self.added.remove(&record);
            self.live -= 1;
        } else if let Some(i) = self.base_slot(record).filter(|&i| self.base.entries()[i].frn == frn) {
            self.kill(i);
        }
    }

    /// Full path of an entry by walking its parents; `None` for orphans.
    fn path_of(&self, entry: Entry, name: &[u8]) -> Option<String> {
        let mut parts = vec![name];
        let mut e = entry;
        for _ in 0..512 {
            if e.parent & RECORD_MASK == ROOT_RECORD {
                let mut path = self.root.clone();
                for (i, p) in parts.iter().rev().enumerate() {
                    if i > 0 {
                        path.push('\\');
                    }
                    path.push_str(&String::from_utf8_lossy(p));
                }
                return Some(path);
            }
            let (parent, parent_name) = self.find(e.parent)?;
            parts.push(parent_name);
            e = parent;
        }
        None
    }

    fn path_of_frn(&self, frn: u64) -> Option<String> {
        if frn & RECORD_MASK == ROOT_RECORD {
            return Some(self.root.clone());
        }
        let (e, name) = self.find(frn)?;
        self.path_of(e, name)
    }

    /// Apply journal records since the checkpoint.  Returns how many were applied.
    pub fn catch_up(&mut self, volume: &Volume) -> Result<usize, DriveError> {
        let journal = volume.journal()?;
        if journal.id != self.journal_id || self.next_usn < journal.first_usn {
            return Err(DriveError::JournalReset);
        }
        let mut records = Vec::new();
        let next = volume.read_changes(self.journal_id, self.next_usn, |r| records.push(r))?;
        let n = records.len();
        for r in records {
            self.apply(r);
        }
        if next != self.next_usn {
            self.next_usn = next;
            self.dirty = true;
        }
        Ok(n)
    }

    fn apply(&mut self, r: UsnRecord) {
        let is_dir = r.attrs & ATTR_DIRECTORY != 0;
        let before = self.find(r.frn).map(|(e, name)| (e, self.path_of(e, name)));
        let kind = if r.reason & REASON_FILE_DELETE != 0 {
            let path = before.as_ref().and_then(|(_, p)| p.clone()).or_else(|| self.join_parent(r.parent, &r.name));
            self.remove(r.frn);
            self.push_recent(DriveChange::new(r.ts, ChangeKind::Deleted, path.unwrap_or(r.name), None, is_dir));
            self.dirty = true;
            return;
        } else if r.reason & REASON_FILE_CREATE != 0 || before.is_none() {
            ChangeKind::Created
        } else if r.reason & REASON_RENAME_NEW_NAME != 0 {
            let (old, _) = before.as_ref().unwrap();
            if old.parent == r.parent { ChangeKind::Renamed } else { ChangeKind::Moved }
        } else {
            ChangeKind::Modified
        };
        if kind != ChangeKind::Modified {
            self.upsert(r.frn, r.parent, &r.name, r.attrs);
        }
        let path = self.path_of_frn(r.frn).unwrap_or_else(|| r.name.clone());
        let from = match kind {
            ChangeKind::Renamed | ChangeKind::Moved => before.and_then(|(_, p)| p),
            _ => None,
        };
        self.push_recent(DriveChange::new(r.ts, kind, path, from, is_dir));
        self.dirty = true;
    }

    fn join_parent(&self, parent: u64, name: &str) -> Option<String> {
        let dir = self.path_of_frn(parent)?;
        Some(if dir.ends_with('\\') { format!("{dir}{name}") } else { format!("{dir}\\{name}") })
    }

    /// Add a change to the activity feed.  A program that rewrites the same
    /// file over and over (some save settings several times a second) would
    /// otherwise flood the feed, so a creation, deletion or edit of a path
    /// changed within the last minute is folded into that entry: its count
    /// goes up and it moves to the end.
    fn push_recent(&mut self, mut change: DriveChange) {
        let fold = change.foldable().then(|| {
            self.recent.iter().rev().take(FOLD_LOOKBACK).position(|c| {
                c.foldable() && c.path == change.path && c.is_dir == change.is_dir && change.ts - c.ts <= FOLD_WINDOW
            })
        });
        if let Some(Some(back)) = fold {
            let i = self.recent.len() - 1 - back;
            let mut c = self.recent.remove(i).unwrap();
            c.since = c.since.or(Some(c.ts));
            c.count += 1;
            c.ts = change.ts;
            c.kind = change.kind;
            change = c;
        } else {
            change.id = self.next_change_id;
            self.next_change_id += 1;
            if self.recent.len() >= RECENT_CAPACITY {
                self.recent.pop_front();
            }
        }
        if self.updated.len() >= RECENT_CAPACITY {
            self.updated.clear(); // nobody is collecting updates (CLI, MCP)
        }
        self.updated.push(change.id);
        self.recent.push_back(change);
    }

    /// Feed entries added or updated since the last call, oldest first.
    pub fn take_updates(&mut self) -> Vec<DriveChange> {
        let ids: std::collections::HashSet<u64> = std::mem::take(&mut self.updated).into_iter().collect();
        let mut out: Vec<DriveChange> = self.recent.iter().filter(|c| ids.contains(&c.id)).cloned().collect();
        out.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        out
    }

    // -- snapshots -----------------------------------------------------------

    pub fn snapshot_path(home: &str, serial: u32) -> String {
        Path::new(home).join("drives").join(format!("{serial:08x}.wmd")).to_string_lossy().into_owned()
    }

    /// The side file holding the changes made since `path` was written.
    fn delta_path(path: &str) -> String {
        format!("{path}.delta")
    }

    /// Delete a drive's saved index (snapshot and side file).
    pub fn remove_snapshot(path: &str) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(Self::delta_path(path));
    }

    /// Save to `path`: fold the changes into a new snapshot when there are many
    /// or they are old (see [`DriveIndex::should_save`]), otherwise just write
    /// them to the small side file next to the snapshot.
    pub fn save(&mut self, path: &str) -> std::io::Result<()> {
        let same_file = matches!(&self.base, Base::Mapped { path: p, .. } if p == path);
        if same_file && self.added.len() + self.dead_count < OVERLAY_LIMIT && self.merged_at.elapsed() < SAVE_EVERY {
            self.write_delta(path)
        } else {
            self.merge(path)
        }
    }

    /// Write a new snapshot with every change folded in, and continue from it.
    pub fn merge(&mut self, path: &str) -> std::io::Result<()> {
        if let Some(dir) = Path::new(path).parent() {
            std::fs::create_dir_all(dir)?;
        }
        let same_file = matches!(&self.base, Base::Mapped { path: p, .. } if p == path);
        let tmp = format!("{path}.tmp");
        if let Err(e) = self.write_snapshot(&tmp) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        if same_file {
            self.base = Base::empty(); // a mapped file cannot be replaced
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            if same_file {
                self.base = Base::map(path)?.base;
            }
            return Err(e);
        }
        let _ = std::fs::remove_file(Self::delta_path(path));
        self.added = HashMap::new();
        self.dead = Vec::new();
        self.dead_count = 0;
        match Base::map(path) {
            Ok(m) => {
                self.base = m.base;
                self.stamp = m.stamp;
            }
            Err(e) => {
                self.base = Base::empty();
                self.live = 0;
                return Err(e);
            }
        }
        self.live = self.base.entries().len();
        self.dirty = false;
        self.saved_at = Instant::now();
        self.merged_at = self.saved_at;
        Ok(())
    }

    /// Stream the live entries into a new snapshot file: names first, then the
    /// entries pointing at them, then the record table.
    fn write_snapshot(&self, path: &str) -> std::io::Result<()> {
        let entries = self.base.entries();
        let all = || {
            let base =
                entries.iter().enumerate().filter(|(i, _)| !self.is_dead(*i)).map(|(_, e)| (*e, self.base_name(e)));
            let added = self.added.values().map(|a| (a.entry, a.name.as_bytes()));
            base.chain(added).map(|(e, name)| (e, &name[..name.len().min(u16::MAX as usize)]))
        };
        let mut file = File::create(path)?;
        let mut w = BufWriter::with_capacity(1 << 20, &file);
        w.write_all(&[0u8; HEADER_LEN])?;
        let (mut names, mut count, mut records) = (0usize, 0usize, 0usize);
        for (e, name) in all() {
            w.write_all(name)?;
            names += name.len();
            count += 1;
            records = records.max(record_of(e.frn) + 1);
        }
        let layout =
            Layout::new(names, count, records).ok_or_else(|| std::io::Error::other("drive index too large to save"))?;
        w.write_all(&vec![0u8; layout.entries_at - HEADER_LEN - names])?;
        let mut by_record = vec![0u32; records];
        let mut off = 0usize;
        for (i, (e, name)) in all().enumerate() {
            w.write_all(&e.frn.to_le_bytes())?;
            w.write_all(&e.parent.to_le_bytes())?;
            w.write_all(&(off as u32).to_le_bytes())?;
            w.write_all(&(name.len() as u16).to_le_bytes())?;
            w.write_all(&[e.flags, 0])?;
            off += name.len();
            by_record[record_of(e.frn)] = i as u32 + 1;
        }
        for r in &by_record {
            w.write_all(&r.to_le_bytes())?;
        }
        w.flush()?;
        drop(w);
        // Ties a side file to the snapshot it was written against.
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let mut header = [0u8; HEADER_LEN];
        header[..8].copy_from_slice(SNAPSHOT_MAGIC);
        header[8..12].copy_from_slice(&self.serial.to_le_bytes());
        header[16..24].copy_from_slice(&self.journal_id.to_le_bytes());
        header[24..32].copy_from_slice(&self.next_usn.to_le_bytes());
        header[32..40].copy_from_slice(&(count as u64).to_le_bytes());
        header[40..48].copy_from_slice(&(records as u64).to_le_bytes());
        header[48..56].copy_from_slice(&(names as u64).to_le_bytes());
        header[56..64].copy_from_slice(&(stamp as u64).to_le_bytes());
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;
        file.sync_all()
    }

    /// Write the changes held in memory, and the journal position, to the side
    /// file.  Layout: magic "WMDELTA1", snapshot stamp, journal id, next USN,
    /// gone count, added count, then the gone snapshot entry numbers (u32 each)
    /// and the added entries (frn, parent, flags, name length, name).
    fn write_delta(&mut self, path: &str) -> std::io::Result<()> {
        let tmp = format!("{path}.delta.tmp");
        {
            let mut w = BufWriter::new(File::create(&tmp)?);
            w.write_all(DELTA_MAGIC)?;
            for v in
                [self.stamp, self.journal_id, self.next_usn as u64, self.dead_count as u64, self.added.len() as u64]
            {
                w.write_all(&v.to_le_bytes())?;
            }
            for (k, &word) in self.dead.iter().enumerate() {
                let mut bits = word;
                while bits != 0 {
                    w.write_all(&((k * 64) as u32 + bits.trailing_zeros()).to_le_bytes())?;
                    bits &= bits - 1;
                }
            }
            for a in self.added.values() {
                let name = &a.name.as_bytes()[..a.name.len().min(u16::MAX as usize)];
                w.write_all(&a.entry.frn.to_le_bytes())?;
                w.write_all(&a.entry.parent.to_le_bytes())?;
                w.write_all(&[a.entry.flags])?;
                w.write_all(&(name.len() as u16).to_le_bytes())?;
                w.write_all(name)?;
            }
            w.flush()?;
            w.get_ref().sync_all()?;
        }
        if let Err(e) = std::fs::rename(&tmp, Self::delta_path(path)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        self.dirty = false;
        self.saved_at = Instant::now();
        Ok(())
    }

    /// Apply the side file next to `path`, if it was written against this
    /// snapshot.  A missing, stale or damaged side file is ignored: the journal
    /// replays those changes from the snapshot's own position.
    fn apply_delta(&mut self, path: &str) {
        let Ok(data) = std::fs::read(Self::delta_path(path)) else { return };
        let u64_at = |o: usize| data.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
        if data.get(..8) != Some(DELTA_MAGIC) || u64_at(8) != Some(self.stamp) {
            return;
        }
        let (Some(journal_id), Some(next_usn), Some(gone), Some(added)) =
            (u64_at(16), u64_at(24), u64_at(32), u64_at(40))
        else {
            return;
        };
        let mut off = 48;
        let mut gone_list = Vec::new();
        for _ in 0..gone {
            let Some(b) = data.get(off..off + 4) else { return };
            gone_list.push(u32::from_le_bytes(b.try_into().unwrap()) as usize);
            off += 4;
        }
        let mut added_list = Vec::new();
        for _ in 0..added {
            let (Some(frn), Some(parent), Some(&flags), Some(len)) =
                (u64_at(off), u64_at(off + 8), data.get(off + 16), data.get(off + 17..off + 19))
            else {
                return;
            };
            let len = u16::from_le_bytes(len.try_into().unwrap()) as usize;
            let Some(name) = data.get(off + 19..off + 19 + len) else { return };
            let entry = Entry { frn, parent, name_off: 0, name_len: len as u16, flags, _pad: 0 };
            added_list.push((entry, String::from_utf8_lossy(name).into_owned()));
            off += 19 + len;
        }
        for i in gone_list {
            if i < self.base.entries().len() && !self.is_dead(i) {
                self.kill(i);
            }
        }
        for (entry, name) in added_list {
            self.put(entry, &name);
        }
        self.journal_id = journal_id;
        self.next_usn = next_usn as i64;
    }

    /// Open a saved snapshot and the changes saved next to it.  Snapshots from
    /// earlier versions are converted.
    pub fn load(path: &str, root: &str) -> std::io::Result<DriveIndex> {
        let mut magic = [0u8; 8];
        File::open(path)?.read_exact(&mut magic)?;
        if &magic == SNAPSHOT_MAGIC_V1 {
            let mut index = Self::load_v1(path, root)?;
            index.merge(path)?;
            return Ok(index);
        }
        let m = Base::map(path)?;
        let mut index = DriveIndex::from_base(root, m.serial, m.base);
        index.journal_id = m.journal_id;
        index.next_usn = m.next_usn;
        index.stamp = m.stamp;
        index.apply_delta(path);
        Ok(index)
    }

    /// The first snapshot format: a plain list of records.
    fn load_v1(path: &str, root: &str) -> std::io::Result<DriveIndex> {
        let data = std::fs::read(path)?;
        let bad = || std::io::Error::other("damaged bettergrep drive snapshot");
        if data.len() < 36 {
            return Err(bad());
        }
        let u64_at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
        let count = u64_at(28) as usize;
        let mut builder = Builder::default();
        builder.entries.reserve_exact(count.min(data.len() / 22));
        let mut off = 36;
        for _ in 0..count {
            if off + 22 > data.len() {
                return Err(bad());
            }
            let attrs = u32::from_le_bytes(data[off + 16..off + 20].try_into().unwrap());
            let len = u16::from_le_bytes([data[off + 20], data[off + 21]]) as usize;
            let name = data.get(off + 22..off + 22 + len).ok_or_else(bad)?;
            builder.add(u64_at(off), u64_at(off + 8), &String::from_utf8_lossy(name), attrs);
            off += 22 + len;
        }
        let mut index = builder.finish(root, u32::from_le_bytes(data[8..12].try_into().unwrap()));
        index.journal_id = u64_at(12);
        index.next_usn = u64_at(20) as i64;
        Ok(index)
    }

    /// Load the snapshot and catch up from the journal, or sweep the volume if
    /// there is no usable snapshot.  Without a volume handle (no admin rights)
    /// the snapshot is returned as-is.
    pub fn open(home: &str, drive: &str, volume: Option<&Volume>) -> Result<(DriveIndex, OpenOutcome), DriveError> {
        let root = normalize_root(drive)
            .ok_or_else(|| DriveError::Io(std::io::Error::other(format!("not a drive: {drive}"))))?;
        let serial = match volume {
            Some(v) => v.serial,
            None => volume_info(&root)?.0,
        };
        let path = Self::snapshot_path(home, serial);
        if let Ok(mut index) = Self::load(&path, &root) {
            let Some(volume) = volume else { return Ok((index, OpenOutcome::SnapshotOnly)) };
            match index.catch_up(volume) {
                Ok(n) => return Ok((index, OpenOutcome::CaughtUp(n))),
                Err(DriveError::JournalReset) => {}
                Err(e) => return Err(e),
            }
        }
        let volume = volume.ok_or(DriveError::AdminRequired)?;
        let mut index = Self::scan(volume)?;
        index.save(&path)?;
        Ok((index, OpenOutcome::Scanned))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OpenOutcome {
    /// Loaded from disk; not updated (no administrator rights).
    SnapshotOnly,
    /// Loaded from disk and updated with this many journal records.
    CaughtUp(usize),
    /// No usable snapshot: the whole volume was read.
    Scanned,
}

// -- search -------------------------------------------------------------------

#[derive(Clone, Debug, serde::Serialize)]
pub struct Hit {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub hidden: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SearchResult {
    pub total: usize,
    pub hits: Vec<Hit>,
    pub millis: f64,
}

/// Lower is better: exact name, then prefix, then word start, then anywhere.
fn score(name: &str, term: &str) -> u8 {
    if name == term {
        0
    } else if name.starts_with(term) {
        1
    } else if name.match_indices(term).any(|(i, _)| i > 0 && !name.as_bytes()[i - 1].is_ascii_alphanumeric()) {
        2
    } else {
        3
    }
}

/// A lowercased search term with what case-insensitive matching needs.
struct Needle {
    lower: String,
    ascii: bool,
    first_lower: u8,
    first_upper: u8,
}

impl Needle {
    fn new(term: &str) -> Needle {
        let lower = term.to_lowercase();
        let first = lower.as_bytes().first().copied().unwrap_or(0);
        Needle { ascii: lower.is_ascii(), first_lower: first, first_upper: first.to_ascii_uppercase(), lower }
    }

    /// Positions where this (ASCII) term occurs in `name`, ignoring case.
    fn positions<'a>(&'a self, name: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
        let needle = self.lower.as_bytes();
        let mut from = 0;
        std::iter::from_fn(move || {
            while let Some(p) = memchr::memchr2(self.first_lower, self.first_upper, name.get(from..)?) {
                let start = from + p;
                from = start + 1;
                if start + needle.len() > name.len() {
                    return None;
                }
                if name[start..start + needle.len()].eq_ignore_ascii_case(needle) {
                    return Some(start);
                }
            }
            None
        })
    }

    fn matches(&self, name: &[u8]) -> bool {
        if self.ascii {
            self.positions(name).next().is_some()
        } else {
            std::str::from_utf8(name).is_ok_and(|s| s.to_lowercase().contains(&self.lower))
        }
    }

    /// Lower is better: exact name, then prefix, then word start, then anywhere.
    fn score(&self, name: &[u8]) -> u8 {
        if !self.ascii {
            return std::str::from_utf8(name).map_or(3, |s| score(&s.to_lowercase(), &self.lower));
        }
        let mut best = 3;
        for at in self.positions(name) {
            let s = if at == 0 {
                if name.len() == self.lower.len() { 0 } else { 1 }
            } else if !name[at - 1].is_ascii_alphanumeric() {
                2
            } else {
                3
            };
            best = best.min(s);
            if best <= 2 {
                break;
            }
        }
        best
    }
}

/// Which kinds of entries a search returns.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum KindFilter {
    #[default]
    Any,
    Files,
    Folders,
}

/// Where a match lives: a snapshot entry, or a change held in memory (by record).
#[derive(Clone, Copy)]
enum Loc {
    Base(u32),
    Added(usize),
}

/// (score, name length, drive, location): sorted by the first two.
type Match = (u8, u16, usize, Loc);

/// Keep only the `limit` best matches.
fn keep_best(matches: &mut Vec<Match>, limit: usize) {
    if matches.len() > limit {
        matches.select_nth_unstable_by_key(limit, |m| (m.0, m.1));
        matches.truncate(limit);
    }
}

/// Case-insensitive search over the names of every file on the given drives.
/// Every whitespace-separated term must occur in the name; `*` and `?` are not
/// special here (plain substring), which is what makes this fast.
pub fn search(indexes: &[&DriveIndex], query: &str, limit: usize, include_hidden: bool) -> SearchResult {
    search_kind(indexes, query, limit, include_hidden, KindFilter::Any)
}

pub fn search_kind(
    indexes: &[&DriveIndex],
    query: &str,
    limit: usize,
    include_hidden: bool,
    kind: KindFilter,
) -> SearchResult {
    let started = std::time::Instant::now();
    let needles: Vec<Needle> = query.split_whitespace().map(Needle::new).collect();
    if needles.is_empty() {
        return SearchResult { total: 0, hits: Vec::new(), millis: 0.0 };
    }
    let test = |e: &Entry, name: &[u8]| -> Option<u8> {
        if (!include_hidden && e.hidden())
            || (kind == KindFilter::Files && e.is_dir())
            || (kind == KindFilter::Folders && !e.is_dir())
            || !needles.iter().all(|n| n.matches(name))
        {
            return None;
        }
        needles.iter().map(|n| n.score(name)).min()
    };
    // Threads trim their own matches as they go, so a one-letter query does not
    // collect millions of them.
    let trim_at = limit.max(64).saturating_mul(2);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
    let mut total = 0;
    let mut matches: Vec<Match> = Vec::new();
    for (d, index) in indexes.iter().enumerate() {
        let entries = index.base.entries();
        let chunk = entries.len().div_ceil(threads).max(1);
        let parts: Vec<(usize, Vec<Match>)> = std::thread::scope(|s| {
            let handles: Vec<_> = entries
                .chunks(chunk)
                .enumerate()
                .map(|(c, part)| {
                    let test = &test;
                    s.spawn(move || {
                        let (mut count, mut out) = (0, Vec::new());
                        for (k, e) in part.iter().enumerate() {
                            let i = c * chunk + k;
                            if index.is_dead(i) {
                                continue;
                            }
                            if let Some(score) = test(e, index.base_name(e)) {
                                count += 1;
                                out.push((score, e.name_len, d, Loc::Base(i as u32)));
                                if out.len() >= trim_at {
                                    keep_best(&mut out, limit);
                                }
                            }
                        }
                        (count, out)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
        });
        for (count, part) in parts {
            total += count;
            matches.extend(part);
        }
        for (&record, a) in &index.added {
            if let Some(score) = test(&a.entry, a.name.as_bytes()) {
                total += 1;
                matches.push((score, a.name.len().min(u16::MAX as usize) as u16, d, Loc::Added(record)));
            }
        }
        keep_best(&mut matches, limit);
    }
    matches.sort_unstable_by_key(|m| (m.0, m.1));
    let hits = matches
        .into_iter()
        .filter_map(|(_, _, d, loc)| {
            let index = indexes[d];
            let (e, name) = match loc {
                Loc::Base(i) => {
                    let e = index.base.entries()[i as usize];
                    (e, index.base_name(&e))
                }
                Loc::Added(record) => {
                    let a = &index.added[&record];
                    (a.entry, a.name.as_bytes())
                }
            };
            Some(Hit {
                name: String::from_utf8_lossy(name).into_owned(),
                path: index.path_of(e, name)?,
                is_dir: e.is_dir(),
                hidden: e.hidden(),
            })
        })
        .collect();
    SearchResult { total, hits, millis: started.elapsed().as_secs_f64() * 1000.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> DriveIndex {
        let mut b = Builder::default();
        b.add(10, ROOT_RECORD, "Users", ATTR_DIRECTORY);
        b.add(11, 10, "Me", ATTR_DIRECTORY);
        b.add(12, 11, "tax-2026.pdf", 0x20);
        b.add(13, 11, "Taxes notes.txt", 0x20);
        b.add(14, 11, "desktop.ini", ATTR_HIDDEN);
        b.finish("C:\\", 1)
    }

    fn rec(frn: u64, parent: u64, name: &str, reason: u32) -> UsnRecord {
        UsnRecord { frn, parent, usn: 0, ts: 1.0, reason, attrs: 0x20, name: name.into() }
    }

    fn paths(ix: &DriveIndex, q: &str) -> Vec<String> {
        search(&[ix], q, 10, false).hits.into_iter().map(|h| h.path).collect()
    }

    #[test]
    fn paths_and_ranked_search() {
        let ix = index();
        let r = search(&[&ix], "tax", 10, false);
        assert_eq!(r.total, 2);
        assert_eq!(r.hits[0].path, r"C:\Users\Me\tax-2026.pdf");
        assert!(search(&[&ix], "desktop", 10, false).hits.is_empty(), "hidden excluded");
        assert_eq!(search(&[&ix], "DESKTOP", 10, true).total, 1);
        assert_eq!(search(&[&ix], "tax notes", 10, false).total, 1);
    }

    #[test]
    fn journal_records_update_the_index() {
        let mut ix = index();
        ix.apply(rec(12, 10, "tax-final.pdf", REASON_RENAME_NEW_NAME));
        assert_eq!(ix.recent.back().unwrap().kind, ChangeKind::Moved);
        assert_eq!(ix.recent.back().unwrap().from.as_deref(), Some(r"C:\Users\Me\tax-2026.pdf"));
        assert_eq!(paths(&ix, "tax-final"), [r"C:\Users\tax-final.pdf"]);
        assert!(paths(&ix, "tax-2026").is_empty(), "old name gone");
        ix.apply(rec(13, 11, "Taxes notes.txt", REASON_FILE_DELETE));
        assert_eq!(search(&[&ix], "notes", 10, false).total, 0);
        ix.apply(rec(15, 11, "new.docx", REASON_FILE_CREATE));
        assert_eq!(ix.recent.back().unwrap().path, r"C:\Users\Me\new.docx");
        ix.apply(rec(15, 11, "new.docx", REASON_FILE_DELETE));
        assert!(paths(&ix, "new.docx").is_empty());
        assert_eq!(ix.len(), 4);
    }

    /// The saved index as a reader without admin rights sees it.
    fn reload(path: &str) -> DriveIndex {
        DriveIndex::load(path, "C:\\").unwrap()
    }

    #[test]
    fn changes_reach_the_side_file_then_a_new_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wmd").to_string_lossy().into_owned();
        let mut ix = index();
        ix.journal_id = 7;
        ix.next_usn = 99;
        ix.save(&path).unwrap();
        assert!(ix.mapped_bytes() > 0, "continues from the mapped file");
        assert_eq!(paths(&ix, "tax-"), [r"C:\Users\Me\tax-2026.pdf"]);

        // Changes on top of the mapped file go to the side file...
        ix.apply(rec(12, 10, "tax-final.pdf", REASON_RENAME_NEW_NAME));
        ix.apply(rec(15, 11, "new.docx", REASON_FILE_CREATE));
        ix.apply(rec(13, 11, "Taxes notes.txt", REASON_FILE_DELETE));
        ix.next_usn = 150;
        let before = std::fs::metadata(&path).unwrap().len();
        ix.save(&path).unwrap();
        assert!(Path::new(&format!("{path}.delta")).exists());
        let check = |loaded: &DriveIndex| {
            assert_eq!((loaded.len(), loaded.journal_id, loaded.next_usn), (5, 7, 150));
            assert_eq!(paths(loaded, "tax"), [r"C:\Users\tax-final.pdf"]);
            assert_eq!(paths(loaded, "new.docx"), [r"C:\Users\Me\new.docx"]);
            assert!(paths(loaded, "notes").is_empty());
        };
        check(&reload(&path));

        // ...and are folded into a new snapshot in place of the mapped one.
        ix.merge(&path).unwrap();
        assert_eq!(ix.memory_bytes(), 0, "nothing left in memory after merging");
        assert!(!Path::new(&format!("{path}.delta")).exists());
        assert_ne!(std::fs::metadata(&path).unwrap().len(), before);
        check(&reload(&path));
    }

    #[test]
    fn a_side_file_from_an_older_snapshot_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wmd").to_string_lossy().into_owned();
        let delta = format!("{path}.delta");
        let mut ix = index();
        ix.save(&path).unwrap();
        ix.apply(rec(15, 11, "new.docx", REASON_FILE_CREATE));
        ix.save(&path).unwrap();
        let stale = std::fs::read(&delta).unwrap();
        ix.apply(rec(15, 11, "new.docx", REASON_FILE_DELETE));
        ix.merge(&path).unwrap();
        std::fs::write(&delta, stale).unwrap();
        assert!(paths(&reload(&path), "new.docx").is_empty());
    }

    #[test]
    fn a_damaged_side_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wmd").to_string_lossy().into_owned();
        let mut ix = index();
        ix.save(&path).unwrap();
        ix.apply(rec(15, 11, "new.docx", REASON_FILE_CREATE));
        ix.save(&path).unwrap();
        let delta = format!("{path}.delta");
        let bytes = std::fs::read(&delta).unwrap();
        std::fs::write(&delta, &bytes[..bytes.len() - 3]).unwrap();
        let loaded = reload(&path);
        assert_eq!(loaded.len(), 5);
        assert!(paths(&loaded, "new.docx").is_empty());
    }

    #[test]
    fn first_format_snapshots_are_converted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wmd").to_string_lossy().into_owned();
        let mut v1 = SNAPSHOT_MAGIC_V1.to_vec();
        v1.extend(9u32.to_le_bytes());
        v1.extend(7u64.to_le_bytes());
        v1.extend(99u64.to_le_bytes());
        let rows: [(u64, u64, u32, &str); 3] =
            [(10, ROOT_RECORD, ATTR_DIRECTORY, "Users"), (11, 10, ATTR_DIRECTORY, "Me"), (12, 11, 0x20, "tax.pdf")];
        v1.extend((rows.len() as u64).to_le_bytes());
        for (frn, parent, attrs, name) in rows {
            v1.extend(frn.to_le_bytes());
            v1.extend(parent.to_le_bytes());
            v1.extend(attrs.to_le_bytes());
            v1.extend((name.len() as u16).to_le_bytes());
            v1.extend(name.as_bytes());
        }
        std::fs::write(&path, v1).unwrap();
        let ix = DriveIndex::load(&path, "C:\\").unwrap();
        assert_eq!((ix.serial, ix.journal_id, ix.next_usn), (9, 7, 99));
        assert_eq!(paths(&ix, "tax"), [r"C:\Users\Me\tax.pdf"]);
        assert_eq!(&std::fs::read(&path).unwrap()[..8], SNAPSHOT_MAGIC);
    }

    #[test]
    fn damaged_snapshots_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wmd").to_string_lossy().into_owned();
        let mut ix = index();
        ix.save(&path).unwrap();
        drop(ix);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[32..40].copy_from_slice(&u64::MAX.to_le_bytes()); // entry count
        std::fs::write(&path, &bytes).unwrap();
        assert!(DriveIndex::load(&path, "C:\\").is_err());
    }

    #[test]
    fn one_letter_queries_count_everything_but_keep_only_the_best() {
        let mut b = Builder::default();
        for i in 0..20_000u64 {
            b.add(100 + i, ROOT_RECORD, &format!("file-{i}.txt"), 0x20);
        }
        b.add(50, ROOT_RECORD, "e", 0x20);
        let ix = b.finish("C:\\", 1);
        let r = search(&[&ix], "e", 5, false);
        assert_eq!((r.total, r.hits.len()), (20_001, 5));
        assert_eq!(r.hits[0].path, r"C:\e");
    }

    #[test]
    fn rewrites_of_one_file_fold_into_one_feed_entry() {
        let mut ix = index();
        let at = |mut r: UsnRecord, ts: f64| {
            r.ts = ts;
            r
        };
        for i in 0..10 {
            let reason = if i % 2 == 0 { REASON_FILE_CREATE } else { REASON_FILE_DELETE };
            ix.apply(at(rec(20, 11, "settings.json", reason), 100.0 + i as f64 * 0.25));
        }
        let updates = ix.take_updates();
        assert_eq!(updates.len(), 1);
        let first = updates[0].clone();
        assert_eq!((first.count, first.kind, first.since), (10, ChangeKind::Deleted, Some(100.0)));
        assert_eq!(ix.recent.len(), 1);

        // A rename stands alone, and a change more than a minute later starts a new entry.
        ix.apply(at(rec(12, 10, "tax-final.pdf", REASON_RENAME_NEW_NAME), 103.0));
        ix.apply(at(rec(20, 11, "settings.json", REASON_FILE_CREATE), 200.0));
        let updates = ix.take_updates();
        assert_eq!(updates.len(), 2);
        assert!(updates.iter().all(|c| c.id != first.id && c.count == 1));
        assert_eq!(ix.recent.len(), 3);
        assert!(ix.take_updates().is_empty());
    }

    #[test]
    fn files_under_a_folder_come_from_the_index() {
        let mut b = Builder::default();
        b.add(10, ROOT_RECORD, "Users", ATTR_DIRECTORY);
        b.add(11, 10, "Me", ATTR_DIRECTORY);
        b.add(12, 11, "a.txt", 0x20);
        b.add(13, 11, "AppData", ATTR_DIRECTORY | ATTR_HIDDEN);
        b.add(14, 13, "b.txt", 0x20);
        b.add(15, ROOT_RECORD, "c.txt", 0x20);
        let ix = b.finish("C:\\", 1);
        let under = |dir: &str, hidden: bool| {
            let mut v = Vec::new();
            assert!(ix.for_each_file_under(
                dir,
                hidden,
                |_| true,
                |p| {
                    v.push(p);
                    true
                }
            ));
            v.sort();
            v
        };
        assert_eq!(under(r"C:\users\me", false), [r"C:\Users\Me\a.txt"], "hidden folders left out");
        assert_eq!(under(r"C:\Users\Me", true), [r"C:\Users\Me\AppData\b.txt", r"C:\Users\Me\a.txt"]);
        assert_eq!(under(r"C:\Users\Me\AppData", false), [r"C:\Users\Me\AppData\b.txt"], "unless asked for");
        assert_eq!(under("C:\\", false), [r"C:\Users\Me\a.txt", r"C:\c.txt"]);
        assert!(!ix.for_each_file_under(r"C:\Nope", false, |_| true, |_| true));
    }

    #[test]
    fn record_parsing() {
        let name: Vec<u8> = "a.txt".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let len = 60 + name.len() + 2;
        let mut rec = vec![0u8; len];
        rec[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        rec[4..6].copy_from_slice(&2u16.to_le_bytes());
        rec[8..16].copy_from_slice(&42u64.to_le_bytes());
        rec[16..24].copy_from_slice(&5u64.to_le_bytes());
        rec[56..58].copy_from_slice(&(name.len() as u16).to_le_bytes());
        rec[58..60].copy_from_slice(&60u16.to_le_bytes());
        rec[60..60 + name.len()].copy_from_slice(&name);
        let mut got = Vec::new();
        parse_records(&rec, |r| got.push(r));
        assert_eq!((got[0].frn, got[0].parent, got[0].name.as_str()), (42, 5, "a.txt"));
    }
}
