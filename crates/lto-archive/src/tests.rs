//! Jobs on the simulated library.

use std::sync::Mutex;

use lto_sim::{Config, SimLibrary};

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

/// Answers questions about missing tapes from a list, recording them.
struct Answers {
    answers: Mutex<Vec<Answer>>,
    asked: Mutex<Vec<String>>,
    /// Put this tape into the mail slot when asked.
    insert: Mutex<Option<(SimLibrary, lto_sim::Cart)>>,
}

impl Answers {
    fn new(a: &[Answer]) -> Answers {
        Answers { answers: Mutex::new(a.iter().rev().copied().collect()), asked: Mutex::new(Vec::new()), insert: Mutex::new(None) }
    }
}

impl Ui for Answers {
    fn missing_tape(&self, barcode: &str, _: Option<&Tape>, _: bool) -> Answer {
        self.asked.lock().unwrap().push(barcode.into());
        if let Some((sim, cart)) = self.insert.lock().unwrap().take() {
            sim.insert(cart).unwrap();
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
    a.library().unwrap().unload_all().unwrap();
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
        let lib = a.library().unwrap();
        let mut l = lib.acquire("LTO005L5", &|| false).unwrap();
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
