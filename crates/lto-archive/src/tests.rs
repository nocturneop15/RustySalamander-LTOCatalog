//! Jobs on the simulated library.

use std::sync::Mutex;

use lto_sim::{Config, SimLibrary};

use lto_scsi::site::{Holds, Places};

use super::*;

fn archive() -> (SimLibrary, Archive) {
    let sim = SimLibrary::new(&Config::default());
    let a = Archive::new(Box::new(sim.clone()), Catalog::in_memory().unwrap(), "test");
    (sim, a)
}

/// Collects what is read.
#[derive(Default)]
struct Got {
    dirs: Vec<String>,
    files: Vec<(String, Vec<u8>)>,
    skip: Vec<String>,
}

impl ReadSink for Got {
    fn dir(&mut self, path: &str, _: Time) -> io::Result<()> {
        self.dirs.push(path.into());
        Ok(())
    }
    fn begin(&mut self, path: &str, _: u64, _: Time) -> io::Result<bool> {
        if self.skip.iter().any(|s| s == path) {
            return Ok(false);
        }
        self.files.push((path.into(), Vec::new()));
        Ok(true)
    }
    fn write(&mut self, data: &[u8]) -> io::Result<bool> {
        self.files.last_mut().unwrap().1.extend_from_slice(data);
        Ok(true)
    }
    fn end(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What a person does when asked: one per question, in order.
type Act = Box<dyn FnOnce() + Send>;

/// Answers questions about missing tapes from a list, recording them and
/// where the tape could be put.
struct Answers {
    answers: Mutex<Vec<Answer>>,
    asked: Mutex<Vec<String>>,
    places: Mutex<Vec<Places>>,
    /// Put this tape into the mail slot when asked.
    insert: Mutex<Option<(SimLibrary, lto_sim::Cart)>>,
    /// What the person does when asked (before answering).
    acts: Mutex<Vec<Act>>,
}

impl Answers {
    fn new(a: &[Answer]) -> Answers {
        Answers {
            answers: Mutex::new(a.iter().rev().copied().collect()),
            asked: Mutex::new(Vec::new()),
            places: Mutex::new(Vec::new()),
            insert: Mutex::new(None),
            acts: Mutex::new(Vec::new()),
        }
    }

    fn then(self, act: impl FnOnce() + Send + 'static) -> Answers {
        self.acts.lock().unwrap().insert(0, Box::new(act));
        self
    }
}

impl Ui for Answers {
    fn missing_tape(&self, barcode: &str, _: Option<&Tape>, _: bool, places: &Places) -> Answer {
        self.asked.lock().unwrap().push(barcode.into());
        self.places.lock().unwrap().push(places.clone());
        if let Some((sim, cart)) = self.insert.lock().unwrap().take() {
            sim.insert(cart).unwrap();
        }
        if let Some(act) = self.acts.lock().unwrap().pop() {
            act();
        }
        self.answers.lock().unwrap().pop().unwrap_or(Answer::Cancel)
    }
}

fn tree(dir: &Path) {
    fs::create_dir_all(dir.join("sub\\empty")).unwrap();
    fs::write(dir.join("a.txt"), b"alpha").unwrap();
    fs::write(dir.join("sub\\b.bin"), vec![9u8; 700_000]).unwrap();
}

#[test]
fn write_read_across_tapes() {
    let (_sim, a) = archive();
    let t = std::env::temp_dir().join(format!("lto-archive-{}", std::process::id()));
    let src = t.join("src");
    tree(&src);
    a.format("LTO001L5", "one", &NoUi).unwrap();
    a.format("LTO002L5", "two", &NoUi).unwrap();
    let mut last = (0, 0);
    a.write("LTO001L5", &[(src.clone(), "backup".into())], &NoUi, &mut |d, t| {
        last = (d, t);
        true
    })
    .unwrap();
    assert_eq!(last, (700_005, 700_005));
    a.write("LTO002L5", &[(src.join("a.txt"), "x\\a.txt".into())], &NoUi, &mut |_, _| true).unwrap();

    // the catalog knows, without a tape
    {
        let c = a.catalog.lock().unwrap();
        let t1 = c.tape("LTO001L5").unwrap().unwrap();
        assert_eq!((t1.files, t1.bytes, t1.generation, t1.name.as_str()), (2, 700_005, 2, "one"));
        assert!(t1.remaining.is_some());
        assert!(c.children("LTO001L5", "backup\\sub\\empty").unwrap().unwrap().is_empty());
    }

    let mut got = Got::default();
    a.read(&["LTO002L5\\x".into(), "LTO001L5\\backup".into()], &mut got, &NoUi).unwrap();
    let names: Vec<&str> = got.files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names.len(), 3);
    assert!(names.contains(&"LTO001L5\\backup\\a.txt") && names.contains(&"LTO001L5\\backup\\sub\\b.bin") && names.contains(&"LTO002L5\\x\\a.txt"));
    assert!(got.dirs.contains(&"LTO001L5\\backup\\sub\\empty".to_string()));
    let b = &got.files.iter().find(|(n, _)| n.ends_with("b.bin")).unwrap().1;
    assert_eq!(b, &vec![9u8; 700_000]);

    // a file not wanted is passed over; a path that isn't there is reported
    let mut got = Got { skip: vec!["LTO001L5\\backup\\sub\\b.bin".into()], ..Got::default() };
    let e = a.read(&["LTO001L5\\backup".into(), "LTO001L5\\nope".into()], &mut got, &NoUi).unwrap_err();
    assert!(matches!(e, Error::Missing(ref m) if m == &vec!["LTO001L5\\nope".to_string()]));
    assert_eq!(got.files.len(), 1);

    // rename, mkdir, delete
    a.rename("LTO001L5", "backup\\a.txt", "backup\\renamed.txt", &NoUi).unwrap();
    a.mkdir("LTO001L5", "new\\deep", &NoUi).unwrap();
    a.delete("LTO001L5", &["backup\\sub".into()], &NoUi).unwrap();
    let c = a.catalog.lock().unwrap();
    assert!(c.entry("LTO001L5", "backup\\renamed.txt").unwrap().is_some());
    assert!(c.entry("LTO001L5", "backup\\sub").unwrap().is_none());
    assert!(c.is_dir("LTO001L5", "new\\deep").unwrap());
    assert_eq!(c.tape("LTO001L5").unwrap().unwrap().generation, 5);
    drop(c);
    assert!(matches!(a.mkdir("LTO001L5", "", &NoUi), Err(Error::Refused(_))));
    fs::remove_dir_all(&t).unwrap();
}

#[test]
fn missing_tapes_are_asked_for() {
    let (sim, a) = archive();
    a.format("LTO003L5", "", &NoUi).unwrap();
    a.format("LTO004L5", "", &NoUi).unwrap();
    let t = std::env::temp_dir().join(format!("lto-archive-m-{}", std::process::id()));
    fs::create_dir_all(&t).unwrap();
    fs::write(t.join("f.txt"), b"f").unwrap();
    a.write("LTO003L5", &[(t.join("f.txt"), "f.txt".into())], &NoUi, &mut |_, _| true).unwrap();
    a.write("LTO004L5", &[(t.join("f.txt"), "g.txt".into())], &NoUi, &mut |_, _| true).unwrap();
    a.site().unwrap().unload_all().unwrap();
    // both tapes taken out of the library
    let c3 = sim.remove("LTO003L5").unwrap();
    sim.remove("LTO004L5").unwrap();

    // asked for 3: inserted, retried, read; asked for 4: skipped
    let ui = Answers::new(&[Answer::Retry, Answer::Skip]);
    *ui.insert.lock().unwrap() = Some((sim.clone(), c3));
    let mut got = Got::default();
    let e = a.read(&["LTO003L5\\f.txt".into(), "LTO004L5\\g.txt".into()], &mut got, &ui).unwrap_err();
    assert!(matches!(e, Error::Skipped(ref s) if s == &vec!["LTO004L5".to_string()]), "{e:?}");
    assert_eq!(*ui.asked.lock().unwrap(), vec!["LTO003L5".to_string(), "LTO004L5".to_string()]);
    assert_eq!(got.files, vec![("LTO003L5\\f.txt".to_string(), b"f".to_vec())]);
    // the catalog knows where 3 is now
    assert!(a.catalog.lock().unwrap().tape("LTO003L5").unwrap().unwrap().in_library());
    assert!(!a.catalog.lock().unwrap().tape("LTO004L5").unwrap().unwrap().in_library());
    // writing to a missing tape: cancelled
    let e = a.write("LTO004L5", &[(t.join("f.txt"), "h.txt".into())], &Answers::new(&[Answer::Cancel]), &mut |_, _| true).unwrap_err();
    assert!(matches!(e, Error::NotInLibrary(_)));
    fs::remove_dir_all(&t).unwrap();
}

#[test]
fn a_stale_catalog_refuses_changes() {
    let (_sim, a) = archive();
    a.format("LTO005L5", "", &NoUi).unwrap();
    // another program changed the tape since: the catalog holds generation 1, the tape 2
    {
        let site = a.site().unwrap();
        let mut l = site.acquire("LTO005L5", &|_| None, &|| false).unwrap();
        let mut v = Volume::mount(l.drive(), "LTO005L5", "other").unwrap();
        v.index.make_dirs("from elsewhere", Time::now()).unwrap();
        v.commit().unwrap();
    }
    let e = a.mkdir("LTO005L5", "mine", &NoUi).unwrap_err();
    assert!(matches!(e, Error::StaleCatalog(_)));
    let c = a.catalog.lock().unwrap();
    assert!(c.is_dir("LTO005L5", "from elsewhere").unwrap(), "updated from the tape");
    assert!(!c.is_dir("LTO005L5", "mine").unwrap());
    drop(c);
    // now it is current: the change is made
    a.mkdir("LTO005L5", "mine", &NoUi).unwrap();
    // import of a tape without LTFS
    assert!(a.import("LTO006L5", &NoUi).unwrap().is_none());
    assert!(!a.catalog.lock().unwrap().tape("LTO006L5").unwrap().unwrap().is_ltfs());
}

#[test]
fn not_enough_space() {
    let mut cfg = Config::default();
    cfg.capacity = 64 * 1024 * 1024;
    let sim = SimLibrary::new(&cfg);
    let a = Archive::new(Box::new(sim), Catalog::in_memory().unwrap(), "test");
    a.format("LTO001L5", "", &NoUi).unwrap();
    let t = std::env::temp_dir().join(format!("lto-archive-s-{}", std::process::id()));
    fs::create_dir_all(&t).unwrap();
    fs::write(t.join("big.bin"), vec![1u8; 70 * 1024 * 1024]).unwrap();
    let e = a.write("LTO001L5", &[(t.join("big.bin"), "big.bin".into())], &NoUi, &mut |_, _| true).unwrap_err();
    assert!(matches!(e, Error::NoSpace(..)), "{e}");
    fs::remove_dir_all(&t).unwrap();
}

// ---- standalone drives and several libraries ----

/// A file to copy onto tapes, in its own folder.
fn one_file(tag: &str) -> PathBuf {
    let t = std::env::temp_dir().join(format!("lto-archive-{tag}-{}", std::process::id()));
    fs::create_dir_all(&t).unwrap();
    fs::write(t.join("f.txt"), b"standalone").unwrap();
    t
}

/// No library, one standalone drive: a new tape is formatted in it, used,
/// taken out; asked for again, it is put back by hand.
#[test]
fn standalone_drive_without_a_library() {
    let drive = SimLibrary::new(&lto_sim::Config::standalone(11));
    let a = Archive::new(Box::new(drive.clone()), Catalog::in_memory().unwrap(), "test");
    let site = a.site().unwrap();
    assert!(site.libraries.is_empty());
    assert_eq!(site.standalone.len(), 1);
    let name = site.standalone[0].name.clone();
    drop(site);

    // nothing in the drive: asked for it, and where to put it
    let ui = Answers::new(&[Answer::Cancel]);
    let e = a.format("NEW001L5", "", &ui).unwrap_err();
    assert!(matches!(e, Error::NotInLibrary(_)), "{e}");
    let places = ui.places.lock().unwrap()[0].clone();
    assert!(places.libraries.is_empty());
    assert_eq!(places.drives, vec![(name.clone(), Holds::Empty)]);
    assert_eq!(places.ejected, None, "an empty drive needs no room made");

    // a brand-new tape (no barcode in its memory, no LTFS) is taken by Format
    drive.insert_drive(0, lto_sim::Cart::new("NEW001L5", lto_sim::Config::default().capacity)).unwrap();
    a.format("NEW001L5", "neu", &NoUi).unwrap();
    let t = one_file("sd");
    a.write("NEW001L5", &[(t.join("f.txt"), "f.txt".into())], &NoUi, &mut |_, _| true).unwrap();
    assert_eq!(a.catalog.lock().unwrap().tape("NEW001L5").unwrap().unwrap().location, format!("drive {name}"));

    // while a job has the drive, its eject button is locked
    {
        let site = a.site().unwrap();
        let lease = site.acquire("NEW001L5", &|_| None, &|| false).unwrap();
        assert!(drive.drive_locked(0));
        assert!(drive.take_drive(0).is_none(), "can't be taken out during a job");
        drop(lease);
        assert!(!drive.drive_locked(0));
    }

    // taken out: reading asks for it; it is put back and read
    let cart = drive.take_drive(0).unwrap();
    let d2 = drive.clone();
    let ui = Answers::new(&[Answer::Retry]).then(move || d2.insert_drive(0, cart).unwrap());
    let mut got = Got::default();
    a.read(&["NEW001L5\\f.txt".into()], &mut got, &ui).unwrap();
    assert_eq!(got.files, vec![("NEW001L5\\f.txt".to_string(), b"standalone".to_vec())]);
    assert_eq!(*ui.asked.lock().unwrap(), vec!["NEW001L5".to_string()]);
    fs::remove_dir_all(&t).unwrap();
}

/// The wrong tape is in the only standalone drive: it is ejected to make
/// room, the person swaps the tapes, and the job goes on.
#[test]
fn the_wrong_tape_is_ejected_to_make_room() {
    let drive = SimLibrary::new(&lto_sim::Config::standalone(11));
    let a = Archive::new(Box::new(drive.clone()), Catalog::in_memory().unwrap(), "test");
    for b in ["AAA001L5", "BBB001L5"] {
        drive.insert_drive(0, lto_sim::Cart::new(b, lto_sim::Config::default().capacity)).unwrap();
        a.format(b, "", &NoUi).unwrap();
        a.mkdir(b, "d", &NoUi).unwrap();
        a.site().unwrap().eject(b, &|| false).unwrap();
        *SHELF.lock().unwrap() = Some(drive.take_drive(0).unwrap());
        if b == "AAA001L5" {
            KEPT.lock().unwrap().replace(SHELF.lock().unwrap().take().unwrap());
        }
    }
    drive.insert_drive(0, KEPT.lock().unwrap().take().unwrap()).unwrap();
    // A is in the drive, B is wanted
    let d2 = drive.clone();
    let ui = Answers::new(&[Answer::Retry]).then(move || {
        let a_out = d2.take_drive(0).expect("A was ejected and can be taken");
        assert_eq!(a_out.barcode, "AAA001L5");
        d2.insert_drive(0, SHELF.lock().unwrap().take().unwrap()).unwrap();
    });
    a.mkdir("BBB001L5", "e", &ui).unwrap();
    let places = ui.places.lock().unwrap()[0].clone();
    let name = a.site().unwrap().standalone[0].name.clone();
    assert_eq!(places.ejected, Some((name.clone(), Holds::Tape("AAA001L5".into()))));
    assert_eq!(places.drives, vec![(name, Holds::Empty)]);
    assert!(a.catalog.lock().unwrap().is_dir("BBB001L5", "e").unwrap());
}

static SHELF: Mutex<Option<lto_sim::Cart>> = Mutex::new(None);
static KEPT: Mutex<Option<lto_sim::Cart>> = Mutex::new(None);

/// A tape in a standalone drive without a barcode in its memory is known
/// by its LTFS volume: its UUID in the catalog, or its volume serial.
#[test]
fn tapes_without_a_barcode_are_known_by_their_volume() {
    let drive = SimLibrary::new(&lto_sim::Config::standalone(11));
    let a = Archive::new(Box::new(drive.clone()), Catalog::in_memory().unwrap(), "test");
    let strip = |d: &SimLibrary, b: &str| d.with_cart(b, |c| c.attrs.retain(|(_, id), _| *id != lto_scsi::tape::attr::BARCODE)).unwrap();
    let swap = |d: &SimLibrary, cart: lto_sim::Cart| {
        let _ = d.take_drive(0);
        d.insert_drive(0, cart).unwrap();
    };

    // seen before: found by the UUID of its volume
    drive.insert_drive(0, lto_sim::Cart::new("UUI001L5", lto_sim::Config::default().capacity)).unwrap();
    a.format("UUI001L5", "", &NoUi).unwrap();
    strip(&drive, "UUI001L5");
    a.reopen_if_idle();
    a.inventory(false).unwrap();
    assert_eq!(a.site().unwrap().standalone[0].holds(), Holds::Tape("UUI001L5".into()));

    // formatted elsewhere, never seen: named by its volume serial
    let other = lto_sim::SimLibrary::new(&lto_sim::Config::standalone(12));
    let cart = {
        other.insert_drive(0, lto_sim::Cart::new("OTH001L5", lto_sim::Config::default().capacity)).unwrap();
        let mut d = lto_scsi::tape::TapeDrive::new(other.drive(0));
        ltfs::format(&mut d, "OTH001L5", "", "other").unwrap();
        strip(&other, "OTH001L5");
        other.take_drive(0).unwrap()
    };
    swap(&drive, cart.clone());
    a.reopen_if_idle();
    a.inventory(false).unwrap();
    assert_eq!(a.site().unwrap().standalone[0].holds(), Holds::Tape("OTH001".into()), "the VOL1 serial");

    // ... unless the catalog knows a tape not yet imported whose barcode starts so
    a.catalog.lock().unwrap().set_locations(&[("OTH001L5".into(), "shelf".into())]).unwrap();
    a.reopen_if_idle();
    a.inventory(false).unwrap();
    assert_eq!(a.site().unwrap().standalone[0].holds(), Holds::Tape("OTH001L5".into()));
    assert!(a.import("OTH001L5", &NoUi).unwrap().is_some());

    // a blank tape without a barcode can't be told: only Format takes it
    swap(&drive, lto_sim::Cart::new("", lto_sim::Config::default().capacity));
    a.reopen_if_idle();
    a.inventory(false).unwrap();
    assert_eq!(a.site().unwrap().standalone[0].holds(), Holds::Unknown);
    let e = a.import("UUI001L5", &NoUi).unwrap_err();
    assert!(matches!(e, Error::NotInLibrary(_)), "{e}");
}

/// Two libraries and a standalone drive: each library loads its own
/// tapes, a tape moved by hand to the standalone drive is used there, the
/// question names every place, and a barcode in both libraries is noted.
#[test]
fn two_libraries_and_a_standalone_drive() {
    let dir = std::env::temp_dir().join(format!("lto-archive-site-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let sim = lto_sim::SimSite::persistent(&dir.join("sim-library.bin"), 2, 1).unwrap();
    let a = Archive::new(Box::new(sim.clone()), Catalog::in_memory().unwrap(), "test");
    let site = a.site().unwrap();
    assert_eq!(site.libraries.len(), 2);
    assert!(site.libraries.iter().all(|l| l.drives.len() == 2), "each library found its own drives");
    assert_eq!(site.standalone.len(), 1);
    assert_eq!(site.drive_count(), 5);
    let (lib1, lib2, sd) = (site.libraries[0].name.clone(), site.libraries[1].name.clone(), site.standalone[0].name.clone());
    assert_ne!(lib1, lib2);
    drop(site);

    // a tape of each library, by its own robot
    let t = one_file("site");
    for b in ["LTO001L5", "LT2001L5"] {
        a.format(b, "", &NoUi).unwrap();
        a.write(b, &[(t.join("f.txt"), "f.txt".into())], &NoUi, &mut |_, _| true).unwrap();
    }
    let loc = |b: &str| a.catalog.lock().unwrap().tape(b).unwrap().unwrap().location;
    assert!(loc("LT2001L5").starts_with(&format!("{lib2}: ")), "{}", loc("LT2001L5"));

    // moved by hand into the standalone drive: read from there
    a.site().unwrap().unload_all().unwrap();
    sim.hand("LTO001L5", "drive1").unwrap();
    a.inventory(false).unwrap();
    assert_eq!(loc("LTO001L5"), format!("drive {sd}"));
    let mut got = Got::default();
    a.read(&["LTO001L5\\f.txt".into(), "LT2001L5\\f.txt".into()], &mut got, &NoUi).unwrap();
    assert_eq!(got.files.len(), 2);

    // on the shelf: the question names both mail slots and the drive
    a.site().unwrap().unload_all().unwrap();
    sim.hand("LT2001L5", "shelf").unwrap();
    let ui = Answers::new(&[Answer::Cancel]);
    let _ = a.mkdir("LT2001L5", "x", &ui);
    let places = ui.places.lock().unwrap()[0].clone();
    assert_eq!(places.libraries, vec![lib1.clone(), lib2.clone()]);
    assert_eq!(places.drives.len(), 1);
    // ... put into the first library this time
    let s2 = sim.clone();
    let ui = Answers::new(&[Answer::Retry]).then(move || s2.hand("LT2001L5", "mail1").unwrap());
    a.mkdir("LT2001L5", "x", &ui).unwrap();
    assert!(loc("LT2001L5").starts_with(&format!("{lib1}: ")), "{}", loc("LT2001L5"));

    // the same barcode in both libraries: noted, the first one's used
    sim.parts[1].insert(lto_sim::Cart::new("LTO002L5", lto_sim::Config::default().capacity)).unwrap();
    a.reopen_if_idle();
    let notes = a.inventory(false).unwrap().all_notes();
    assert!(notes.iter().any(|n| n.contains("LTO002L5") && n.contains("both")), "{notes:?}");
    fs::remove_dir_all(&t).unwrap();
    drop(a);
    let _ = fs::remove_dir_all(&dir);
}

/// A library's drive that holds another tape than the program thinks
/// (tapes moved at the library's panel, or drives matched to the wrong tape
/// devices): the cartridge says so, and nothing is done to it; the
/// inventory is read again, and then the right tape is used.
#[test]
fn the_wrong_tape_in_a_drive_is_left_alone() {
    let (sim, a) = archive();
    a.format("LTO001L5", "one", &NoUi).unwrap();
    a.format("LTO002L5", "two", &NoUi).unwrap();
    let d1 = (0..2).find(|&i| sim.drive_tape(i).as_deref() == Some("LTO001L5")).expect("in a drive");
    let d2 = 1 - d1;
    assert_eq!(sim.drive_tape(d2).as_deref(), Some("LTO002L5"));
    // swapped behind the program's back
    let (c1, c2) = (sim.take_drive(d1).unwrap(), sim.take_drive(d2).unwrap());
    sim.insert_drive(d1, c2).unwrap();
    sim.insert_drive(d2, c1).unwrap();
    // formatting "LTO001L5" would hit LTO002L5: refused
    let e = a.format("LTO001L5", "again", &NoUi).unwrap_err();
    assert!(matches!(&e, Error::WrongTape { wanted, found, .. } if wanted == "LTO001L5" && found.starts_with("LTO002L5")), "{e}");
    // LTO002L5 is untouched
    a.import("LTO002L5", &NoUi).unwrap();
    assert_eq!(a.catalog.lock().unwrap().tape("LTO002L5").unwrap().unwrap().name, "two");
    // the inventory was read again: now the right tape is formatted
    a.format("LTO001L5", "again", &NoUi).unwrap();
    assert_eq!(a.catalog.lock().unwrap().tape("LTO001L5").unwrap().unwrap().name, "again");
    a.import("LTO002L5", &NoUi).unwrap();
    assert_eq!(a.catalog.lock().unwrap().tape("LTO002L5").unwrap().unwrap().name, "two");
}

/// What a cartridge says it is, against the barcode asked for.
#[test]
fn a_cartridge_is_the_tape_it_says() {
    assert!(Said::Barcode("300AFGL5".into()).is("300afgl5"));
    assert!(Said::Barcode("300AFG".into()).is("300AFGL5"), "cut to six characters");
    assert!(!Said::Barcode("301AFGL5".into()).is("300AFGL5"));
    assert!(!Said::Barcode("300AF".into()).is("300AFGL5"), "too short to tell");
    assert!(Said::Serial("300AFG".into()).is("300AFGL5"));
    assert!(!Said::Serial("301AFG".into()).is("300AFGL5"));
}

/// The library's devices as a computer sees them: `drives` are which of
/// the simulated drives appear as tape devices, in that order (one twice:
/// a SAS drive's two ports).
struct Seen {
    sim: SimLibrary,
    drives: Vec<usize>,
}

impl lto_scsi::library::Devices for Seen {
    fn open_changers(&self) -> io::Result<Vec<Box<dyn lto_scsi::Transport>>> {
        Ok(vec![self.sim.changer()])
    }
    fn open_drives(&self) -> io::Result<Vec<Box<dyn lto_scsi::Transport>>> {
        Ok(self.drives.iter().map(|&i| self.sim.drive(i)).collect())
    }
}

/// The TS3100 case: library drive 2 seen twice (both devices say the same
/// serial number), drive 1 not at all. Drive 2 is used once, drive 1 not:
/// before, drive 1 got the second device by order, so a job for the tape
/// in drive 1 was done to the tape in drive 2.
#[test]
fn a_drive_seen_twice_is_one_drive() {
    let sim = SimLibrary::new(&Config::default());
    let site = lto_scsi::site::Site::open(&Seen { sim: sim.clone(), drives: vec![1, 1] }).unwrap();
    let lib = &site.libraries[0];
    assert_eq!(lib.drives.len(), 1, "{:?}", site.all_notes());
    let inv = lib.inventory();
    assert_eq!(lib.drives[0].element, inv.drives[1].address, "the device is library drive 2");
    let notes = site.all_notes().join("\n");
    assert!(notes.contains("seen a second time"), "{notes}");
    assert!(notes.contains("isn't among this computer's tape devices"), "{notes}");
    assert!(!notes.contains("matched in order"), "{notes}");
    drop(site);
    // jobs go to drive 2, and to the tape asked for
    let a = Archive::new(Box::new(Seen { sim: sim.clone(), drives: vec![1, 1] }), Catalog::in_memory().unwrap(), "test");
    a.format("LTO001L5", "one", &NoUi).unwrap();
    assert_eq!(sim.drive_tape(1).as_deref(), Some("LTO001L5"));
    assert_eq!(sim.drive_tape(0), None, "drive 1 isn't used");
    a.format("LTO002L5", "two", &NoUi).unwrap();
    assert_eq!(sim.drive_tape(1).as_deref(), Some("LTO002L5"));
    a.import("LTO001L5", &NoUi).unwrap();
    assert_eq!(a.catalog.lock().unwrap().tape("LTO001L5").unwrap().unwrap().name, "one");
}

/// Retry pressed before the library has taken in what was put into its
/// mail slot (the door just closed, the robot can't reach it yet): the job
/// waits for the library and then loads the tape, without asking again.
#[test]
fn retry_before_the_library_has_read_its_mail_slot() {
    let (sim, a) = archive();
    a.format("LTO001L5", "one", &NoUi).unwrap();
    a.site().unwrap().eject("LTO001L5", &|| false).unwrap();
    let cart = sim.take().expect("in the mail slot");
    let s2 = sim.clone();
    let ui = Answers::new(&[Answer::Retry]).then(move || {
        {
            let mut st = s2.state.lock().unwrap();
            st.mail[0] = Some(cart);
            st.mail_accessible = false;
        }
        let s3 = s2.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(4));
            s3.state.lock().unwrap().mail_accessible = true;
        });
    });
    let started = std::time::Instant::now();
    assert!(a.label("LTO001L5", &ui).unwrap().is_some(), "the tape was loaded and read");
    assert_eq!(*ui.asked.lock().unwrap(), ["LTO001L5"], "asked once, not again after Retry");
    assert!(started.elapsed() >= std::time::Duration::from_secs(4), "it waited for the library");
}

/// A tape taken out of the mail slot just before an Eject: the library
/// answers the inventory with UNIT ATTENTION 28/01 (its contents changed).
/// That is news, not an error: the command is sent again. And while the
/// robot can't reach the mail slot (the library still taking it in), Eject
/// waits for it.
#[test]
fn a_unit_attention_or_a_busy_mail_slot_does_not_stop_eject() {
    use lto_scsi::sense::{key, Sense};
    let (sim, a) = archive();
    a.format("LTO001L5", "one", &NoUi).unwrap();
    a.format("LTO002L5", "two", &NoUi).unwrap();
    let site = a.site().unwrap();
    site.eject("LTO001L5", &|| false).unwrap();
    // taken out of the mail slot: the next inventory meets the news
    sim.take().unwrap();
    sim.state.lock().unwrap().fail_next = Some((0xB8, Sense::new(key::UNIT_ATTENTION, 0x28, 0x01)));
    site.refresh(false, &|_| None).unwrap();
    // the door just closed: the robot reaches the mail slot two seconds later
    sim.state.lock().unwrap().mail_accessible = false;
    let s2 = sim.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(2));
        s2.state.lock().unwrap().mail_accessible = true;
    });
    site.refresh(false, &|_| None).unwrap();
    site.eject("LTO002L5", &|| false).unwrap();
    t.join().unwrap();
    assert_eq!(sim.take().map(|c| c.barcode).as_deref(), Some("LTO002L5"));
    // and a wait can be cancelled
    sim.state.lock().unwrap().mail_accessible = false;
    site.refresh(false, &|_| None).unwrap();
    assert!(matches!(site.eject("LTO003L5", &|| true), Err(lto_scsi::library::LoadError::Cancelled)));
}

/// Retry pressed, and the tape shows up in the mail slot a few seconds
/// later (the library reports it only once it has read it): it is looked
/// for for up to RETRY_WAIT before the question comes again.
#[test]
fn after_retry_the_tape_is_looked_for_a_while() {
    let (sim, a) = archive();
    a.format("LTO001L5", "one", &NoUi).unwrap();
    a.site().unwrap().eject("LTO001L5", &|| false).unwrap();
    let cart = sim.take().expect("in the mail slot");
    let s2 = sim.clone();
    let ui = Answers::new(&[Answer::Retry]).then(move || {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(4));
            s2.insert(cart).unwrap();
        });
    });
    let started = std::time::Instant::now();
    assert!(a.label("LTO001L5", &ui).unwrap().is_some(), "the tape was loaded and read");
    assert_eq!(*ui.asked.lock().unwrap(), ["LTO001L5"], "asked once");
    assert!(started.elapsed() < RETRY_WAIT, "taken as soon as it showed up");
}

// ------------------------------------------------------------- backups

/// A library whose tapes hold about 10 MB of data each, with LTO001L5 and LTO002L5
/// formatted.
fn small_library() -> (SimLibrary, Archive) {
    let sim = SimLibrary::new(&Config { capacity: 24 * 1024 * 1024, ..Config::default() });
    let a = Archive::new(Box::new(sim.clone()), Catalog::in_memory().unwrap(), "test");
    a.format("LTO001L5", "one", &NoUi).unwrap();
    a.format("LTO002L5", "two", &NoUi).unwrap();
    (sim, a)
}

/// A folder of `n` files of `size` bytes in `sub`, each different, and an
/// empty folder.
fn backup_source(name: &str, n: usize, size: usize) -> PathBuf {
    let src = std::env::temp_dir().join(format!("lto-backup-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&src);
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::create_dir_all(src.join("empty")).unwrap();
    for i in 0..n {
        let data: Vec<u8> = (0..size).map(|j| (j * 7 + i * 13) as u8).collect();
        fs::write(src.join("sub").join(format!("f{i:02}.bin")), data).unwrap();
    }
    src
}

/// Every file of a backup set on the tapes: (path under the set, tape).
fn backed_up(a: &Archive, tapes: &[&str], set: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for t in tapes {
        for (p, _) in a.catalog.lock().unwrap().files_under(t, set).unwrap() {
            out.push((p[set.len() + 1..].to_owned(), t.to_string()));
        }
    }
    out.sort();
    out
}

/// Answers a backup's questions: new tapes are put into the mail slot from
/// `carts` (a refused one taken out first), a blank one is formatted or not
/// as `format` says; Cancel once `cancel_after` files are done.
struct BackupUi {
    sim: SimLibrary,
    carts: Mutex<Vec<lto_sim::Cart>>,
    problems: Mutex<Vec<String>>,
    format: Answer,
    formatted: Mutex<Vec<String>>,
    cancel_after: Option<u64>,
    cancelled: std::sync::atomic::AtomicBool,
}

impl BackupUi {
    fn new(sim: &SimLibrary, carts: Vec<lto_sim::Cart>) -> BackupUi {
        BackupUi {
            sim: sim.clone(),
            carts: Mutex::new(carts.into_iter().rev().collect()),
            problems: Mutex::new(Vec::new()),
            format: Answer::Retry,
            formatted: Mutex::new(Vec::new()),
            cancel_after: None,
            cancelled: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl Ui for BackupUi {
    fn step(&self, step: Step) {
        if let (Step::Backup { files, .. }, Some(n)) = (step, self.cancel_after) {
            if files.0 >= n {
                self.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    fn cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn missing_tape(&self, _: &str, _: Option<&Tape>, _: bool, _: &Places) -> Answer {
        Answer::Cancel
    }
    fn new_tape(&self, _: u64, _: &Places, problem: Option<NewTapeProblem>) -> Answer {
        self.problems.lock().unwrap().push(format!("{problem:?}"));
        // a tape refused: taken out of the mail slot again
        if let Some(NewTapeProblem::HasData(b)) = problem {
            self.sim.remove(b).unwrap();
        }
        let mut carts = self.carts.lock().unwrap();
        match carts.pop() {
            Some(c) => {
                // the mail slot still holds a tape: Retry with that one
                if let Err(c) = self.sim.insert(c) {
                    carts.push(c);
                }
                Answer::Retry
            }
            None => Answer::Cancel,
        }
    }
    fn confirm_format(&self, barcode: &str) -> Answer {
        self.formatted.lock().unwrap().push(barcode.to_owned());
        self.format
    }
}

/// 12 files of 2 MB onto tapes of about 10 MB of data: the two ticked tapes, then new
/// ones asked for, formatted after the question. Every file is whole on
/// exactly one tape (one cut by the end of a tape goes onto the next),
/// under the same set folder, and reads back; the Archive attributes are
/// cleared.
#[test]
fn a_backup_spans_tapes_and_asks_for_new_ones() {
    let (sim, a) = small_library();
    let src = backup_source("span", 12, 2 * 1024 * 1024);
    let cap = 24 * 1024 * 1024;
    let ui = BackupUi::new(&sim, vec![lto_sim::Cart::new("NEW001L5", cap), lto_sim::Cart::new("NEW002L5", cap)]);
    let set = "Backup T\\span";
    let r = a.backup(&src, set, &["LTO002L5".into(), "lto001l5".into()], BackupOptions { archive_only: false, clear_archive: true, ..Default::default() }, &ui).unwrap();
    assert!(!r.cancelled && r.too_big.is_empty() && r.unreadable.is_empty(), "{r:?}");
    assert_eq!(r.written(), (12, 12 * 2 * 1024 * 1024));
    let order: Vec<&str> = r.tapes.iter().map(|t| t.barcode.as_str()).collect();
    assert_eq!(&order[..2], ["LTO001L5", "LTO002L5"], "ticked tapes in barcode order first");
    assert!(order.len() >= 3, "{order:?}");
    assert_eq!(*ui.formatted.lock().unwrap(), ["NEW001L5"]);
    let got = backed_up(&a, &order, set);
    let names: Vec<&str> = got.iter().map(|(p, _)| p.as_str()).collect();
    let want: Vec<String> = (0..12).map(|i| format!("sub\\f{i:02}.bin")).collect();
    assert_eq!(names, want, "each file once");
    // the empty folder is on the first tape
    assert!(a.catalog.lock().unwrap().entry("LTO001L5", &format!("{set}\\empty")).unwrap().is_some());
    // one from the last tape reads back whole
    let (last_name, last_tape) = got.iter().find(|(_, t)| t == order.last().unwrap()).unwrap().clone();
    let mut g = Got::default();
    a.read(&[format!("{last_tape}\\{set}\\{last_name}")], &mut g, &NoUi).unwrap();
    assert_eq!(g.files[0].1, fs::read(src.join(&last_name)).unwrap());
    for i in 0..12 {
        assert!(!archive_set(&src.join(format!("sub\\f{i:02}.bin"))).unwrap(), "cleared");
    }
    let _ = fs::remove_dir_all(&src);
}

/// Archive-only takes the files with the attribute set; clearing it is a
/// choice; a run with nothing changed writes nothing.
#[test]
fn an_archive_only_backup_takes_what_changed() {
    let (_sim, a) = small_library();
    let src = backup_source("arch", 3, 1000);
    for i in 0..3 {
        clear_archive(&src.join(format!("sub\\f{i:02}.bin"))).unwrap();
    }
    let b = src.join("sub\\f01.bin");
    set_archive(&b).unwrap();
    let only = |clear| BackupOptions { archive_only: true, clear_archive: clear, ..Default::default() };
    // differential: the attribute stays
    let r = a.backup(&src, "Backup 1\\arch", &["LTO001L5".into()], only(false), &NoUi).unwrap();
    assert_eq!(r.written().0, 1);
    assert_eq!(backed_up(&a, &["LTO001L5"], "Backup 1\\arch"), [("sub\\f01.bin".to_string(), "LTO001L5".to_string())]);
    assert!(archive_set(&b).unwrap());
    // incremental: cleared
    let r = a.backup(&src, "Backup 2\\arch", &["LTO001L5".into()], only(true), &NoUi).unwrap();
    assert_eq!(r.written().0, 1);
    assert!(!archive_set(&b).unwrap());
    // nothing changed: nothing written, no empty set folder
    let r = a.backup(&src, "Backup 3\\arch", &["LTO001L5".into()], only(true), &NoUi).unwrap();
    assert_eq!((r.total_files, r.tapes.len()), (0, 0));
    assert!(a.catalog.lock().unwrap().entry("LTO001L5", "Backup 3").unwrap().is_none());
    let _ = fs::remove_dir_all(&src);
}

/// A file larger than a whole tape is left out and reported, its
/// attribute left set; the rest is backed up.
#[test]
fn a_file_larger_than_a_tape_is_left_out() {
    let (_sim, a) = small_library();
    let src = backup_source("big", 1, 1000);
    let big = src.join("big.bin");
    fs::write(&big, vec![1u8; 30 * 1024 * 1024]).unwrap();
    let r = a.backup(&src, "Backup B\\big", &["LTO001L5".into()], BackupOptions { archive_only: false, clear_archive: true, ..Default::default() }, &NoUi).unwrap();
    assert_eq!(r.too_big, [big.clone()]);
    assert_eq!(r.written().0, 1);
    assert!(archive_set(&big).unwrap(), "left set");
    let _ = fs::remove_dir_all(&src);
}

/// A new tape that holds files is refused (and the question says so); a
/// blank one is formatted only after the question.
#[test]
fn a_new_tape_with_data_is_refused() {
    let (sim, a) = small_library();
    // LTO003L5 holds a file and lies in the mail slot
    a.format("LTO003L5", "three", &NoUi).unwrap();
    let t = std::env::temp_dir().join(format!("lto-backup-{}-data.txt", std::process::id()));
    fs::write(&t, b"keep me").unwrap();
    a.write("LTO003L5", &[(t.clone(), "keep.txt".into())], &NoUi, &mut |_, _| true).unwrap();
    a.site().unwrap().eject("LTO003L5", &|| false).unwrap();
    let src = backup_source("refuse", 6, 2 * 1024 * 1024);
    let ui = BackupUi::new(&sim, vec![lto_sim::Cart::new("NEW009L5", 24 * 1024 * 1024)]);
    // only LTO001L5 ticked: full after four files; the mail slot offers LTO003L5 first
    let r = a.backup(&src, "Backup R\\refuse", &["LTO001L5".into()], BackupOptions { archive_only: false, clear_archive: false, ..Default::default() }, &ui).unwrap();
    assert_eq!(r.written().0, 6, "{r:?}");
    let problems = ui.problems.lock().unwrap().clone();
    assert!(problems.iter().any(|p| p.contains("HasData(\"LTO003L5\")")), "{problems:?}");
    assert_eq!(*ui.formatted.lock().unwrap(), ["NEW009L5"]);
    assert!(!r.tapes.iter().any(|t| t.barcode == "LTO003L5"), "the tape with data wasn't written");
    assert_eq!(a.catalog.lock().unwrap().tape("LTO003L5").unwrap().unwrap().files, 1);
    let _ = (fs::remove_dir_all(&src), fs::remove_file(&t));
}

/// Cancel in the middle: the index of what was written is on the tape, and
/// only those files have their attribute cleared.
#[test]
fn a_cancelled_backup_keeps_what_it_wrote() {
    let (sim, a) = small_library();
    let src = backup_source("cancel", 6, 100_000);
    let mut ui = BackupUi::new(&sim, Vec::new());
    ui.cancel_after = Some(2);
    let r = a.backup(&src, "Backup C\\cancel", &["LTO001L5".into()], BackupOptions { archive_only: false, clear_archive: true, ..Default::default() }, &ui).unwrap();
    assert!(r.cancelled);
    assert_eq!(r.written().0, 2);
    assert_eq!(backed_up(&a, &["LTO001L5"], "Backup C\\cancel").len(), 2);
    let set: Vec<bool> = (0..6).map(|i| archive_set(&src.join(format!("sub\\f{i:02}.bin"))).unwrap()).collect();
    assert_eq!(set, [false, false, true, true, true, true]);
    let _ = fs::remove_dir_all(&src);
}

/// The index written along the way: at the end of the data only, the tape
/// doesn't go to the index partition each time (fewer LOCATEs); either way
/// everything is on the tape and reads back, and the index at the end of
/// the tape is in both partitions.
#[test]
fn a_backup_writes_its_interim_index_where_chosen() {
    const LOCATE16: u8 = 0x92;
    backup::TEST_COMMIT_EVERY.with(|c| c.set(Some(1024 * 1024)));
    let src = backup_source("interim", 6, 1024 * 1024);
    let mut locates = Vec::new();
    for interim_index in [InterimIndex::DataPartition, InterimIndex::BothPartitions] {
        let (sim, a) = small_library();
        let before = sim.count(LOCATE16);
        let r = a.backup(&src, "Backup I\\interim", &["LTO001L5".into()], BackupOptions { archive_only: false, clear_archive: false, interim_index }, &NoUi).unwrap();
        assert_eq!(r.written().0, 6, "{r:?}");
        locates.push(sim.count(LOCATE16) - before);
        // the catalog's index is the tape's, and the files read back
        let mut g = Got::default();
        a.read(&["LTO001L5\\Backup I\\interim\\sub\\f05.bin".to_string()], &mut g, &NoUi).unwrap();
        assert_eq!(g.files[0].1, fs::read(src.join("sub\\f05.bin")).unwrap());
        // a fresh catalog, reading the tape, finds the same index
        // generation
        let generation = a.catalog.lock().unwrap().tape("LTO001L5").unwrap().unwrap().generation;
        let fresh = Archive::new(Box::new(sim.clone()), Catalog::in_memory().unwrap(), "test");
        fresh.import("LTO001L5", &NoUi).unwrap();
        assert_eq!(fresh.catalog.lock().unwrap().tape("LTO001L5").unwrap().unwrap().generation, generation);
    }
    backup::TEST_COMMIT_EVERY.with(|c| c.set(None));
    assert!(locates[0] + 5 <= locates[1], "data partition only: fewer moves {locates:?}");
    let _ = fs::remove_dir_all(&src);
}

/// A ticked tape that was written to after the catalog last saw it (here
/// through another catalog): the backup brings the catalog up to date and
/// goes on, instead of stopping; what was written before stays.
#[test]
fn a_backup_onto_a_tape_the_catalog_is_behind() {
    let (sim, a) = small_library();
    let other = Archive::new(Box::new(sim.clone()), Catalog::in_memory().unwrap(), "test");
    other.import("LTO001L5", &NoUi).unwrap();
    let t = std::env::temp_dir().join(format!("lto-backup-{}-elsewhere.txt", std::process::id()));
    fs::write(&t, b"written elsewhere").unwrap();
    other.write("LTO001L5", &[(t.clone(), "elsewhere.txt".into())], &NoUi, &mut |_, _| true).unwrap();
    let src = backup_source("behind", 2, 1000);
    let r = a.backup(&src, "Backup S\\behind", &["LTO001L5".into()], BackupOptions::default(), &NoUi).unwrap();
    assert_eq!(r.written().0, 2, "{r:?}");
    assert!(a.catalog.lock().unwrap().entry("LTO001L5", "elsewhere.txt").unwrap().is_some(), "the catalog caught up");
    assert_eq!(backed_up(&a, &["LTO001L5"], "Backup S\\behind").len(), 2);
    let mut g = Got::default();
    a.read(&["LTO001L5\\elsewhere.txt".to_string()], &mut g, &NoUi).unwrap();
    assert_eq!(g.files[0].1, b"written elsewhere");
    let _ = (fs::remove_dir_all(&src), fs::remove_file(&t));
}

/// The detailed log of a backup: the SCSI commands (data records summed
/// up), what mount found in each partition, the catalog compared with the
/// tape, each file with its block, and each index written.
#[test]
fn a_backup_is_in_the_detailed_log() {
    let (_sim, a) = small_library();
    let src = backup_source("logged", 3, 1000);
    let file = std::env::temp_dir().join(format!("lto-backup-{}-detail.log", std::process::id()));
    let _ = fs::remove_file(&file);
    lto_scsi::log::open(&file).unwrap();
    let r = a.backup(&src, "Backup L\\logged", &["LTO001L5".into()], BackupOptions::default(), &NoUi);
    lto_scsi::log::close();
    assert_eq!(r.unwrap().written().0, 3);
    let text = fs::read_to_string(&file).unwrap();
    for want in [
        "job: backup",
        "acquire LTO001L5: in ",
        "the cartridge says LTO001L5",
        "LOCATE [92 ",
        "ltfs last index of partition a: blocks ",
        "ltfs last index of partition b: blocks ",
        "ltfs mount LTO001L5: volume ",
        "catalog LTO001L5: holds volume ",
        ": the same",
        "backup: \"sub\\f00.bin\" 1000 bytes at b:",
        "WRITE x",
        "ltfs index LTO001L5: generation 2 into the data partition at block ",
        "ltfs commit LTO001L5: generation 2 into the index partition",
        "catalog LTO001L5: recorded volume ",
        "backup done: 3 files",
    ] {
        assert!(text.contains(want), "{want:?} isn't in the log:\n{text}");
    }
    // values, not program internals
    for internal in ["Some(", "None", "Ok(", "Err(", "Time {", "\\\\"] {
        assert!(!text.contains(internal), "{internal:?} is in the log:\n{text}");
    }
    let _ = (fs::remove_dir_all(&src), fs::remove_file(&file));
}

/// The TS3100 as it is used: one drive, three tapes formatted by the
/// plugin and ticked, a backup filling all three, with an interim index
/// now and then (every 100 GB on real tapes), written either way.
#[test]
fn a_backup_fills_three_ticked_tapes_in_one_drive() {
    let file = std::env::temp_dir().join(format!("lto-backup-{}-three.log", std::process::id()));
    let _ = fs::remove_file(&file);
    lto_scsi::log::open(&file).unwrap();
    for interim_index in [InterimIndex::BothPartitions, InterimIndex::DataPartition] {
        for drives in [1, 2] {
            let cap = 24 * 1024 * 1024;
            let sim = SimLibrary::new(&Config { capacity: cap, drives, ..Config::default() });
            let a = Archive::new(Box::new(sim.clone()), Catalog::in_memory().unwrap(), "test");
            for b in ["LTO001L5", "LTO002L5", "LTO003L5"] {
                a.format(b, b, &NoUi).unwrap();
            }
            backup::TEST_COMMIT_EVERY.with(|c| c.set(Some(3 * 1024 * 1024)));
            let src = backup_source(&format!("three-{drives}-{interim_index:?}"), 13, 2 * 1024 * 1024);
            let r = a.backup(&src, "Backup 3\\three", &["LTO001L5".into(), "LTO002L5".into(), "LTO003L5".into()], BackupOptions { interim_index, ..Default::default() }, &NoUi);
            backup::TEST_COMMIT_EVERY.with(|c| c.set(None));
            let r = r.unwrap_or_else(|e| panic!("{drives} drives, {interim_index:?}: {e}"));
            let order: Vec<&str> = r.tapes.iter().map(|t| t.barcode.as_str()).collect();
            assert_eq!(order, ["LTO001L5", "LTO002L5", "LTO003L5"], "{drives} drives, {interim_index:?}: {r:?}");
            let _ = fs::remove_dir_all(&src);
        }
    }
    lto_scsi::log::close();
    let text = fs::read_to_string(&file).unwrap();
    let mine: Vec<&str> = text.lines().filter(|l| l.contains("three_ticked") && l.contains("OUT OF DATE")).collect();
    assert!(mine.is_empty(), "{}", mine.join("
"));
}

/// A cartridge in the drive that holds another tape's volume than the one
/// asked for (the catalog has another volume for that barcode): nothing is
/// written, the catalog isn't overwritten with the wrong tape's index, and
/// the error names the tape the volume belongs to.
#[test]
fn another_volume_in_the_drive_is_never_written_or_recorded() {
    let (sim, a) = small_library();
    let before = a.catalog.lock().unwrap().tape("LTO002L5").unwrap().unwrap();
    // LTO002L5's cartridge now holds what LTO001L5 holds (labels and
    // memory say LTO002L5, as a mixed-up drive would show)
    let one = sim.cart("LTO001L5").unwrap();
    sim.with_cart("LTO002L5", |c| c.parts = one.parts.clone()).unwrap();
    let src = backup_source("other", 2, 1000);
    // the backup ends with its report, which says why it stopped
    let r = a.backup(&src, "Backup O\\other", &["LTO002L5".into()], BackupOptions::default(), &NoUi).unwrap();
    let e = r.error.expect("stopped");
    assert!(e.contains("LTO002L5") && e.contains("(the volume in the drive is tape LTO001L5's)"), "{e}");
    assert_eq!(r.failed_tapes.len(), 1);
    assert!(r.tapes.is_empty());
    let after = a.catalog.lock().unwrap().tape("LTO002L5").unwrap().unwrap();
    assert_eq!((after.uuid, after.generation), (before.uuid, before.generation), "the catalog kept LTO002L5's own volume");
    assert_eq!(sim.cart("LTO002L5").unwrap().parts, one.parts, "nothing written");
    let _ = fs::remove_dir_all(&src);
}

/// Folders have an Archive attribute too: a backup clears it on the
/// folders it put on the tape; archive-only takes a folder whose attribute
/// is set even when nothing in it changed (an empty one too), and leaves
/// out the folders whose attribute is clear and that hold nothing changed.
#[test]
fn folders_have_their_archive_attribute_cleared_too() {
    let (_sim, a) = small_library();
    let src = backup_source("folders", 2, 1000);
    fs::create_dir_all(src.join("quiet")).unwrap();
    let dirs = [src.join("sub"), src.join("empty"), src.join("quiet")];
    for d in &dirs {
        set_archive(d).unwrap();
    }
    let all = BackupOptions { clear_archive: true, ..Default::default() };
    a.backup(&src, "Backup F1\\folders", &["LTO001L5".into()], all, &NoUi).unwrap();
    for d in &dirs {
        assert!(!archive_set(d).unwrap(), "{} cleared", d.display());
    }
    // a new empty folder, and one whose attribute is set again
    fs::create_dir_all(src.join("new")).unwrap();
    set_archive(&src.join("new")).unwrap();
    set_archive(&src.join("empty")).unwrap();
    let only = BackupOptions { archive_only: true, clear_archive: true, ..Default::default() };
    let r = a.backup(&src, "Backup F2\\folders", &["LTO001L5".into()], only, &NoUi).unwrap();
    assert_eq!((r.total_files, r.total_folders), (0, 2), "{r:?}");
    let c = a.catalog.lock().unwrap();
    assert!(c.entry("LTO001L5", "Backup F2\\folders\\new").unwrap().is_some());
    assert!(c.entry("LTO001L5", "Backup F2\\folders\\empty").unwrap().is_some());
    assert!(c.entry("LTO001L5", "Backup F2\\folders\\quiet").unwrap().is_none(), "unchanged: left out");
    drop(c);
    assert!(!archive_set(&src.join("new")).unwrap() && !archive_set(&src.join("empty")).unwrap());
    // nothing changed now
    let r = a.backup(&src, "Backup F3\\folders", &["LTO001L5".into()], only, &NoUi).unwrap();
    assert_eq!((r.total_files, r.total_folders, r.tapes.len()), (0, 0, 0));
    let _ = fs::remove_dir_all(&src);
}

/// The backup's step says the average speed once data has been written,
/// and not before.
#[test]
fn a_backup_step_has_its_average_speed() {
    struct Speeds(Mutex<Vec<Option<u64>>>);
    impl Ui for Speeds {
        fn step(&self, step: Step) {
            if let Step::Backup { speed, .. } = step {
                self.0.lock().unwrap().push(speed);
            }
        }
        fn missing_tape(&self, _: &str, _: Option<&Tape>, _: bool, _: &Places) -> Answer {
            Answer::Cancel
        }
    }
    let (_sim, a) = small_library();
    let src = backup_source("speed", 3, 100_000);
    let ui = Speeds(Mutex::new(Vec::new()));
    a.backup(&src, "Backup S\\speed", &["LTO001L5".into()], BackupOptions::default(), &ui).unwrap();
    let seen = ui.0.into_inner().unwrap();
    assert_eq!(seen.first(), Some(&None), "{seen:?}");
    assert!(seen.len() >= 3 && seen[1..].iter().all(|s| s.is_some_and(|b| b > 0)), "{seen:?}");
    let _ = fs::remove_dir_all(&src);
}

/// Records what a job says about damage, and answers whether a backup
/// goes on with another tape.
#[derive(Default)]
struct DamageUi {
    warnings: Mutex<Vec<IndexWarning>>,
    failed: Mutex<Vec<(String, String)>>,
    go_on: bool,
}

impl Ui for DamageUi {
    fn missing_tape(&self, _: &str, _: Option<&Tape>, _: bool, _: &Places) -> Answer {
        Answer::Cancel
    }
    fn index_damaged(&self, w: &IndexWarning) {
        self.warnings.lock().unwrap().push(w.clone());
    }
    fn tape_failed(&self, barcode: &str, error: &str, _: u64) -> Answer {
        self.failed.lock().unwrap().push((barcode.into(), error.into()));
        if self.go_on {
            Answer::Retry
        } else {
            Answer::Cancel
        }
    }
}

fn condition(a: &Archive, barcode: &str) -> Vec<lto_catalog::Condition> {
    a.catalog.lock().unwrap().tape(barcode).unwrap().unwrap().condition
}

/// The data partition's index can't be read: the job says so, reads with
/// the index partition's, refuses changes, and the catalog notes the tape;
/// a catalog newer than that index isn't set back.
#[test]
fn a_damaged_index_is_said_and_the_other_used() {
    use lto_catalog::Condition as C;
    let (sim, a) = small_library();
    let src = backup_source("dmg-index", 3, 100_000);
    a.write("LTO001L5", &[(src.clone(), "x".into())], &NoUi, &mut |_, _| true).unwrap();
    sim.damage_last_index("LTO001L5", 1).unwrap();
    let ui = DamageUi::default();
    let mut g = Got::default();
    a.read(&[r"LTO001L5\x".into()], &mut g, &ui).unwrap();
    assert_eq!(g.files.len(), 3, "read with the other index");
    {
        let w = ui.warnings.lock().unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].damage[0].partition, w[0].damage[0].label, w[0].partition), ('b', false, 'a'));
        assert_eq!(w[0].catalog_generation, None, "both copies were the same generation");
    }
    let cond = condition(&a, "LTO001L5");
    assert!(cond.contains(&C::Index('b')) && cond.contains(&C::Alert(4)), "{cond:?}");
    assert!(a.take_notices().iter().any(|n| n.flag == 4 && n.barcode == "LTO001L5"));
    assert!(a.take_notices().is_empty(), "taken once");
    assert!(matches!(a.mkdir("LTO001L5", "new", &ui), Err(Error::Ltfs(ltfs::Error::ReadOnly(_)))));
    let _ = fs::remove_dir_all(&src);
}

/// A check reads every file, goes on past one that can't be read, notes
/// the tape; a check without a fault clears what was noted.
#[test]
fn a_check_finds_the_files_that_cannot_be_read() {
    use lto_catalog::Condition as C;
    let (sim, a) = small_library();
    let src = backup_source("verify", 4, 600_000);
    a.write("LTO001L5", &[(src.clone(), "v".into())], &NoUi, &mut |_, _| true).unwrap();
    let r = a.verify(&["LTO001L5".into()], &NoUi).unwrap();
    assert_eq!((r[0].files, r[0].total_files, r[0].bad.len(), r[0].bytes), (4, 4, 0, 2_400_000));
    assert!(condition(&a, "LTO001L5").is_empty());
    // the second file's first block is damaged
    let ix = a.import("LTO001L5", &NoUi).unwrap().unwrap();
    let Some(Node::File(f)) = ix.find(r"v\sub\f01.bin").cloned() else { panic!("f01") };
    sim.damage("LTO001L5", 1, f.extents[0].start_block as usize);
    let steps = Mutex::new(0u32);
    struct Count<'a>(&'a Mutex<u32>);
    impl Ui for Count<'_> {
        fn step(&self, s: Step) {
            if matches!(s, Step::Verifying { .. }) {
                *self.0.lock().unwrap() += 1;
            }
        }
        fn missing_tape(&self, _: &str, _: Option<&Tape>, _: bool, _: &Places) -> Answer {
            Answer::Cancel
        }
    }
    let r = a.verify(&["LTO001L5".into()], &Count(&steps)).unwrap();
    assert_eq!((r[0].files, r[0].bad.len()), (3, 1), "{r:?}");
    assert!(r[0].bad[0].0.ends_with("f01.bin"), "{:?}", r[0].bad);
    assert!(*steps.lock().unwrap() >= 4);
    let cond = condition(&a, "LTO001L5");
    assert!(cond.contains(&C::Read) && cond.contains(&C::Unreadable(1)) && cond.contains(&C::Alert(5)), "{cond:?}");
    // the place is good again (another drive read it): cleared
    sim.with_cart("LTO001L5", |c| c.damaged.clear());
    let r = a.verify(&["LTO001L5".into()], &NoUi).unwrap();
    assert!(r[0].bad.is_empty());
    assert!(condition(&a, "LTO001L5").is_empty());
    // a tape not put in is skipped
    let r = a.verify(&["NOPE01L5".into()], &DamageUi::default());
    assert!(matches!(r, Err(Error::NotInLibrary(_))) || r.unwrap()[0].skipped);
    let _ = fs::remove_dir_all(&src);
}

/// A tape that can't be written: the person goes on with the next one,
/// and what its index doesn't hold is written there; or stops, and the
/// report says why.
#[test]
fn a_backup_goes_on_after_a_tape_fails() {
    use lto_catalog::Condition as C;
    let (sim, a) = small_library();
    let src = backup_source("failover", 4, 1024 * 1024);
    // LTO001L5 fails in its second file (two 512 KiB blocks a file), and
    // its index can't be written there either
    let end = sim.cart("LTO001L5").unwrap().parts[1].len();
    sim.damage("LTO001L5", 1, end + 3);
    let ui = DamageUi { go_on: true, ..DamageUi::default() };
    let set = r"Backup T\failover";
    let r = a.backup(&src, set, &["LTO001L5".into(), "LTO002L5".into()], BackupOptions { archive_only: false, clear_archive: true, ..Default::default() }, &ui).unwrap();
    assert_eq!(r.error, None, "{r:?}");
    assert_eq!(r.failed_tapes.len(), 1);
    assert_eq!(r.failed_tapes[0].0, "LTO001L5");
    assert_eq!(ui.failed.lock().unwrap().len(), 1);
    assert_eq!(r.written(), (4, 4 * 1024 * 1024), "{r:?}");
    let got = backed_up(&a, &["LTO001L5", "LTO002L5"], set);
    assert_eq!(got.len(), 4);
    assert!(got.iter().all(|(_, t)| t == "LTO002L5"), "the first tape's index never held them: {got:?}");
    assert!(condition(&a, "LTO001L5").contains(&C::Write));
    for i in 0..4 {
        assert!(!archive_set(&src.join(format!(r"sub\f{i:02}.bin"))).unwrap());
    }
    // stopping instead: the report says why
    let src2 = backup_source("failover-stop", 2, 1024 * 1024);
    let r = a.backup(&src2, r"Backup T\stop", &["LTO001L5".into()], BackupOptions { archive_only: false, clear_archive: true, ..Default::default() }, &DamageUi::default()).unwrap();
    assert!(r.error.is_some() && !r.cancelled, "{r:?}");
    assert_eq!(r.failed_tapes.len(), 1);
    let _ = (fs::remove_dir_all(&src), fs::remove_dir_all(&src2));
}

/// The drive asks to be cleaned: the job's notices say so; nothing goes
/// against the tape.
#[test]
fn a_drive_asking_for_cleaning_is_said() {
    let (sim, a) = small_library();
    sim.raise_alert(0, lto_sim::alert::CLEAN_NOW);
    sim.raise_alert(1, lto_sim::alert::CLEAN_NOW);
    a.import("LTO001L5", &NoUi).unwrap();
    let n = a.take_notices();
    assert_eq!(n.iter().map(|n| n.flag).collect::<Vec<_>>(), [20], "{n:?}");
    assert!(condition(&a, "LTO001L5").is_empty(), "cleaning is the drive's");
}
