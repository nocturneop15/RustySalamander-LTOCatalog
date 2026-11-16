//! Jobs on a tape library with an offline catalog. The plugin and `ltocat`
//! both use them:
//!
//! - [`Archive::inventory`]: where every tape is;
//! - [`Archive::import`]: read a tape's index into the catalog;
//! - [`Archive::read`]: files from tapes, each tape read front to back;
//! - [`Archive::write`], [`Archive::mkdir`], [`Archive::rename`],
//!   [`Archive::delete`]: change a tape's volume;
//! - [`Archive::format`]: make a tape an empty LTFS volume;
//! - [`Archive::verify`]: read every file of tapes, to find what can't be.
//!
//! A tape whose label or index can't be read in one partition is used with
//! the other partition's, read-only ([`Ui::index_damaged`] says so). After
//! each use of a drive its TapeAlert flags are read: what they say about
//! the tape goes into the catalog ([`lto_catalog::Condition`]), and
//! everything they say waits in [`Archive::take_notices`] for the person.
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

use lto_catalog::{Catalog, Condition, Tape};
use lto_scsi::library::{Lease, LoadError};
use lto_scsi::log;
use lto_scsi::site::{Devices, Places, Site};
use lto_scsi::tape::{attr, TapeDrive};
use ltfs::{File, Index, Node, Time, Times, Volume};

mod backup;
pub use backup::{archive_set, backup_set_name, clear_archive, set_archive, BackupOptions, BackupReport, InterimIndex, NewTapeProblem, TapeDone, COMMIT_EVERY};

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
    /// Backing up a folder onto this tape: (done, of) files and bytes, and
    /// the average speed of writing data so far in this backup, bytes a
    /// second (loading tapes and writing indexes not counted; None before
    /// any data was written).
    Backup { tape: &'a str, files: (u64, u64), bytes: (u64, u64), speed: Option<u64> },
    /// Waiting for a new tape for a backup to show up in a mail slot.
    NewTape,
    /// Checking this tape: (done, of) files and bytes read, and the average
    /// speed of reading its data so far, bytes a second (loading the tape
    /// not counted; None before any data was read).
    Verifying { tape: &'a str, files: (u64, u64), bytes: (u64, u64), speed: Option<u64> },
}

/// One partition's label or index of a tape couldn't be read: the other
/// partition's was used, and the tape is read-only.
#[derive(Debug, Clone)]
pub struct IndexWarning {
    pub barcode: String,
    /// What couldn't be read.
    pub damage: Vec<ltfs::Damage>,
    /// The index used: its partition, generation and when it was written.
    pub partition: char,
    pub generation: u64,
    pub written: Time,
    /// The generation the catalog holds for the volume, when it is newer
    /// than the one used: files written since are listed in the catalog,
    /// but can't be read from the tape.
    pub catalog_generation: Option<u64>,
}

/// Something a drive said during a job, for the person (TapeAlert).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// The drive's name.
    pub drive: String,
    /// The tape that was in it.
    pub barcode: String,
    /// The TapeAlert flag raised; see [`lto_scsi::tapealert::describe`].
    pub flag: u8,
}

/// What a check of a tape found ([`Archive::verify`]).
#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    pub barcode: String,
    /// Files and bytes on the tape, and those read.
    pub total_files: u64,
    pub total_bytes: u64,
    pub files: u64,
    pub bytes: u64,
    /// Files that couldn't be read: (path on the tape, why).
    pub bad: Vec<(String, String)>,
    /// One partition's label or index couldn't be read.
    pub damaged_index: bool,
    /// Skipped at the person's word (it wasn't put in).
    pub skipped: bool,
    /// Why the tape couldn't be checked (to the end).
    pub error: Option<String>,
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
    /// A backup's tapes are full and `left` bytes are still to go: ask for
    /// a new tape (Retry: it was put in; anything else stops the backup).
    /// `problem` says why the last one put in wasn't taken. The default
    /// stops.
    fn new_tape(&self, left: u64, places: &Places, problem: Option<NewTapeProblem>) -> Answer {
        let _ = (left, places, problem);
        Answer::Cancel
    }
    /// The new tape put in for a backup isn't an LTFS volume (blank, or
    /// another format): Retry formats it and uses it, Skip asks for another
    /// tape, Cancel stops the backup. The default stops.
    fn confirm_format(&self, barcode: &str) -> Answer {
        let _ = barcode;
        Answer::Cancel
    }
    /// A tape's label or index couldn't be read in one partition; the job
    /// goes on with the other partition's, read-only, once this returns
    /// (the plugin shows it in a window the person closes). The default
    /// says nothing.
    fn index_damaged(&self, warning: &IndexWarning) {
        let _ = warning;
    }
    /// A backup's tape failed with `error` (it couldn't be read or written)
    /// and `left` bytes are still to go: Retry goes on with another tape,
    /// anything else stops the backup. The default stops.
    fn tape_failed(&self, barcode: &str, error: &str, left: u64) -> Answer {
        let _ = (barcode, error, left);
        Answer::Cancel
    }
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
    /// The tape mounted as `barcode` holds another LTFS volume than the one
    /// the catalog has for it (`belongs_to`: the tape the catalog has that
    /// volume for); nothing was done to it, the catalog wasn't changed.
    OtherVolume { barcode: String, catalog: String, tape: String, belongs_to: Option<String> },
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
            Error::OtherVolume { barcode, catalog, tape, belongs_to } => {
                write!(f, "the tape mounted as {barcode} holds the LTFS volume {tape}, but the catalog has the volume {catalog} for {barcode}")?;
                if let Some(b) = belongs_to {
                    write!(f, " (the volume in the drive is tape {b}'s)")?;
                }
                write!(f, "; nothing was done to it and the catalog wasn't changed. Run an inventory; if the tape was formatted again by another program, import it")
            }
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
    /// What the drives said since the last [`Archive::take_notices`].
    notices: Mutex<Vec<Notice>>,
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
        Archive { devices, site: Mutex::new(None), catalog: Mutex::new(catalog), creator: creator.to_owned(), notices: Mutex::new(Vec::new()) }
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
        let found = self.identify_inner(drive, blank_as);
        log!("identify the cartridge in a drive: {}", found.as_deref().unwrap_or("unknown"));
        found
    }

    fn identify_inner(&self, drive: &mut TapeDrive, blank_as: Option<&str>) -> Option<String> {
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
        log!("acquire {barcode}: skip {skip}, blank {blank}");
        loop {
            ui.step(Step::Loading(barcode));
            match site.acquire(barcode, &identify, &|| ui.cancelled()) {
                Ok(mut l) => {
                    log!("acquire {barcode}: in {}{}", l.drive_name, if l.standalone() { " (standalone)" } else { "" });
                    // a library's drive: the cartridge in it must agree that it is
                    // the tape, before anything is read or written (a format of
                    // the wrong tape destroys it)
                    if !l.standalone() {
                        let said = cartridge_says(l.drive());
                        log!("acquire {barcode}: the cartridge says {}", said.as_ref().map(|s| s.text()).unwrap_or_else(|| "nothing".into()));
                        if let Some(found) = said.filter(|s| !s.is(barcode)) {
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
                Err(LoadError::Scsi(e)) => {
                    log!("acquire {barcode}: failed: {e}");
                    return Err(e.into());
                }
                Err(LoadError::NotInLibrary(_)) => {
                    log!("acquire {barcode}: in no library or drive");
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
                    let answer = ui.missing_tape(barcode, tape.as_ref(), skip, &places);
                    log!("acquire {barcode}: asked for it, answer {answer:?}");
                    match answer {
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
        let (files, bytes) = v.index.totals();
        log!(
            "catalog {}: recorded volume {}, generation {}, {files} files, {}; free: {} of {}",
            v.barcode,
            v.index.volume_uuid,
            v.index.generation,
            lto_scsi::log::bytes(bytes),
            lto_scsi::log::or_unknown(remaining.map(lto_scsi::log::bytes)),
            lto_scsi::log::or_unknown(capacity.map(lto_scsi::log::bytes))
        );
        self.catalog.lock().unwrap().import(&v.barcode, &v.index, capacity, remaining, wp)?;
        Ok(())
    }

    /// Whether the catalog holds the volume's index as it is on the tape;
    /// if not, it is updated, and false comes back.
    fn sync(&self, v: &mut Volume) -> Result<bool> {
        let known = self.catalog.lock().unwrap().tape(&v.barcode)?;
        let current = known.as_ref().is_some_and(|t| t.uuid == v.index.volume_uuid && t.generation == v.index.generation);
        match &known {
            Some(t) => log!(
                "catalog {}: holds volume {} generation {} (written {}, imported {}); the tape holds {} generation {} (written {}): {}",
                v.barcode,
                if t.uuid.is_empty() { "(none)" } else { t.uuid.as_str() },
                t.generation,
                lto_scsi::log::or_unknown(t.updated.map(|x| x.format())),
                lto_scsi::log::or_unknown(t.imported.map(|x| x.format())),
                v.index.volume_uuid,
                v.index.generation,
                v.index.update_time.format(),
                if current { "the same" } else { "OUT OF DATE, updated from the tape" }
            ),
            None => log!("catalog {}: not in the catalog; the tape holds {} generation {}", v.barcode, v.index.volume_uuid, v.index.generation),
        }
        // another volume than the catalog's for this barcode: the wrong
        // cartridge in the drive, never recorded under this barcode
        if let Some(t) = known.as_ref().filter(|t| !t.uuid.is_empty() && t.uuid != v.index.volume_uuid) {
            let belongs_to = self.catalog.lock().unwrap().tape_by_uuid(&v.index.volume_uuid).ok().flatten().map(|o| o.barcode);
            log!(
                "catalog {}: ANOTHER VOLUME in the drive ({} belongs to {}): stopped",
                v.barcode,
                v.index.volume_uuid,
                belongs_to.as_deref().unwrap_or("no tape in the catalog")
            );
            return Err(Error::OtherVolume { barcode: v.barcode.clone(), catalog: t.uuid.clone(), tape: v.index.volume_uuid.clone(), belongs_to });
        }
        if !current {
            // one partition's index couldn't be read and the other's is
            // older than the catalog's: the catalog isn't set back
            if !v.damage.is_empty() && self.catalog_is_newer(v)? {
                log!("catalog {}: kept: newer than the index that could be read", v.barcode);
                return Ok(false);
            }
            self.record(v)?;
        }
        Ok(current)
    }

    /// Read the tape's index into the catalog (whether it holds LTFS at
    /// all, too). The volume's index comes back, `None` for a tape without LTFS.
    pub fn import(&self, barcode: &str, ui: &dyn Ui) -> Result<Option<Index>> {
        log!("job: import {barcode}");
        let site = self.site()?;
        let mut lease = self.acquire(&site, barcode, ui, false, false)?.expect("no skip");
        let drive = lease.drive();
        if ltfs::probe(drive)?.is_none() {
            let wp = drive.write_protected().unwrap_or(false);
            self.catalog.lock().unwrap().mark_not_ltfs(barcode, wp)?;
            return Ok(None);
        }
        let r = self.mount(drive, barcode, ui).and_then(|mut v| {
            if v.damage.is_empty() || !self.catalog_is_newer(&v)? {
                self.record(&mut v)?;
            }
            Ok(Some(v.index.clone()))
        });
        self.after_use(&mut lease, barcode, r.as_ref().err());
        r
    }

    /// What the drives said since this was last asked (TapeAlert flags),
    /// in the order they said it.
    pub fn take_notices(&self) -> Vec<Notice> {
        std::mem::take(&mut *self.notices.lock().unwrap())
    }

    /// Mount the tape's volume in `drive`. When one partition's label or
    /// index can't be read, the person is told ([`Ui::index_damaged`]) and
    /// the catalog notes it.
    fn mount<'d>(&self, drive: &'d mut TapeDrive, barcode: &str, ui: &dyn Ui) -> Result<Volume<'d>> {
        let v = match Volume::mount(drive, barcode, &self.creator) {
            Err(ltfs::Error::NotLtfs(_)) => return Err(Error::NotLtfs(barcode.to_owned())),
            r => r?,
        };
        if !v.damage.is_empty() {
            let catalog_generation = {
                let cat = self.catalog.lock().unwrap();
                cat.add_condition(barcode, &v.damage.iter().map(|d| Condition::Index(d.partition)).collect::<Vec<_>>())?;
                cat.tape(barcode)?.filter(|t| t.uuid == v.index.volume_uuid && t.generation > v.index.generation).map(|t| t.generation)
            };
            let w = IndexWarning {
                barcode: barcode.to_owned(),
                damage: v.damage.clone(),
                partition: v.index.location.partition,
                generation: v.index.generation,
                written: v.index.update_time,
                catalog_generation,
            };
            log!(
                "warning {barcode}: {} can't be read; the index of partition {} (generation {}, written {}) is used, read-only; the catalog's generation: {}",
                lto_scsi::log::list(w.damage.iter().map(|d| format!("the {} of partition {} ({})", if d.label { "label" } else { "index" }, d.partition, d.error))),
                w.partition,
                w.generation,
                w.written.format(),
                lto_scsi::log::or_unknown(w.catalog_generation)
            );
            ui.index_damaged(&w);
        }
        Ok(v)
    }

    /// Whether the catalog holds a newer generation of the volume than the
    /// index read from the tape (one partition's couldn't be read): the
    /// catalog is kept then, not set back.
    fn catalog_is_newer(&self, v: &Volume) -> Result<bool> {
        let t = self.catalog.lock().unwrap().tape(&v.barcode)?;
        Ok(t.is_some_and(|t| t.uuid == v.index.volume_uuid && t.generation > v.index.generation))
    }

    /// A drive was used for `barcode` (the job failed with `failure`): read
    /// its TapeAlert flags. Those about the tape, and a failure to read or
    /// write it, go into the catalog; all wait for the person in
    /// [`Self::take_notices`]. The flags about the tape come back.
    fn after_use(&self, lease: &mut Lease, barcode: &str, failure: Option<&Error>) -> Vec<u8> {
        let drive_name = lease.drive_name.clone();
        let flags = match lease.drive().tape_alerts() {
            Ok(f) => f,
            Err(e) => {
                log!("tapealert {drive_name}: can't be read: {e}");
                Vec::new()
            }
        };
        let mut conds = Vec::new();
        let mut tape_flags = Vec::new();
        for f in &flags {
            let Some(a) = lto_scsi::tapealert::describe(*f) else { continue };
            log!("tapealert {drive_name} ({barcode}): flag {f}: {}", a.text);
            if a.concern == lto_scsi::tapealert::Concern::Tape {
                conds.push(Condition::Alert(*f));
                tape_flags.push(*f);
            }
            self.notices.lock().unwrap().push(Notice { drive: drive_name.clone(), barcode: barcode.to_owned(), flag: *f });
        }
        if let Some(c) = failure.and_then(medium_condition) {
            conds.push(c);
        }
        if !conds.is_empty() {
            log!("catalog {barcode}: condition {}", lto_scsi::log::list(conds.iter().map(|c| c.text())));
            if let Err(e) = self.catalog.lock().unwrap().add_condition(barcode, &conds) {
                log!("catalog {barcode}: the condition can't be recorded: {e}");
            }
        }
        tape_flags
    }

    /// Mount `barcode`'s volume (asking for the tape if needed) and run `f`.
    fn with_volume<R>(&self, barcode: &str, ui: &dyn Ui, f: impl FnOnce(&mut Volume) -> Result<R>) -> Result<R> {
        let site = self.site()?;
        let mut lease = self.acquire(&site, barcode, ui, false, false)?.expect("no skip");
        let r = self.mount(lease.drive(), barcode, ui).and_then(|mut v| f(&mut v));
        self.after_use(&mut lease, barcode, r.as_ref().err());
        r
    }

    /// Read `items` (paths starting with the barcode; a folder with
    /// everything inside, a bare barcode the whole tape) into `sink`. Tapes
    /// already in a drive are read first, the rest one after the other; the
    /// files of each tape in the order they are on it.
    pub fn read(&self, items: &[String], sink: &mut dyn ReadSink, ui: &dyn Ui) -> Result<()> {
        log!("job: read {} items", items.len());
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
            let r = self.mount(lease.drive(), &barcode, ui).and_then(|mut v| self.read_tape(&mut v, &barcode, paths, sink, ui, &mut missing));
            self.after_use(&mut lease, &barcode, r.as_ref().err());
            r?;
        }
        if !missing.is_empty() {
            return Err(Error::Missing(missing));
        }
        if !skipped.is_empty() {
            return Err(Error::Skipped(skipped));
        }
        Ok(())
    }

    /// Read `paths` (a folder with everything inside, "" the whole tape) of
    /// the mounted tape into `sink`, each file once, in the order they are
    /// on the tape; paths not on it go into `missing`.
    fn read_tape(&self, v: &mut Volume, barcode: &str, paths: &[String], sink: &mut dyn ReadSink, ui: &dyn Ui, missing: &mut Vec<String>) -> Result<()> {
        self.sync(v)?;
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
            sink.dir(&join(barcode, d), *t)?;
        }
        for (path, f) in in_tape_order(files) {
            if ui.cancelled() {
                return Err(Error::Cancelled);
            }
            ui.step(Step::Reading(barcode, &path));
            if !sink.begin(&join(barcode, &path), f.length, f.times.modify)? {
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
        Ok(())
    }

    /// Check tapes: read every file of each (in the order they are on it,
    /// the data thrown away), going on past files that can't be read. A tape
    /// read to the end without a fault is cleared of what the catalog held
    /// against it; one with faults gets them noted. A tape not put in when
    /// asked for is skipped.
    pub fn verify(&self, tapes: &[String], ui: &dyn Ui) -> Result<Vec<VerifyReport>> {
        log!("job: verify {}", lto_scsi::log::list(tapes));
        let site = self.site()?;
        let mut out = Vec::new();
        for barcode in tapes.iter().map(|b| b.trim().to_uppercase()).filter(|b| !b.is_empty()) {
            if ui.cancelled() {
                return Err(Error::Cancelled);
            }
            let mut report = VerifyReport { barcode: barcode.clone(), ..VerifyReport::default() };
            let Some(mut lease) = self.acquire(&site, &barcode, ui, true, false)? else {
                report.skipped = true;
                out.push(report);
                continue;
            };
            let r = self.mount(lease.drive(), &barcode, ui).and_then(|mut v| self.verify_tape(&mut v, &mut report, ui));
            let tape_flags = self.after_use(&mut lease, &barcode, r.as_ref().err());
            match r {
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => report.error = Some(e.to_string()),
                Ok(()) => {}
            }
            let cat = self.catalog.lock().unwrap();
            if report.bad.is_empty() && report.error.is_none() && !report.damaged_index && tape_flags.is_empty() {
                log!("verify {barcode}: all {} files read, nothing wrong", report.files);
                cat.clear_condition(&barcode)?;
            } else if !report.bad.is_empty() {
                cat.add_condition(&barcode, &[Condition::Read, Condition::Unreadable(report.bad.len() as u64)])?;
            }
            drop(cat);
            log!("verify {barcode}: {} of {} files read, {} can't be read{}", report.files, report.total_files, report.bad.len(), report.error.as_deref().map(|e| format!("; stopped: {e}")).unwrap_or_default());
            out.push(report);
        }
        Ok(out)
    }

    /// [`Self::verify`] of the mounted tape.
    fn verify_tape(&self, v: &mut Volume, report: &mut VerifyReport, ui: &dyn Ui) -> Result<()> {
        report.damaged_index = !v.damage.is_empty();
        if !report.damaged_index || !self.catalog_is_newer(v)? {
            self.sync(v)?;
        }
        let mut dirs = Vec::new();
        let mut files: BTreeMap<String, File> = BTreeMap::new();
        collect(&Node::Dir(v.index.root.clone()), "", &mut dirs, &mut files);
        report.total_files = files.len() as u64;
        report.total_bytes = files.values().map(|f| f.length).sum();
        let barcode = report.barcode.clone();
        // time spent reading the files' data, for the average speed
        let mut reading = Duration::ZERO;
        for (path, f) in in_tape_order(files) {
            if ui.cancelled() {
                return Err(Error::Cancelled);
            }
            ui.step(Step::Verifying { tape: &barcode, files: (report.files, report.total_files), bytes: (report.bytes, report.total_bytes), speed: backup::speed(report.bytes, reading) });
            let mut read = 0u64;
            let began = Instant::now();
            let mut shown = began;
            let r = v.read_file(&f, &mut |b| {
                read += b.len() as u64;
                if shown.elapsed() >= Duration::from_secs(1) {
                    shown = Instant::now();
                    let speed = backup::speed(report.bytes + read, reading + began.elapsed());
                    ui.step(Step::Verifying { tape: &barcode, files: (report.files, report.total_files), bytes: (report.bytes + read, report.total_bytes), speed });
                }
                Ok(!ui.cancelled())
            });
            reading += began.elapsed();
            report.bytes += read;
            match r {
                Ok(()) => {
                    log!("verify {barcode}: \"{path}\" {read} bytes read");
                    report.files += 1;
                }
                Err(ltfs::Error::Cancelled) => return Err(Error::Cancelled),
                // the tape couldn't be read there, or the data isn't all
                // there: noted, and on with the next file
                Err(e @ ltfs::Error::Corrupt(_)) => {
                    log!("verify {barcode}: \"{path}\" isn't whole: {e}");
                    report.bad.push((path, e.to_string()));
                }
                Err(ltfs::Error::Scsi(e)) if e.is_medium() => {
                    log!("verify {barcode}: \"{path}\" can't be read: {e}");
                    report.bad.push((path, e.to_string()));
                }
                // the drive or its connection: no use going on
                Err(e) => return Err(e.into()),
            }
        }
        log!("verify {barcode}: {} read in {:.1} s: {}/s on average", lto_scsi::log::bytes(report.bytes), reading.as_secs_f64(), lto_scsi::log::or_unknown(backup::speed(report.bytes, reading).map(lto_scsi::log::bytes)));
        Ok(())
    }

    /// Check that a change can be made to the tape's volume: writable, and
    /// the catalog current (else it is updated and the change refused).
    fn ready_to_change(&self, v: &mut Volume) -> Result<()> {
        if let Err(e) = v.writable() {
            log!("{}: can't be changed: {e}", v.barcode);
            return Err(e.into());
        }
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

    /// Write the volume's index to the data partition only (see
    /// [`Volume::sync`]) and put it into the catalog.
    fn sync_index(&self, v: &mut Volume) -> Result<()> {
        v.sync()?;
        self.record(v)
    }

    /// Copy files and folders from disk onto a tape: `(path on disk, path
    /// on the tape)`, folders with everything inside. An item already at a
    /// path is replaced. `progress(done, total)` in bytes; false cancels
    /// (what was written before stays, in a consistent index).
    pub fn write(&self, barcode: &str, items: &[(PathBuf, String)], ui: &dyn Ui, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<()> {
        log!("job: write {} items onto {barcode}", items.len());
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
        log!("job: make folder {path:?} on {barcode}");
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
        log!("job: rename {old:?} to {new:?} on {barcode}");
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
        log!("job: delete {} items on {barcode}", paths.len());
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
        log!("job: format {barcode}, name {name:?}");
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
        log!("catalog {barcode}: recorded the new volume {}, generation {}", f.index.volume_uuid, f.index.generation);
        self.catalog.lock().unwrap().import(barcode, &f.index, capacity, remaining, false)?;
        Ok(f.index)
    }
}

/// Files in the order they are on the tape (by their first block).
fn in_tape_order(files: BTreeMap<String, File>) -> Vec<(String, File)> {
    let mut list: Vec<(String, File)> = files.into_iter().collect();
    list.sort_by_key(|(_, f)| f.extents.first().map(|e| (e.partition, e.start_block)).unwrap_or(('\0', 0)));
    list
}

/// The SCSI error inside a job's error, if it is one.
fn scsi_of(e: &Error) -> Option<&lto_scsi::Error> {
    match e {
        Error::Library(e) | Error::Ltfs(ltfs::Error::Scsi(e)) => Some(e),
        _ => None,
    }
}

/// What a job's failure says about its tape: it couldn't be read, or
/// written, there (a medium error).
fn medium_condition(e: &Error) -> Option<Condition> {
    let s = scsi_of(e).filter(|s| s.is_medium())?;
    match s {
        lto_scsi::Error::Check { op, .. } if op.starts_with("WRITE") => Some(Condition::Write),
        _ => Some(Condition::Read),
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
