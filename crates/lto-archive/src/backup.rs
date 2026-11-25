//! Backing up a folder (or items selected in it) onto tapes: everything,
//! or only the files whose Archive attribute is set, into a dated folder
//! on each tape used. Each tape's files can be read back right after they
//! are written and compared with the files on disk; an archive then
//! deletes from disk the files found the same.
//! When a tape is full the backup goes on with the next tape ticked, then
//! with new tapes asked for through a mail slot. When a tape fails (it
//! can't be read or written), the person can go on with another one: what
//! that tape's index doesn't hold is written again there.

use std::collections::BTreeSet;

use super::*;
use crate::prefetch::Prefetch;

/// What to back up, and what to do afterwards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackupOptions {
    /// Only files whose Archive attribute is set (changed since the last
    /// backup that cleared it).
    pub archive_only: bool,
    /// Clear the Archive attribute of each file once it is on a tape (its
    /// index written), so the next archive-only backup leaves it out.
    pub clear_archive: bool,
    /// Where the index written every [`COMMIT_EVERY`] goes. The index at
    /// the end of each tape always goes into both partitions.
    pub interim_index: InterimIndex,
    /// Read each tape's files back right after they are written (once its
    /// index is written), comparing them with the files on disk.
    pub verify: bool,
    /// An archive: at the end, the files on the tapes leave the disk
    /// ([`delete_source`]).
    pub delete: DeleteSource,
    /// Bytes of the next files read from disk ahead of the tape, while it
    /// writes (0: each file is read when it is written).
    pub read_ahead: u64,
    /// The folder on the tapes may already be on a tape: the backup goes
    /// into it, replacing its files of the same names (a name chosen
    /// without the date, for that). Otherwise a tape that already holds it
    /// isn't written: two backups never share a folder by chance.
    pub replace: bool,
}

impl BackupOptions {
    /// What the job is called in the log: `archive` when the files leave
    /// the disk, else `backup`.
    pub fn job(&self) -> &'static str {
        if self.delete == DeleteSource::Keep {
            "backup"
        } else {
            "archive"
        }
    }
}

/// What an archive does with the files on disk once they are on the tapes:
/// those read back the same are deleted (an archive always reads them back:
/// [`BackupOptions::verify`] is turned on for it), and the folders left
/// empty. A file is deleted only while it is the very file that was read
/// back (the same file on the same volume, its size and time unchanged),
/// and only when the tape holds all of it: a file with alternate data
/// streams (which LTFS doesn't keep) stays.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DeleteSource {
    /// Kept: a backup.
    #[default]
    Keep,
    /// Deleted without asking.
    Auto,
    /// Deleted once the person confirms ([`Ui::confirm_delete`]).
    Ask,
}

/// Where a backup writes the index it writes along the way (every
/// [`COMMIT_EVERY`]), so a failure loses at most that much.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InterimIndex {
    /// At the end of the data only, as LTFS does while a volume is
    /// mounted: a few seconds, the tape doesn't move.
    #[default]
    DataPartition,
    /// Into the index partition too: the tape moves to the index partition
    /// and back each time (a minute or two); other LTFS programs find the
    /// newest index there even without a recovery.
    BothPartitions,
}

impl InterimIndex {
    /// As kept in settings: `data` or `both`.
    pub fn key(self) -> &'static str {
        match self {
            InterimIndex::DataPartition => "data",
            InterimIndex::BothPartitions => "both",
        }
    }

    /// From settings; anything else is the default.
    pub fn from_key(k: &str) -> InterimIndex {
        if k.trim().eq_ignore_ascii_case("both") {
            InterimIndex::BothPartitions
        } else {
            InterimIndex::DataPartition
        }
    }
}

/// What went onto one tape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TapeDone {
    pub barcode: String,
    pub files: u64,
    pub bytes: u64,
}

/// How a backup went.
#[derive(Debug, Clone, Default)]
pub struct BackupReport {
    /// The folder on the tapes (`Backup 2026-11-14 15.30\Documents`).
    pub set: String,
    /// The tapes written, in order.
    pub tapes: Vec<TapeDone>,
    /// Files larger than a whole tape: left out (their Archive attribute
    /// stays set).
    pub too_big: Vec<PathBuf>,
    /// Files and folders that couldn't be read, with why.
    pub unreadable: Vec<(PathBuf, String)>,
    /// Files whose Archive attribute couldn't be cleared, or were changed
    /// while they were written (theirs is left set).
    pub not_cleared: Vec<PathBuf>,
    /// Stopped by the person; what was written is on the tapes.
    pub cancelled: bool,
    /// Tapes that failed during the backup, with why; the backup went on
    /// with others (or stopped: [`Self::error`]).
    pub failed_tapes: Vec<(String, String)>,
    /// Why the backup stopped before everything was written, other than
    /// the person's Cancel; what was written is on the tapes.
    pub error: Option<String>,
    /// Folders found to back up (the folder itself not counted).
    pub total_folders: u64,
    /// Files and bytes found to back up.
    pub total_files: u64,
    pub total_bytes: u64,
    /// The files on the tapes: each in a tape index that was written.
    pub on_tapes: Vec<FileDone>,
    /// The files were read back ([`BackupOptions::verify`]).
    pub checked: bool,
    /// With [`BackupOptions::verify`]: the files read back from the tapes
    /// and found the same as on disk.
    pub verified: Vec<FileDone>,
    /// With [`BackupOptions::verify`]: files written but read back
    /// differently, or not at all, with why.
    pub verify_failed: Vec<(PathBuf, String)>,
    /// The folders on disk that went onto the tapes (not the folder backed
    /// up itself): those left empty go when the files are deleted.
    pub folders: Vec<PathBuf>,
    /// The folders that stay whatever is deleted: the folder backed up, or
    /// the folders the items backed up are in. Nothing outside them goes.
    pub keep_folders: Vec<PathBuf>,
    /// With [`BackupOptions::delete`]: what deleting did.
    pub deleted: Option<DeleteReport>,
}

/// A file on a tape (or read back from it and found the same as on disk),
/// with its size and modification time then: it is deleted only while it
/// still has them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDone {
    pub path: PathBuf,
    pub length: u64,
    pub modified: Option<std::time::SystemTime>,
    /// Which file it was when it was read back (its volume and file
    /// number): it is deleted only while the path leads to that file.
    /// `None`: not read back, never deleted.
    pub id: Option<FileId>,
}

/// A file's identity on disk: (volume serial number, file number).
pub type FileId = (u64, u64);

/// The identity of the open file `f`.
#[cfg(windows)]
pub(crate) fn file_id(f: &fs::File) -> Option<FileId> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) } == 0 {
        return None;
    }
    Some((info.dwVolumeSerialNumber as u64, (info.nFileIndexHigh as u64) << 32 | info.nFileIndexLow as u64))
}

#[cfg(unix)]
pub(crate) fn file_id(f: &fs::File) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    f.metadata().ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(not(any(windows, unix)))]
pub(crate) fn file_id(_: &fs::File) -> Option<FileId> {
    None
}

/// Why a file to delete was kept.
enum Kept {
    /// Not the file read back any more (another file, size or time).
    Changed,
    /// It can't go: why.
    Failed(String),
}

/// The names of the data streams of the open file other than its main one
/// (`None`: the file system doesn't say, it has none).
#[cfg(windows)]
fn other_streams(f: &fs::File) -> io::Result<Vec<String>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_HANDLE_EOF, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED};
    use windows_sys::Win32::Storage::FileSystem::{FileStreamInfo, GetFileInformationByHandleEx};
    // FILE_STREAM_INFO entries: next offset (u32), name length (u32), size
    // (i64), allocation size (i64), the name (UTF-16)
    let mut buf = vec![0u64; 8192];
    let ok = unsafe { GetFileInformationByHandleEx(f.as_raw_handle() as _, FileStreamInfo, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32) };
    if ok == 0 {
        let e = io::Error::last_os_error();
        return match e.raw_os_error().map(|c| c as u32) {
            // no stream at all (a folder without named streams)
            Some(ERROR_HANDLE_EOF) => Ok(Vec::new()),
            // a file system that has no named streams (FAT, exFAT): none;
            // one that has them but didn't list them: nobody knows
            Some(ERROR_INVALID_PARAMETER | ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED) if !named_streams_possible(f)? => Ok(Vec::new()),
            _ => Err(e),
        };
    }
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() * 8) };
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 24 <= bytes.len() {
        let next = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let end = (at + 24 + len).min(bytes.len());
        let name: Vec<u16> = bytes[at + 24..end].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        let name = String::from_utf16_lossy(&name);
        if name != "::$DATA" {
            out.push(name);
        }
        if next == 0 {
            break;
        }
        at += next;
    }
    Ok(out)
}

/// Whether the file system the open file `f` is on can hold named data
/// streams (NTFS, ReFS…).
#[cfg(windows)]
fn named_streams_possible(f: &fs::File) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;
    const FILE_NAMED_STREAMS: u32 = 0x0004_0000;
    let mut flags = 0u32;
    if unsafe { GetVolumeInformationByHandleW(f.as_raw_handle() as _, std::ptr::null_mut(), 0, std::ptr::null_mut(), std::ptr::null_mut(), &mut flags, std::ptr::null_mut(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags & FILE_NAMED_STREAMS != 0)
}

/// Remove the empty folder `dir` that went onto the tapes: not a link or
/// a junction, and only without named data streams (a folder can have
/// them on NTFS, and the tape doesn't hold them); when its streams can't be
/// listed, it stays. Removed through the handle it was checked on.
#[cfg(windows)]
fn remove_dir_checked(dir: &Path) -> std::result::Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileDispositionInfo, SetFileInformationByHandle, DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let h = unsafe { CreateFileW(wide.as_ptr(), DELETE | FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, std::ptr::null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error().to_string());
    }
    let d = unsafe { fs::File::from_raw_handle(h as _) };
    let m = d.metadata().map_err(|e| e.to_string())?;
    if !m.is_dir() || m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err("it isn't a folder any more (a link or a junction)".into());
    }
    match other_streams(&d) {
        Ok(s) if s.is_empty() => {}
        Ok(s) => return Err(format!("it has data streams the tape doesn't hold ({})", s.join(", "))),
        Err(e) => return Err(format!("its data streams can't be listed: {e}")),
    }
    let del = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    // fails when it isn't empty: then it stays
    if unsafe { SetFileInformationByHandle(d.as_raw_handle() as _, FileDispositionInfo, (&del as *const FILE_DISPOSITION_INFO).cast(), std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32) } == 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(())
}

#[cfg(not(windows))]
fn remove_dir_checked(dir: &Path) -> std::result::Result<(), String> {
    if fs::symlink_metadata(dir).map_err(|e| e.to_string())?.file_type().is_symlink() {
        return Err("it is a link".into());
    }
    fs::remove_dir(dir).map_err(|e| e.to_string())
}

/// Delete `v`'s file, only while it is the file read back: opened without
/// following a link, its identity, size and time checked on that handle,
/// and deleted through it (no other file can take its place meanwhile).
#[cfg(windows)]
fn delete_checked(v: &FileDone) -> std::result::Result<(), Kept> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileBasicInfo, FileDispositionInfo, SetFileInformationByHandle, DELETE, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO, FILE_DISPOSITION_INFO,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_WRITE_ATTRIBUTES, OPEN_EXISTING,
    };
    let Some(want) = v.id else { return Err(Kept::Failed("it wasn't read back from the tape".into())) };
    let wide: Vec<u16> = v.path.as_os_str().encode_wide().chain(Some(0)).collect();
    // no one may write it while it is checked and deleted
    let h = unsafe { CreateFileW(wide.as_ptr(), DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT, std::ptr::null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        let e = io::Error::last_os_error();
        return Err(if e.kind() == io::ErrorKind::NotFound { Kept::Changed } else { Kept::Failed(e.to_string()) });
    }
    let f = unsafe { fs::File::from_raw_handle(h as _) };
    let m = f.metadata().map_err(|e| Kept::Failed(e.to_string()))?;
    if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !m.is_file() || m.len() != v.length || m.modified().ok() != v.modified || file_id(&f) != Some(want) {
        return Err(Kept::Changed);
    }
    // the tape holds the main stream only: one more, and the file stays
    match other_streams(&f) {
        Ok(s) if s.is_empty() => {}
        Ok(s) => return Err(Kept::Failed(format!("it has alternate data streams the tape doesn't hold ({})", s.join(", ")))),
        Err(e) => return Err(Kept::Failed(format!("its data streams can't be listed: {e}"))),
    }
    // a read-only file goes too: it is on the tapes
    let attrs = m.file_attributes();
    if attrs & FILE_ATTRIBUTE_READONLY != 0 {
        let rest = attrs & !FILE_ATTRIBUTE_READONLY;
        let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
        basic.FileAttributes = if rest == 0 { FILE_ATTRIBUTE_NORMAL } else { rest };
        unsafe { SetFileInformationByHandle(f.as_raw_handle() as _, FileBasicInfo, (&basic as *const FILE_BASIC_INFO).cast(), std::mem::size_of::<FILE_BASIC_INFO>() as u32) };
    }
    let del = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    if unsafe { SetFileInformationByHandle(f.as_raw_handle() as _, FileDispositionInfo, (&del as *const FILE_DISPOSITION_INFO).cast(), std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32) } == 0 {
        return Err(Kept::Failed(io::Error::last_os_error().to_string()));
    }
    // gone once the handle is closed
    drop(f);
    Ok(())
}

#[cfg(not(windows))]
fn delete_checked(v: &FileDone) -> std::result::Result<(), Kept> {
    let Some(want) = v.id else { return Err(Kept::Failed("it wasn't read back from the tape".into())) };
    let f = fs::File::open(&v.path).map_err(|_| Kept::Changed)?;
    let now = fs::symlink_metadata(&v.path).map_err(|_| Kept::Changed)?;
    let m = f.metadata().map_err(|e| Kept::Failed(e.to_string()))?;
    if now.file_type().is_symlink() || !m.is_file() || m.len() != v.length || m.modified().ok() != v.modified || file_id(&f) != Some(want) {
        return Err(Kept::Changed);
    }
    fs::remove_file(&v.path).map_err(|e| Kept::Failed(e.to_string()))
}

/// What deleting the files on the tapes from disk did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeleteReport {
    pub files: u64,
    pub bytes: u64,
    pub folders: u64,
    /// Changed since they were checked (size or time): kept.
    pub changed: Vec<PathBuf>,
    /// Couldn't be deleted, with why.
    pub failed: Vec<(PathBuf, String)>,
    /// Stopped (Cancel): how many of the files to delete weren't reached.
    /// They stay, on disk and on the tapes, and no folder was removed.
    pub stopped: Option<u64>,
}

/// Delete from disk the files of a backup that are safe on the tapes
/// ([`BackupReport::deletable`]), for good (an archive frees the disk; the
/// Recycle Bin wouldn't), then the folders that went onto the tapes and
/// are left empty. A file changed since it was written or checked is kept,
/// and so are [`BackupReport::keep_folders`]. `progress(files done, of)`;
/// false stops it.
pub fn delete_source(report: &BackupReport, progress: &mut dyn FnMut(u64, u64) -> bool) -> DeleteReport {
    let files = report.deletable();
    log!("job: delete the {} files of \"{}\" {} from disk", files.len(), report.set, if report.checked { "read back the same from the tapes" } else { "on the tapes" });
    let mut out = DeleteReport::default();
    let total = files.len() as u64;
    // only inside the folders backed up (or the items' folders), never those
    let keep = &report.keep_folders;
    let inside = |d: &Path| !keep.iter().any(|k| k == d) && keep.iter().any(|k| d.starts_with(k));
    // the folders to remove once empty: those on the tapes, and those the
    // files deleted were in
    let mut folders: BTreeSet<PathBuf> = report.folders.iter().filter(|f| inside(f)).cloned().collect();
    for (i, v) in files.iter().enumerate() {
        if !progress(i as u64, total) {
            out.stopped = Some(total - i as u64);
            log!("delete: STOPPED after {} files; {} not reached, and no folder is removed", out.files, total - i as u64);
            break;
        }
        let mut dir = v.path.parent();
        while let Some(d) = dir.filter(|d| inside(d)) {
            folders.insert(d.to_path_buf());
            dir = d.parent();
        }
        match delete_checked(v) {
            Ok(()) => {
                log!("delete: \"{}\"", v.path.display());
                out.files += 1;
                out.bytes += v.length;
            }
            Err(Kept::Changed) => {
                log!("delete: \"{}\" changed since it was written or checked (or is another file now): kept", v.path.display());
                out.changed.push(v.path.clone());
            }
            Err(Kept::Failed(e)) => {
                log!("delete: \"{}\" kept: {e}", v.path.display());
                out.failed.push((v.path.clone(), e));
            }
        }
    }
    // the deepest first; a folder with anything left in it stays. Once
    // stopped, nothing more goes, folders neither
    let mut folders: Vec<PathBuf> = if out.stopped.is_some() { Vec::new() } else { folders.into_iter().collect() };
    folders.sort_by_key(|f| std::cmp::Reverse(f.components().count()));
    for f in folders {
        if !progress(total, total) {
            out.stopped = Some(0);
            log!("delete: STOPPED before the folders left empty were all removed");
            break;
        }
        match remove_dir_checked(&f) {
            Ok(()) => {
                log!("delete: folder \"{}\"", f.display());
                out.folders += 1;
            }
            Err(why) => log!("delete: folder \"{}\" stays: {why}", f.display()),
        }
    }
    log!("delete done: {} files ({}), {} folders; changed {}, failed {}", out.files, lto_scsi::log::bytes(out.bytes), out.folders, out.changed.len(), out.failed.len());
    out
}

impl BackupReport {
    /// Files and bytes written.
    pub fn written(&self) -> (u64, u64) {
        self.tapes.iter().fold((0, 0), |(f, b), t| (f + t.files, b + t.bytes))
    }

    /// Files and bytes found the same on the tapes as on disk.
    pub fn verified_size(&self) -> (u64, u64) {
        (self.verified.len() as u64, self.verified.iter().map(|v| v.length).sum())
    }

    /// The files an archive deletes from disk: those read back the same.
    /// Without the check, none: being in a tape's index doesn't prove the
    /// tape gives them back.
    pub fn deletable(&self) -> &[FileDone] {
        if self.checked {
            &self.verified
        } else {
            &[]
        }
    }

    /// Files and bytes of [`Self::deletable`].
    pub fn deletable_size(&self) -> (u64, u64) {
        let d = self.deletable();
        (d.len() as u64, d.iter().map(|v| v.length).sum())
    }
}

/// Why a new tape put in for a backup can't be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewTapeProblem<'a> {
    /// No tape that wasn't used yet showed up in a mail slot or drive.
    NothingNew,
    /// It holds files.
    HasData(&'a str),
}

/// How long to write before the index is written, so a failure (power, a
/// crash) loses at most this much of what is on the tape.
pub const COMMIT_EVERY: u64 = 100 * 1024 * 1024 * 1024;

#[cfg(test)]
thread_local! {
    /// Tests write the interim index sooner.
    pub(crate) static TEST_COMMIT_EVERY: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

fn commit_every() -> u64 {
    #[cfg(test)]
    if let Some(n) = TEST_COMMIT_EVERY.with(|c| c.get()) {
        return n;
    }
    COMMIT_EVERY
}

/// The folder on the tapes for a backup of `src` now:
/// `Backup YYYY-MM-DD HH.MM.SS\<folder name>` (a drive's root: its letter).
pub fn backup_set_name(src: &Path) -> String {
    set_name("Backup", src)
}

/// As [`backup_set_name`], for an archive: `Archive YYYY-MM-DD HH.MM.SS\<folder name>`.
pub fn archive_set_name(src: &Path) -> String {
    set_name("Archive", src)
}

fn set_name(kind: &str, src: &Path) -> String {
    set_folder(&format!("{kind} {}", local_stamp()), src)
}

/// `name` with the local date and time: `name YYYY-MM-DD HH.MM.SS`.
pub fn dated(name: &str) -> String {
    format!("{name} {}", local_stamp())
}

/// The folder on the tapes for `src` under the folder `top`:
/// `top\<folder name>` (a drive's root: its letter).
pub fn set_folder(top: &str, src: &Path) -> String {
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .or_else(|| src.to_string_lossy().chars().find(|c| c.is_ascii_alphabetic()).map(|c| c.to_ascii_uppercase().to_string()))
        .unwrap_or_else(|| "backup".into());
    format!("{top}\\{name}")
}

/// The local date and time to the second, `YYYY-MM-DD HH.MM.SS` (dots:
/// `:` can't be in a name): two backups started in the same minute get
/// folders of their own.
#[cfg(windows)]
fn local_stamp() -> String {
    let mut t = unsafe { std::mem::zeroed::<windows_sys::Win32::Foundation::SYSTEMTIME>() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    format!("{:04}-{:02}-{:02} {:02}.{:02}.{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

/// Elsewhere: UTC.
#[cfg(not(windows))]
fn local_stamp() -> String {
    let s = Time::now().to_string();
    format!("{} {}", &s[..10], s[11..19].replace(':', "."))
}

/// Whether the file's Archive attribute is set.
#[cfg(windows)]
fn has_archive(m: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_ARCHIVE != 0
}

#[cfg(not(windows))]
fn has_archive(_: &fs::Metadata) -> bool {
    true
}

/// Clear the file's Archive attribute.
#[cfg(windows)]
pub fn clear_archive(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_ARCHIVE, INVALID_FILE_ATTRIBUTES};
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let a = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if a == INVALID_FILE_ATTRIBUTES {
        return Err(io::Error::last_os_error());
    }
    if a & FILE_ATTRIBUTE_ARCHIVE != 0 && unsafe { SetFileAttributesW(wide.as_ptr(), a & !FILE_ATTRIBUTE_ARCHIVE) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn clear_archive(_: &Path) -> io::Result<()> {
    Ok(())
}

/// Set the file's Archive attribute (tests; a person's "it changed").
#[cfg(windows)]
pub fn set_archive(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_ARCHIVE, INVALID_FILE_ATTRIBUTES};
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let a = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if a == INVALID_FILE_ATTRIBUTES || unsafe { SetFileAttributesW(wide.as_ptr(), a | FILE_ATTRIBUTE_ARCHIVE) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn set_archive(_: &Path) -> io::Result<()> {
    Ok(())
}

/// Whether the file's Archive attribute is set (tests, ltocat).
pub fn archive_set(path: &Path) -> io::Result<bool> {
    Ok(has_archive(&fs::metadata(path)?))
}

/// Something to back up: a folder (`size` None) or a file. `rel` is its
/// path under the folder backed up, `\`-separated ("" for the folder itself).
struct Entry {
    disk: PathBuf,
    rel: String,
    size: Option<u64>,
}

/// The items `items` (files, and folders with everything in them, from
/// anywhere), as [`scan`] takes what is in a folder: each under its own
/// name (a drive's root: its letter), made unique (`name (2)`). `roots`:
/// the names given, and the items.
fn scan_items(items: &[PathBuf], archive_only: bool, out: &mut Vec<Entry>, unreadable: &mut Vec<(PathBuf, String)>, roots: &mut BTreeMap<String, PathBuf>) {
    let mut items: Vec<&PathBuf> = items.iter().collect();
    items.sort();
    items.dedup();
    for p in items {
        let disk = p.clone();
        let base = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .or_else(|| p.to_string_lossy().chars().find(|c| c.is_ascii_alphabetic()).map(|c| c.to_ascii_uppercase().to_string()))
            .unwrap_or_else(|| "item".into());
        let taken = |n: &str| roots.keys().any(|k| k.eq_ignore_ascii_case(n));
        let mut name = base.clone();
        let mut i = 2;
        while taken(&name) {
            name = format!("{base} ({i})");
            i += 1;
        }
        roots.insert(name.clone(), disk.clone());
        let meta = match fs::symlink_metadata(&disk) {
            Ok(m) => m,
            Err(e) => {
                unreadable.push((disk, e.to_string()));
                continue;
            }
        };
        if meta.is_dir() {
            if !archive_only || has_archive(&meta) {
                out.push(Entry { disk: disk.clone(), rel: name.clone(), size: None });
            }
            scan(&disk, &name, archive_only, out, unreadable);
        } else if meta.is_file() && (!archive_only || has_archive(&meta)) {
            out.push(Entry { disk, rel: name, size: Some(meta.len()) });
        }
    }
}

/// The folders (each, with everything) and files under `dir`, in name
/// order, folders before what is in them. Links are left out; what can't be
/// read is noted and left out.
fn scan(dir: &Path, rel: &str, archive_only: bool, out: &mut Vec<Entry>, unreadable: &mut Vec<(PathBuf, String)>) {
    let mut entries = match fs::read_dir(dir).and_then(|d| d.collect::<io::Result<Vec<_>>>()) {
        Ok(e) => e,
        Err(e) => {
            unreadable.push((dir.to_path_buf(), e.to_string()));
            return;
        }
    };
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let sub = if rel.is_empty() { name } else { format!("{rel}\\{name}") };
        let meta = match fs::symlink_metadata(e.path()) {
            Ok(m) => m,
            Err(err) => {
                unreadable.push((e.path(), err.to_string()));
                continue;
            }
        };
        if meta.is_dir() {
            // with archive-only, a folder is taken when its own attribute
            // is set (an empty one too); the others are made only where a
            // file goes
            if !archive_only || has_archive(&meta) {
                out.push(Entry { disk: e.path(), rel: sub.clone(), size: None });
            }
            scan(&e.path(), &sub, archive_only, out, unreadable);
        } else if meta.is_file() && (!archive_only || has_archive(&meta)) {
            out.push(Entry { disk: e.path(), rel: sub, size: Some(meta.len()) });
        }
    }
}

/// How a tape's part of a backup ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// Everything is written.
    Done,
    /// The tape is full: go on with the next.
    Full,
    /// The person stopped it.
    Cancelled,
}

/// A backup in progress.
struct Run {
    /// The folder backed up whole (its content right in the set folder);
    /// else items, each under its name in `roots`.
    base: Option<PathBuf>,
    roots: BTreeMap<String, PathBuf>,
    /// The folder on disk whose times the set folder gets (the items' one
    /// folder), if any.
    set_disk: Option<PathBuf>,
    opts: BackupOptions,
    plan: Vec<Entry>,
    /// The next entry of `plan` to write.
    next: usize,
    done_files: u64,
    done_bytes: u64,
    /// Bytes sent to tapes (files carried over to the next tape too), and
    /// the time spent sending them: the average speed.
    written: u64,
    writing: std::time::Duration,
    report: BackupReport,
}

/// How far a backup had got when a tape's index was last written: what a
/// tape that fails after it goes back to.
#[derive(Debug, Clone, Copy)]
struct Mark {
    next: usize,
    done_files: u64,
    done_bytes: u64,
    too_big: usize,
    unreadable: usize,
    on_tape: (u64, u64),
}

/// A file or folder written since the tape's last index: (on disk, its
/// modification time when read, a file's length).
type Unsaved = (PathBuf, Option<std::time::SystemTime>, Option<u64>);

/// Bytes a second, once there is something to go by.
pub(crate) fn speed(bytes: u64, took: std::time::Duration) -> Option<u64> {
    let s = took.as_secs_f64();
    (bytes > 0 && s > 0.0).then(|| (bytes as f64 / s) as u64)
}

impl Run {
    fn mark(&self, on_tape: &TapeDone) -> Mark {
        Mark {
            next: self.next,
            done_files: self.done_files,
            done_bytes: self.done_bytes,
            too_big: self.report.too_big.len(),
            unreadable: self.report.unreadable.len(),
            on_tape: (on_tape.files, on_tape.bytes),
        }
    }

    /// Back to `m`: what was written since isn't in any index on the tape,
    /// so it goes onto the next one.
    fn rewind(&mut self, m: Mark, on_tape: &mut TapeDone) {
        let job = self.opts.job();
        log!("{job}: {} files since the tape's last index go onto the next tape", self.done_files - m.done_files);
        self.next = m.next;
        self.done_files = m.done_files;
        self.done_bytes = m.done_bytes;
        self.report.too_big.truncate(m.too_big);
        self.report.unreadable.truncate(m.unreadable);
        (on_tape.files, on_tape.bytes) = m.on_tape;
    }

    /// The folder `rel` (under the folder backed up) on the tape, made with
    /// the folders above it, each with the times of the folder on disk.
    fn ensure_dir(&self, v: &mut Volume, rel: &str) -> Result<String> {
        let now = Time::now();
        let mut dest = self.report.set.clone();
        let mut disk: Option<PathBuf> = self.base.clone().or_else(|| self.set_disk.clone());
        let make = |v: &mut Volume, dest: &str, disk: Option<&Path>| -> Result<()> {
            if v.index.find(dest).is_none() {
                let t = disk.and_then(|d| fs::metadata(d).ok()).map(|m| times_of(&m, now)).unwrap_or(Times::all(now));
                v.index.make_dirs(dest, now).map_err(Error::Refused)?.times = t;
            }
            Ok(())
        };
        make(v, &dest, disk.as_deref())?;
        for (i, p) in ltfs::index::split(rel).into_iter().enumerate() {
            dest = format!("{dest}\\{p}");
            // an item: where it is on disk; below it, its folders
            disk = if i == 0 && self.base.is_none() { self.roots.get(p).cloned() } else { disk.map(|d| d.join(p)) };
            make(v, &dest, disk.as_deref())?;
        }
        Ok(dest)
    }

    /// Write what is left onto the tape in `v`, until done, full or
    /// cancelled. The index is written at the end, every [`COMMIT_EVERY`],
    /// and before an error is passed on; the files an index made safe get
    /// their Archive attribute cleared then.
    fn onto(&mut self, a: &Archive, v: &mut Volume, ui: &dyn Ui) -> Result<Ended> {
        let job = self.opts.job();
        // a backup only adds its own folder to the tape's newest index: a
        // catalog behind the tape (another program wrote to it, or a job
        // was cut short) is brought up to date, not a reason to stop
        if let Err(e) = v.writable() {
            log!("{job}: {} can't be written: {e}", v.barcode);
            return Err(e.into());
        }
        a.sync(v)?;
        let barcode = v.barcode.clone();
        // the folder already there, from another backup: not mixed into it
        // (unless asked); this backup's own, from a tape it came back to, is
        if !self.opts.replace && !self.report.tapes.iter().any(|t| t.barcode == barcode) && v.index.find(&self.report.set).is_some() {
            log!("{job}: {barcode} already holds \"{}\": not written", self.report.set);
            return Err(Error::Refused(format!("tape {barcode} already holds the folder \"{}\" of another backup; its files aren't replaced. Choose another name", self.report.set)));
        }
        let dp = ltfs::volume::part_number(v.label.data_partition) as u8;
        let capacity = v.drive().capacity(dp).ok().and_then(|(_, c)| c);
        let started_empty = v.index.totals().0 == 0;
        let mut on_tape = TapeDone { barcode: barcode.clone(), ..TapeDone::default() };
        // the files written onto this tape, to read back: (path on the tape,
        // on disk, modification time when read)
        let mut to_check: Vec<(String, PathBuf, Option<std::time::SystemTime>)> = Vec::new();
        let mut mark = self.mark(&on_tape);
        // written since the last index: (file, its modification time when read)
        let mut unsaved: Vec<Unsaved> = Vec::new();
        let mut unsaved_bytes = 0u64;
        let mut ended = Ended::Done;
        let mut failed = None;
        // the files ahead (not those too large for a tape), read from disk
        // while the tape writes
        let ahead = |run: &Run| -> Vec<(usize, PathBuf)> {
            run.plan[run.next..]
                .iter()
                .enumerate()
                .filter(|(_, e)| e.size.is_some_and(|s| !capacity.is_some_and(|c| s > c)))
                .map(|(i, e)| (run.next + i, e.disk.clone()))
                .collect()
        };
        let mut prefetch = Prefetch::start(ahead(self), self.opts.read_ahead);
        while self.next < self.plan.len() {
            if ui.cancelled() {
                ended = Ended::Cancelled;
                break;
            }
            let (disk, rel, size) = {
                let e = &self.plan[self.next];
                (e.disk.clone(), e.rel.clone(), e.size)
            };
            let Some(size) = size else {
                if let Err(e) = self.ensure_dir(v, &rel) {
                    failed = Some(e);
                    break;
                }
                // its Archive attribute is cleared with the files', once an
                // index holds it (a drive's root is left alone)
                if disk.parent().is_some() {
                    unsaved.push((disk.clone(), fs::metadata(&disk).and_then(|m| m.modified()).ok(), None));
                }
                self.next += 1;
                continue;
            };
            // larger than a whole tape: it can't go anywhere
            if capacity.is_some_and(|c| size > c) {
                log!(
                    "{job}: \"{rel}\" ({}) is larger than the tape ({}): left out",
                    lto_scsi::log::bytes(size),
                    lto_scsi::log::or_unknown(capacity.map(lto_scsi::log::bytes))
                );
                self.report.too_big.push(disk);
                self.next += 1;
                continue;
            }
            let (dir, name) = parent_and_name(&rel);
            let dest_dir = match self.ensure_dir(v, &dir) {
                Ok(d) => d,
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            };
            ui.step(Step::Backup { tape: &barcode, files: (self.done_files, self.report.total_files), bytes: (self.done_bytes, self.report.total_bytes), speed: speed(self.written, self.writing) });
            // the file read ahead (never out of step; if so, read again from here)
            let opened = match prefetch.next_file() {
                Some((i, m)) if i == self.next => m,
                other => {
                    log!("{job}: read ahead out of step at file {} (got {:?}): started again", self.next, other.map(|(i, _)| i));
                    prefetch = Prefetch::start(ahead(self), self.opts.read_ahead);
                    match prefetch.next_file() {
                        Some((i, m)) if i == self.next => m,
                        _ => Err(io::Error::other("the file wasn't read")),
                    }
                }
            };
            let meta = match opened {
                Ok(m) => m,
                Err(e) => {
                    log!("{job}: \"{}\" can't be opened: {e}", disk.display());
                    self.report.unreadable.push((disk, e.to_string()));
                    self.next += 1;
                    continue;
                }
            };
            let now = Time::now();
            let mut this_file = 0u64;
            let base = self.done_bytes;
            let (files, total_bytes, written, writing) = ((self.done_files, self.report.total_files), self.report.total_bytes, self.written, self.writing);
            let began = std::time::Instant::now();
            let mut shown = began;
            let mut f = prefetch.reader();
            let mut hashing = crate::digest::Hashing::new(&mut f);
            let r = v.write_data(&mut hashing, &mut |n| {
                this_file += n;
                // a large file: the status moves on every second
                if shown.elapsed() >= std::time::Duration::from_secs(1) {
                    shown = std::time::Instant::now();
                    ui.step(Step::Backup { tape: &barcode, files, bytes: (base + this_file, total_bytes), speed: speed(written + this_file, writing + began.elapsed()) });
                }
                !ui.cancelled()
            });
            self.written += this_file;
            self.writing += began.elapsed();
            match r {
                Ok((extents, length)) => {
                    log!("{job}: \"{rel}\" {length} bytes at {}", extents.first().map(|e| format!("{}:{}", e.partition, e.start_block)).unwrap_or_else(|| "-".into()));
                    let sum = hashing.attribute(length);
                    let file = File {
                        name,
                        length,
                        readonly: meta.permissions().readonly(),
                        open_for_write: false,
                        times: times_of(&meta, now),
                        uid: v.index.next_uid(),
                        xattrs: sum.into_iter().collect(),
                        extents,
                        symlink: None,
                        extra: Vec::new(),
                    };
                    if let Err(e) = v.index.put(&dest_dir, Node::File(file), now) {
                        failed = Some(Error::Refused(e));
                        break;
                    }
                    self.done_files += 1;
                    self.done_bytes = base + length;
                    on_tape.files += 1;
                    on_tape.bytes += length;
                    if self.opts.verify {
                        to_check.push((format!("{dest_dir}\\{}", parent_and_name(&rel).1), disk.clone(), meta.modified().ok()));
                    }
                    unsaved.push((disk, meta.modified().ok(), Some(length)));
                    unsaved_bytes += length;
                    self.next += 1;
                    if unsaved_bytes >= commit_every() {
                        log!("{job}: {} since the last index: an interim index ({})", lto_scsi::log::bytes(unsaved_bytes), self.opts.interim_index.key());
                        ui.step(Step::WritingIndex(&barcode));
                        let written = match self.opts.interim_index {
                            InterimIndex::DataPartition => a.sync_index(v),
                            InterimIndex::BothPartitions => a.commit(v),
                        };
                        if let Err(e) = written {
                            failed = Some(e);
                            break;
                        }
                        self.cleared(&mut unsaved);
                        unsaved_bytes = 0;
                        mark = self.mark(&on_tape);
                    }
                }
                // early warning: the part written isn't in the index; the
                // whole file goes onto the next tape
                Err(ltfs::Error::Full) => {
                    if started_empty && on_tape.files == 0 {
                        // it doesn't fit even on an empty tape
                        self.report.too_big.push(disk);
                        self.next += 1;
                    }
                    ended = Ended::Full;
                    break;
                }
                Err(ltfs::Error::Cancelled) => {
                    ended = Ended::Cancelled;
                    break;
                }
                // the file on disk couldn't be read to the end: left out
                Err(ltfs::Error::Io(e)) => {
                    log!("{job}: \"{}\" couldn't be read to the end: {e}", disk.display());
                    self.report.unreadable.push((disk, e.to_string()));
                    self.next += 1;
                }
                Err(e) => {
                    failed = Some(e.into());
                    break;
                }
            }
        }
        // what was written gets an index, whatever stopped it
        log!("{job}: {barcode} stops ({ended:?}, error: {}): its index", failed.as_ref().map_or_else(|| "none".to_owned(), |e| e.to_string()));
        ui.step(Step::WritingIndex(&barcode));
        let committed = a.commit(v);
        match &committed {
            Ok(()) => self.cleared(&mut unsaved),
            Err(e) => {
                log!("{job}: {barcode}: its index can't be written: {e}");
                self.rewind(mark, &mut on_tape);
            }
        }
        if on_tape.files > 0 {
            match self.report.tapes.iter_mut().find(|t| t.barcode == on_tape.barcode) {
                Some(t) => {
                    t.files += on_tape.files;
                    t.bytes += on_tape.bytes;
                }
                None => self.report.tapes.push(on_tape),
            }
        }
        match (failed, committed) {
            (Some(e), _) => Err(e),
            (None, Err(e)) => Err(e),
            // read back what was written, before the tape goes: once a
            // backup was cancelled, not
            (None, Ok(())) if ended != Ended::Cancelled && !to_check.is_empty() => match self.check(v, &to_check, ui) {
                Ok(()) => Ok(ended),
                Err(Error::Cancelled) => Ok(Ended::Cancelled),
                Err(e) => Err(e),
            },
            (None, Ok(())) => Ok(ended),
        }
    }

    /// Read the files `files` (path on the tape, on disk, modification time
    /// when written) back from the tape in `v`, in the order they are on
    /// it, and compare each with the file on disk: found the same, it is
    /// [`BackupReport::verified`]; else [`BackupReport::verify_failed`].
    /// A drive's or connection's fault stops it.
    fn check(&mut self, v: &mut Volume, files: &[(String, PathBuf, Option<std::time::SystemTime>)], ui: &dyn Ui) -> Result<()> {
        use std::io::Read;
        let barcode = v.barcode.clone();
        // the files are looked for in the index read again from the tape,
        // as a later mount finds them: not in the one in memory
        let fresh = match v.index_on_tape() {
            Ok(ix) if ix.generation == v.index.generation => ix,
            Ok(ix) => {
                let why = format!("the tape's index read again is generation {}, not the one written ({})", ix.generation, v.index.generation);
                log!("check {barcode}: {why}");
                self.report.verify_failed.extend(files.iter().map(|(_, d, _)| (d.clone(), why.clone())));
                return Ok(());
            }
            Err(ltfs::Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => {
                let why = format!("the tape's index can't be read again: {e}");
                log!("check {barcode}: {why}");
                self.report.verify_failed.extend(files.iter().map(|(_, d, _)| (d.clone(), why.clone())));
                return Ok(());
            }
        };
        let mut on_tape: BTreeMap<String, File> = BTreeMap::new();
        let mut disk_of: BTreeMap<String, (PathBuf, Option<std::time::SystemTime>)> = BTreeMap::new();
        for (path, disk, modified) in files {
            match fresh.find(path) {
                Some(Node::File(f)) => {
                    on_tape.insert(path.clone(), f.clone());
                    disk_of.insert(path.clone(), (disk.clone(), *modified));
                }
                _ => self.report.verify_failed.push((disk.clone(), "not in the tape's index".into())),
            }
        }
        let total_files = on_tape.len() as u64;
        let total_bytes: u64 = on_tape.values().map(|f| f.length).sum();
        log!("check {barcode}: reading back {total_files} files, {}", lto_scsi::log::bytes(total_bytes));
        let (mut done_files, mut done_bytes) = (0u64, 0u64);
        let mut reading = Duration::ZERO;
        for (path, f) in in_tape_order(on_tape) {
            if ui.cancelled() {
                return Err(Error::Cancelled);
            }
            let (disk, modified) = disk_of.remove(&path).expect("listed");
            ui.step(Step::Verifying { tape: &barcode, files: (done_files, total_files), bytes: (done_bytes, total_bytes), speed: speed(done_bytes, reading) });
            let mut differs: Option<String> = None;
            let mut on_disk = fs::File::open(&disk).map(|f| io::BufReader::with_capacity(1 << 20, f));
            if let Err(e) = &on_disk {
                differs = Some(format!("the file on disk can't be opened: {e}"));
            }
            // the file compared: an archive deletes only this one
            let id = on_disk.as_ref().ok().and_then(|d| file_id(d.get_ref()));
            let mut buf = Vec::new();
            let mut read = 0u64;
            let began = Instant::now();
            let mut shown = began;
            let r = match &mut on_disk {
                Err(_) => Ok(()),
                Ok(d) => v.read_file(&f, &mut |b| {
                    read += b.len() as u64;
                    buf.resize(b.len(), 0);
                    match d.read_exact(&mut buf) {
                        Ok(()) if buf == b => {}
                        Ok(()) => {
                            differs = Some(format!("differs from the file on disk at byte {}", read - b.len() as u64));
                            return Ok(false);
                        }
                        Err(e) => {
                            differs = Some(format!("the file on disk can't be read there (at byte {}): {e}", read - b.len() as u64));
                            return Ok(false);
                        }
                    }
                    if shown.elapsed() >= Duration::from_secs(1) {
                        shown = Instant::now();
                        let speed = speed(done_bytes + read, reading + began.elapsed());
                        ui.step(Step::Verifying { tape: &barcode, files: (done_files, total_files), bytes: (done_bytes + read, total_bytes), speed });
                    }
                    Ok(!ui.cancelled())
                }),
            };
            reading += began.elapsed();
            done_bytes += read;
            done_files += 1;
            let why = match r {
                Ok(()) => differs.or_else(|| {
                    // the file on disk longer, or changed since it was written
                    let longer = on_disk.as_mut().ok().is_some_and(|d| d.read(&mut [0u8; 1]).is_ok_and(|n| n > 0));
                    let now = fs::metadata(&disk).ok().and_then(|m| m.modified().ok());
                    if longer {
                        Some("the file on disk is longer".into())
                    } else if now != modified {
                        Some("the file on disk changed since it was written".into())
                    } else {
                        None
                    }
                }),
                Err(_) if differs.is_some() => differs,
                Err(ltfs::Error::Cancelled) => return Err(Error::Cancelled),
                Err(e @ ltfs::Error::Corrupt(_)) => Some(e.to_string()),
                Err(ltfs::Error::Scsi(e)) if e.is_medium() => Some(e.to_string()),
                // the drive or its connection: no use going on
                Err(e) => return Err(e.into()),
            };
            match why {
                None => {
                    log!("check {barcode}: \"{path}\" {read} bytes, the same as on disk");
                    // its Archive attribute is cleared only now, once it is
                    // known to be on the tape
                    if self.opts.clear_archive {
                        let same = fs::metadata(&disk).ok().and_then(|m| m.modified().ok()) == modified;
                        if !same || clear_archive(&disk).is_err() {
                            self.report.not_cleared.push(disk.clone());
                        }
                    }
                    self.report.verified.push(FileDone { path: disk, length: f.length, modified, id });
                }
                Some(why) => {
                    log!("check {barcode}: \"{path}\" FAILED: {why}");
                    self.report.verify_failed.push((disk, why));
                }
            }
        }
        log!("check {barcode}: {} read in {:.1} s: {}/s on average", lto_scsi::log::bytes(done_bytes), reading.as_secs_f64(), lto_scsi::log::or_unknown(speed(done_bytes, reading).map(lto_scsi::log::bytes)));
        Ok(())
    }

    /// The files of an index just written: on the tapes, and their Archive
    /// attribute cleared (when asked), unless they changed while they were
    /// written.
    fn cleared(&mut self, files: &mut Vec<Unsaved>) {
        for (path, modified, length) in files.drain(..) {
            if let Some(length) = length {
                self.report.on_tapes.push(FileDone { path: path.clone(), length, modified, id: None });
            }
            // with the check, a file's attribute is cleared once it was read
            // back the same ([`Self::check`]), not before
            if !self.opts.clear_archive || (self.opts.verify && length.is_some()) {
                continue;
            }
            let same = fs::metadata(&path).ok().and_then(|m| m.modified().ok()) == modified;
            if !same || clear_archive(&path).is_err() {
                self.report.not_cleared.push(path);
            }
        }
    }
}

impl Archive {
    /// Back up the folder `src` into the folder `set` (see
    /// [`backup_set_name`]) on the tapes `tapes` (in barcode order; each an
    /// LTFS volume the catalog knows), then on new tapes asked for through
    /// [`Ui::new_tape`]. A file goes onto one tape whole; when a tape is full
    /// it goes onto the next. A file larger than a whole tape, or one that
    /// can't be read, is left out and reported.
    pub fn backup(&self, src: &Path, set: &str, tapes: &[String], opts: BackupOptions, ui: &dyn Ui) -> Result<BackupReport> {
        if !fs::metadata(src)?.is_dir() {
            return Err(Error::Refused(format!("\"{}\" isn't a folder", src.display())));
        }
        self.backup_from(Some(src), &[], set, tapes, opts, ui)
    }

    /// As [`Self::backup`], of the items `items` (files, and folders with
    /// everything in them, from any folders): on the tapes each is in the
    /// folder `set` under its name.
    pub fn backup_items(&self, items: &[PathBuf], set: &str, tapes: &[String], opts: BackupOptions, ui: &dyn Ui) -> Result<BackupReport> {
        if items.is_empty() {
            return Err(Error::Refused("nothing to back up".into()));
        }
        self.backup_from(None, items, set, tapes, opts, ui)
    }

    /// [`Self::backup`] of the folder `base`, or [`Self::backup_items`].
    fn backup_from(&self, base: Option<&Path>, items: &[PathBuf], set: &str, tapes: &[String], mut opts: BackupOptions, ui: &dyn Ui) -> Result<BackupReport> {
        let job = opts.job();
        // an archive deletes only what was read back the same
        if opts.delete != DeleteSource::Keep && !opts.verify {
            log!("{job}: files are deleted only once read back from the tapes: the check is on");
            opts.verify = true;
        }
        // the tapes ticked: LTFS volumes, in barcode order
        let ticked: BTreeSet<String> = tapes.iter().map(|b| b.trim().to_uppercase()).filter(|b| !b.is_empty()).collect();
        for b in &ticked {
            let t = self.catalog.lock().unwrap().tape(b)?;
            if !t.as_ref().is_some_and(|t| t.is_ltfs()) {
                return Err(Error::NotLtfs(b.clone()));
            }
        }
        let mut report = BackupReport { set: set.to_owned(), checked: opts.verify, ..BackupReport::default() };
        let mut plan = Vec::new();
        let mut roots = BTreeMap::new();
        match base {
            Some(src) => {
                plan.push(Entry { disk: src.to_path_buf(), rel: String::new(), size: None });
                scan(src, "", opts.archive_only, &mut plan, &mut report.unreadable);
                report.keep_folders = vec![src.to_path_buf()];
            }
            None => {
                scan_items(items, opts.archive_only, &mut plan, &mut report.unreadable, &mut roots);
                let mut parents: Vec<PathBuf> = items.iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect();
                parents.sort();
                parents.dedup();
                report.keep_folders = parents;
            }
        }
        // the items' one folder gives the set folder its times
        let set_disk = match (base, report.keep_folders.as_slice()) {
            (None, [one]) => Some(one.clone()),
            _ => None,
        };
        report.folders = plan.iter().filter(|e| e.size.is_none() && !e.rel.is_empty()).map(|e| e.disk.clone()).collect();
        report.total_files = plan.iter().filter(|e| e.size.is_some()).count() as u64;
        report.total_bytes = plan.iter().filter_map(|e| e.size).sum();
        report.total_folders = plan.iter().filter(|e| e.size.is_none() && !e.rel.is_empty()).count() as u64;
        // archive-only and nothing changed: nothing to write (no empty set folder)
        if report.total_files == 0 && report.total_folders == 0 && opts.archive_only {
            return Ok(report);
        }
        // a tape ticked already holding the folder, from another backup:
        // refused before anything is written (checked on the tape again)
        if !opts.replace {
            let cat = self.catalog.lock().unwrap();
            for b in &ticked {
                if cat.entry(b, set)?.is_some() {
                    return Err(Error::Refused(format!("tape {b} already holds the folder \"{set}\" of another backup; its files aren't replaced. Choose another name")));
                }
            }
        }
        log!(
            "job: {job} {} into \"{set}\", tapes ticked: {}; {}, Archive attribute {}, interim index: {}, read ahead: {}, check after writing: {}, delete from disk: {:?}; {} files, {}, {} folders, {} unreadable",
            match base {
                Some(src) => format!("\"{}\"", src.display()),
                None => format!("the items {}", lto_scsi::log::list(roots.values().map(|p| format!("\"{}\"", p.display())))),
            },
            lto_scsi::log::list(ticked.iter()),
            if opts.archive_only { "only files with the Archive attribute" } else { "all files" },
            if opts.clear_archive { "cleared" } else { "kept" },
            opts.interim_index.key(),
            lto_scsi::log::bytes(opts.read_ahead),
            opts.verify,
            opts.delete,
            report.total_files,
            lto_scsi::log::bytes(report.total_bytes),
            plan.iter().filter(|e| e.size.is_none()).count(),
            report.unreadable.len()
        );
        let mut run = Run { base: base.map(Path::to_path_buf), roots, set_disk, opts, plan, next: 0, done_files: 0, done_bytes: 0, written: 0, writing: std::time::Duration::ZERO, report };
        // whatever stops it, the backup ends with its report
        match self.onto_tapes(&mut run, &ticked, ui) {
            Ok(()) => {}
            Err(Error::Cancelled) => run.report.cancelled = true,
            Err(e) => {
                log!("{job}: stopped: {e}");
                run.report.error = Some(e.to_string());
            }
        }
        let (files, bytes) = run.report.written();
        log!("{job}: {} sent to tapes in {:.1} s of writing: {}/s on average", lto_scsi::log::bytes(run.written), run.writing.as_secs_f64(), lto_scsi::log::or_unknown(speed(run.written, run.writing).map(lto_scsi::log::bytes)));
        log!(
            "{job} done: {files} files, {} on {}; too big {}, unreadable {}, attribute left {}, cancelled {}, tapes failed: {}, stopped by: {}",
            lto_scsi::log::bytes(bytes),
            lto_scsi::log::list(run.report.tapes.iter().map(|t| &t.barcode)),
            run.report.too_big.len(),
            run.report.unreadable.len(),
            run.report.not_cleared.len(),
            run.report.cancelled,
            lto_scsi::log::list(run.report.failed_tapes.iter().map(|(b, _)| b)),
            run.report.error.as_deref().unwrap_or("nothing")
        );
        if opts.verify {
            log!("{job}: {} files the same on the tapes as on disk, {} not", run.report.verified.len(), run.report.verify_failed.len());
        }
        // an archive: what is safe on the tapes leaves the disk
        // (not once the person cancelled: nothing is deleted then)
        let (n, b) = run.report.deletable_size();
        let deleting = opts.delete != DeleteSource::Keep && n > 0 && !run.report.cancelled;
        let keep = deleting && opts.delete == DeleteSource::Ask && !ui.confirm_delete(n, b, run.report.verify_failed.len() as u64);
        if keep {
            log!("{job}: the person keeps the files on disk");
        }
        if deleting && !keep {
            let total = n;
            let deleted = delete_source(&run.report, &mut |n, _| {
                if n % 100 == 0 {
                    ui.step(Step::Deleting { files: (n, total) });
                }
                !ui.cancelled()
            });
            run.report.deleted = Some(deleted);
        }
        lto_scsi::log::flush_counts();
        Ok(run.report)
    }

    /// Write the backup onto the tapes ticked, in barcode order, then onto
    /// new ones asked for. A tape that fails is left: the person is asked
    /// whether to go on with another.
    fn onto_tapes(&self, run: &mut Run, ticked: &BTreeSet<String>, ui: &dyn Ui) -> Result<()> {
        let job = run.opts.job();
        let mut queue: Vec<String> = ticked.iter().cloned().collect();
        queue.reverse();
        // tapes of this run, and tapes refused for it
        let mut seen: Vec<String> = ticked.iter().cloned().collect();
        while run.next < run.plan.len() {
            if ui.cancelled() {
                return Err(Error::Cancelled);
            }
            let barcode = match queue.pop() {
                Some(b) => b,
                None => {
                    let left = run.report.total_bytes.saturating_sub(run.done_bytes);
                    match self.new_backup_tape(job, &mut seen, left, ui)? {
                        Some(b) => b,
                        None => return Err(Error::Cancelled),
                    }
                }
            };
            log!("{job}: onto {barcode}, from file {} of {}", run.done_files, run.report.total_files);
            let r = self.with_volume(&barcode, ui, |v| run.onto(self, v, ui));
            log!("{job}: {barcode} ended: {}", match &r { Ok(e) => format!("{e:?}"), Err(e) => format!("error: {e}") });
            match r {
                Ok(Ended::Done | Ended::Full) => {}
                Ok(Ended::Cancelled) | Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => {
                    // a tape that failed: on with another, if the person wants
                    run.report.failed_tapes.push((barcode.clone(), e.to_string()));
                    if run.next >= run.plan.len() {
                        return Err(e);
                    }
                    let left = run.report.total_bytes.saturating_sub(run.done_bytes);
                    let answer = ui.tape_failed(&barcode, &e.to_string(), left);
                    log!("{job}: {barcode} failed; asked whether to go on with another tape: {answer:?}");
                    if answer != Answer::Retry {
                        return Err(e);
                    }
                }
            }
        }
        Ok(())
    }

    /// A new tape for a backup whose tapes are full: asked for, looked for
    /// in the mail slots and standalone drives (one not in `seen`), and
    /// checked: an empty LTFS volume is used, a tape with files refused, a
    /// blank one (or one without LTFS) formatted if the person confirms.
    /// `None`: the person stopped the backup.
    fn new_backup_tape(&self, job: &str, seen: &mut Vec<String>, left: u64, ui: &dyn Ui) -> Result<Option<String>> {
        let mut problem: Option<String> = None;
        let mut nothing_new = false;
        loop {
            let site = self.site()?;
            let places = site.places();
            let p = match (&problem, nothing_new) {
                (Some(b), _) => Some(NewTapeProblem::HasData(b)),
                (None, true) => Some(NewTapeProblem::NothingNew),
                (None, false) => None,
            };
            if ui.new_tape(left, &places, p) != Answer::Retry {
                return Ok(None);
            }
            problem = None;
            nothing_new = false;
            let Some(b) = self.look_for_new_tape(&site, seen, ui)? else {
                nothing_new = true;
                continue;
            };
            seen.push(b.clone());
            let known = self.catalog.lock().unwrap().tape(&b)?;
            match &known {
                Some(t) => log!("{job}: new tape {b}: in the catalog volume {}, generation {}, {} files", if t.uuid.is_empty() { "(none)" } else { t.uuid.as_str() }, t.generation, t.files),
                None => log!("{job}: new tape {b}: not in the catalog"),
            }
            let files = match known.filter(|t| t.is_ltfs()) {
                // files on it in the catalog: refused without loading it
                Some(t) if t.files > 0 => Some(t.files),
                // empty in the catalog, or not LTFS there: what is really
                // on it? (the catalog may be behind the tape)
                _ => match self.label(&b, ui)? {
                    Some(_) => Some(self.import(&b, ui)?.map(|ix| ix.totals().0).unwrap_or(0)),
                    None => None,
                },
            };
            log!("{job}: new tape {b}: {}", files.map_or_else(|| "no LTFS volume on it".to_owned(), |n| format!("{n} files on it")));
            match files {
                Some(0) => return Ok(Some(b)),
                Some(_) => problem = Some(b),
                None => match ui.confirm_format(&b) {
                    Answer::Retry => {
                        self.format_checked(&b, &b, ui, true)?;
                        return Ok(Some(b));
                    }
                    Answer::Skip => {}
                    Answer::Cancel => return Ok(None),
                },
            }
        }
    }

    /// A tape in a mail slot or standalone drive that isn't in `seen` (nor
    /// a cleaning cartridge), looked for every few seconds for up to
    /// [`RETRY_WAIT`] (longer while a library still reads its mail slot).
    fn look_for_new_tape(&self, site: &Site, seen: &[String], ui: &dyn Ui) -> Result<Option<String>> {
        let start = Instant::now();
        loop {
            let busy = match site.refresh(false, &|d| self.identify(d, None)) {
                Ok(()) => false,
                Err(e) if e.sense().is_some_and(|s| s.key == lto_scsi::sense::key::NOT_READY) => true,
                Err(e) => return Err(e.into()),
            };
            let _ = self.record_locations(site);
            let new = site.offered().into_iter().find(|b| !b.to_uppercase().starts_with("CLN") && !seen.iter().any(|s| s.eq_ignore_ascii_case(b)));
            if new.is_some() {
                return Ok(new);
            }
            let waited = start.elapsed();
            if waited >= SETTLE_LIMIT || (waited >= RETRY_WAIT && !busy && !site.settling()) {
                return Ok(None);
            }
            ui.step(Step::NewTape);
            let pause = Instant::now();
            while pause.elapsed() < SETTLE_PAUSE {
                if ui.cancelled() {
                    return Err(Error::Cancelled);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}
