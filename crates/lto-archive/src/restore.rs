//! Files read from tapes, written into a folder on disk without harming
//! what is there:
//!
//! - every path stays inside the folder: names that would climb out of it
//!   (`..`, a drive), or that Windows takes for a device (`CON`), are
//!   refused, and so is any link or junction on the way (a folder that is a
//!   link elsewhere would carry the files out of it). On Windows the folder
//!   is resolved once to where it really is, and it and every folder used
//!   under it are held open while the copy runs, checked on that handle:
//!   held so, none can be renamed, deleted or swapped for a junction
//!   between the check and the files written into it;
//! - each file is written to a new file of its own next to where it goes,
//!   and takes its name only once the tape gave all of it: a tape, disk or
//!   Cancel stopping it halfway leaves the file that was there as it was,
//!   and the partial one is removed;
//! - an existing file is replaced only when asked to ([`Overwrite`]).

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ltfs::Time;

use crate::ReadSink;

/// What to do when a file read from a tape is already on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overwrite {
    /// Stop with an error; the file on disk stays.
    Never,
    /// Replace it, once the new one is whole.
    Replace,
}

/// A file being written: the temporary file, and where it goes.
struct Partial {
    file: fs::File,
    tmp: PathBuf,
    target: PathBuf,
    size: u64,
    written: u64,
    modified: Time,
}

/// Writes files read from tapes under a folder (`root\BARCODE\path`).
pub struct ToFolder {
    root: PathBuf,
    overwrite: Overwrite,
    /// The folders checked (or made) under the root: no link among them.
    checked: BTreeSet<PathBuf>,
    /// The root and those folders, held open so they stay what was checked.
    held: Vec<Held>,
    file: Option<Partial>,
}

/// A folder held open: on Windows, without letting anyone delete or
/// rename it (or so replace it) while the handle is open.
#[allow(dead_code)]
struct Held(fs::File);

/// Open the folder `dir` and hold it, checking on the handle (not the
/// path) that it is a real folder, not a link, a junction or a file.
#[cfg(windows)]
fn hold(dir: &Path) -> io::Result<Held> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE};
    let f = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir)
        .map_err(|e| io::Error::new(e.kind(), format!("\"{}\" can't be held open: {e}", dir.display())))?;
    let m = f.metadata()?;
    if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid(format!("\"{}\" is a link or a junction: nothing is written through it", dir.display())));
    }
    if !m.is_dir() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("\"{}\" is a file, not a folder", dir.display())));
    }
    Ok(Held(f))
}

#[cfg(not(windows))]
fn hold(dir: &Path) -> io::Result<Held> {
    let m = fs::symlink_metadata(dir)?;
    if is_link(&m) || !m.is_dir() {
        return Err(invalid(format!("\"{}\" isn't a folder", dir.display())));
    }
    Ok(Held(fs::File::open(dir)?))
}

/// Where the folder `dir` really is (links and junctions on the way to it
/// followed once, now): what is written goes there, whatever happens to
/// the way to it later.
#[cfg(windows)]
fn real_path(dir: &Path) -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, FILE_FLAG_BACKUP_SEMANTICS};
    let f = fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(dir)?;
    let mut buf = vec![0u16; 32768];
    let n = unsafe { GetFinalPathNameByHandleW(f.as_raw_handle() as _, buf.as_mut_ptr(), buf.len() as u32, 0) } as usize;
    if n == 0 || n >= buf.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])))
}

#[cfg(not(windows))]
fn real_path(dir: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(dir)
}

/// Temporary names, unique within the process.
static NEXT: AtomicU64 = AtomicU64::new(0);

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Whether what `m` describes is a link, or a junction or another reparse
/// point (going through it may lead anywhere).
fn is_link(m: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    m.file_type().is_symlink()
}

impl ToFolder {
    /// Into the folder `root` (made if it isn't there), as it really is
    /// now: a link or junction on the way to it is followed once, here.
    pub fn new(root: &Path, overwrite: Overwrite) -> io::Result<ToFolder> {
        fs::create_dir_all(root)?;
        let real = real_path(root)?;
        let held = vec![hold(&real)?];
        Ok(ToFolder { root: real, overwrite, checked: BTreeSet::new(), held, file: None })
    }

    /// The names under the root for an LTFS path (the barcode first), in
    /// their Windows form; refused when one could lead out of the root.
    fn parts(path: &str) -> io::Result<Vec<String>> {
        let parts: Vec<String> = ltfs::index::split(path).into_iter().map(ltfs::names::to_windows).collect();
        if parts.is_empty() || !ltfs::names::safe_relative(&parts.join("\\")) {
            return Err(invalid(format!("a name on the tape can't be used on disk: {path}")));
        }
        Ok(parts)
    }

    /// The folder `parts` under the root, made where missing; each folder
    /// on the way must be a real folder, not a link or a junction.
    fn folder(&mut self, parts: &[String]) -> io::Result<PathBuf> {
        let mut at = self.root.clone();
        for p in parts {
            at.push(p);
            if self.checked.contains(&at) {
                continue;
            }
            match fs::symlink_metadata(&at) {
                Ok(m) if is_link(&m) => return Err(invalid(format!("\"{}\" is a link or a junction: nothing is written through it", at.display()))),
                Ok(m) if m.is_dir() => {}
                Ok(_) => return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("\"{}\" is a file, not a folder", at.display()))),
                Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::create_dir(&at) {
                    // made meanwhile: checked on its handle below
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                },
                Err(e) => return Err(e),
            }
            // checked again on the handle that holds it from now on
            self.held.push(hold(&at)?);
            self.checked.insert(at.clone());
        }
        Ok(at)
    }

    /// The folder `path` (an LTFS path, the barcode first).
    pub fn make_dir(&mut self, path: &str) -> io::Result<PathBuf> {
        let parts = Self::parts(path)?;
        self.folder(&parts)
    }

    /// A file `path` of `size` bytes begins: a temporary file next to where
    /// it goes. The file there (if any) is checked now, and again when the
    /// new one takes its place.
    pub fn begin_file(&mut self, path: &str, size: u64, modified: Time) -> io::Result<()> {
        self.abandon();
        let parts = Self::parts(path)?;
        let (name, dirs) = parts.split_last().expect("not empty");
        let dir = self.folder(dirs)?;
        let target = dir.join(name);
        self.may_replace(&target)?;
        let tmp = dir.join(format!("~ltocat-{}-{}.part", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let file = create_staging(&tmp)?;
        self.file = Some(Partial { file, tmp, target, size, written: 0, modified });
        Ok(())
    }

    /// Whether a file may go to `target`: nothing there, or a file to
    /// replace when [`Overwrite::Replace`]; never a folder or a link.
    fn may_replace(&self, target: &Path) -> io::Result<()> {
        match fs::symlink_metadata(target) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
            Ok(m) if is_link(&m) => Err(invalid(format!("\"{}\" is a link or a junction: it isn't replaced", target.display()))),
            Ok(m) if m.is_dir() => Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("\"{}\" is a folder", target.display()))),
            Ok(_) if self.overwrite == Overwrite::Never => Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("\"{}\" already exists; it was left as it is", target.display()))),
            Ok(_) => Ok(()),
        }
    }

    /// The next bytes of the file begun.
    pub fn write_data(&mut self, data: &[u8]) -> io::Result<()> {
        let p = self.file.as_mut().ok_or_else(|| io::Error::other("no file begun"))?;
        p.file.write_all(data)?;
        p.written += data.len() as u64;
        Ok(())
    }

    /// The file begun is whole: it takes its name. Where it went comes back.
    pub fn finish(&mut self) -> io::Result<PathBuf> {
        let Some(p) = self.file.take() else { return Err(io::Error::other("no file begun")) };
        let r = (|| {
            if p.written != p.size {
                return Err(invalid(format!("\"{}\": the tape gave {} bytes of {}", p.target.display(), p.written, p.size)));
            }
            let _ = p.file.set_modified(p.modified.to_system());
            self.may_replace(&p.target)?;
            // the very file written (no one else could open it) takes its
            // name, through its handle
            publish(&p.file, &p.tmp, &p.target, self.overwrite)
        })();
        match r {
            Ok(()) => {
                drop(p.file);
                Ok(p.target)
            }
            Err(e) => {
                discard(p.file, &p.tmp);
                Err(e)
            }
        }
    }

    /// The file begun isn't finished: its temporary file goes.
    pub fn abandon(&mut self) {
        if let Some(p) = self.file.take() {
            discard(p.file, &p.tmp);
        }
    }
}

impl Drop for ToFolder {
    fn drop(&mut self) {
        self.abandon();
    }
}

/// The temporary file a file read from a tape is written into: new, and
/// on Windows open to no one else (not to read, write, rename or delete)
/// until it has its name.
#[cfg(windows)]
fn create_staging(tmp: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES};
    fs::OpenOptions::new().write(true).create_new(true).access_mode(FILE_GENERIC_WRITE | FILE_READ_ATTRIBUTES | DELETE).share_mode(0).open(tmp)
}

#[cfg(not(windows))]
fn create_staging(tmp: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().write(true).create_new(true).open(tmp)
}

/// The temporary file `f` takes the name `target`, through its handle; an
/// existing file is replaced only with [`Overwrite::Replace`].
#[cfg(windows)]
fn publish(f: &fs::File, _tmp: &Path, target: &Path, overwrite: Overwrite) -> io::Result<()> {
    use std::mem::{offset_of, size_of};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{FileRenameInfo, SetFileInformationByHandle, FILE_RENAME_INFO};
    let name: Vec<u16> = target.as_os_str().encode_wide().collect();
    let at = offset_of!(FILE_RENAME_INFO, FileName);
    let size = (at + (name.len() + 1) * 2).max(size_of::<FILE_RENAME_INFO>());
    // FILE_RENAME_INFO with the name after it, aligned as the struct is
    let mut buf = vec![0u64; size.div_ceil(8)];
    unsafe {
        let info = buf.as_mut_ptr() as *mut FILE_RENAME_INFO;
        (*info).Anonymous.Flags = 0;
        (*info).Anonymous.ReplaceIfExists = (overwrite == Overwrite::Replace) as _;
        (*info).RootDirectory = std::ptr::null_mut();
        (*info).FileNameLength = (name.len() * 2) as u32;
        let dst = (buf.as_mut_ptr() as *mut u8).add(at) as *mut u16;
        std::ptr::copy_nonoverlapping(name.as_ptr(), dst, name.len());
    }
    if unsafe { SetFileInformationByHandle(f.as_raw_handle() as _, FileRenameInfo, buf.as_ptr().cast(), size as u32) } == 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(e.kind(), format!("\"{}\": {e}", target.display())));
    }
    Ok(())
}

#[cfg(not(windows))]
fn publish(_f: &fs::File, tmp: &Path, target: &Path, overwrite: Overwrite) -> io::Result<()> {
    match overwrite {
        Overwrite::Replace => fs::rename(tmp, target),
        Overwrite::Never => {
            fs::hard_link(tmp, target)?;
            fs::remove_file(tmp)
        }
    }
}

/// The temporary file `f` goes: through its handle (on Windows), so only
/// it, never a file someone put at its name.
#[cfg(windows)]
fn discard(f: fs::File, _tmp: &Path) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO};
    let del = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    unsafe { SetFileInformationByHandle(f.as_raw_handle() as _, FileDispositionInfo, (&del as *const FILE_DISPOSITION_INFO).cast(), std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32) };
    drop(f);
}

#[cfg(not(windows))]
fn discard(f: fs::File, tmp: &Path) {
    drop(f);
    let _ = fs::remove_file(tmp);
}

impl ReadSink for ToFolder {
    fn dir(&mut self, path: &str, _: Time) -> io::Result<()> {
        self.make_dir(path).map(|_| ())
    }
    fn begin(&mut self, path: &str, size: u64, modified: Time) -> io::Result<bool> {
        self.begin_file(path, size, modified)?;
        Ok(true)
    }
    fn write(&mut self, data: &[u8]) -> io::Result<bool> {
        self.write_data(data)?;
        Ok(true)
    }
    fn end(&mut self) -> io::Result<()> {
        self.finish().map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lto-restore-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(s: &mut ToFolder, path: &str, data: &[u8]) -> io::Result<PathBuf> {
        s.begin_file(path, data.len() as u64, Time::now())?;
        s.write_data(data)?;
        s.finish()
    }

    fn leftovers(d: &Path) -> Vec<String> {
        fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".part")).collect()
    }

    #[test]
    fn a_file_cut_short_leaves_the_old_one() {
        let d = scratch("cut");
        let mut s = ToFolder::new(&d, Overwrite::Replace).unwrap();
        let t = put(&mut s, "T1\\a.txt", b"old").unwrap();
        // the tape stops halfway: the sink is dropped without end()
        s.begin_file("T1\\a.txt", 10, Time::now()).unwrap();
        s.write_data(b"new").unwrap();
        drop(s);
        assert_eq!(fs::read(&t).unwrap(), b"old");
        assert!(leftovers(&d.join("T1")).is_empty());
        // fewer bytes than the index says: not published
        let mut s = ToFolder::new(&d, Overwrite::Replace).unwrap();
        s.begin_file("T1\\a.txt", 10, Time::now()).unwrap();
        s.write_data(b"new").unwrap();
        assert!(s.finish().is_err());
        assert_eq!(fs::read(&t).unwrap(), b"old");
        assert!(leftovers(&d.join("T1")).is_empty());
        // whole: replaced
        put(&mut s, "T1\\a.txt", b"newer").unwrap();
        assert_eq!(fs::read(&t).unwrap(), b"newer");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn existing_files_stay_unless_replacing() {
        let d = scratch("keep");
        let mut s = ToFolder::new(&d, Overwrite::Never).unwrap();
        let t = put(&mut s, "T1\\x\\a.txt", b"first").unwrap();
        let e = put(&mut s, "T1\\x\\a.txt", b"second").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&t).unwrap(), b"first");
        assert!(leftovers(&d.join("T1").join("x")).is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn names_that_leave_the_folder_are_refused() {
        let d = scratch("names");
        let mut s = ToFolder::new(&d, Overwrite::Replace).unwrap();
        for bad in ["T1\\..\\..\\x.txt","T1\\CON", "T1\\dir\\nul.txt"] {
            assert!(s.begin_file(bad, 1, Time::now()).is_err(), "{bad}");
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn links_on_the_way_are_refused() {
        let d = scratch("links");
        let outside = scratch("links-outside");
        fs::create_dir_all(d.join("T1")).unwrap();
        let link = d.join("T1").join("sub");
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(&outside, &link).is_ok()
            || std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&link).arg(&outside).output().is_ok_and(|o| o.status.success());
        #[cfg(not(windows))]
        let made = std::os::unix::fs::symlink(&outside, &link).is_ok();
        if made {
            let mut s = ToFolder::new(&d, Overwrite::Replace).unwrap();
            assert!(put(&mut s, "T1\\sub\\a.txt", b"x").is_err());
            assert!(s.make_dir("T1\\sub\\deeper").is_err());
            assert!(fs::read_dir(&outside).unwrap().next().is_none(), "nothing written through the link");
            let _ = fs::remove_dir(&link);
        }
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&outside);
    }

    /// The temporary file is open to no one else while it is written: it
    /// can't be read, changed, renamed or deleted before it has its name.
    #[cfg(windows)]
    #[test]
    fn the_file_being_written_is_ours_alone() {
        let d = scratch("staging");
        let mut s = ToFolder::new(&d, Overwrite::Never).unwrap();
        s.begin_file("T1\\a.txt", 3, Time::now()).unwrap();
        let tmp = fs::read_dir(d.join("T1")).unwrap().map(|e| e.unwrap().path()).find(|p| p.extension().is_some_and(|x| x == "part")).unwrap();
        assert!(fs::OpenOptions::new().write(true).open(&tmp).is_err(), "no one else writes it");
        assert!(fs::File::open(&tmp).is_err(), "nor reads it");
        assert!(fs::rename(&tmp, d.join("T1").join("x.txt")).is_err(), "nor renames it");
        assert!(fs::remove_file(&tmp).is_err(), "nor deletes it");
        s.write_data(b"abc").unwrap();
        let t = s.finish().unwrap();
        assert_eq!(fs::read(&t).unwrap(), b"abc");
        assert!(!tmp.exists());
        // Never: a file there stays, the new one goes
        s.begin_file("T1\\a.txt", 3, Time::now()).unwrap_err();
        fs::write(d.join("T1").join("b.txt"), b"old").unwrap();
        let mut s2 = ToFolder::new(&d, Overwrite::Never).unwrap();
        s2.begin_file("T1\\c.txt", 3, Time::now()).unwrap();
        s2.write_data(b"new").unwrap();
        // someone puts a file at the name meanwhile: not replaced
        fs::write(d.join("T1").join("c.txt"), b"theirs").unwrap();
        assert!(s2.finish().is_err());
        assert_eq!(fs::read(d.join("T1").join("c.txt")).unwrap(), b"theirs");
        assert!(fs::read_dir(d.join("T1")).unwrap().all(|e| !e.unwrap().path().to_string_lossy().ends_with(".part")));
        drop((s, s2));
        let _ = fs::remove_dir_all(&d);
    }

    /// A folder checked is held: it can't be renamed away (and a junction
    /// put in its place) while files are written into it.
    #[cfg(windows)]
    #[test]
    fn folders_in_use_stay_put() {
        let d = scratch("held");
        let mut s = ToFolder::new(&d, Overwrite::Replace).unwrap();
        put(&mut s, "T1\\sub\\a.txt", b"one").unwrap();
        assert!(fs::rename(d.join("T1").join("sub"), d.join("T1").join("moved")).is_err(), "held");
        assert!(fs::rename(d.join("T1"), d.join("T9")).is_err(), "held");
        put(&mut s, "T1\\sub\\b.txt", b"two").unwrap();
        drop(s);
        fs::rename(d.join("T1").join("sub"), d.join("T1").join("moved")).unwrap();
        let _ = fs::remove_dir_all(&d);
    }

    /// A root reached through a junction is where the junction led when
    /// the copy began; changing the junction later changes nothing.
    #[cfg(windows)]
    #[test]
    fn the_root_is_where_it_really_is() {
        let real = scratch("root-real");
        let other = scratch("root-other");
        let base = scratch("root-link");
        let link = base.join("j");
        let mk = |to: &Path| std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&link).arg(to).output().is_ok_and(|o| o.status.success());
        if mk(&real) {
            let mut s = ToFolder::new(&link, Overwrite::Replace).unwrap();
            put(&mut s, "T1\\a.txt", b"one").unwrap();
            // the junction pointed elsewhere meanwhile
            fs::remove_dir(&link).unwrap();
            assert!(mk(&other));
            put(&mut s, "T1\\b.txt", b"two").unwrap();
            drop(s);
            assert!(real.join("T1").join("b.txt").exists());
            assert!(fs::read_dir(&other).unwrap().next().is_none(), "nothing went where the junction leads now");
            let _ = fs::remove_dir(&link);
        }
        for d in [&base, &real, &other] {
            let _ = fs::remove_dir_all(d);
        }
    }
}
