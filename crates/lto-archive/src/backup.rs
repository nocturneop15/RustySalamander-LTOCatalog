//! Backing up a folder onto tapes: everything in it, or only the files
//! whose Archive attribute is set, into a dated folder on each tape used.
//! When a tape is full the backup goes on with the next tape ticked, then
//! with new tapes asked for through a mail slot. When a tape fails (it
//! can't be read or written), the person can go on with another one: what
//! that tape's index doesn't hold is written again there.

use std::collections::BTreeSet;

use super::*;

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
struct Run<'a> {
    src: &'a Path,
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

/// Bytes a second, once there is something to go by.
fn speed(bytes: u64, took: std::time::Duration) -> Option<u64> {
    let s = took.as_secs_f64();
    (bytes > 0 && s > 0.0).then(|| (bytes as f64 / s) as u64)
}

impl Run<'_> {
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
        log!("backup: {} files since the tape's last index go onto the next tape", self.done_files - m.done_files);
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
        // a backup only adds its own folder to the tape's newest index: a
        // catalog behind the tape (another program wrote to it, or a job
        // was cut short) is brought up to date, not a reason to stop
        if let Err(e) = v.writable() {
            log!("backup: {} can't be written: {e}", v.barcode);
            return Err(e.into());
        }
        a.sync(v)?;
        let barcode = v.barcode.clone();
        let dp = ltfs::volume::part_number(v.label.data_partition) as u8;
        let capacity = v.drive().capacity(dp).ok().and_then(|(_, c)| c);
        let started_empty = v.index.totals().0 == 0;
        let mut on_tape = TapeDone { barcode: barcode.clone(), ..TapeDone::default() };
        let mut mark = self.mark(&on_tape);
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
                // its Archive attribute is cleared with the files', once an
                // index holds it (a drive's root is left alone)
                if disk.parent().is_some() {
                    unsaved.push((disk.clone(), fs::metadata(&disk).and_then(|m| m.modified()).ok()));
                }
                self.next += 1;
                continue;
            };
            // larger than a whole tape: it can't go anywhere
            if capacity.is_some_and(|c| size > c) {
                log!(
                    "backup: \"{rel}\" ({}) is larger than the tape ({}): left out",
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
            let opened = fs::File::open(&disk).and_then(|f| f.metadata().map(|m| (f, m)));
            let (mut f, meta) = match opened {
                Ok(x) => x,
                Err(e) => {
                    log!("backup: \"{}\" can't be opened: {e}", disk.display());
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
            let r = v.write_data(&mut f, &mut |n| {
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
                    log!("backup: \"{rel}\" {length} bytes at {}", extents.first().map(|e| format!("{}:{}", e.partition, e.start_block)).unwrap_or_else(|| "-".into()));
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
                    if unsaved_bytes >= commit_every() {
                        log!("backup: {} since the last index: an interim index ({})", lto_scsi::log::bytes(unsaved_bytes), self.opts.interim_index.key());
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
                    log!("backup: \"{}\" couldn't be read to the end: {e}", disk.display());
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
        log!("backup: {barcode} stops ({ended:?}, error: {}): its index", failed.as_ref().map_or_else(|| "none".to_owned(), |e| e.to_string()));
        ui.step(Step::WritingIndex(&barcode));
        let committed = a.commit(v);
        match &committed {
            Ok(()) => self.cleared(&mut unsaved),
            Err(e) => {
                log!("backup: {barcode}: its index can't be written: {e}");
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
        report.total_folders = plan.iter().filter(|e| e.size.is_none() && !e.rel.is_empty()).count() as u64;
        // archive-only and nothing changed: nothing to write (no empty set folder)
        if report.total_files == 0 && report.total_folders == 0 && opts.archive_only {
            return Ok(report);
        }
        log!(
            "job: backup \"{}\" into \"{set}\", tapes ticked: {}; {}, Archive attribute {}, interim index: {}; {} files, {}, {} folders, {} unreadable",
            src.display(),
            lto_scsi::log::list(ticked.iter()),
            if opts.archive_only { "only files with the Archive attribute" } else { "all files" },
            if opts.clear_archive { "cleared" } else { "kept" },
            opts.interim_index.key(),
            report.total_files,
            lto_scsi::log::bytes(report.total_bytes),
            plan.iter().filter(|e| e.size.is_none()).count(),
            report.unreadable.len()
        );
        let mut run = Run { src, opts, plan, next: 0, done_files: 0, done_bytes: 0, written: 0, writing: std::time::Duration::ZERO, report };
        // whatever stops it, the backup ends with its report
        match self.onto_tapes(&mut run, &ticked, ui) {
            Ok(()) => {}
            Err(Error::Cancelled) => run.report.cancelled = true,
            Err(e) => {
                log!("backup: stopped: {e}");
                run.report.error = Some(e.to_string());
            }
        }
        let (files, bytes) = run.report.written();
        log!("backup: {} sent to tapes in {:.1} s of writing: {}/s on average", lto_scsi::log::bytes(run.written), run.writing.as_secs_f64(), lto_scsi::log::or_unknown(speed(run.written, run.writing).map(lto_scsi::log::bytes)));
        log!(
            "backup done: {files} files, {} on {}; too big {}, unreadable {}, attribute left {}, cancelled {}, tapes failed: {}, stopped by: {}",
            lto_scsi::log::bytes(bytes),
            lto_scsi::log::list(run.report.tapes.iter().map(|t| &t.barcode)),
            run.report.too_big.len(),
            run.report.unreadable.len(),
            run.report.not_cleared.len(),
            run.report.cancelled,
            lto_scsi::log::list(run.report.failed_tapes.iter().map(|(b, _)| b)),
            run.report.error.as_deref().unwrap_or("nothing")
        );
        lto_scsi::log::flush_counts();
        Ok(run.report)
    }

    /// Write the backup onto the tapes ticked, in barcode order, then onto
    /// new ones asked for. A tape that fails is left: the person is asked
    /// whether to go on with another.
    fn onto_tapes(&self, run: &mut Run, ticked: &BTreeSet<String>, ui: &dyn Ui) -> Result<()> {
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
                    match self.new_backup_tape(&mut seen, left, ui)? {
                        Some(b) => b,
                        None => return Err(Error::Cancelled),
                    }
                }
            };
            log!("backup: onto {barcode}, from file {} of {}", run.done_files, run.report.total_files);
            let r = self.with_volume(&barcode, ui, |v| run.onto(self, v, ui));
            log!("backup: {barcode} ended: {}", match &r { Ok(e) => format!("{e:?}"), Err(e) => format!("error: {e}") });
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
                    log!("backup: {barcode} failed; asked whether to go on with another tape: {answer:?}");
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
            match &known {
                Some(t) => log!("backup: new tape {b}: in the catalog volume {}, generation {}, {} files", if t.uuid.is_empty() { "(none)" } else { t.uuid.as_str() }, t.generation, t.files),
                None => log!("backup: new tape {b}: not in the catalog"),
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
            log!("backup: new tape {b}: {}", files.map_or_else(|| "no LTFS volume on it".to_owned(), |n| format!("{n} files on it")));
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
