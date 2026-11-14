//! `ltocat`: the tape library and its catalog from the command line. Made
//! for checking a library before the plugin uses it, and for work without
//! the program. Run it as administrator (SCSI pass-through needs it).

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use lto_archive::{Answer, Archive, ReadSink, Ui};
use lto_catalog::place::{self, Settings};
use lto_catalog::{Catalog, Tape};
use lto_scsi::site::{Ejected, Holds, Places};
use lto_scsi::library::Devices;
use ltfs::Time;

const HELP: &str = "ltocat - LTO tape library with an offline LTFS catalog

usage: ltocat [options] <command> [arguments]

checking the hardware (nothing is moved or written):
  devices                    tape drives and changers Windows shows
  selftest                   open the libraries, match drives, read the inventories

libraries and drives (drives no library claims are standalone: tapes go in by hand):
  inventory [--rescan]       where every tape is (--rescan: scan barcodes again, slow)
  load <barcode>             get a tape into a drive (a library's robot, or asks for it)
  unload                     tapes in library drives back to their slots, standalone drives eject
  eject <barcode>            a library's tape to its mail slot, a standalone drive's out
  insert <barcode>           move a tape from a library's mail slot into a free slot

tapes and the catalog:
  tapes                      the tapes in the catalog
  label <barcode>            the tape's LTFS label (loads it)
  import <barcode>|all       read tapes' indexes into the catalog (all: every tape in the library)
  index <barcode> [<file>]   the tape's newest index, saved as XML to <file>
  ls <barcode>[\\path]        a folder, from the catalog
  search <text>              files whose name contains <text>, on all tapes
  note <barcode> <text>      a note about a tape (where it is kept)
  forget <barcode>           take a tape out of the catalog

files:
  get <barcode>\\<path> <folder>      copy from tape to a folder
  put <file or folder> <barcode>\\<path>   copy onto a tape (into the folder <path>)
  mkdir <barcode>\\<path>
  mv <barcode>\\<path> <new path>
  rm <barcode>\\<path>
  format <barcode> [--name <volume name>]  ERASES the tape, makes it an empty LTFS volume
  backup <folder> [<barcode>...] [--archive-only] [--keep-archive]
                             back up a folder into \"Backup <date> <time>\\<folder>\" on the tapes
                             (in barcode order, then new ones asked for); --archive-only: only
                             files with the Archive attribute; --keep-archive: don't clear it

options:
  --program <folder> the RustySalamander folder (with rustysalamander.exe): use the plugin's
                     catalog and library settings, wherever the program keeps them (its folder,
                     or %APPDATA% in multi-user mode). Default: ltocat's own folder, when
                     rustysalamander.exe is there.
  --catalog <file>   this catalog instead
  --changer <list>   changers, comma-separated: \\\\.\\Changer0 or scsi:<port>:<bus>:<target>:<lun>
                     (default: every one found)
  --drives <list>    tape drives, comma-separated: \\\\.\\Tape0,\\\\.\\Tape1 (default: all)
  --sim <file>       simulated devices instead of the hardware: the first library in <file>,
                     the rest next to it
  --sim-libraries <n>  simulated libraries (1)
  --sim-drives <n>   simulated standalone drives (0)
  --yes              don't ask before moving or writing tapes

simulation only:
  sim-move <barcode> drive<N>|mail<N>|shelf   move a tape by hand (a new one if it's nowhere)
";

struct Opts {
    /// The catalog: --catalog, or the plugin's (--program); none when
    /// neither is known.
    catalog: Option<PathBuf>,
    changers: Vec<String>,
    drives: Vec<String>,
    sim: Option<PathBuf>,
    sim_libraries: usize,
    sim_drives: usize,
    yes: bool,
    rescan: bool,
    name: String,
    archive_only: bool,
    keep_archive: bool,
    args: Vec<String>,
}

/// A comma- or semicolon-separated list.
fn list(s: &str) -> Vec<String> {
    s.split([',', ';']).map(|d| d.trim().to_owned()).filter(|d| !d.is_empty()).collect()
}

fn parse() -> Result<Opts, String> {
    let mut o = Opts {
        catalog: None,
        changers: Vec::new(),
        drives: Vec::new(),
        sim: None,
        sim_libraries: 1,
        sim_drives: 0,
        yes: false,
        rescan: false,
        name: String::new(),
        archive_only: false,
        keep_archive: false,
        args: Vec::new(),
    };
    let mut program: Option<PathBuf> = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = |what: &str| it.next().ok_or_else(|| format!("{what} needs a value"));
        match a.as_str() {
            "--catalog" => o.catalog = Some(place::check_catalog_path(&val("--catalog")?).map_err(|e| format!("--catalog: {e}"))?),
            "--program" => program = Some(PathBuf::from(val("--program")?)),
            "--changer" => o.changers = list(&val("--changer")?),
            "--drives" => o.drives = list(&val("--drives")?),
            "--sim" => o.sim = Some(PathBuf::from(val("--sim")?)),
            "--sim-libraries" => o.sim_libraries = val("--sim-libraries")?.parse().map_err(|_| "--sim-libraries needs a number")?,
            "--sim-drives" => o.sim_drives = val("--sim-drives")?.parse().map_err(|_| "--sim-drives needs a number")?,
            "--name" => o.name = val("--name")?,
            "--yes" | "-y" => o.yes = true,
            "--rescan" => o.rescan = true,
            "--archive-only" => o.archive_only = true,
            "--keep-archive" => o.keep_archive = true,
            "-h" | "--help" | "help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            _ if a.starts_with("--") => return Err(format!("unknown option {a}")),
            _ => o.args.push(a),
        }
    }
    if o.args.is_empty() {
        print!("{HELP}");
        std::process::exit(0);
    }
    // the plugin's catalog and library settings, from RustySalamander's folder
    let own = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)).filter(|d| place::is_program_dir(d));
    if let Some(dir) = program.or(own) {
        if !place::is_program_dir(&dir) {
            return Err(format!("{} has no {}", dir.display(), place::PROGRAM_EXE));
        }
        let data = place::data_dir_of_program(&dir);
        let s = Settings::load(&place::settings_file(&data));
        o.catalog.get_or_insert_with(|| s.catalog_path(&data));
        if o.changers.is_empty() {
            o.changers = list(&s.changer);
        }
        if o.drives.is_empty() {
            o.drives = list(&s.drives);
        }
    }
    Ok(o)
}

impl Opts {
    /// The catalog file (its folder made if new), or why there is none.
    fn catalog(&self) -> Result<&Path, String> {
        let file = self.catalog.as_deref().ok_or_else(|| "which catalog? Give --program <RustySalamander folder> or --catalog <file>".to_owned())?;
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        Ok(file)
    }
}

fn sim(o: &Opts) -> io::Result<Option<lto_sim::SimSite>> {
    o.sim.as_ref().map(|f| lto_sim::SimSite::persistent(f, o.sim_libraries, o.sim_drives)).transpose()
}

fn devices(o: &Opts) -> io::Result<Box<dyn Devices>> {
    if let Some(site) = sim(o)? {
        return Ok(Box::new(site));
    }
    #[cfg(windows)]
    {
        Ok(Box::new(lto_scsi::windows::WindowsDevices { changers: o.changers.clone(), drives: o.drives.clone() }))
    }
    #[cfg(not(windows))]
    {
        Err(io::Error::other("only Windows has the hardware support; use --sim"))
    }
}

fn archive(o: &Opts) -> Result<Archive, String> {
    let file = o.catalog()?;
    let cat = Catalog::open(file).map_err(|e| format!("the catalog {}: {e}", file.display()))?;
    Ok(Archive::new(devices(o).map_err(|e| e.to_string())?, cat, &ltfs::creator()))
}

fn ask(question: &str) -> String {
    print!("{question} ");
    let _ = io::stdout().flush();
    let mut s = String::new();
    // no more input (a closed or redirected stdin): the answer is "q", never
    // an endless Retry
    match io::stdin().lock().read_line(&mut s) {
        Ok(0) | Err(_) => {
            println!();
            "q".into()
        }
        Ok(_) => s.trim().to_owned(),
    }
}

fn confirm(o: &Opts, what: &str) -> bool {
    o.yes || ask(&format!("{what} Continue? [y/N]")).eq_ignore_ascii_case("y")
}

struct Console;

/// What a standalone drive holds, for people.
fn holds(h: &Holds) -> String {
    match h {
        Holds::Empty => "nothing".into(),
        Holds::Tape(b) => b.clone(),
        Holds::Unknown => "a tape that can't be identified".into(),
    }
}

impl Ui for Console {
    fn step(&self, step: lto_archive::Step) {
        use lto_archive::Step as S;
        let text = match step {
            S::Devices => "looking for tape libraries and drives".to_owned(),
            S::Loading(b) => format!("{b}: getting it into a drive"),
            S::Settling(b) => format!("{b}: waiting for the library to read its mail slot"),
            S::Mounting(b, d) => format!("{b} is in {d}: reading its LTFS volume"),
            S::Formatting(b) => format!("{b}: formatting (a few minutes)"),
            S::Reading(b, p) => format!("{b}: reading {p}"),
            S::Writing(b, p) => format!("{b}: writing {p}"),
            S::WritingIndex(b) => format!("{b}: writing its index"),
            S::Backup { tape, files, bytes } => format!("{tape}: backing up, {} of {} files, {} of {}", files.0, files.1, size(bytes.0), size(bytes.1)),
            S::NewTape => "waiting for a new tape in the mail slot".to_owned(),
        };
        eprintln!("... {text}");
    }
    fn missing_tape(&self, barcode: &str, tape: Option<&Tape>, skip: bool, places: &Places) -> Answer {
        let note = tape.map(|t| t.note.as_str()).filter(|n| !n.is_empty()).map(|n| format!(" ({n})")).unwrap_or_default();
        if let Some((d, h)) = &places.ejected {
            println!("{d} ejected {} to make room.", holds(h));
        }
        for (d, h) in places.drives.iter().filter(|(_, h)| *h != Holds::Empty) {
            println!("{d} holds {}.", holds(h));
        }
        let mut wher = Vec::new();
        if !places.libraries.is_empty() {
            wher.push(format!("into the mail slot of {}", places.libraries.join(" or ")));
        }
        if !places.drives.is_empty() {
            wher.push(format!("into drive {}", places.drives.iter().map(|(d, _)| d.as_str()).collect::<Vec<_>>().join(" or ")));
        }
        let a = ask(&format!(
            "Tape {barcode}{note} is in no library or drive. Put it {}, then press Enter{} (q to cancel):",
            wher.join(", or "),
            if skip { ", s to skip it" } else { "" }
        ));
        match a.to_lowercase().as_str() {
            "q" => Answer::Cancel,
            "s" if skip => Answer::Skip,
            _ => Answer::Retry,
        }
    }
    fn new_tape(&self, left: u64, places: &Places, problem: Option<lto_archive::NewTapeProblem>) -> Answer {
        match problem {
            Some(lto_archive::NewTapeProblem::NothingNew) => println!("No new tape showed up."),
            Some(lto_archive::NewTapeProblem::HasData(b)) => println!("Tape {b} has files on it, so it isn't used. Take it out."),
            None => {}
        }
        let wher = if places.libraries.is_empty() { "into a drive".to_owned() } else { format!("into the mail slot of {}", places.libraries.join(" or ")) };
        let a = ask(&format!("The tapes are full; {} is still to back up. Put a new tape {wher} (a blank one is formatted after asking), then press Enter (q to stop):", size(left)));
        if a.eq_ignore_ascii_case("q") {
            Answer::Cancel
        } else {
            Answer::Retry
        }
    }
    fn confirm_format(&self, barcode: &str) -> Answer {
        let a = ask(&format!("Tape {barcode} isn't an LTFS volume. Type its barcode to format it (ERASES it) and use it, s for another tape, q to stop:"));
        if a.eq_ignore_ascii_case(barcode) {
            Answer::Retry
        } else if a.eq_ignore_ascii_case("q") {
            Answer::Cancel
        } else {
            Answer::Skip
        }
    }
}

fn size(b: u64) -> String {
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

/// Writes files read from tape under a folder.
struct ToFolder {
    root: PathBuf,
    file: Option<(fs::File, PathBuf, Time)>,
    done: u64,
    started: Instant,
}

impl ToFolder {
    fn target(&self, path: &str) -> io::Result<PathBuf> {
        let rel: Vec<String> = ltfs::index::split(path).into_iter().map(ltfs::names::to_windows).collect();
        let rel = rel.join("\\");
        if !ltfs::names::safe_relative(&rel) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unsafe path on tape: {path}")));
        }
        Ok(self.root.join(rel))
    }
}

impl ReadSink for ToFolder {
    fn dir(&mut self, path: &str, _: Time) -> io::Result<()> {
        fs::create_dir_all(self.target(path)?)
    }
    fn begin(&mut self, path: &str, len: u64, modified: Time) -> io::Result<bool> {
        let t = self.target(path)?;
        if let Some(p) = t.parent() {
            fs::create_dir_all(p)?;
        }
        println!("  {path} ({})", size(len));
        self.file = Some((fs::File::create(&t)?, t, modified));
        Ok(true)
    }
    fn write(&mut self, data: &[u8]) -> io::Result<bool> {
        self.file.as_mut().unwrap().0.write_all(data)?;
        self.done += data.len() as u64;
        Ok(true)
    }
    fn end(&mut self) -> io::Result<()> {
        if let Some((f, _, m)) = self.file.take() {
            let _ = f.set_modified(m.to_system());
        }
        Ok(())
    }
}

fn need(o: &Opts, n: usize, usage: &str) -> Result<(), String> {
    if o.args.len() < n + 1 {
        return Err(format!("usage: ltocat {usage}"));
    }
    Ok(())
}

fn print_tapes(tapes: &[Tape]) {
    println!("{:<10} {:<12} {:>8} {:>10} {:>10} {:>5}  {}", "barcode", "where", "files", "data", "free", "gen", "name / note");
    for t in tapes {
        let free = t.remaining.map(size).unwrap_or_default();
        let wh = if t.location.is_empty() { "outside".to_string() } else { t.location.clone() };
        let kind = if t.is_ltfs() {
            t.name.clone()
        } else if t.imported.is_some() {
            "(no LTFS)".into()
        } else {
            "(not imported)".into()
        };
        let note = if t.note.is_empty() { String::new() } else { format!(" / {}", t.note) };
        println!("{:<10} {:<12} {:>8} {:>10} {:>10} {:>5}  {kind}{note}", t.barcode, wh, t.files, size(t.bytes), free, t.generation);
    }
}

fn run(o: &Opts) -> Result<(), String> {
    let e = |e: lto_archive::Error| e.to_string();
    let cmd = o.args[0].as_str();
    match cmd {
        "devices" => {
            #[cfg(windows)]
            {
                let names = if o.drives.is_empty() { lto_scsi::windows::tape_names() } else { o.drives.clone() };
                if names.is_empty() {
                    println!("no tape drives (\\\\.\\Tape0 …): is the drive driver installed, and is this an administrator prompt?");
                }
                for n in names {
                    match lto_scsi::windows::DeviceTransport::open(&n) {
                        Ok(t) => {
                            let addr = t.address().map(|a| format!("scsi:{}:{}:{}:{}", a.port, a.path, a.target, a.lun)).unwrap_or_else(|e| format!("({e})"));
                            let mut d = lto_scsi::tape::TapeDrive::new(Box::new(t));
                            let id = d.inquiry().map(|i| format!("{} {} {}", i.vendor, i.product, i.revision)).unwrap_or_else(|e| format!("INQUIRY failed: {e}"));
                            let serial = d.serial_number().unwrap_or_else(|e| format!("({e})"));
                            println!("drive   {n:<14} {addr:<14} {id}  serial {serial}");
                        }
                        Err(e) => println!("drive   {n}: {e}"),
                    }
                }
                let wd = lto_scsi::windows::WindowsDevices { changers: o.changers.clone(), drives: o.drives.clone() };
                let (found, tried) = if o.changers.is_empty() { wd.find_changers() } else { (o.changers.clone(), Vec::new()) };
                for t in tried {
                    println!("        tried {t}");
                }
                for c in &found {
                    match lto_scsi::windows::open_any(c) {
                        Ok(t) => {
                            let mut ch = lto_scsi::changer::Changer::new(t);
                            let id = ch.inquiry().map(|i| format!("{} {} {}", i.vendor, i.product, i.revision)).unwrap_or_else(|e| format!("INQUIRY failed: {e}"));
                            let serial = ch.serial_number().unwrap_or_else(|e| format!("({e})"));
                            println!("changer {c:<14} {id}  serial {serial}");
                        }
                        Err(e) => println!("changer {c}: {e}"),
                    }
                }
                if found.is_empty() {
                    println!("no changer found: the drives are used as standalone drives (give a changer with --changer)");
                }
            }
            Ok(())
        }
        "selftest" => {
            let a = archive(o)?;
            let site = a.inventory(false).map_err(e)?;
            for lib in &site.libraries {
                println!("changer {}: {} {} {} at {}", lib.name, lib.identity.vendor, lib.identity.product, lib.identity.revision, lib.changer_name);
                // the library's drives by element, as the changer names them,
                // and the tape device each one is used through (if any)
                let inv = lib.inventory();
                for el in &inv.drives {
                    let says = el.device_id.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("(not identified)");
                    match lib.drives.iter().find(|d| d.element == el.address) {
                        Some(d) => println!("  drive {:#06x} ({says}): used at {} serial {}, holds {}", el.address, d.name, d.serial.trim(), d.tape().unwrap_or_else(|| "nothing".into())),
                        None => println!("  drive {:#06x} ({says}): not used, no tape device on this computer", el.address),
                    }
                }
                let full = inv.slots.iter().filter(|s| s.full).count();
                println!("  slots: {} ({} with a tape), mail slots: {}, drives: {}", inv.slots.len(), full, inv.mail_slots.len(), inv.drives.len());
                let no_label = inv.all().filter(|e| e.full && e.barcode.is_none()).count();
                if no_label > 0 {
                    println!("  warning: {no_label} tapes have no readable barcode");
                }
            }
            for d in &site.standalone {
                println!("standalone drive {} serial {}, holds {}", d.name, d.serial, holds(&d.holds()));
            }
            for n in site.all_notes() {
                println!("note: {n}");
            }
            println!("ok: {} libraries and {} drives can be used.", site.libraries.len(), site.drive_count());
            Ok(())
        }
        "inventory" => {
            let a = archive(o)?;
            let site = a.inventory(o.rescan).map_err(e)?;
            for lib in &site.libraries {
                println!("{} ({}):", lib.name, lib.changer_name);
                let inv = lib.inventory();
                for el in inv.all().filter(|e| e.kind != lto_scsi::changer::Kind::Transport) {
                    let what = if el.full { el.barcode.clone().unwrap_or_else(|| "(no barcode)".into()) } else { "-".into() };
                    println!("  {:<12} {:#06x}  {what}{}", inv.place(el.address), el.address, if el.exception { "  (exception)" } else { "" });
                }
            }
            for d in &site.standalone {
                println!("standalone {:<14} {}", d.name, holds(&d.holds()));
            }
            Ok(())
        }
        "load" => {
            need(o, 1, "load <barcode>")?;
            if !confirm(o, &format!("The robot will load {} into a drive.", o.args[1])) {
                return Ok(());
            }
            let a = archive(o)?;
            let drive = a.load(&o.args[1].to_uppercase(), &Console).map_err(e)?;
            println!("{} is in {drive}", o.args[1].to_uppercase());
            Ok(())
        }
        "unload" => {
            if !confirm(o, "The robots will put every tape in the drives back into the slots, and standalone drives eject theirs.") {
                return Ok(());
            }
            let a = archive(o)?;
            a.site().map_err(e)?.unload_all().map_err(|e| e.to_string())?;
            a.inventory(false).map_err(e)?;
            Ok(())
        }
        "eject" | "insert" => {
            need(o, 1, &format!("{cmd} <barcode>"))?;
            let a = archive(o)?;
            let site = a.inventory(false).map_err(e)?;
            let b = o.args[1].to_uppercase();
            if cmd == "eject" {
                match site.eject(&b, &|| false).map_err(|e| e.to_string())? {
                    Ejected::MailSlot(lib) => println!("{b} is in the mail slot of {lib}."),
                    Ejected::Drive(d) => println!("{b} came out of {d}."),
                }
            } else {
                let (lib, slot) = site.import(&b).map_err(|e| e.to_string())?;
                println!("{b} is in {lib}, element {slot:#06x}.");
            }
            a.inventory(false).map_err(e)?;
            Ok(())
        }
        "tapes" => {
            let c = Catalog::open(o.catalog()?).map_err(|e| e.to_string())?;
            print_tapes(&c.tapes().map_err(|e| e.to_string())?);
            Ok(())
        }
        "label" => {
            need(o, 1, "label <barcode>")?;
            let a = archive(o)?;
            match a.label(&o.args[1].to_uppercase(), &Console).map_err(e)? {
                Some(lb) => println!("{}", lb.to_xml()),
                None => println!("{} holds no LTFS volume.", o.args[1]),
            }
            Ok(())
        }
        "import" => {
            need(o, 1, "import <barcode>|all")?;
            let a = archive(o)?;
            let barcodes: Vec<String> = if o.args[1] == "all" {
                let site = a.inventory(false).map_err(e)?;
                site.locations().into_iter().map(|(b, _)| b).filter(|b| !b.to_uppercase().starts_with("CLN")).collect()
            } else {
                o.args[1..].to_vec()
            };
            for b in barcodes {
                match a.import(&b, &Console) {
                    Ok(Some(ix)) => {
                        let (n, s) = ix.totals();
                        println!("{b}: generation {}, {n} files, {}", ix.generation, size(s));
                    }
                    Ok(None) => println!("{b}: no LTFS volume"),
                    Err(err) => println!("{b}: {err}"),
                }
            }
            Ok(())
        }
        "index" => {
            need(o, 1, "index <barcode> [<file>]")?;
            let a = archive(o)?;
            let ix = a.import(&o.args[1], &Console).map_err(e)?.ok_or("the tape holds no LTFS volume")?;
            let (n, s) = ix.totals();
            println!("{}: {} ({}), generation {}, written {} by {}, {n} files, {}", o.args[1], ix.root.name, ix.volume_uuid, ix.generation, ix.update_time.format(), ix.creator, size(s));
            if let Some(f) = o.args.get(2) {
                fs::write(f, ix.to_xml()).map_err(|e| format!("{f}: {e}"))?;
                println!("saved to {f}");
            }
            Ok(())
        }
        "ls" => {
            need(o, 1, "ls <barcode>[\\path]")?;
            let c = Catalog::open(o.catalog()?).map_err(|e| e.to_string())?;
            let (b, p) = lto_archive::split_tape(&o.args[1]);
            let mut list = c.children(&b, &p).map_err(|e| e.to_string())?.ok_or("no such folder in the catalog")?;
            list.sort_by(|x, y| y.dir.cmp(&x.dir).then(x.name.to_lowercase().cmp(&y.name.to_lowercase())));
            for en in list {
                let t = en.modified.format();
                println!("{:<19} {:>12}  {}{}", &t[..19].replace('T', " "), if en.dir { "<DIR>".into() } else { size(en.size) }, en.name, if en.dir { "\\" } else { "" });
            }
            Ok(())
        }
        "search" => {
            need(o, 1, "search <text>")?;
            let c = Catalog::open(o.catalog()?).map_err(|e| e.to_string())?;
            for (b, p, s) in c.search(&o.args[1], 1000).map_err(|e| e.to_string())? {
                println!("{b}\\{p}  ({})", size(s));
            }
            Ok(())
        }
        "note" => {
            need(o, 2, "note <barcode> <text>")?;
            let c = Catalog::open(o.catalog()?).map_err(|e| e.to_string())?;
            c.set_note(&o.args[1], &o.args[2..].join(" ")).map_err(|e| e.to_string())
        }
        "forget" => {
            need(o, 1, "forget <barcode>")?;
            let c = Catalog::open(o.catalog()?).map_err(|e| e.to_string())?;
            c.forget(&o.args[1]).map_err(|e| e.to_string())
        }
        "get" => {
            need(o, 2, "get <barcode>\\<path> <folder>")?;
            let a = archive(o)?;
            let mut sink = ToFolder { root: PathBuf::from(&o.args[2]), file: None, done: 0, started: Instant::now() };
            fs::create_dir_all(&sink.root).map_err(|e| e.to_string())?;
            a.read(&[o.args[1].clone()], &mut sink, &Console).map_err(e)?;
            let secs = sink.started.elapsed().as_secs_f64().max(0.001);
            println!("{} in {secs:.0} s ({}/s)", size(sink.done), size((sink.done as f64 / secs) as u64));
            Ok(())
        }
        "put" => {
            need(o, 2, "put <file or folder> <barcode>\\<path>")?;
            let src = PathBuf::from(&o.args[1]);
            let (b, dest) = lto_archive::split_tape(&o.args[2]);
            let name = src.file_name().ok_or("no file name")?.to_string_lossy().into_owned();
            let target = if dest.is_empty() { name } else { format!("{dest}\\{name}") };
            if !confirm(o, &format!("{} will be written to tape {b} as {target}.", src.display())) {
                return Ok(());
            }
            let a = archive(o)?;
            let started = Instant::now();
            let mut last = Instant::now();
            a.write(&b, &[(src, target)], &Console, &mut |d, t| {
                if last.elapsed().as_secs() >= 2 || d == t {
                    last = Instant::now();
                    print!("\r  {} of {}   ", size(d), size(t));
                    let _ = io::stdout().flush();
                }
                true
            })
            .map_err(e)?;
            println!("\ndone in {:.0} s", started.elapsed().as_secs_f64());
            Ok(())
        }
        "mkdir" | "rm" => {
            need(o, 1, &format!("{cmd} <barcode>\\<path>"))?;
            let (b, p) = lto_archive::split_tape(&o.args[1]);
            let a = archive(o)?;
            if cmd == "mkdir" {
                a.mkdir(&b, &p, &Console).map_err(e)
            } else {
                if !confirm(o, &format!("{p} will be removed from the index of {b} (the space isn't freed).")) {
                    return Ok(());
                }
                a.delete(&b, &[p], &Console).map_err(e)
            }
        }
        "mv" => {
            need(o, 2, "mv <barcode>\\<path> <new path>")?;
            let (b, p) = lto_archive::split_tape(&o.args[1]);
            archive(o)?.rename(&b, &p, &o.args[2], &Console).map_err(e)
        }
        "format" => {
            need(o, 1, "format <barcode> [--name <volume name>]")?;
            let b = &o.args[1];
            println!("Formatting ERASES everything on tape {b}.");
            if ask(&format!("Type the barcode ({b}) to go on:")) != *b {
                println!("Nothing was done.");
                return Ok(());
            }
            let a = archive(o)?;
            let ix = a.format(b, &o.name, &Console).map_err(e)?;
            println!("{b} is an empty LTFS volume {}.", ix.volume_uuid);
            Ok(())
        }
        "backup" => {
            need(o, 1, "backup <folder> [<barcode>...] [--archive-only] [--keep-archive]")?;
            let src = PathBuf::from(&o.args[1]);
            let tapes: Vec<String> = o.args[2..].iter().map(|b| b.to_uppercase()).collect();
            let set = lto_archive::backup_set_name(&src);
            let opts = lto_archive::BackupOptions { archive_only: o.archive_only, clear_archive: !o.keep_archive };
            let r = archive(o)?.backup(&src, &set, &tapes, opts, &Console).map_err(e)?;
            if r.total_files == 0 && opts.archive_only {
                println!("Nothing to back up: no file in {} has the Archive attribute set.", src.display());
                return Ok(());
            }
            let (files, bytes) = r.written();
            println!("{}{files} of {} files ({}) backed up into \"{set}\".", if r.cancelled { "Cancelled. " } else { "" }, r.total_files, size(bytes));
            for t in &r.tapes {
                println!("  {}: {} files, {}", t.barcode, t.files, size(t.bytes));
            }
            for p in &r.too_big {
                println!("left out, larger than a tape: {}", p.display());
            }
            for (p, why) in &r.unreadable {
                println!("couldn't be read: {}: {why}", p.display());
            }
            for p in &r.not_cleared {
                println!("Archive attribute left set: {}", p.display());
            }
            Ok(())
        }
        "sim-move" => {
            need(o, 2, "sim-move <barcode> drive<N>|mail<N>|shelf")?;
            let site = sim(o).map_err(|e| e.to_string())?.ok_or("sim-move works on simulated devices only (--sim <file>)")?;
            site.hand(&o.args[1], &o.args[2])?;
            println!("{} is in {}.", o.args[1].to_uppercase(), o.args[2]);
            Ok(())
        }
        other => Err(format!("unknown command {other}; ltocat --help lists them")),
    }
}

fn main() {
    let o = match parse() {
        Ok(o) => o,
        Err(m) => {
            eprintln!("ltocat: {m}");
            std::process::exit(2);
        }
    };
    if let Err(m) = run(&o) {
        eprintln!("ltocat: {m}");
        std::process::exit(1);
    }
}
