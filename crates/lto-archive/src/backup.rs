//! Backing up a folder onto tapes: everything in it, or only the files
//! whose Archive attribute is set, into a dated folder on each tape used.
//! When a tape is full the backup goes on with the next tape ticked, then
//! with new tapes asked for through a mail slot.

use std::collections::BTreeSet;

use super::*;

/// What to back up, and what to do afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupOptions {
    /// Only files whose Archive attribute is set (changed since the last
    /// backup that cleared it).
    pub archive_only: bool,
    /// Clear the Archive attribute of each file once it is on a tape (its
    /// index written), so the next archive-only backup leaves it out.
    pub clear_archive: bool,
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
    /// Files and bytes found to back up.
    pub total_files: u64,
    pub total_bytes: u64,
}

impl BackupReport {
    /// Files and bytes written.
    pub fn written(&self) -> (u64, u64) {
        self.tapes.iter().fold((0, 0), |(f, b), t| (f + t.files, b + t.bytes))
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
const COMMIT_EVERY: u64 = 100 * 1024 * 1024 * 1024;

/// The folder on the tapes for a backup of `src` now:
/// `Backup YYYY-MM-DD HH.MM\<folder name>` (a drive's root: its letter).
pub fn backup_set_name(src: &Path) -> String {
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .or_else(|| src.to_string_lossy().chars().find(|c| c.is_ascii_alphabetic()).map(|c| c.to_ascii_uppercase().to_string()))
        .unwrap_or_else(|| "backup".into());
    format!("Backup {}\\{name}", local_stamp())
}

/// The local date and time, `YYYY-MM-DD HH.MM` (dots: `:` can't be in a name).
#[cfg(windows)]
fn local_stamp() -> String {
    let mut t = unsafe { std::mem::zeroed::<windows_sys::Win32::Foundation::SYSTEMTIME>() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    format!("{:04}-{:02}-{:02} {:02}.{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute)
}

/// Elsewhere: UTC.
#[cfg(not(windows))]
fn local_stamp() -> String {
    let s = Time::now().to_string();
    format!("{} {}", &s[..10], s[11..16].replace(':', "."))
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
            // with archive-only, a folder is made only where a file goes
            if !archive_only {
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
struct Run<'a> {
    src: &'a Path,
    opts: BackupOptions,
    plan: Vec<Entry>,
    /// The next entry of `plan` to write.
    next: usize,
    done_files: u64,
    done_bytes: u64,
    report: BackupReport,
}

impl Run<'_> {
    /// The folder `rel` (under the folder backed up) on the tape, made with
    /// the folders above it, each with the times of the folder on disk.
    fn ensure_dir(&self, v: &mut Volume, rel: &str) -> Result<String> {
        let now = Time::now();
        let mut dest = self.report.set.clone();
        let mut disk = self.src.to_path_buf();
        let make = |v: &mut Volume, dest: &str, disk: &Path| -> Result<()> {
            if v.index.find(dest).is_none() {
                let t = fs::metadata(disk).ok().map(|m| times_of(&m, now)).unwrap_or(Times::all(now));
                v.index.make_dirs(dest, now).map_err(Error::Refused)?.times = t;
            }
            Ok(())
        };
        make(v, &dest, &disk)?;
        for p in ltfs::index::split(rel) {
            dest = format!("{dest}\\{p}");
            disk.push(p);
            make(v, &dest, &disk)?;
        }
        Ok(dest)
    }

    /// Write what is left onto the tape in `v`, until done, full or
    /// cancelled. The index is written at the end, every [`COMMIT_EVERY`],
    /// and before an error is passed on; the files an index made safe get
    /// their Archive attribute cleared then.
    fn onto(&mut self, a: &Archive, v: &mut Volume, ui: &dyn Ui) -> Result<Ended> {
        a.ready_to_change(v)?;
        let barcode = v.barcode.clone();
        let dp = ltfs::volume::part_number(v.label.data_partition) as u8;
        let capacity = v.drive().capacity(dp).ok().and_then(|(_, c)| c);
        let started_empty = v.index.totals().0 == 0;
        let mut on_tape = TapeDone { barcode: barcode.clone(), ..TapeDone::default() };
        // written since the last index: (file, its modification time when read)
        let mut unsaved: Vec<(PathBuf, Option<std::time::SystemTime>)> = Vec::new();
        let mut unsaved_bytes = 0u64;
        let mut ended = Ended::Done;
        let mut failed = None;
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
                self.next += 1;
                continue;
            };
            // larger than a whole tape: it can't go anywhere
            if capacity.is_some_and(|c| size > c) {
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
            ui.step(Step::Backup { tape: &barcode, files: (self.done_files, self.report.total_files), bytes: (self.done_bytes, self.report.total_bytes) });
            let opened = fs::File::open(&disk).and_then(|f| f.metadata().map(|m| (f, m)));
            let (mut f, meta) = match opened {
                Ok(x) => x,
                Err(e) => {
                    self.report.unreadable.push((disk, e.to_string()));
                    self.next += 1;
                    continue;
                }
            };
            let now = Time::now();
            let mut this_file = 0u64;
            let base = self.done_bytes;
            let r = v.write_data(&mut f, &mut |n| {
                this_file += n;
                !ui.cancelled()
            });
            match r {
                Ok((extents, length)) => {
                    let file = File {
                        name,
                        length,
                        readonly: meta.permissions().readonly(),
                        open_for_write: false,
                        times: times_of(&meta, now),
                        uid: v.index.next_uid(),
                        xattrs: Vec::new(),
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
                    unsaved.push((disk, meta.modified().ok()));
                    unsaved_bytes += length;
                    self.next += 1;
                    if unsaved_bytes >= COMMIT_EVERY {
                        ui.step(Step::WritingIndex(&barcode));
                        if let Err(e) = a.commit(v) {
                            failed = Some(e);
                            break;
                        }
                        self.cleared(&mut unsaved);
                        unsaved_bytes = 0;
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
        ui.step(Step::WritingIndex(&barcode));
        let committed = a.commit(v);
        if committed.is_ok() {
            self.cleared(&mut unsaved);
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
            (None, Ok(())) => Ok(ended),
        }
    }

    /// The files of an index just written: their Archive attribute cleared
    /// (when asked), unless they changed while they were written.
    fn cleared(&mut self, files: &mut Vec<(PathBuf, Option<std::time::SystemTime>)>) {
        for (path, modified) in files.drain(..) {
            if !self.opts.clear_archive {
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
        // the tapes ticked: LTFS volumes, in barcode order
        let ticked: BTreeSet<String> = tapes.iter().map(|b| b.trim().to_uppercase()).filter(|b| !b.is_empty()).collect();
        for b in &ticked {
            let t = self.catalog.lock().unwrap().tape(b)?;
            if !t.as_ref().is_some_and(|t| t.is_ltfs()) {
                return Err(Error::NotLtfs(b.clone()));
            }
        }
        let mut report = BackupReport { set: set.to_owned(), ..BackupReport::default() };
        let mut plan = vec![Entry { disk: src.to_path_buf(), rel: String::new(), size: None }];
        scan(src, "", opts.archive_only, &mut plan, &mut report.unreadable);
        report.total_files = plan.iter().filter(|e| e.size.is_some()).count() as u64;
        report.total_bytes = plan.iter().filter_map(|e| e.size).sum();
        // archive-only and nothing changed: nothing to write (no empty set folder)
        if report.total_files == 0 && opts.archive_only {
            return Ok(report);
        }
        let mut run = Run { src, opts, plan, next: 0, done_files: 0, done_bytes: 0, report };
        let mut queue: Vec<String> = ticked.iter().cloned().collect();
        queue.reverse();
        // tapes of this run, and tapes refused for it
        let mut seen: Vec<String> = ticked.iter().cloned().collect();
        while run.next < run.plan.len() {
            if ui.cancelled() {
                run.report.cancelled = true;
                break;
            }
            let barcode = match queue.pop() {
                Some(b) => b,
                None => {
                    let left = run.report.total_bytes.saturating_sub(run.done_bytes);
                    match self.new_backup_tape(&mut seen, left, ui)? {
                        Some(b) => b,
                        None => {
                            run.report.cancelled = true;
                            break;
                        }
                    }
                }
            };
            match self.with_volume(&barcode, ui, |v| run.onto(self, v, ui)) {
                Ok(Ended::Done) => {}
                Ok(Ended::Full) => {}
                Ok(Ended::Cancelled) => {
                    run.report.cancelled = true;
                    break;
                }
                Err(Error::Cancelled) => {
                    run.report.cancelled = true;
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(run.report)
    }

    /// A new tape for a backup whose tapes are full: asked for, looked for
    /// in the mail slots and standalone drives (one not in `seen`), and
    /// checked: an empty LTFS volume is used, a tape with files refused, a
    /// blank one (or one without LTFS) formatted if the person confirms.
    /// `None`: the person stopped the backup.
    fn new_backup_tape(&self, seen: &mut Vec<String>, left: u64, ui: &dyn Ui) -> Result<Option<String>> {
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
            let files = match known.filter(|t| t.is_ltfs()) {
                Some(t) => Some(t.files),
                // not in the catalog as LTFS: what is on it?
                None => match self.label(&b, ui)? {
                    Some(_) => Some(self.import(&b, ui)?.map(|ix| ix.totals().0).unwrap_or(0)),
                    None => None,
                },
            };
            match files {
                Some(0) => return Ok(Some(b)),
                Some(_) => problem = Some(b),
                None => match ui.confirm_format(&b) {
                    Answer::Retry => {
                        self.format(&b, &b, ui)?;
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
