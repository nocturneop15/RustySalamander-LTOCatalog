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
//! A job that needs a tape the library doesn't have asks [`Ui::missing_tape`]
//! what to do. Paths here are LTFS names, `\`-separated, the tape's barcode
//! first.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lto_catalog::{Catalog, Tape};
use lto_scsi::changer::{Inventory, Kind};
use lto_scsi::library::{Devices, Lease, Library, LoadError};
use ltfs::{File, Index, Node, Time, Times, Volume};

/// What to do about a tape that isn't in the library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// It was put into the mail slot (or a magazine): look again.
    Retry,
    /// Go on without it.
    Skip,
    /// Stop the job.
    Cancel,
}

/// How a job talks to the person behind it.
pub trait Ui: Sync {
    /// Whether the person wants the job to stop.
    fn cancelled(&self) -> bool {
        false
    }
    /// The tape isn't in the library: ask for it. `skip` says whether
    /// skipping it is possible (a read of several tapes).
    fn missing_tape(&self, barcode: &str, tape: Option<&Tape>, skip: bool) -> Answer;
}

/// A Ui that asks nothing: missing tapes stop the job.
pub struct NoUi;

impl Ui for NoUi {
    fn missing_tape(&self, _: &str, _: Option<&Tape>, _: bool) -> Answer {
        Answer::Cancel
    }
}

/// Why a job failed.
#[derive(Debug)]
pub enum Error {
    /// The library couldn't be opened.
    NoLibrary(String),
    /// The tape isn't in the library and the person didn't insert it.
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
    Cancelled,
    Library(lto_scsi::Error),
    Ltfs(ltfs::Error),
    Catalog(lto_catalog::Error),
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoLibrary(m) => write!(f, "the tape library can't be used: {m}"),
            Error::NotInLibrary(b) => write!(f, "tape {b} is not in the library"),
            Error::NotInCatalog(b) => write!(f, "tape {b} is not in the catalog; import it first"),
            Error::NotLtfs(b) => write!(f, "tape {b} isn't formatted for LTFS"),
            Error::StaleCatalog(b) => write!(f, "the catalog of tape {b} was out of date; it has been updated from the tape, check the files and try again"),
            Error::Missing(p) => write!(f, "not on the tape: {}", p.join(", ")),
            Error::Skipped(b) => write!(f, "tapes skipped: {}", b.join(", ")),
            Error::NoSpace(need, free) => write!(f, "not enough space on the tape: {need} bytes needed, {free} free"),
            Error::Refused(m) => write!(f, "{m}"),
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

/// A tape library and its catalog.
pub struct Archive {
    devices: Box<dyn Devices>,
    library: Mutex<Option<Arc<Library>>>,
    pub catalog: Mutex<Catalog>,
    creator: String,
}

/// Split `BARCODE\path` into the barcode and the path inside the tape.
pub fn split_tape(path: &str) -> (String, String) {
    let parts = ltfs::index::split(path);
    match parts.split_first() {
        Some((b, rest)) => ((*b).to_owned(), rest.join("\\")),
        None => (String::new(), String::new()),
    }
}

/// A place in the library, for people.
pub fn location_text(inv: &Inventory, address: u16) -> String {
    let Some(el) = inv.at(address) else { return String::new() };
    let nth = |list: &[lto_scsi::changer::Element]| list.iter().position(|e| e.address == address).map(|i| i + 1).unwrap_or(0);
    match el.kind {
        Kind::Slot => format!("slot {}", nth(&inv.slots)),
        Kind::Drive => format!("drive {}", nth(&inv.drives)),
        Kind::MailSlot => "mail slot".into(),
        Kind::Transport => "robot".into(),
    }
}

impl Archive {
    pub fn new(devices: Box<dyn Devices>, catalog: Catalog, creator: &str) -> Archive {
        Archive { devices, library: Mutex::new(None), catalog: Mutex::new(catalog), creator: creator.to_owned() }
    }

    /// The library, opened the first time it is needed.
    pub fn library(&self) -> Result<Arc<Library>> {
        let mut l = self.library.lock().unwrap();
        if let Some(lib) = &*l {
            return Ok(lib.clone());
        }
        let lib = Arc::new(Library::open(&*self.devices).map_err(|e| Error::NoLibrary(e.to_string()))?);
        *l = Some(lib.clone());
        drop(l);
        // where everything is, at once
        let inv = lib.inventory();
        let _ = self.record_locations(&inv);
        Ok(lib)
    }

    /// The library if it is open already (for status lines).
    pub fn library_if_open(&self) -> Option<Arc<Library>> {
        self.library.lock().unwrap().clone()
    }

    /// Close the library (it is opened again when needed, e.g. after the
    /// settings changed).
    pub fn close_library(&self) {
        *self.library.lock().unwrap() = None;
    }

    fn record_locations(&self, inv: &Inventory) -> Result<()> {
        let locs: Vec<(String, String)> = inv.all().filter(|e| e.full && e.kind != Kind::Transport).filter_map(|e| Some((e.barcode.clone()?, location_text(inv, e.address)))).collect();
        self.catalog.lock().unwrap().set_locations(&locs)?;
        Ok(())
    }

    /// Read the library's inventory again and record where every tape is.
    pub fn inventory(&self, rescan: bool) -> Result<Inventory> {
        let lib = self.library()?;
        let inv = lib.refresh(rescan)?;
        self.record_locations(&inv)?;
        Ok(inv)
    }

    /// Load `barcode`, asking for it while it isn't in the library.
    fn acquire<'l>(&self, lib: &'l Library, barcode: &str, ui: &dyn Ui, skip: bool) -> Result<Option<Lease<'l>>> {
        loop {
            match lib.acquire(barcode, &|| ui.cancelled()) {
                Ok(l) => {
                    let inv = lib.inventory();
                    let _ = self.record_locations(&inv);
                    return Ok(Some(l));
                }
                Err(LoadError::Cancelled) => return Err(Error::Cancelled),
                Err(LoadError::Scsi(e)) => return Err(e.into()),
                Err(LoadError::NotInLibrary(_)) => {
                    // the library looked again: record where everything is
                    let _ = self.record_locations(&lib.inventory());
                    let tape = self.catalog.lock().unwrap().tape(barcode)?;
                    match ui.missing_tape(barcode, tape.as_ref(), skip) {
                        Answer::Retry => {
                            let inv = lib.refresh(false)?;
                            self.record_locations(&inv)?;
                        }
                        Answer::Skip if skip => return Ok(None),
                        Answer::Skip | Answer::Cancel => return Err(if ui.cancelled() { Error::Cancelled } else { Error::NotInLibrary(barcode.to_owned()) }),
                    }
                }
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
        let lib = self.library()?;
        let mut lease = self.acquire(&lib, barcode, ui, false)?.expect("no skip");
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
        let lib = self.library()?;
        let mut lease = self.acquire(&lib, barcode, ui, false)?.expect("no skip");
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
        let lib = self.library()?;
        let loaded: Vec<String> = lib.drives.iter().filter_map(|d| d.tape()).map(|t| t.to_uppercase()).collect();
        let mut order: Vec<String> = by_tape.keys().cloned().collect();
        order.sort_by_key(|b| !loaded.contains(b));
        let (mut missing, mut skipped) = (Vec::new(), Vec::new());
        for barcode in order {
            if ui.cancelled() {
                return Err(Error::Cancelled);
            }
            let paths = &by_tape[&barcode];
            let Some(mut lease) = self.acquire(&lib, &barcode, ui, true)? else {
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
    pub fn format(&self, barcode: &str, name: &str, ui: &dyn Ui) -> Result<Index> {
        let lib = self.library()?;
        let mut lease = self.acquire(&lib, barcode, ui, false)?.expect("no skip");
        let drive = lease.drive();
        let f = ltfs::format(drive, barcode, name, &self.creator)?;
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
