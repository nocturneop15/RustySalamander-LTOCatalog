//! Jobs on a tape library with an offline catalog. The plugin and `ltocat`
//! both use them:
//!
//! - [`Archive::inventory`]: where every tape is;
//! - [`Archive::import`]: read a tape's index into the catalog;
//! - [`Archive::read`]: files from tapes, each tape read front to back;
//! - [`Archive::write`], [`Archive::mkdir`], [`Archive::rename`],
//!   [`Archive::delete`]: change a tape's volume;
//! - [`Archive::format`]: make a tape an empty LTFS volume.
//!
//! The tapes are in any number of libraries and standalone drives
//! ([`lto_scsi::site::Site`]). A tape in a standalone drive is identified by
//! the barcode in its cartridge memory, else by its LTFS volume: the
//! volume's UUID in the catalog, or the volume serial of its VOL1 label.
//!
//! A job that needs a tape no library or drive has asks [`Ui::missing_tape`]
//! what to do. Paths here are LTFS names, `\`-separated, the tape's barcode
//! first.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lto_catalog::{Catalog, Tape};
use lto_scsi::library::{Lease, LoadError};
use lto_scsi::site::{Devices, Places, Site};
use lto_scsi::tape::{attr, TapeDrive};
use ltfs::{File, Index, Node, Time, Times, Volume};

/// What to do about a tape that no library or drive has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// It was put into a mail slot (or a magazine) or a drive: look again.
    Retry,
    /// Go on without it.
    Skip,
    /// Stop the job.
    Cancel,
}

/// What a job is doing now, for the person to follow it (the program's
/// status bar, `ltocat`'s output).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step<'a> {
    /// Looking for the libraries and drives.
    Devices,
    /// Getting the tape into a drive (a robot moving it, or waiting for one).
    Loading(&'a str),
    /// Waiting for a library to read what was put into its mail slot.
    Settling(&'a str),
    /// The tape is in this drive; its LTFS volume is being read.
    Mounting(&'a str, &'a str),
    /// Formatting the tape (minutes; can't be stopped halfway).
    Formatting(&'a str),
    /// Reading a file from the tape (tape, path).
    Reading(&'a str, &'a str),
    /// Writing a file onto the tape (tape, path).
    Writing(&'a str, &'a str),
    /// Writing the tape's index after a change.
    WritingIndex(&'a str),
}

/// How a job talks to the person behind it.
pub trait Ui {
    /// What the job is doing now; the default says nothing.
    fn step(&self, step: Step) {
        let _ = step;
    }
    /// Whether the person wants the job to stop.
    fn cancelled(&self) -> bool {
        false
    }
    /// No library or drive has the tape: ask for it. `places` says where
    /// it can be put (libraries' mail slots, standalone drives and what
    /// they hold). `skip` says whether skipping it is possible (a read of
    /// several tapes).
    fn missing_tape(&self, barcode: &str, tape: Option<&Tape>, skip: bool, places: &Places) -> Answer;
}

/// A Ui that asks nothing: missing tapes stop the job.
pub struct NoUi;

impl Ui for NoUi {
    fn missing_tape(&self, _: &str, _: Option<&Tape>, _: bool, _: &Places) -> Answer {
        Answer::Cancel
    }
}

/// Why a job failed.
#[derive(Debug)]
pub enum Error {
    /// No library and no tape drive could be used.
    NoLibrary(String),
    /// No library or drive has the tape, and the person didn't put it in.
    NotInLibrary(String),
    /// The tape isn't in the catalog (it was never imported).
    NotInCatalog(String),
    /// The tape holds no LTFS volume.
    NotLtfs(String),
    /// The catalog's contents of the tape were older than the tape's; they
    /// have been updated, and the change was not made.
    StaleCatalog(String),
    /// Items that aren't on the tape.
    Missing(Vec<String>),
    /// Tapes skipped at the person's request.
    Skipped(Vec<String>),
    /// Not enough space on the tape: (needed, free) bytes.
    NoSpace(u64, u64),
    /// Not possible (writing at the top level, a name in use…).
    Refused(String),
    /// The drive that was to hold `wanted` holds a cartridge that says it is
    /// `found` (the library's drives matched to the wrong tape devices, or
    /// tapes moved outside this program); nothing was done to it.
    WrongTape { wanted: String, found: String, drive: String },
    Cancelled,
    Library(lto_scsi::Error),
    Ltfs(ltfs::Error),
    Catalog(lto_catalog::Error),
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoLibrary(m) => write!(f, "no tape library or drive can be used: {m}"),
            Error::NotInLibrary(b) => write!(f, "tape {b} is in no library or tape drive"),
            Error::NotInCatalog(b) => write!(f, "tape {b} is not in the catalog; import it first"),
            Error::NotLtfs(b) => write!(f, "tape {b} isn't formatted for LTFS"),
            Error::StaleCatalog(b) => write!(f, "the catalog of tape {b} was out of date; it has been updated from the tape, check the files and try again"),
            Error::Missing(p) => write!(f, "not on the tape: {}", p.join(", ")),
            Error::Skipped(b) => write!(f, "tapes skipped: {}", b.join(", ")),
            Error::NoSpace(need, free) => write!(f, "not enough space on the tape: {need} bytes needed, {free} free"),
            Error::Refused(m) => write!(f, "{m}"),
            Error::WrongTape { wanted, found, drive } => write!(
                f,
                "drive {drive} was to hold tape {wanted}, but the cartridge in it says it is {found}; nothing was done to it. The library's drives may be matched to the wrong tape devices, or tapes were moved outside this program: run an inventory, and check `ltocat selftest`"
            ),
            Error::Cancelled => write!(f, "cancelled"),
            Error::Library(e) => write!(f, "{e}"),
            Error::Ltfs(e) => write!(f, "{e}"),
            Error::Catalog(e) => write!(f, "the catalog: {e}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<lto_scsi::Error> for Error {
    fn from(e: lto_scsi::Error) -> Error {
        Error::Library(e)
    }
}
impl From<ltfs::Error> for Error {
    fn from(e: ltfs::Error) -> Error {
        match e {
            ltfs::Error::Cancelled => Error::Cancelled,
            e => Error::Ltfs(e),
        }
    }
}
impl From<lto_catalog::Error> for Error {
    fn from(e: lto_catalog::Error) -> Error {
        Error::Catalog(e)
    }
}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        if e.kind() == io::ErrorKind::Interrupted {
            Error::Cancelled
        } else {
            Error::Io(e)
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Where files read from tapes go.
pub trait ReadSink {
    /// A folder (empty ones too): `path` starts with the barcode.
    fn dir(&mut self, path: &str, modified: Time) -> io::Result<()>;
    /// A file begins; false: not wanted, skip it.
    fn begin(&mut self, path: &str, size: u64, modified: Time) -> io::Result<bool>;
    /// The next bytes of the file begun; false: stop everything.
    fn write(&mut self, data: &[u8]) -> io::Result<bool>;
    /// The file is complete.
    fn end(&mut self) -> io::Result<()>;
}

/// The tape devices and the catalog.
pub struct Archive {
    devices: Box<dyn Devices>,
    site: Mutex<Option<Arc<Site>>>,
    pub catalog: Mutex<Catalog>,
    creator: String,
}

/// How long to wait for a library to read what was put into its mail slot
/// (an IBM TS3100 takes about a minute after its door is closed).
pub const SETTLE_LIMIT: Duration = Duration::from_secs(180);
/// After Retry, how long to look for the tape before asking again (a tape
/// put into a mail slot takes a while to show up).
pub const RETRY_WAIT: Duration = Duration::from_secs(30);
/// Between two looks at a library that is still reading its mail slot.
const SETTLE_PAUSE: Duration = Duration::from_secs(3);

/// Split `BARCODE\path` into the barcode and the path inside the tape.
pub fn split_tape(path: &str) -> (String, String) {
    let parts = ltfs::index::split(path);
    match parts.split_first() {
        Some((b, rest)) => ((*b).to_owned(), rest.join("\\")),
        None => (String::new(), String::new()),
    }
}

/// What a cartridge says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Said {
    /// The barcode in its cartridge memory (written by a library, by
    /// Format, or by other LTFS software).
    Barcode(String),
    /// The volume serial of its LTFS VOL1 label: the barcode's first six
    /// characters.
    Serial(String),
}

impl Said {
    /// Whether it is the tape `barcode`. A barcode in the memory may be
    /// written whole or cut to six characters.
    fn is(&self, barcode: &str) -> bool {
        let want: String = barcode.trim().to_uppercase();
        match self {
            Said::Barcode(b) => {
                let b = b.trim().to_uppercase();
                let (short, long) = if b.len() <= want.len() { (&b, &want) } else { (&want, &b) };
                b == want || (short.len() >= 6 && long.starts_with(short.as_str()))
            }
            Said::Serial(s) => {
                let alnum: String = want.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                alnum.starts_with(&s.to_uppercase())
            }
        }
    }

    fn text(&self) -> String {
        match self {
            Said::Barcode(b) => b.trim().to_owned(),
            Said::Serial(s) => format!("{s}…"),
        }
    }
}

/// What the cartridge in `drive` says it is, `None` when it says nothing
/// (a new tape, or one that can't be read): the barcode in its memory, else
/// the volume serial of its LTFS label.
fn cartridge_says(drive: &mut TapeDrive) -> Option<Said> {
    if let Ok(Some(v)) = drive.read_attribute(0, attr::BARCODE) {
        let b = lto_scsi::ascii(&v);
        if !b.trim().is_empty() {
            return Some(Said::Barcode(b.trim().to_owned()));
        }
    }
    match ltfs::volume::identify(drive) {
        Ok(Some((volser, _))) if !volser.trim().is_empty() => Some(Said::Serial(volser.trim().to_owned())),
        _ => None,
    }
}

impl Archive {
    pub fn new(devices: Box<dyn Devices>, catalog: Catalog, creator: &str) -> Archive {
        Archive { devices, site: Mutex::new(None), catalog: Mutex::new(catalog), creator: creator.to_owned() }
    }

    /// The libraries and drives, opened the first time they are needed.
    pub fn site(&self) -> Result<Arc<Site>> {
        let mut l = self.site.lock().unwrap();
        if let Some(site) = &*l {
            return Ok(site.clone());
        }
        let site = Arc::new(Site::open(&*self.devices).map_err(|e| Error::NoLibrary(e.to_string()))?);
        *l = Some(site.clone());
        drop(l);
        // where everything is, at once
        let _ = site.refresh(false, &|d| self.identify(d, None));
        let _ = self.record_locations(&site);
        Ok(site)
    }

    /// The libraries and drives if they are open already (for status lines).
    pub fn site_if_open(&self) -> Option<Arc<Site>> {
        self.site.lock().unwrap().clone()
    }

    /// Look for the devices again at their next use (a library or drive
    /// connected since), unless a job is using them now.
    pub fn reopen_if_idle(&self) {
        let mut l = self.site.lock().unwrap();
        if l.as_ref().is_some_and(|s| Arc::strong_count(s) == 1) {
            *l = None;
        }
    }

    /// The LTFS label of the tape (got into a drive as a job would), or
    /// `None` when it holds no LTFS volume.
    pub fn label(&self, barcode: &str, ui: &dyn Ui) -> Result<Option<ltfs::Label>> {
        let site = self.site()?;
        let mut lease = self.acquire(&site, barcode, ui, false, false)?.expect("no skip");
        Ok(ltfs::probe(lease.drive())?)
    }

    /// Get `barcode` into a drive (asking for it as a job would) and leave
    /// it there: the drive's name.
    pub fn load(&self, barcode: &str, ui: &dyn Ui) -> Result<String> {
        let site = self.site()?;
        let lease = self.acquire(&site, barcode, ui, false, false)?.expect("no skip");
        Ok(lease.drive_name.clone())
    }

    fn record_locations(&self, site: &Site) -> Result<()> {
        self.catalog.lock().unwrap().set_locations(&site.locations())?;
        Ok(())
    }

    /// Read the libraries' inventories again, look into the standalone
    /// drives, and record where every tape is.
    pub fn inventory(&self, rescan: bool) -> Result<Arc<Site>> {
        let site = self.site()?;
        site.refresh(rescan, &|d| self.identify(d, None))?;
        self.record_locations(&site)?;
        Ok(site)
    }

    /// Which tape is in a standalone drive (loaded and ready):
    ///
    /// 1. the barcode in its cartridge memory (written by a library, by
    ///    Format, or by other LTFS software);
    /// 2. its LTFS volume's UUID, for a tape the catalog has seen;
    /// 3. the volume serial of its VOL1 label (the barcode's first six
    ///    characters): the catalog's one tape not yet imported whose barcode
    ///    starts with it, else, for a tape never seen, the serial itself.
    ///
    /// A tape without LTFS and without a barcode is unknown, except to a
    /// Format, which takes it as `blank_as`.
    fn identify(&self, drive: &mut TapeDrive, blank_as: Option<&str>) -> Option<String> {
        if let Ok(Some(v)) = drive.read_attribute(0, attr::BARCODE) {
            let b = lto_scsi::ascii(&v);
            if !b.is_empty() {
                return Some(b.to_uppercase());
            }
        }
        match ltfs::volume::identify(drive) {
            Ok(Some((volser, label))) => {
                let cat = self.catalog.lock().unwrap();
                if let Ok(Some(t)) = cat.tape_by_uuid(&label.volume_uuid) {
                    return Some(t.barcode);
                }
                let volser = volser.to_uppercase();
                if volser.is_empty() {
                    return None;
                }
                let tapes = cat.tapes().unwrap_or_default();
                // a full barcode beginning with the serial, of a tape not imported yet
                let fresh: Vec<&Tape> = tapes.iter().filter(|t| t.uuid.is_empty() && t.barcode.len() > volser.len() && t.barcode.to_uppercase().starts_with(&volser)).collect();
                match fresh[..] {
                    [one] => Some(one.barcode.clone()),
                    // a tape of that name holds another volume: can't tell
                    [] if !tapes.iter().any(|t| t.barcode.eq_ignore_ascii_case(&volser) && !t.uuid.is_empty()) => Some(volser),
                    _ => None,
                }
            }
            Ok(None) => blank_as.map(str::to_owned),
            Err(_) => None,
        }
    }

    /// Get `barcode` into a drive, asking for it while no library or drive
    /// has it. In a library, the cartridge put into the drive is checked to
    /// be that tape ([`cartridge_says`]); if it says it is another one,
    /// [`Error::WrongTape`]. `blank`: a tape without LTFS or barcode in a standalone drive
    /// is taken as this one (for Format).
    fn acquire<'s>(&self, site: &'s Site, barcode: &str, ui: &dyn Ui, skip: bool, blank: bool) -> Result<Option<Lease<'s>>> {
        let blank_as = blank.then_some(barcode);
        let identify = |d: &mut TapeDrive| self.identify(d, blank_as);
        // a library still reading its mail slot was waited for
        let mut waited = false;
        loop {
            ui.step(Step::Loading(barcode));
            match site.acquire(barcode, &identify, &|| ui.cancelled()) {
                Ok(mut l) => {
                    // a library's drive: the cartridge in it must agree that it is
                    // the tape, before anything is read or written (a format of
                    // the wrong tape destroys it)
                    if !l.standalone() {
                        if let Some(found) = cartridge_says(l.drive()).filter(|s| !s.is(barcode)) {
                            let drive = l.drive_name.clone();
                            drop(l);
                            let _ = site.refresh(false, &identify);
                            let _ = self.record_locations(site);
                            return Err(Error::WrongTape { wanted: barcode.to_owned(), found: found.text(), drive });
                        }
                    }
                    let _ = self.record_locations(site);
                    ui.step(Step::Mounting(barcode, &l.drive_name));
                    return Ok(Some(l));
                }
                Err(LoadError::Cancelled) => return Err(Error::Cancelled),
                Err(LoadError::Scsi(e)) => return Err(e.into()),
                Err(LoadError::NotInLibrary(_)) => {
                    // everything was looked at again: record where things are
                    let _ = self.record_locations(site);
                    // a tape just put into a mail slot, not read yet: wait for it
                    // before asking
                    if !waited && site.settling() {
                        waited = true;
                        self.settle(site, barcode, &identify, ui, Duration::ZERO)?;
                        continue;
                    }
                    let tape = self.catalog.lock().unwrap().tape(barcode)?;
                    // no empty standalone drive: eject the idle one's tape
                    let ejected = site.make_room();
                    let places = Places { ejected, ..site.places() };
                    match ui.missing_tape(barcode, tape.as_ref(), skip, &places) {
                        Answer::Retry => {
                            // pressed before the library read its mail slot: wait for that
                            waited = true;
                            self.settle(site, barcode, &identify, ui, RETRY_WAIT)?;
                            self.record_locations(site)?;
                        }
                        Answer::Skip if skip => return Ok(None),
                        Answer::Skip | Answer::Cancel => return Err(if ui.cancelled() { Error::Cancelled } else { Error::NotInLibrary(barcode.to_owned()) }),
                    }
                }
            }
        }
    }

    /// Read the libraries and drives again, until `barcode` is where it can
    /// be used (a library's robot can reach it, or a standalone drive holds
    /// it), every few seconds: for `patience` (after Retry, the tape may take
    /// a while to show up), and longer while a library is still taking in
    /// its mail slot (it hasn't read the barcode of a tape put there, or
    /// answers "not ready" meanwhile), for at most [`SETTLE_LIMIT`]. Cancel
    /// stops the wait.
    fn settle(&self, site: &Site, barcode: &str, identify: lto_scsi::site::Identify, ui: &dyn Ui, patience: Duration) -> Result<()> {
        let start = Instant::now();
        loop {
            let busy = match site.refresh(false, identify) {
                Ok(()) => None,
                Err(e) if e.sense().is_some_and(|s| s.key == lto_scsi::sense::key::NOT_READY) => Some(e),
                Err(e) => return Err(e.into()),
            };
            let here = site.reachable(barcode) || site.loaded().iter().any(|b| b.eq_ignore_ascii_case(barcode));
            if busy.is_none() && (here || (!site.settling() && start.elapsed() >= patience)) {
                return Ok(());
            }
            if start.elapsed() >= SETTLE_LIMIT {
                return match busy {
                    Some(e) => Err(e.into()),
                    None => Ok(()),
                };
            }
            ui.step(Step::Settling(barcode));
            let pause = Instant::now();
            while pause.elapsed() < SETTLE_PAUSE {
                if ui.cancelled() {
                    return Err(Error::Cancelled);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    /// Put the volume's index into the catalog, with the space left.
    fn record(&self, v: &mut Volume) -> Result<()> {
        let dp = ltfs::volume::part_number(v.label.data_partition) as u8;
        let (remaining, capacity) = v.drive().capacity(dp).unwrap_or((None, None));
        let wp = v.read_only.is_some() && v.drive().write_protected().unwrap_or(false);
        self.catalog.lock().unwrap().import(&v.barcode, &v.index, capacity, remaining, wp)?;
        Ok(())
    }

    /// Whether the catalog holds the volume's index as it is on the tape;
    /// if not, it is updated, and false comes back.
    fn sync(&self, v: &mut Volume) -> Result<bool> {
        let known = self.catalog.lock().unwrap().tape(&v.barcode)?;
        let current = known.as_ref().is_some_and(|t| t.uuid == v.index.volume_uuid && t.generation == v.index.generation);
        if !current {
            self.record(v)?;
        }
        Ok(current)
    }

    /// Read the tape's index into the catalog (whether it holds LTFS at
    /// all, too). The volume's index comes back, `None` for a tape without LTFS.
    pub fn import(&self, barcode: &str, ui: &dyn Ui) -> Result<Option<Index>> {
        let site = self.site()?;
        let mut lease = self.acquire(&site, barcode, ui, false, false)?.expect("no skip");
        let drive = lease.drive();
        if ltfs::probe(drive)?.is_none() {
            let wp = drive.write_protected().unwrap_or(false);
            self.catalog.lock().unwrap().mark_not_ltfs(barcode, wp)?;
            return Ok(None);
        }
        let mut v = Volume::mount(drive, barcode, &self.creator)?;
        self.record(&mut v)?;
        Ok(Some(v.index.clone()))
    }

    /// Mount `barcode`'s volume (asking for the tape if needed) and run `f`.
    fn with_volume<R>(&self, barcode: &str, ui: &dyn Ui, f: impl FnOnce(&mut Volume) -> Result<R>) -> Result<R> {
        let site = self.site()?;
        let mut lease = self.acquire(&site, barcode, ui, false, false)?.expect("no skip");
        let mut v = match Volume::mount(lease.drive(), barcode, &self.creator) {
            Err(ltfs::Error::NotLtfs(_)) => return Err(Error::NotLtfs(barcode.to_owned())),
            r => r?,
        };
        f(&mut v)
    }

    /// Read `items` (paths starting with the barcode; a folder with
    /// everything inside, a bare barcode the whole tape) into `sink`. Tapes
    /// already in a drive are read first, the rest one after the other; the
    /// files of each tape in the order they are on it.
    pub fn read(&self, items: &[String], sink: &mut dyn ReadSink, ui: &dyn Ui) -> Result<()> {
        let mut by_tape: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for it in items {
            let (b, p) = split_tape(it);
            if b.is_empty() {
                continue;
            }
            by_tape.entry(b.to_uppercase()).or_default().push(p);
        }
        if by_tape.is_empty() {
            return Ok(());
        }
        let site = self.site()?;
        let loaded: Vec<String> = site.loaded().into_iter().map(|t| t.to_uppercase()).collect();
        let mut order: Vec<String> = by_tape.keys().cloned().collect();
        order.sort_by_key(|b| !loaded.contains(b));
        let (mut missing, mut skipped) = (Vec::new(), Vec::new());
        for barcode in order {
            if ui.cancelled() {
                return Err(Error::Cancelled);
            }
            let paths = &by_tape[&barcode];
            let Some(mut lease) = self.acquire(&site, &barcode, ui, true, false)? else {
                skipped.push(barcode);
                continue;
            };
            let mut v = match Volume::mount(lease.drive(), &barcode, &self.creator) {
                Err(ltfs::Error::NotLtfs(_)) => return Err(Error::NotLtfs(barcode)),
                r => r?,
            };
            self.sync(&mut v)?;
            // what to read: folders and files, each file once
            let mut dirs: Vec<(String, Time)> = Vec::new();
            let mut files: BTreeMap<String, File> = BTreeMap::new();
            for p in paths {
                if ui.cancelled() {
                    return Err(Error::Cancelled);
                }
                let node = if p.is_empty() { None } else { v.index.find(p).cloned() };
                match (p.is_empty(), node) {
                    (true, _) => collect(&Node::Dir(v.index.root.clone()), "", &mut dirs, &mut files),
                    (false, Some(n)) => collect(&n, &canonical(&v.index, p), &mut dirs, &mut files),
                    (false, None) => missing.push(format!("{barcode}\\{p}")),
                }
            }
            for (d, t) in &dirs {
                sink.dir(&join(&barcode, d), *t)?;
            }
            let mut list: Vec<(String, File)> = files.into_iter().collect();
            list.sort_by_key(|(_, f)| f.extents.first().map(|e| (e.partition, e.start_block)).unwrap_or(('\0', 0)));
            for (path, f) in list {
                if ui.cancelled() {
                    return Err(Error::Cancelled);
                }
                ui.step(Step::Reading(&barcode, &path));
                if !sink.begin(&join(&barcode, &path), f.length, f.times.modify)? {
                    // not wanted, or the person stopped everything
                    if ui.cancelled() {
                        return Err(Error::Cancelled);
                    }
                    continue;
                }
                let mut stopped = false;
                v.read_file(&f, &mut |b| {
                    let go = sink.write(b)? && !ui.cancelled();
                    stopped |= !go;
                    Ok(go)
                })
                .map_err(|e| if stopped { Error::Cancelled } else { e.into() })?;
                sink.end()?;
            }
        }
        if !missing.is_empty() {
            return Err(Error::Missing(missing));
        }
        if !skipped.is_empty() {
            return Err(Error::Skipped(skipped));
        }
        Ok(())
    }

    /// Check that a change can be made to the tape's volume: writable, and
    /// the catalog current (else it is updated and the change refused).
    fn ready_to_change(&self, v: &mut Volume) -> Result<()> {
        v.writable()?;
        if !self.sync(v)? {
            return Err(Error::StaleCatalog(v.barcode.clone()));
        }
        Ok(())
    }

    /// Write the volume's index and put it into the catalog.
    fn commit(&self, v: &mut Volume) -> Result<()> {
        v.commit()?;
        self.record(v)
    }

    /// Copy files and folders from disk onto a tape: `(path on disk, path
    /// on the tape)`, folders with everything inside. An item already at a
    /// path is replaced. `progress(done, total)` in bytes; false cancels
    /// (what was written before stays, in a consistent index).
    pub fn write(&self, barcode: &str, items: &[(PathBuf, String)], ui: &dyn Ui, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<()> {
        let mut plan: Vec<(PathBuf, String, bool)> = Vec::new();
        for (disk, dest) in items {
            if ltfs::index::split(dest).is_empty() {
                return Err(Error::Refused("files can be copied only into a tape's folder".into()));
            }
            expand(disk, dest, &mut plan)?;
        }
        let total: u64 = plan.iter().filter(|(_, _, d)| !d).map(|(p, _, _)| fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum();
        let known = self.catalog.lock().unwrap().tape(barcode)?;
        if let Some(free) = known.as_ref().and_then(|t| t.remaining) {
            if total > free {
                return Err(Error::NoSpace(total, free));
            }
        }
        self.with_volume(barcode, ui, |v| {
            self.ready_to_change(v)?;
            if let Some(free) = v.remaining()? {
                if total > free {
                    return Err(Error::NoSpace(total, free));
                }
            }
            let mut done = 0u64;
            let mut first_error = None;
            for (disk, dest, is_dir) in &plan {
                let now = Time::now();
                if *is_dir {
                    let t = fs::metadata(disk).ok().map(|m| times_of(&m, now)).unwrap_or(Times::all(now));
                    match v.index.make_dirs(dest, now) {
                        Ok(d) => d.times = t,
                        Err(e) => {
                            first_error = Some(Error::Refused(e));
                            break;
                        }
                    }
                    continue;
                }
                let (dir, name) = parent_and_name(dest);
                if matches!(v.index.find(dest), Some(Node::Dir(_))) {
                    first_error = Some(Error::Refused(format!("a folder named \"{name}\" is in the way")));
                    break;
                }
                ui.step(Step::Writing(barcode, dest));
                let r = (|| -> Result<()> {
                    let meta = fs::metadata(disk)?;
                    let mut f = fs::File::open(disk)?;
                    let (extents, length) = v.write_data(&mut f, &mut |n| {
                        done += n;
                        progress(done, total) && !ui.cancelled()
                    })?;
                    let uid = v.index.next_uid();
                    let file = File {
                        name: name.clone(),
                        length,
                        readonly: meta.permissions().readonly(),
                        open_for_write: false,
                        times: times_of(&meta, now),
                        uid,
                        xattrs: Vec::new(),
                        extents,
                        symlink: None,
                        extra: Vec::new(),
                    };
                    v.index.put(&dir, Node::File(file), now).map_err(Error::Refused)?;
                    Ok(())
                })();
                if let Err(e) = r {
                    first_error = Some(e);
                    break;
                }
            }
            // what was written gets an index, whatever stopped it
            ui.step(Step::WritingIndex(barcode));
            let committed = self.commit(v);
            match (first_error, committed) {
                (Some(e), _) => Err(e),
                (None, r) => r,
            }
        })
    }

    /// Make a folder on a tape.
    pub fn mkdir(&self, barcode: &str, path: &str, ui: &dyn Ui) -> Result<()> {
        if ltfs::index::split(path).is_empty() {
            return Err(Error::Refused("tapes can't be made as folders; use Format tape".into()));
        }
        self.with_volume(barcode, ui, |v| {
            self.ready_to_change(v)?;
            if v.index.find(path).is_some() {
                return Err(Error::Io(io::Error::new(io::ErrorKind::AlreadyExists, format!("\"{path}\" already exists"))));
            }
            v.index.make_dirs(path, Time::now()).map_err(Error::Refused)?;
            self.commit(v)
        })
    }

    /// Rename or move an item within a tape.
    pub fn rename(&self, barcode: &str, old: &str, new: &str, ui: &dyn Ui) -> Result<()> {
        if ltfs::index::split(old).is_empty() || ltfs::index::split(new).is_empty() {
            return Err(Error::Refused("tapes can't be renamed".into()));
        }
        self.with_volume(barcode, ui, |v| {
            self.ready_to_change(v)?;
            if v.index.find(old).is_none() {
                return Err(Error::Missing(vec![old.to_owned()]));
            }
            let same = old.to_lowercase() == new.to_lowercase();
            if !same && v.index.find(new).is_some() {
                return Err(Error::Io(io::Error::new(io::ErrorKind::AlreadyExists, format!("\"{new}\" already exists"))));
            }
            v.index.rename(old, new, Time::now()).map_err(Error::Refused)?;
            self.commit(v)
        })
    }

    /// Delete items of a tape (from its index: LTFS never frees space).
    pub fn delete(&self, barcode: &str, paths: &[String], ui: &dyn Ui) -> Result<()> {
        if paths.iter().any(|p| ltfs::index::split(p).is_empty()) {
            return Err(Error::Refused("tapes can't be deleted; use Forget tape to take one out of the catalog".into()));
        }
        self.with_volume(barcode, ui, |v| {
            self.ready_to_change(v)?;
            let now = Time::now();
            for p in paths {
                v.index.remove(p, now);
            }
            self.commit(v)
        })
    }

    /// Format a tape as an empty LTFS volume. Everything on it is lost.
    /// In a standalone drive, a tape without LTFS and without a barcode in
    /// its memory is taken as `barcode` (a new tape); a tape that is
    /// another one is never formatted in its place.
    pub fn format(&self, barcode: &str, name: &str, ui: &dyn Ui) -> Result<Index> {
        let site = self.site()?;
        let mut lease = self.acquire(&site, barcode, ui, false, true)?.expect("no skip");
        let drive_name = lease.drive_name.clone();
        let drive = lease.drive();
        ui.step(Step::Formatting(barcode));
        let r = ltfs::format(drive, barcode, name, &self.creator);
        let f = match r {
            Ok(f) => f,
            Err(e) => {
                drop(lease);
                // it was taken for this tape only to be formatted
                site.forget(&drive_name);
                return Err(e.into());
            }
        };
        let (remaining, capacity) = drive.capacity(1).unwrap_or((None, None));
        self.catalog.lock().unwrap().import(barcode, &f.index, capacity, remaining, false)?;
        Ok(f.index)
    }
}

fn join(barcode: &str, path: &str) -> String {
    if path.is_empty() {
        barcode.to_owned()
    } else {
        format!("{barcode}\\{path}")
    }
}

fn parent_and_name(path: &str) -> (String, String) {
    let parts = ltfs::index::split(path);
    let (name, dirs) = parts.split_last().map(|(n, d)| (n.to_string(), d.join("\\"))).unwrap_or_default();
    (dirs, name)
}

/// The path as the index spells it (the person may have typed another case).
fn canonical(index: &Index, path: &str) -> String {
    let mut out = Vec::new();
    let mut dir = &index.root;
    for p in ltfs::index::split(path) {
        let Some(n) = dir.children.iter().find(|c| c.name() == p).or_else(|| dir.children.iter().find(|c| c.name().to_lowercase() == p.to_lowercase())) else { return path.to_owned() };
        out.push(n.name().to_owned());
        if let Node::Dir(d) = n {
            dir = d;
        }
    }
    out.join("\\")
}

/// The folders and files of `node` at `path`.
fn collect(node: &Node, path: &str, dirs: &mut Vec<(String, Time)>, files: &mut BTreeMap<String, File>) {
    match node {
        Node::File(f) => {
            if f.symlink.is_none() {
                files.insert(path.to_owned(), f.clone());
            }
        }
        Node::Dir(d) => {
            if !path.is_empty() {
                dirs.push((path.to_owned(), d.times.modify));
            }
            for c in &d.children {
                let p = if path.is_empty() { c.name().to_owned() } else { format!("{path}\\{}", c.name()) };
                collect(c, &p, dirs, files);
            }
        }
    }
}

/// What is to be written for `disk` going to `dest`: folders first, then
/// their contents.
fn expand(disk: &Path, dest: &str, out: &mut Vec<(PathBuf, String, bool)>) -> Result<()> {
    let meta = fs::symlink_metadata(disk)?;
    if meta.is_dir() {
        out.push((disk.to_path_buf(), dest.to_owned(), true));
        let mut entries: Vec<_> = fs::read_dir(disk)?.collect::<io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            expand(&e.path(), &format!("{dest}\\{name}"), out)?;
        }
    } else if meta.is_file() {
        out.push((disk.to_path_buf(), dest.to_owned(), false));
    }
    // links and other things are left out
    Ok(())
}

fn times_of(m: &fs::Metadata, now: Time) -> Times {
    let t = |r: io::Result<std::time::SystemTime>| r.map(Time::from).unwrap_or(now);
    let modify = t(m.modified());
    Times { creation: t(m.created()), change: modify, modify, access: t(m.accessed()), backup: Some(now) }
}

#[cfg(test)]
mod tests;
