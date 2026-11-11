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
        a.site().unwrap().eject(b).unwrap();
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
