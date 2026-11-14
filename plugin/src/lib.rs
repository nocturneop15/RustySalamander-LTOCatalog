//! The LTO Catalog plugin: LTFS tapes in a tape library (a TS3100 with LTO
//! drives, or any SCSI medium changer) as folders, browsed from an offline
//! catalog without loading a tape. Copying out of a tape or onto one has the
//! library's robot load it; a tape that isn't in the library is asked for.
//!
//! The connection `lto://library` lists the tapes by barcode; inside each,
//! its LTFS volume as the catalog last saw it. Names LTFS allows but Windows
//! doesn't are shown with those characters in the private-use area
//! ([`ltfs::names`]).

mod ask;

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use lto_archive::{Answer, Archive, ReadSink, Step, Ui};
use lto_catalog::{Catalog, Tape};
use lto_scsi::library::Devices;
use ltfs::names::{from_windows, safe_relative, to_windows};
use ltfs::Time;
use lto_catalog::place::{self, Settings};
use lto_scsi::site::{Ejected, Holds, Places};
use sal_plugin_api::ui::decode;
use sal_plugin_api::{
    export_plugin, join_inner, mount_of, n_, tr, trf, Command, Field, FieldKind, FolderPlugin, Form, Index, Item, Mount, Plugin, Progress, Reply, Results, Sink, Values, ATTR_DIRECTORY,
};
use lto_catalog::{Find, Found};

/// The most a catalog search finds.
const SEARCH_LIMIT: usize = 10_000;

/// The word typed to confirm formatting tapes.
const ERASE: &str = "ERASE";

/// The connection's scheme and authority: `lto://library`.
const SCHEME: &str = "lto";
const AUTHORITY: &str = "library";
const ATTR_READONLY: u32 = 0x1;

/// Answers questions about missing tapes in tests: (barcode, skip possible,
/// where it can be put).
type Asker = Box<dyn Fn(&str, bool, &Places) -> Answer + Send + Sync>;

pub struct LtoCatalog {
    data: PathBuf,
    settings: Mutex<Settings>,
    archive: Mutex<Arc<Archive>>,
    /// Windows for questions while a job runs.
    asks: sal_window::Windows,
    /// The program's look, from the last command (for the question windows).
    look: Mutex<Values>,
    asker: Option<Asker>,
    /// The command running now (the program shows its step, with Cancel).
    cmd: CommandState,
    /// Catalog searches, by their connection's authority (`search-3`).
    searches: Mutex<BTreeMap<String, Arc<Vec<Found>>>>,
    next_search: std::sync::atomic::AtomicUsize,
}

/// The state of the command [`LtoCatalog::run`] is running.
#[derive(Default)]
struct CommandState {
    running: AtomicBool,
    /// Cancel was pressed: the command stops at the next point it can.
    cancel: AtomicBool,
    /// What it is doing, translated.
    step: Mutex<String>,
}

/// Marks a command as running while it lives.
struct Running<'a>(&'a LtoCatalog);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.cmd.running.store(false, Ordering::Relaxed);
        self.0.cmd.step.lock().unwrap().clear();
    }
}

/// A step of a job in words.
fn step_text(step: Step) -> String {
    match step {
        Step::Devices => tr("looking for tape libraries and drives…"),
        Step::Loading(b) => trf("{barcode}: getting it into a drive…", &[("barcode", &b)]),
        Step::Mounting(b, d) => trf("{barcode} is in {drive}: reading its LTFS volume…", &[("barcode", &b), ("drive", &d)]),
        Step::Formatting(b) => trf("{barcode}: formatting (a few minutes; it can't be stopped halfway)…", &[("barcode", &b)]),
        Step::Reading(b, p) => trf("{barcode}: reading {path}", &[("barcode", &b), ("path", &to_inner(p))]),
        Step::Writing(b, p) => trf("{barcode}: writing {path}", &[("barcode", &b), ("path", &to_inner(p))]),
        Step::WritingIndex(b) => trf("{barcode}: writing its index…", &[("barcode", &b)]),
    }
}

export_plugin!(LtoCatalog);

/// A list in a setting: comma- or semicolon-separated.
fn list(s: &str) -> Vec<String> {
    s.split([',', ';']).map(|d| d.trim().to_owned()).filter(|d| !d.is_empty()).collect()
}

/// A number from an environment variable.
fn env_count(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// The tape devices: the simulator when `LTOCATALOG_SIM` is set (`1` for
/// files in the plugin's data folder, or the first library's file), with
/// `LTOCATALOG_SIM_LIBRARIES` libraries (1) and `LTOCATALOG_SIM_DRIVES`
/// standalone drives (0); else the hardware.
fn devices(data: &Path, s: &Settings) -> Result<Box<dyn Devices>, String> {
    if let Some(v) = std::env::var_os("LTOCATALOG_SIM").filter(|v| !v.is_empty()) {
        let file = if v == "1" { data.join("sim-library.bin") } else { PathBuf::from(v) };
        let (libraries, drives) = (env_count("LTOCATALOG_SIM_LIBRARIES", 1), env_count("LTOCATALOG_SIM_DRIVES", 0));
        return lto_sim::SimSite::persistent(&file, libraries, drives).map(|l| Box::new(l) as Box<dyn Devices>).map_err(|e| format!("{}: {e}", file.display()));
    }
    Ok(Box::new(lto_scsi::windows::WindowsDevices { changers: list(&s.changer), drives: list(&s.drives) }))
}

fn open_archive(data: &Path, s: &Settings) -> Result<Archive, String> {
    let path = s.catalog_path(data);
    if let Some(d) = path.parent() {
        let _ = fs::create_dir_all(d);
    }
    let cat = Catalog::open(&path).map_err(|e| trf("The catalog {file} can't be opened: {error}", &[("file", &path.display()), ("error", &e)]))?;
    Ok(Archive::new(devices(data, s)?, cat, &ltfs::creator()))
}

impl Plugin for LtoCatalog {
    const ID: &'static str = "ltocatalog";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");
    const NAME: &'static str = n_("LTO tape catalog");
    const DESCRIPTION: &'static str = n_("LTFS tapes in an LTO tape library, browsed from an offline catalog; the library loads a tape when files are copied from or to it. Start with Plugins → Open tape catalog.");
    // windows drawn by the program (interface 9); with older programs the
    // insert-tape question can't be shown
    const PROGRAM_MIN: &'static str = "0.9.4-beta";
    // the program's update check: releases `ltocatalog-v<version>` of this
    // repository, with `ltocatalog-<version>-api<n>.zip`
    const UPDATES: sal_plugin_api::Updates =
        sal_plugin_api::Updates { repo: "nocturneop15/RustySalamander-LTOCatalog", tag: "ltocatalog-v", changelog: "CHANGELOG.md" };

    fn start(dir: &Path) -> Result<Self, String> {
        // its own folder, or %APPDATA% in multi-user mode (lto_catalog::place)
        let data = sal_plugin_api::data_dir(dir);
        let settings = Settings::load(&place::settings_file(&data));
        let archive = open_archive(&data, &settings)?;
        Ok(LtoCatalog::with(data, settings, archive, None))
    }
}

impl LtoCatalog {
    fn with(data: PathBuf, settings: Settings, archive: Archive, asker: Option<Asker>) -> LtoCatalog {
        LtoCatalog {
            data,
            settings: Mutex::new(settings),
            archive: Mutex::new(Arc::new(archive)),
            asks: sal_window::Windows::new(sal_ui::icon::program, [480.0, 210.0]),
            look: Mutex::new(Values::new()),
            asker,
            cmd: CommandState::default(),
            searches: Mutex::new(BTreeMap::new()),
            next_search: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The results of the catalog search `file` is in, if it is one.
    fn search_of(&self, file: &Path) -> Option<Arc<Vec<Found>>> {
        let (s, a, _) = mount_of(file)?;
        if s != SCHEME {
            return None;
        }
        self.searches.lock().unwrap().get(&a).cloned()
    }

    /// Whether files can be read from `file`: the catalog, or a search's results.
    fn readable(&self, file: &Path) -> bool {
        is_ours(file) || self.search_of(file).is_some()
    }

    fn search_form(&self, values: &Values) -> Reply {
        let chosen = chosen_tapes(values);
        let choice = |id: &str, label: String| (id.to_owned(), label);
        Reply::Form(Form {
            title: tr("Search the catalog"),
            ok: tr("Search"),
            command: "search_do".into(),
            fields: vec![
                field("text", tr("Name"), FieldKind::Text, values.get("text").map(String::as_str).unwrap_or("")),
                field("match", tr("Look for"), FieldKind::Choice(vec![choice("contains", tr("names containing it")), choice("mask", tr("names matching a mask (*.mkv)"))]), "contains"),
                field("tape", tr("Only on tape"), FieldKind::Text, if chosen.len() == 1 { &chosen[0] } else { "" }),
                note(tr("A mask: * any characters, ? one. Only on tape: a barcode, or empty for every tape. The catalog is searched; no tape is loaded. Found files are copied out of the results like out of the tapes.")),
            ],
        })
    }

    /// A catalog search: its results in RustySalamander's results window.
    fn cmd_search(&self, values: &Values) -> io::Result<Reply> {
        let text = values.get("text").map(|t| t.trim()).unwrap_or("");
        if text.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, tr("Type a name, or a part of one, to look for.")));
        }
        if text.chars().any(char::is_control) || text.chars().count() > 256 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, tr("A name to look for has at most 256 characters, without line breaks.")));
        }
        let tape = typed_barcode(values, "tape")?;
        let what = if values.get("match").map(String::as_str) == Some("mask") { Find::Mask(text) } else { Find::Contains(text) };
        let found = self.archive().catalog.lock().unwrap().find(&what, (!tape.is_empty()).then_some(tape.as_str()), SEARCH_LIMIT).map_err(|e| io::Error::other(e.to_string()))?;
        if found.is_empty() {
            return Ok(Reply::Message(trf("Nothing in the catalog is named like \"{text}\".", &[("text", &text)])));
        }
        let n = found.len();
        let authority = format!("search-{}", self.next_search.fetch_add(1, Ordering::Relaxed) + 1);
        self.searches.lock().unwrap().insert(authority.clone(), Arc::new(found));
        let title = if n == SEARCH_LIMIT {
            trf("Tape catalog: \"{text}\" (the first {n} found)", &[("text", &text), ("n", &n)])
        } else {
            trf("Tape catalog: \"{text}\" ({n} found)", &[("text", &text), ("n", &n)])
        };
        Ok(Reply::Results(Results {
            title,
            name: trf("tapes: {text}", &[("text", &text)]),
            mount: Mount { scheme: SCHEME.into(), authority, folder: String::new() },
            flat: String::new(),
            again: "search".into(),
            refresh: String::new(),
        }))
    }

    /// A command starts: it runs until the returned guard is dropped,
    /// saying `what` until a job says more.
    fn begin(&self, what: String) -> Running<'_> {
        self.cmd.cancel.store(false, Ordering::Relaxed);
        *self.cmd.step.lock().unwrap() = what;
        self.cmd.running.store(true, Ordering::Relaxed);
        Running(self)
    }

    /// What the running command does now.
    fn say(&self, what: String) {
        *self.cmd.step.lock().unwrap() = what;
    }

    /// The job interface of the running command (Cancel in the program
    /// stops it).
    fn command_ui(&self) -> JobUi<'_> {
        JobUi { plugin: self, cancelled: &self.cmd.cancel }
    }

    fn archive(&self) -> Arc<Archive> {
        self.archive.lock().unwrap().clone()
    }

    /// Ask for a missing tape, saying where it can be put.
    fn ask_tape(&self, barcode: &str, tape: Option<&Tape>, skip: bool, places: &Places) -> Answer {
        if let Some(a) = &self.asker {
            return a(barcode, skip, places);
        }
        let (text, mut detail) = ask_text(barcode, places);
        if let Some(t) = tape {
            if !t.note.is_empty() {
                detail.push(trf("Note: {note}", &[("note", &t.note)]));
            }
            if t.is_ltfs() && !t.name.is_empty() {
                detail.push(trf("Volume: {name}", &[("name", &t.name)]));
            }
        }
        let mut buttons = vec![tr("Retry")];
        if skip {
            buttons.push(tr("Skip this tape"));
        }
        buttons.push(tr("Cancel"));
        let cancel = buttons.len() - 1;
        let look = self.look.lock().unwrap().clone();
        match ask::ask(&self.asks, look, tr("Insert tape"), text, detail.join("\n"), buttons, 0, cancel) {
            Some(0) => Answer::Retry,
            Some(1) if skip => Answer::Skip,
            _ => Answer::Cancel,
        }
    }
}

/// What a standalone drive holds, for people.
fn holds_text(h: &Holds) -> String {
    match h {
        Holds::Empty => tr("empty"),
        Holds::Tape(b) => b.clone(),
        Holds::Unknown => tr("a tape that can't be identified"),
    }
}

/// The question for a missing tape: where to put it, and what the
/// standalone drives hold (after one was emptied to make room).
fn ask_text(barcode: &str, places: &Places) -> (String, Vec<String>) {
    let libs = places.libraries.join(", ");
    let drives = places.drives.iter().map(|(d, _)| d.as_str()).collect::<Vec<_>>().join(", ");
    let text = match (places.libraries.len(), places.drives.len()) {
        (1, 0) => trf("Tape {barcode} is not in the library. Put it into the mail slot (or a magazine), then press Retry.", &[("barcode", &barcode)]),
        (_, 0) => trf("Tape {barcode} is in none of the libraries. Put it into the mail slot of one of them ({libraries}), then press Retry.", &[("barcode", &barcode), ("libraries", &libs)]),
        (0, 1) => trf("Tape {barcode} is not in the drive. Insert it into drive {drive}, then press Retry.", &[("barcode", &barcode), ("drive", &drives)]),
        (0, _) => trf("Tape {barcode} is in none of the drives. Insert it into one of them ({drives}), then press Retry.", &[("barcode", &barcode), ("drives", &drives)]),
        _ => trf("Tape {barcode} is in no library or drive. Put it into the mail slot of {libraries}, or insert it into drive {drives}, then press Retry.", &[("barcode", &barcode), ("libraries", &libs), ("drives", &drives)]),
    };
    let mut detail = Vec::new();
    if let Some((d, h)) = &places.ejected {
        detail.push(match h {
            Holds::Tape(b) => trf("Tape {tape} was ejected from drive {drive} to make room.", &[("tape", b), ("drive", d)]),
            _ => trf("The tape in drive {drive} was ejected to make room.", &[("drive", d)]),
        });
    }
    for (d, h) in &places.drives {
        match h {
            Holds::Tape(b) => detail.push(trf("Drive {drive} holds tape {tape}.", &[("drive", d), ("tape", b)])),
            Holds::Unknown => detail.push(trf("Drive {drive} holds a tape that can't be identified: no barcode in its memory, and its volume isn't in the catalog.", &[("drive", d)])),
            Holds::Empty => {}
        }
    }
    (text, detail)
}

/// The plugin as a job's [`Ui`].
struct JobUi<'a> {
    plugin: &'a LtoCatalog,
    cancelled: &'a AtomicBool,
}

impl Ui for JobUi<'_> {
    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
    fn step(&self, step: Step) {
        self.plugin.say(step_text(step));
    }
    fn missing_tape(&self, barcode: &str, tape: Option<&Tape>, skip: bool, places: &Places) -> Answer {
        let a = self.plugin.ask_tape(barcode, tape, skip, places);
        if a == Answer::Cancel {
            self.cancelled.store(true, Ordering::Relaxed);
        }
        a
    }
}

/// An inner path (Windows names, the barcode first) as an LTFS path.
fn to_ltfs(inner: &str) -> String {
    let parts = ltfs::index::split(inner);
    let mut out: Vec<String> = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        out.push(if i == 0 { p.to_uppercase() } else { from_windows(p) });
    }
    out.join("\\")
}

/// An LTFS path (the barcode first) as an inner path.
fn to_inner(path: &str) -> String {
    ltfs::index::split(path).into_iter().enumerate().map(|(i, p)| if i == 0 { p.to_owned() } else { to_windows(p) }).collect::<Vec<_>>().join("\\")
}

fn is_ours(file: &Path) -> bool {
    mount_of(file).is_some_and(|(s, a, _)| s == SCHEME && a == AUTHORITY)
}

fn not_ours() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, tr("This isn't the tape catalog."))
}

/// A job's error as the program shows it.
fn io_err(e: lto_archive::Error) -> io::Error {
    use lto_archive::Error as E;
    use io::ErrorKind as K;
    let (kind, msg) = match e {
        E::Cancelled => return io::Error::new(K::Interrupted, tr("Cancelled.")),
        E::NotInLibrary(b) => (K::NotFound, trf("Tape {barcode} is in no library or tape drive.", &[("barcode", &b)])),
        E::NotInCatalog(b) => (K::NotFound, trf("Tape {barcode} is not in the catalog yet. Import it first: Plugins → Import tapes….", &[("barcode", &b)])),
        E::NotLtfs(b) => (K::InvalidData, trf("Tape {barcode} isn't formatted for LTFS. Format it first: Plugins → Format a tape….", &[("barcode", &b)])),
        E::StaleCatalog(b) => (
            K::Other,
            trf("The catalog of tape {barcode} was out of date and has been updated from the tape. Nothing was changed; check the files and try again.", &[("barcode", &b)]),
        ),
        E::Missing(list) => (K::NotFound, trf("Not on the tape: {items}", &[("items", &list.iter().map(|p| to_inner(p)).collect::<Vec<_>>().join(", "))])),
        E::Skipped(list) => (K::Other, trf("These tapes were skipped, their files weren't copied: {tapes}", &[("tapes", &list.join(", "))])),
        E::NoSpace(need, free) => (K::StorageFull, trf("Not enough space on the tape: {need} bytes needed, {free} bytes free.", &[("need", &need), ("free", &free)])),
        E::NoLibrary(m) => (K::NotFound, trf("No tape library or drive can be used: {reason}", &[("reason", &m)])),
        E::WrongTape { wanted, found, drive } => {
            let text = trf("Drive {drive} was to hold tape {wanted}, but the cartridge in it says it is {found}. Nothing was done to it. The library's drives may be matched to the wrong tape devices, or tapes were moved at the library's panel: run Inventory and try again; if it happens again, send the output of \"ltocat selftest\".", &[("drive", &drive), ("wanted", &wanted), ("found", &found)]);
            (K::InvalidData, text)
        }
        E::Io(e) => return e,
        E::Ltfs(ltfs::Error::Full) => (K::StorageFull, tr("The tape is full. The files copied before it filled up are on it.")),
        E::Ltfs(ltfs::Error::ReadOnly(m)) => (K::PermissionDenied, trf("The tape can't be written: {reason}", &[("reason", &m)])),
        other => (K::Other, other.to_string()),
    };
    io::Error::new(kind, msg)
}

fn refused(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, tr(msg))
}

/// Writes files read from tapes into a folder on disk.
struct ToDisk<'a, 'p> {
    out: &'a Path,
    file: Option<(fs::File, Time)>,
    done: u64,
    total: u64,
    progress: &'a mut Progress<'p>,
    cancelled: &'a AtomicBool,
}

impl ToDisk<'_, '_> {
    fn target(&self, path: &str) -> io::Result<PathBuf> {
        let inner = to_inner(path);
        if !safe_relative(&inner) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, trf("A name on the tape can't be used on disk: {name}", &[("name", &path)])));
        }
        Ok(self.out.join(inner))
    }

    fn report(&mut self) -> bool {
        let go = (self.progress)(self.done, self.total);
        if !go {
            self.cancelled.store(true, Ordering::Relaxed);
        }
        go
    }
}

impl ReadSink for ToDisk<'_, '_> {
    fn dir(&mut self, path: &str, _: Time) -> io::Result<()> {
        fs::create_dir_all(self.target(path)?)
    }
    fn begin(&mut self, path: &str, _size: u64, modified: Time) -> io::Result<bool> {
        let t = self.target(path)?;
        if let Some(p) = t.parent() {
            fs::create_dir_all(p)?;
        }
        self.file = Some((fs::File::create(&t)?, modified));
        Ok(self.report())
    }
    fn write(&mut self, data: &[u8]) -> io::Result<bool> {
        self.file.as_mut().expect("begun").0.write_all(data)?;
        self.done += data.len() as u64;
        Ok(self.report())
    }
    fn end(&mut self) -> io::Result<()> {
        if let Some((f, m)) = self.file.take() {
            let _ = f.set_modified(m.to_system());
        }
        Ok(())
    }
}

/// Gives files read from tapes to the program's sink (dragging out).
struct ToSink<'a> {
    sink: &'a mut dyn Sink,
    cancelled: &'a AtomicBool,
}

impl ReadSink for ToSink<'_> {
    fn dir(&mut self, _: &str, _: Time) -> io::Result<()> {
        Ok(())
    }
    fn begin(&mut self, path: &str, size: u64, _: Time) -> io::Result<bool> {
        Ok(self.sink.begin(&to_inner(path), size))
    }
    fn write(&mut self, data: &[u8]) -> io::Result<bool> {
        let go = self.sink.write(data);
        if !go {
            self.cancelled.store(true, Ordering::Relaxed);
        }
        Ok(go)
    }
    fn end(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The barcodes of the tapes selected in the panel (or under the cursor),
/// when it shows the catalog.
fn chosen_tapes(values: &Values) -> Vec<String> {
    let mut paths: Vec<String> = values.get("selected").and_then(|s| decode(s)).unwrap_or_default();
    if paths.is_empty() {
        paths.extend(values.get("cursor").filter(|c| !c.is_empty()).cloned());
    }
    let mut out = Vec::new();
    for p in paths {
        if let Some((s, a, inner)) = mount_of(Path::new(&p)) {
            if s == SCHEME && a == AUTHORITY {
                if let Some(b) = ltfs::index::split(&inner).first() {
                    let b = b.to_uppercase();
                    if !out.contains(&b) {
                        out.push(b);
                    }
                }
            }
        }
    }
    out
}

fn field(id: &str, label: String, kind: FieldKind, value: &str) -> Field {
    Field { id: id.into(), label, kind, value: value.into(), enabled_if: None }
}

fn note(text: String) -> Field {
    Field { id: String::new(), label: text, kind: FieldKind::Note, value: String::new(), enabled_if: None }
}

fn size_text(b: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Barcodes typed in a field, separated by commas or spaces.
fn barcodes_in(text: &str) -> Vec<String> {
    text.split([',', ';', ' ', '\t', '\n']).map(|s| s.trim().to_uppercase()).filter(|s| !s.is_empty()).collect()
}

/// Whether `b` can be a cartridge's barcode: letters A–Z and digits, at
/// most 16 (LTO labels have 8, such as LTO001L5). Anything else would end
/// up in the catalog, in tape paths and on the tape.
fn barcode_ok(b: &str) -> bool {
    b.len() <= 16 && b.chars().all(|c| c.is_ascii_alphanumeric())
}

fn bad_barcode(b: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, trf("\"{barcode}\" isn't a barcode: a barcode has letters A–Z and digits only, at most 16, such as LTO001L5.", &[("barcode", &b.chars().take(40).collect::<String>())]))
}

/// The barcode typed into the field `key`, upper case; empty if none.
fn typed_barcode(values: &Values, key: &str) -> io::Result<String> {
    let b = values.get(key).map(|b| b.trim().to_uppercase()).unwrap_or_default();
    if barcode_ok(&b) {
        Ok(b)
    } else {
        Err(bad_barcode(&b))
    }
}

/// Cleaning cartridges have barcodes starting with CLN.
fn is_cleaning(barcode: &str) -> bool {
    barcode.to_uppercase().starts_with("CLN")
}

impl LtoCatalog {
    fn mount() -> Reply {
        Reply::Mount(Mount { scheme: SCHEME.into(), authority: AUTHORITY.into(), folder: String::new() })
    }

    fn cmd_inventory(&self) -> io::Result<Reply> {
        let a = self.archive();
        // libraries and drives connected since are found too
        a.reopen_if_idle();
        a.inventory(false).map_err(io_err)?;
        let tapes = a.catalog.lock().unwrap().tapes().map_err(|e| io::Error::other(e.to_string()))?;
        let inside: Vec<&Tape> = tapes.iter().filter(|t| t.in_library() && !is_cleaning(&t.barcode)).collect();
        let new: Vec<&str> = inside.iter().filter(|t| t.imported.is_none()).map(|t| t.barcode.as_str()).collect();
        let mut msg = trf("Tapes in the libraries and drives: {n}.", &[("n", &inside.len())]);
        if !new.is_empty() {
            msg.push('\n');
            msg.push_str(&trf("Not in the catalog yet: {tapes}. Use Import tapes… to read them.", &[("tapes", &new.join(", "))]));
        }
        Ok(Reply::Message(msg))
    }

    fn cmd_import_form(&self, values: &Values) -> Reply {
        let chosen = chosen_tapes(values);
        let which = if chosen.is_empty() { "new" } else { "list" };
        Reply::Form(Form {
            title: tr("Import tapes"),
            ok: tr("Import"),
            command: "import_do".into(),
            fields: vec![
                note(tr("Each tape is loaded and its index read into the catalog. This takes a minute or two per tape.")),
                field(
                    "which",
                    tr("Tapes"),
                    FieldKind::Choice(vec![
                        ("new".into(), tr("Tapes in the library that aren't in the catalog yet")),
                        ("all".into(), tr("Every tape in the library")),
                        ("list".into(), tr("These tapes:")),
                    ]),
                    which,
                ),
                Field { enabled_if: Some(("which".into(), "list".into())), ..field("barcodes", tr("Barcodes"), FieldKind::Text, &chosen.join(", ")) },
            ],
        })
    }

    fn cmd_import(&self, values: &Values) -> io::Result<Reply> {
        let a = self.archive();
        let which = values.get("which").map(String::as_str).unwrap_or("new");
        let barcodes: Vec<String> = if which == "list" {
            let list = barcodes_in(values.get("barcodes").map(String::as_str).unwrap_or(""));
            if let Some(bad) = list.iter().find(|b| !barcode_ok(b)) {
                return Err(bad_barcode(bad));
            }
            list
        } else {
            a.inventory(false).map_err(io_err)?;
            let tapes = a.catalog.lock().unwrap().tapes().map_err(|e| io::Error::other(e.to_string()))?;
            tapes.into_iter().filter(|t| t.in_library() && !is_cleaning(&t.barcode) && (which == "all" || t.imported.is_none())).map(|t| t.barcode).collect()
        };
        if barcodes.is_empty() {
            return Ok(Reply::Message(tr("There are no tapes to import.")));
        }
        let ui = self.command_ui();
        let mut lines = Vec::new();
        let total = barcodes.len();
        for (n, b) in barcodes.iter().enumerate() {
            if ui.cancelled() {
                lines.push(trf("Cancelled: {n} tapes weren't imported.", &[("n", &(total - n))]));
                break;
            }
            self.say(trf("tape {n} of {total}: {barcode}", &[("n", &(n + 1)), ("total", &total), ("barcode", b)]));
            lines.push(match a.import(b, &ui) {
                Ok(Some(ix)) => {
                    let (n, s) = ix.totals();
                    trf("{barcode}: {files} files, {size}", &[("barcode", &b), ("files", &n), ("size", &size_text(s))])
                }
                Ok(None) => trf("{barcode}: no LTFS volume", &[("barcode", &b)]),
                Err(e) => format!("{b}: {}", io_err(e)),
            });
        }
        Ok(Reply::Message(lines.join("\n")))
    }

    /// The tapes in the libraries and drives, each to tick (those selected
    /// in the panel ticked), a barcode for another, the volume name and the
    /// confirmation.
    fn cmd_format_form(&self, values: &Values) -> io::Result<Reply> {
        let chosen = chosen_tapes(values);
        let mut fields = vec![note(tr("Formatting erases everything on the tapes and makes each an empty LTFS volume."))];
        let a = self.archive();
        match a.inventory(false) {
            Ok(_) => {
                let tapes = a.catalog.lock().unwrap().tapes().map_err(|e| io::Error::other(e.to_string()))?;
                let inside: Vec<&Tape> = tapes.iter().filter(|t| t.in_library() && !is_cleaning(&t.barcode)).collect();
                if inside.is_empty() {
                    fields.push(note(tr("No tape is in a library or drive now.")));
                }
                for t in inside {
                    let what = if t.is_ltfs() {
                        trf("LTFS, {files} files, {size}", &[("files", &t.files), ("size", &size_text(t.bytes))])
                    } else if t.imported.is_some() {
                        tr("no LTFS volume")
                    } else {
                        tr("not imported")
                    };
                    let ticked = if chosen.contains(&t.barcode.to_uppercase()) { "1" } else { "" };
                    fields.push(field(&format!("tape:{}", t.barcode), format!("{}  ·  {}  ·  {what}", t.barcode, t.location), FieldKind::Check, ticked));
                }
            }
            Err(e) => fields.push(note(io_err(e).to_string())),
        }
        let listed = |b: &String| fields.iter().any(|f| f.id == format!("tape:{b}"));
        let other = chosen.iter().find(|b| !listed(b)).cloned().unwrap_or_default();
        fields.push(field("barcode", tr("Another tape (barcode)"), FieldKind::Text, &other));
        fields.push(field("name", tr("Volume name"), FieldKind::Text, ""));
        fields.push(note(tr("Another tape: one that isn't listed, such as a new one in a standalone drive. Volume name: empty names each tape by its barcode.")));
        fields.push(field("confirm", trf("Type {word} to confirm", &[("word", &ERASE)]), FieldKind::Text, ""));
        Ok(Reply::Form(Form { title: tr("Format tapes"), ok: tr("Format"), command: "format_do".into(), fields }))
    }

    fn cmd_format(&self, values: &Values) -> io::Result<Reply> {
        // the tapes ticked, and the one typed
        let mut list: Vec<String> = values.iter().filter(|(k, v)| k.starts_with("tape:") && !v.is_empty()).map(|(k, _)| k[5..].to_uppercase()).collect();
        if let Some(bad) = list.iter().find(|b| !barcode_ok(b)) {
            return Err(bad_barcode(bad));
        }
        let typed = typed_barcode(values, "barcode")?;
        if !typed.is_empty() && !list.contains(&typed) {
            list.push(typed);
        }
        list.sort();
        if list.is_empty() {
            return Ok(Reply::Message(tr("Nothing was done: no tape was ticked or typed.")));
        }
        // ERASE, or for one tape its barcode again
        let confirm = values.get("confirm").map(|b| b.trim().to_uppercase()).unwrap_or_default();
        if confirm != ERASE && !(list.len() == 1 && confirm == list[0]) {
            return Ok(Reply::Message(trf("Nothing was done: type {word} to confirm.", &[("word", &ERASE)])));
        }
        // the name goes into the tape's index (XML), which can't hold control characters
        let name = values.get("name").map(|n| n.trim()).unwrap_or("");
        if name.chars().any(char::is_control) || name.chars().count() > 256 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, tr("A volume name can have at most 256 characters, without line breaks or other control characters.")));
        }
        // no name: each tape is named by its barcode
        let name_of = |barcode: &str| if name.is_empty() { barcode.to_owned() } else { name.to_owned() };
        let ui = self.command_ui();
        if let [barcode] = &list[..] {
            let ix = self.archive().format(barcode, &name_of(barcode), &ui).map_err(io_err)?;
            return Ok(Reply::Message(trf("Tape {barcode} is now an empty LTFS volume ({uuid}).", &[("barcode", barcode), ("uuid", &ix.volume_uuid)])));
        }
        let (total, mut lines) = (list.len(), Vec::new());
        for (n, b) in list.iter().enumerate() {
            if ui.cancelled() {
                lines.push(trf("Cancelled: {n} tapes weren't formatted.", &[("n", &(total - n))]));
                break;
            }
            self.say(trf("tape {n} of {total}: {barcode}", &[("n", &(n + 1)), ("total", &total), ("barcode", b)]));
            lines.push(match self.archive().format(b, &name_of(b), &ui) {
                Ok(_) => trf("{barcode}: an empty LTFS volume", &[("barcode", b)]),
                Err(e) => format!("{b}: {}", io_err(e)),
            });
        }
        Ok(Reply::Message(lines.join("\n")))
    }

    fn one_tape_form(&self, values: &Values, title: String, ok: String, command: &str, extra: Vec<Field>) -> Reply {
        let chosen = chosen_tapes(values);
        let mut fields = vec![field("barcode", tr("Barcode"), FieldKind::Text, chosen.first().map(String::as_str).unwrap_or(""))];
        fields.extend(extra);
        Reply::Form(Form { title, ok, command: command.into(), fields })
    }

    fn cmd_eject(&self, values: &Values) -> io::Result<Reply> {
        let b = typed_barcode(values, "barcode")?;
        let a = self.archive();
        let site = a.inventory(false).map_err(io_err)?;
        let out = site.eject(&b).map_err(|e| match e {
            lto_scsi::library::LoadError::NotInLibrary(b) => io_err(lto_archive::Error::NotInLibrary(b)),
            other => io::Error::other(other.to_string()),
        })?;
        a.inventory(false).map_err(io_err)?;
        Ok(Reply::Message(match out {
            Ejected::MailSlot(_) => trf("Tape {barcode} is in the mail slot.", &[("barcode", &b)]),
            Ejected::Drive(d) => trf("Tape {barcode} came out of drive {drive}.", &[("barcode", &b), ("drive", &d)]),
        }))
    }

    fn cmd_note(&self, values: &Values) -> io::Result<Reply> {
        let b = typed_barcode(values, "barcode")?;
        if b.is_empty() {
            return Ok(Reply::Done);
        }
        // one line of text, as the status shows it
        let note: String = values.get("note").map(|n| n.trim()).unwrap_or("").chars().map(|c| if c.is_control() { ' ' } else { c }).take(1000).collect();
        self.archive().catalog.lock().unwrap().set_note(&b, &note).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Reply::Done)
    }

    fn cmd_forget(&self, values: &Values) -> io::Result<Reply> {
        let b = typed_barcode(values, "barcode")?;
        let confirm = values.get("confirm").map(|b| b.trim().to_uppercase()).unwrap_or_default();
        if b.is_empty() || b != confirm {
            return Ok(Reply::Message(tr("Nothing was done: the barcode typed again doesn't match.")));
        }
        self.archive().catalog.lock().unwrap().forget(&b).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Reply::Message(trf("Tape {barcode} is no longer in the catalog. The tape itself is unchanged.", &[("barcode", &b)])))
    }

    fn cmd_status(&self) -> io::Result<Reply> {
        let a = self.archive();
        let mut lines = Vec::new();
        a.reopen_if_idle();
        match a.inventory(false) {
            Ok(site) => {
                for lib in &site.libraries {
                    lines.push(trf("Library: {vendor} {product} ({device})", &[("vendor", &lib.identity.vendor), ("product", &lib.name), ("device", &lib.changer_name)]));
                    for (i, d) in lib.drives.iter().enumerate() {
                        let holds = d.tape().unwrap_or_else(|| tr("empty"));
                        lines.push(trf("Drive {n}: {device}, serial {serial}: {tape}", &[("n", &(i + 1)), ("device", &d.name), ("serial", &d.serial), ("tape", &holds)]));
                    }
                }
                for d in &site.standalone {
                    lines.push(trf("Standalone drive {device}, serial {serial}: {tape}", &[("device", &d.name), ("serial", &d.serial), ("tape", &holds_text(&d.holds()))]));
                }
                if site.libraries.is_empty() {
                    lines.push(tr("No tape library was found: tapes are put into the drives by hand."));
                }
                lines.extend(site.all_notes());
            }
            Err(e) => lines.push(io_err(e).to_string()),
        }
        lines.push(String::new());
        let tapes = a.catalog.lock().unwrap().tapes().map_err(|e| io::Error::other(e.to_string()))?;
        for t in tapes.iter().filter(|t| !is_cleaning(&t.barcode)) {
            let place = if t.in_library() { t.location.clone() } else { tr("in no library or drive") };
            let what = if t.is_ltfs() {
                trf("{files} files, {size}, {free} free", &[("files", &t.files), ("size", &size_text(t.bytes)), ("free", &t.remaining.map(size_text).unwrap_or_else(|| "?".into()))])
            } else if t.imported.is_some() {
                tr("no LTFS volume")
            } else {
                tr("not imported")
            };
            let note = if t.note.is_empty() { String::new() } else { format!(" — {}", t.note) };
            lines.push(format!("{}  ({place}): {what}{note}", t.barcode));
        }
        Ok(Reply::Message(lines.join("\n")))
    }

    fn cmd_settings_form(&self) -> Reply {
        let s = self.settings.lock().unwrap().clone();
        Reply::Form(Form {
            title: tr("Library settings"),
            ok: tr("Save"),
            command: "settings_do".into(),
            fields: vec![
                note(tr("Leave the devices empty to find every library and drive. Drives no library claims are standalone: tapes are put into them by hand. The program needs to run as administrator to reach the devices.")),
                field("changer", tr("Changers (\\\\.\\Changer0 or scsi:port:bus:target:lun, comma-separated)"), FieldKind::Text, &s.changer),
                field("drives", tr("Tape drives, in libraries and standalone (\\\\.\\Tape0, \\\\.\\Tape1)"), FieldKind::Text, &s.drives),
                field("catalog", tr("Catalog file"), FieldKind::File, &s.catalog_path(&self.data).display().to_string()),
            ],
        })
    }

    fn cmd_settings(&self, values: &Values) -> io::Result<Reply> {
        let mut s = Settings {
            changer: values.get("changer").cloned().unwrap_or_default(),
            drives: values.get("drives").cloned().unwrap_or_default(),
            catalog: values.get("catalog").cloned().unwrap_or_default(),
        };
        if Path::new(s.catalog.trim()) == place::default_catalog(&self.data) {
            s.catalog.clear();
        }
        // never a device or a folder: SQLite writes to what it opens
        if !s.catalog.trim().is_empty() {
            let p = place::check_catalog_path(&s.catalog).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, trf("The catalog file can't be used: {reason}", &[("reason", &e)])))?;
            s.catalog = p.display().to_string();
        }
        // a line break or NUL in a device name is a mistake, never a device
        if (s.changer.clone() + &s.drives).chars().any(char::is_control) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, tr("A device name can't contain line breaks or other control characters.")));
        }
        let archive = open_archive(&self.data, &s).map_err(io::Error::other)?;
        s.save(&place::settings_file(&self.data))?;
        *self.settings.lock().unwrap() = s;
        *self.archive.lock().unwrap() = Arc::new(archive);
        Ok(Reply::Message(tr("Saved. The library is opened again when it is needed.")))
    }
}

impl FolderPlugin for LtoCatalog {
    fn id(&self) -> &str {
        Self::ID
    }

    fn handles(&self, _name: &str) -> bool {
        false
    }

    fn can_write(&self, file: &Path) -> bool {
        is_ours(file)
    }

    fn status(&self) -> String {
        let a = self.archive();
        let tapes = a.catalog.lock().unwrap().tapes().unwrap_or_default();
        let files: u64 = tapes.iter().map(|t| t.files).sum();
        let mut s = trf("Catalog: {tapes} tapes, {files} files", &[("tapes", &tapes.iter().filter(|t| t.is_ltfs()).count()), ("files", &files)]);
        if let Some(site) = a.site_if_open() {
            s.push_str("; ");
            s.push_str(&trf("libraries: {libraries}, drives: {drives}", &[("libraries", &site.libraries.len()), ("drives", &site.drive_count())]));
        }
        s
    }

    fn list(&self, file: &Path, folder: &str) -> io::Result<Index> {
        // a search's results: all at once, the program browses their folders
        if let Some(found) = self.search_of(file) {
            let items = found
                .iter()
                .enumerate()
                .map(|(i, f)| Item {
                    path: to_inner(&format!("{}\\{}", f.tape, f.path)),
                    dir: f.entry.dir,
                    size: f.entry.size,
                    modified: Some(f.entry.modified.to_system()),
                    attrs: if f.entry.dir { ATTR_DIRECTORY } else { 0 } | if f.entry.readonly { ATTR_READONLY } else { 0 },
                    index: Some(i as u32),
                })
                .collect();
            return Ok(Index { items, complete: true });
        }
        if !is_ours(file) {
            return Err(not_ours());
        }
        let a = self.archive();
        let cat = a.catalog.lock().unwrap();
        let err = |e: lto_catalog::Error| io::Error::other(e.to_string());
        if ltfs::index::split(folder).is_empty() {
            let items = cat
                .tapes()
                .map_err(err)?
                .into_iter()
                .filter(|t| !is_cleaning(&t.barcode))
                .map(|t| Item {
                    path: t.barcode.clone(),
                    dir: true,
                    size: 0,
                    modified: t.updated.map(|u| u.to_system()),
                    attrs: ATTR_DIRECTORY | if t.write_protected { ATTR_READONLY } else { 0 },
                    index: None,
                })
                .collect();
            return Ok(Index { items, complete: false });
        }
        let (barcode, path) = lto_archive::split_tape(&to_ltfs(folder));
        match cat.children(&barcode, &path).map_err(err)? {
            Some(children) => Ok(Index {
                items: children
                    .into_iter()
                    .map(|e| Item {
                        path: join_inner(folder, &to_windows(&e.name)),
                        dir: e.dir,
                        size: e.size,
                        modified: Some(e.modified.to_system()),
                        attrs: if e.dir { ATTR_DIRECTORY } else { 0 } | if e.readonly { ATTR_READONLY } else { 0 },
                        index: None,
                    })
                    .collect(),
                complete: false,
            }),
            None => {
                let msg = match cat.tape(&barcode).map_err(err)? {
                    Some(t) if path.is_empty() && t.imported.is_some() && !t.is_ltfs() => trf("Tape {barcode} isn't formatted for LTFS. Format it first: Plugins → Format a tape….", &[("barcode", &barcode)]),
                    Some(t) if path.is_empty() && !t.is_ltfs() => trf("Tape {barcode} is not in the catalog yet. Import it first: Plugins → Import tapes….", &[("barcode", &barcode)]),
                    _ => trf("\"{folder}\" isn't a folder.", &[("folder", &folder)]),
                };
                Err(io::Error::new(io::ErrorKind::NotFound, msg))
            }
        }
    }

    fn extract(&self, file: &Path, items: &[String], out: &Path, progress: &mut Progress) -> io::Result<()> {
        if !self.readable(file) {
            return Err(not_ours());
        }
        let a = self.archive();
        let paths: Vec<String> = items.iter().map(|i| to_ltfs(i)).collect();
        let mut total = 0;
        {
            let cat = a.catalog.lock().unwrap();
            for p in &paths {
                let (b, inner) = lto_archive::split_tape(p);
                total += cat.files_under(&b, &inner).map(|v| v.iter().map(|(_, s)| s).sum::<u64>()).unwrap_or(0);
                if inner.is_empty() {
                    fs::create_dir_all(out.join(&b))?;
                }
            }
        }
        let cancelled = AtomicBool::new(false);
        let mut sink = ToDisk { out, file: None, done: 0, total, progress, cancelled: &cancelled };
        let r = a.read(&paths, &mut sink, &JobUi { plugin: self, cancelled: &cancelled });
        (sink.progress)(sink.done.max(total), total.max(sink.done));
        r.map_err(io_err)
    }

    fn stream(&self, file: &Path, items: &[String], sink: &mut dyn Sink) -> io::Result<()> {
        if !self.readable(file) {
            return Err(not_ours());
        }
        let paths: Vec<String> = items.iter().map(|i| to_ltfs(i)).collect();
        let cancelled = AtomicBool::new(false);
        let mut s = ToSink { sink, cancelled: &cancelled };
        self.archive().read(&paths, &mut s, &JobUi { plugin: self, cancelled: &cancelled }).map_err(io_err)
    }

    fn add(&self, file: &Path, items: &[(PathBuf, String)], progress: &mut Progress) -> io::Result<()> {
        if !is_ours(file) {
            return Err(not_ours());
        }
        let mut by_tape: BTreeMap<String, Vec<(PathBuf, String)>> = BTreeMap::new();
        for (disk, inner) in items {
            let (b, p) = lto_archive::split_tape(&to_ltfs(inner));
            if p.is_empty() {
                return Err(refused(n_("Files can be copied only into a tape, not next to the tapes. Open a tape first.")));
            }
            by_tape.entry(b).or_default().push((disk.clone(), p));
        }
        let a = self.archive();
        let cancelled = AtomicBool::new(false);
        let ui = JobUi { plugin: self, cancelled: &cancelled };
        for (b, list) in by_tape {
            a.write(&b, &list, &ui, &mut |d, t| {
                let go = progress(d, t);
                if !go {
                    cancelled.store(true, Ordering::Relaxed);
                }
                go
            })
            .map_err(io_err)?;
        }
        Ok(())
    }

    fn delete(&self, file: &Path, items: &[String], _progress: &mut Progress) -> io::Result<()> {
        if !is_ours(file) {
            return Err(not_ours());
        }
        let mut by_tape: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for i in items {
            let (b, p) = lto_archive::split_tape(&to_ltfs(i));
            if p.is_empty() {
                return Err(refused(n_("Tapes can't be deleted here. To take a tape out of the catalog, use Plugins → Forget a tape….")));
            }
            by_tape.entry(b).or_default().push(p);
        }
        let a = self.archive();
        let cancelled = AtomicBool::new(false);
        for (b, list) in by_tape {
            a.delete(&b, &list, &JobUi { plugin: self, cancelled: &cancelled }).map_err(io_err)?;
        }
        Ok(())
    }

    fn rename(&self, file: &Path, old: &str, new: &str, _progress: &mut Progress) -> io::Result<()> {
        if !is_ours(file) {
            return Err(not_ours());
        }
        let (b1, p1) = lto_archive::split_tape(&to_ltfs(old));
        let (b2, p2) = lto_archive::split_tape(&to_ltfs(new));
        if p1.is_empty() || p2.is_empty() {
            return Err(refused(n_("Tapes can't be renamed: a tape's name is its barcode.")));
        }
        if b1 != b2 {
            return Err(refused(n_("Items can't be moved from one tape to another by renaming; copy them instead.")));
        }
        let cancelled = AtomicBool::new(false);
        self.archive().rename(&b1, &p1, &p2, &JobUi { plugin: self, cancelled: &cancelled }).map_err(io_err)
    }

    fn mkdir(&self, file: &Path, inner: &str, _progress: &mut Progress) -> io::Result<()> {
        if !is_ours(file) {
            return Err(not_ours());
        }
        let (b, p) = lto_archive::split_tape(&to_ltfs(inner));
        if p.is_empty() {
            return Err(refused(n_("Tapes can't be made as folders. Put a tape into the library and use Plugins → Format a tape….")));
        }
        let cancelled = AtomicBool::new(false);
        self.archive().mkdir(&b, &p, &JobUi { plugin: self, cancelled: &cancelled }).map_err(io_err)
    }

    /// The empty root is the command running now: what it does, for the
    /// program's status bar.
    fn activity(&self, root: &Path) -> sal_plugin_api::Activity {
        if !root.as_os_str().is_empty() {
            return sal_plugin_api::Activity::default();
        }
        sal_plugin_api::Activity { running: self.cmd.running.load(Ordering::Relaxed), text: self.cmd.step.lock().unwrap().clone() }
    }

    /// Cancel of the running command: it stops at the next point it can
    /// (between tapes, between files; never in the middle of a format).
    fn stop(&self, root: &Path) {
        if root.as_os_str().is_empty() {
            self.cmd.cancel.store(true, Ordering::Relaxed);
            self.say(tr("cancelling…"));
        }
    }

    /// A search's results window or panel closed: they are forgotten.
    fn close(&self, root: &Path) {
        if let Some((_, a, _)) = mount_of(root) {
            self.searches.lock().unwrap().remove(&a);
        }
    }

    fn commands(&self) -> Vec<Command> {
        let c = |id: &str, label: String| Command { id: id.into(), label };
        vec![
            c("open", tr("Open tape catalog")),
            c("search", tr("Search the catalog…")),
            c("inventory", tr("Inventory the libraries and drives")),
            c("import", tr("Import tapes…")),
            c("status", tr("Library status")),
            c("format", tr("Format tapes…")),
            c("eject", tr("Eject a tape…")),
            c("unload", tr("Put the tapes in the drives back")),
            c("note", tr("Note about a tape…")),
            c("forget", tr("Forget a tape…")),
            c("settings", tr("Library settings…")),
        ]
    }

    /// A right-click on tapes: what is done to a tape (on the clicked or
    /// selected ones).
    fn item_commands(&self) -> Option<Vec<Command>> {
        let c = |id: &str, label: String| Command { id: id.into(), label };
        Some(vec![
            c("search", tr("Search the catalog…")),
            c("import", tr("Import tape…")),
            c("format", tr("Format tape…")),
            c("eject", tr("Eject tape…")),
            c("note", tr("Note about tape…")),
            c("forget", tr("Forget tape…")),
        ])
    }

    fn run(&self, command: &str, input: &Values) -> io::Result<Reply> {
        // the program's look, for the windows that ask about tapes later
        if input.contains_key("theme") {
            *self.look.lock().unwrap() = input.clone();
        }
        // long commands say what they do, and can be cancelled
        let starting = match command {
            "inventory" | "status" => Some(tr("reading the libraries' inventories…")),
            "import_do" => Some(tr("importing tapes…")),
            "format_do" => Some(tr("formatting…")),
            "format" => Some(tr("reading the libraries' inventories…")),
            "search_do" => Some(tr("searching the catalog…")),
            "eject_do" => Some(tr("ejecting the tape…")),
            "unload" => Some(tr("putting the tapes back…")),
            "settings_do" => Some(tr("opening the catalog and the devices…")),
            _ => None,
        };
        let _running = starting.map(|what| self.begin(what));
        match command {
            "open" => Ok(Self::mount()),
            "inventory" => self.cmd_inventory(),
            "import" => Ok(self.cmd_import_form(input)),
            "import_do" => self.cmd_import(input),
            "status" => self.cmd_status(),
            "format" => self.cmd_format_form(input),
            "search" => Ok(self.search_form(input)),
            "search_do" => self.cmd_search(input),
            "format_do" => self.cmd_format(input),
            "eject" => Ok(self.one_tape_form(
                input,
                tr("Eject a tape"),
                tr("Eject"),
                "eject_do",
                vec![note(tr("A library moves the tape to its mail slot; a standalone drive ejects it."))],
            )),
            "eject_do" => self.cmd_eject(input),
            "unload" => {
                let a = self.archive();
                a.site().map_err(io_err)?.unload_all().map_err(|e| io::Error::other(e.to_string()))?;
                a.inventory(false).map_err(io_err)?;
                Ok(Reply::Done)
            }
            "note" => {
                let chosen = chosen_tapes(input);
                let current = chosen.first().and_then(|b| self.archive().catalog.lock().unwrap().tape(b).ok().flatten()).map(|t| t.note).unwrap_or_default();
                Ok(self.one_tape_form(input, tr("Note about a tape"), tr("Save"), "note_do", vec![field("note", tr("Note (where the tape is kept)"), FieldKind::Text, &current)]))
            }
            "note_do" => self.cmd_note(input),
            "forget" => Ok(self.one_tape_form(
                input,
                tr("Forget a tape"),
                tr("Forget"),
                "forget_do",
                vec![field("confirm", tr("Type the barcode again"), FieldKind::Text, ""), note(tr("The tape is taken out of the catalog; nothing on it changes."))],
            )),
            "forget_do" => self.cmd_forget(input),
            "settings" => Ok(self.cmd_settings_form()),
            "settings_do" => self.cmd_settings(input),
            other => Err(io::Error::new(io::ErrorKind::Unsupported, format!("unknown command \"{other}\""))),
        }
    }
}

#[cfg(test)]
mod tests;
