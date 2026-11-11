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
use lto_scsi::library::Devices;
use ltfs::Time;

const HELP: &str = "ltocat - LTO tape library with an offline LTFS catalog

usage: ltocat [options] <command> [arguments]

checking the hardware (nothing is moved or written):
  devices                    tape drives and the changer Windows shows
  selftest                   open the library, match drives, read the inventory

the library:
  inventory [--rescan]       where every tape is (--rescan: scan barcodes again, slow)
  load <barcode>             load a tape into a drive
  unload                     put every tape in the drives back into the slots
  eject <barcode>            move a tape to the mail slot
  insert <barcode>           move a tape from the mail slot into a free slot

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

options:
  --program <folder> the RustySalamander folder (with rustysalamander.exe): use the plugin's
                     catalog and library settings, wherever the program keeps them (its folder,
                     or %APPDATA% in multi-user mode). Default: ltocat's own folder, when
                     rustysalamander.exe is there.
  --catalog <file>   this catalog instead
  --changer <dev>    the changer: \\\\.\\Changer0 or scsi:<port>:<bus>:<target>:<lun> (default: found)
  --drives <list>    tape drives, comma-separated: \\\\.\\Tape0,\\\\.\\Tape1 (default: all)
  --sim <file>       a simulated library kept in <file>, instead of the hardware
  --yes              don't ask before moving or writing tapes
";

struct Opts {
    /// The catalog: --catalog, or the plugin's (--program); none when
    /// neither is known.
    catalog: Option<PathBuf>,
    changer: String,
    drives: Vec<String>,
    sim: Option<PathBuf>,
    yes: bool,
    rescan: bool,
    name: String,
    args: Vec<String>,
}


fn parse() -> Result<Opts, String> {
    let mut o = Opts { catalog: None, changer: String::new(), drives: Vec::new(), sim: None, yes: false, rescan: false, name: String::new(), args: Vec::new() };
    let mut program: Option<PathBuf> = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = |what: &str| it.next().ok_or_else(|| format!("{what} needs a value"));
        match a.as_str() {
            "--catalog" => o.catalog = Some(PathBuf::from(val("--catalog")?)),
            "--program" => program = Some(PathBuf::from(val("--program")?)),
            "--changer" => o.changer = val("--changer")?,
            "--drives" => o.drives = val("--drives")?.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect(),
            "--sim" => o.sim = Some(PathBuf::from(val("--sim")?)),
            "--name" => o.name = val("--name")?,
            "--yes" | "-y" => o.yes = true,
            "--rescan" => o.rescan = true,
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
        if o.changer.is_empty() {
            o.changer = s.changer.trim().to_owned();
        }
        if o.drives.is_empty() {
            o.drives = s.drives.split([',', ';']).map(|d| d.trim().to_owned()).filter(|d| !d.is_empty()).collect();
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

fn devices(o: &Opts) -> io::Result<Box<dyn Devices>> {
    if let Some(f) = &o.sim {
        return Ok(Box::new(lto_sim::SimLibrary::persistent(f, &lto_sim::Config::default())?));
    }
    #[cfg(windows)]
    {
        Ok(Box::new(lto_scsi::windows::WindowsDevices { changer: o.changer.clone(), drives: o.drives.clone() }))
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
    let _ = io::stdin().lock().read_line(&mut s);
    s.trim().to_owned()
}

fn confirm(o: &Opts, what: &str) -> bool {
    o.yes || ask(&format!("{what} Continue? [y/N]")).eq_ignore_ascii_case("y")
}

struct Console;

impl Ui for Console {
    fn missing_tape(&self, barcode: &str, tape: Option<&Tape>, skip: bool) -> Answer {
        let note = tape.map(|t| t.note.as_str()).filter(|n| !n.is_empty()).map(|n| format!(" ({n})")).unwrap_or_default();
        let a = ask(&format!(
            "Tape {barcode}{note} is not in the library. Put it into the mail slot, then press Enter{} (q to cancel):",
            if skip { ", s to skip it" } else { "" }
        ));
        match a.to_lowercase().as_str() {
            "q" => Answer::Cancel,
            "s" if skip => Answer::Skip,
            _ => Answer::Retry,
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
                let wd = lto_scsi::windows::WindowsDevices { changer: o.changer.clone(), drives: o.drives.clone() };
                let (found, tried) = if o.changer.is_empty() { wd.find_changer() } else { (Some(o.changer.clone()), Vec::new()) };
                for t in tried {
                    println!("        tried {t}");
                }
                match found {
                    Some(c) => {
                        let mut ch = lto_scsi::changer::Changer::new(lto_scsi::windows::open_any(&c).map_err(|e| e.to_string())?);
                        let id = ch.inquiry().map(|i| format!("{} {} {}", i.vendor, i.product, i.revision)).unwrap_or_else(|e| format!("INQUIRY failed: {e}"));
                        println!("changer {c:<14} {id}");
                    }
                    None => println!("changer not found; give it with --changer"),
                }
            }
            Ok(())
        }
        "selftest" => {
            let a = archive(o)?;
            let lib = a.library().map_err(e)?;
            println!("changer: {} {} {} at {}", lib.identity.vendor, lib.identity.product, lib.identity.revision, lib.changer_name);
            for n in &lib.notes {
                println!("note: {n}");
            }
            for (i, d) in lib.drives.iter().enumerate() {
                println!("drive {}: element {:#06x}, {} serial {}, holds {}", i + 1, d.element, d.name, d.serial, d.tape().unwrap_or_else(|| "nothing".into()));
            }
            let inv = lib.inventory();
            let full = inv.slots.iter().filter(|s| s.full).count();
            println!("slots: {} ({} with a tape), mail slots: {}, drives: {}", inv.slots.len(), full, inv.mail_slots.len(), inv.drives.len());
            let no_label = inv.all().filter(|e| e.full && e.barcode.is_none()).count();
            if no_label > 0 {
                println!("warning: {no_label} tapes have no readable barcode");
            }
            println!("ok: the library can be used.");
            Ok(())
        }
        "inventory" => {
            let a = archive(o)?;
            let inv = a.inventory(o.rescan).map_err(e)?;
            for el in inv.all().filter(|e| e.kind != lto_scsi::changer::Kind::Transport) {
                let what = if el.full { el.barcode.clone().unwrap_or_else(|| "(no barcode)".into()) } else { "-".into() };
                println!("{:<12} {:#06x}  {what}{}", lto_archive::location_text(&inv, el.address), el.address, if el.exception { "  (exception)" } else { "" });
            }
            Ok(())
        }
        "load" => {
            need(o, 1, "load <barcode>")?;
            if !confirm(o, &format!("The robot will load {} into a drive.", o.args[1])) {
                return Ok(());
            }
            let a = archive(o)?;
            let lib = a.library().map_err(e)?;
            let l = lib.acquire(&o.args[1], &|| false).map_err(|e| e.to_string())?;
            println!("{} is in {}", l.barcode, l.drive_name);
            Ok(())
        }
        "unload" => {
            if !confirm(o, "The robot will put every tape in the drives back into the slots.") {
                return Ok(());
            }
            let a = archive(o)?;
            a.library().map_err(e)?.unload_all().map_err(|e| e.to_string())?;
            a.inventory(false).map_err(e)?;
            Ok(())
        }
        "eject" | "insert" => {
            need(o, 1, &format!("{cmd} <barcode>"))?;
            let a = archive(o)?;
            let lib = a.library().map_err(e)?;
            if cmd == "eject" {
                lib.export(&o.args[1]).map_err(|e| e.to_string())?;
                println!("{} is in the mail slot.", o.args[1]);
            } else {
                lib.refresh(false).map_err(|e| e.to_string())?;
                lib.import(&o.args[1]).map_err(|e| e.to_string())?;
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
            let lib = a.library().map_err(e)?;
            let mut l = lib.acquire(&o.args[1], &|| false).map_err(|e| e.to_string())?;
            match ltfs::probe(l.drive()).map_err(|e| e.to_string())? {
                Some(lb) => println!("{}", lb.to_xml()),
                None => println!("{} holds no LTFS volume.", o.args[1]),
            }
            Ok(())
        }
        "import" => {
            need(o, 1, "import <barcode>|all")?;
            let a = archive(o)?;
            let barcodes: Vec<String> = if o.args[1] == "all" {
                let inv = a.inventory(false).map_err(e)?;
                inv.all().filter(|e| e.full && e.kind != lto_scsi::changer::Kind::Transport).filter_map(|e| e.barcode.clone()).filter(|b| !b.to_uppercase().starts_with("CLN")).collect()
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
