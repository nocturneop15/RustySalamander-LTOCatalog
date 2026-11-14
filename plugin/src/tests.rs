//! The plugin on a simulated library, called as the program calls it.

use std::sync::atomic::AtomicUsize;

use lto_sim::{Config, SimLibrary};
use sal_plugin_api::mount_root;

use super::*;

fn plugin(asker: Option<Asker>) -> (SimLibrary, LtoCatalog, PathBuf) {
    let sim = SimLibrary::new(&Config::default());
    let archive = Archive::new(Box::new(sim.clone()), Catalog::in_memory().unwrap(), "test");
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!("ltocatalog-plugin-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir_all(&dir).unwrap();
    (sim, LtoCatalog::with(dir.clone(), Settings::default(), archive, asker), dir)
}

fn root() -> PathBuf {
    mount_root(SCHEME, AUTHORITY)
}

fn names(ix: &Index) -> Vec<String> {
    let mut n: Vec<String> = ix.items.iter().map(|i| i.path.clone()).collect();
    n.sort();
    n
}

#[test]
fn browse_copy_in_and_out() {
    let (_sim, p, dir) = plugin(None);
    let r = root();
    assert!(matches!(p.run("open", &Values::new()).unwrap(), Reply::Mount(_)));
    // the library's tapes appear after an inventory, not yet imported
    assert!(matches!(p.run("inventory", &Values::new()).unwrap(), Reply::Message(m) if m.contains("LTO001L5")));
    assert_eq!(p.list(&r, "").unwrap().items.len(), 6);
    let e = p.list(&r, "LTO001L5").unwrap_err();
    assert!(e.to_string().contains("not in the catalog"), "{e}");

    // format through the form: a wrong confirmation does nothing
    let mut v = Values::new();
    v.insert("barcode".into(), "lto001l5".into());
    v.insert("name".into(), "Archiv".into());
    v.insert("confirm".into(), "LTO002L5".into());
    assert!(matches!(p.run("format_do", &v).unwrap(), Reply::Message(m) if m.contains("Nothing was done")));
    v.insert("confirm".into(), "LTO001L5".into());
    p.run("format_do", &v).unwrap();
    assert!(p.list(&r, "LTO001L5").unwrap().items.is_empty());

    // copy in: a folder with an awkward name, and a file
    let src = dir.join("src");
    fs::create_dir_all(src.join("fotky")).unwrap();
    fs::write(src.join("fotky\\léto 🦎.jpg"), vec![5u8; 600_000]).unwrap();
    fs::write(src.join("a.txt"), b"alpha").unwrap();
    let mut last = (0, 0);
    p.add(&r, &[(src.join("fotky"), "LTO001L5\\fotky".into()), (src.join("a.txt"), "LTO001L5\\a.txt".into())], &mut |d, t| {
        last = (d, t);
        true
    })
    .unwrap();
    assert_eq!(last, (600_005, 600_005));
    assert_eq!(names(&p.list(&r, "LTO001L5").unwrap()), ["LTO001L5\\a.txt", "LTO001L5\\fotky"]);
    let photos = p.list(&r, "LTO001L5\\fotky").unwrap();
    assert_eq!(photos.items[0].size, 600_000);
    // a file next to the tapes is refused
    assert!(p.add(&r, &[(src.join("a.txt"), "a.txt".into())], &mut |_, _| true).is_err());

    // copy out, with progress
    let out = dir.join("out");
    fs::create_dir_all(&out).unwrap();
    let mut last = (0, 0);
    p.extract(&r, &["LTO001L5\\fotky".into(), "LTO001L5\\a.txt".into()], &out, &mut |d, t| {
        last = (d, t);
        true
    })
    .unwrap();
    assert_eq!(fs::read(out.join("LTO001L5\\fotky\\léto 🦎.jpg")).unwrap(), vec![5u8; 600_000]);
    assert_eq!(fs::read(out.join("LTO001L5\\a.txt")).unwrap(), b"alpha");
    assert_eq!(last, (600_005, 600_005));
    // cancelled
    let e = p.extract(&r, &["LTO001L5\\fotky".into()], &dir.join("out2"), &mut |_, _| false).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::Interrupted);

    // dragged out: the program's sink gets the files
    struct Got(Vec<(String, u64, usize)>);
    impl Sink for Got {
        fn begin(&mut self, inner: &str, size: u64) -> bool {
            self.0.push((inner.into(), size, 0));
            true
        }
        fn write(&mut self, data: &[u8]) -> bool {
            self.0.last_mut().unwrap().2 += data.len();
            true
        }
    }
    let mut got = Got(Vec::new());
    p.stream(&r, &["LTO001L5".into()], &mut got).unwrap();
    got.0.sort();
    assert_eq!(got.0, vec![("LTO001L5\\a.txt".to_string(), 5, 5), ("LTO001L5\\fotky\\léto 🦎.jpg".to_string(), 600_000, 600_000)]);

    // folders, renames, deletes
    p.mkdir(&r, "LTO001L5\\new", &mut |_, _| true).unwrap();
    p.rename(&r, "LTO001L5\\a.txt", "LTO001L5\\new\\b.txt", &mut |_, _| true).unwrap();
    assert_eq!(names(&p.list(&r, "LTO001L5\\new").unwrap()), ["LTO001L5\\new\\b.txt"]);
    p.delete(&r, &["LTO001L5\\fotky".into()], &mut |_, _| true).unwrap();
    assert_eq!(names(&p.list(&r, "LTO001L5").unwrap()), ["LTO001L5\\new"]);
    assert!(p.mkdir(&r, "NEWTAPE", &mut |_, _| true).is_err());
    assert!(p.delete(&r, &["LTO001L5".into()], &mut |_, _| true).is_err());
    assert!(p.rename(&r, "LTO001L5\\new", "LTO002L5\\new", &mut |_, _| true).is_err());
    assert!(p.status().contains("1 tapes"));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn names_windows_forbids() {
    let (_sim, p, dir) = plugin(None);
    let r = root();
    let mut v = Values::new();
    v.insert("barcode".into(), "LTO002L5".into());
    v.insert("confirm".into(), "LTO002L5".into());
    p.run("format_do", &v).unwrap();
    // another program wrote a name with a colon and a question mark
    {
        let a = p.archive();
        let site = a.site().unwrap();
        let mut l = site.acquire("LTO002L5", &|_| None, &|| false).unwrap();
        let mut vol = ltfs::Volume::mount(l.drive(), "LTO002L5", "other").unwrap();
        vol.index.make_dirs("12:00 why?", Time::now()).unwrap();
        vol.commit().unwrap();
        let ix = vol.index.clone();
        drop(vol);
        a.catalog.lock().unwrap().import("LTO002L5", &ix, None, None, false).unwrap();
    }
    let items = p.list(&r, "LTO002L5").unwrap().items;
    let shown = &items[0].path;
    assert!(!shown.contains(':') && !shown.contains('?'), "{shown}");
    // and it can be used by that name
    p.mkdir(&r, &format!("{shown}\\inside"), &mut |_, _| true).unwrap();
    assert_eq!(p.list(&r, shown).unwrap().items.len(), 1);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_missing_tape_is_asked_for() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let a2 = asked.clone();
    let (sim, p, dir) = plugin(Some(Box::new(move |b: &str, skip, _: &Places| {
        a2.lock().unwrap().push((b.to_owned(), skip));
        Answer::Cancel
    })));
    let r = root();
    let mut v = Values::new();
    v.insert("barcode".into(), "LTO003L5".into());
    v.insert("confirm".into(), "LTO003L5".into());
    p.run("format_do", &v).unwrap();
    p.mkdir(&r, "LTO003L5\\d", &mut |_, _| true).unwrap();
    p.run("unload", &Values::new()).unwrap();
    sim.remove("LTO003L5").unwrap();
    // still browsable without the tape
    assert_eq!(p.list(&r, "LTO003L5").unwrap().items.len(), 1);
    let e = p.extract(&r, &["LTO003L5\\d".into()], &dir, &mut |_, _| true).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::Interrupted);
    assert_eq!(*asked.lock().unwrap(), vec![("LTO003L5".to_string(), true)]);
    let e = p.mkdir(&r, "LTO003L5\\e", &mut |_, _| true).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::Interrupted);
    assert_eq!(asked.lock().unwrap()[1], ("LTO003L5".to_string(), false));
    let Reply::Message(m) = p.run("status", &Values::new()).unwrap() else { panic!() };
    assert!(m.contains("LTO003L5") && m.contains("in no library or drive"), "{m}");
    fs::remove_dir_all(&dir).unwrap();
}

/// Started through the C interface, as the program starts it, with the
/// simulated library and a catalog of its own.
#[test]
fn starts_through_the_program() {
    use sal_plugin_api::abi::{Bytes, HostApi, Str, StrOut, WindowFns, VERSION};
    use std::ffi::c_void;
    unsafe extern "C" fn translate(_: *mut c_void, msg: Str, out: *const StrOut) {
        (*out).send(msg.get());
    }
    unsafe extern "C" fn open_window(_: *mut c_void, _: *mut c_void, _: *const WindowFns, _: Bytes) -> bool {
        false
    }
    unsafe extern "C" fn repaint(_: *mut c_void, _: u64) {}
    static HOST: HostApi = HostApi { version: VERSION, ctx: std::ptr::null_mut(), translate, open_window, repaint };
    let dir = std::env::temp_dir().join(format!("ltocatalog-start-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    Settings { catalog: dir.join("cat.sqlite").display().to_string(), ..Settings::default() }.save(&dir.join("settings")).unwrap();
    // SAFETY: only this test reads the variable
    unsafe { std::env::set_var("LTOCATALOG_SIM", dir.join("sim.bin")) };
    let desc = unsafe { &*rustysalamander_plugin() };
    let p = unsafe { sal_plugin_api::host::start(desc, &HOST, &dir) }.unwrap();
    assert_eq!(p.id(), "ltocatalog");
    assert_eq!(p.commands().len(), 11);
    p.run("inventory", &Values::new()).unwrap();
    assert_eq!(p.list(&root(), "").unwrap().items.len(), 6);
    assert!(dir.join("cat.sqlite").exists() && dir.join("sim.bin").exists());
    drop(p);
    let _ = fs::remove_dir_all(&dir);
}

/// Every text the code translates is in `lang/en.toml`, and nothing else;
/// `cs.toml` translates every one of them.
#[test]
fn language_files_match_the_code() {
    let code = include_str!("lib.rs").to_string() + include_str!("ask.rs");
    let code = code.split("#[cfg(test)]").next().unwrap();
    let mut used = std::collections::BTreeSet::new();
    for marker in ["tr(\"", "trf(\"", "n_(\""] {
        let mut rest = code;
        while let Some(at) = rest.find(marker) {
            let s = &rest[at + marker.len()..];
            let mut out = String::new();
            let mut chars = s.chars();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => match chars.next() {
                        Some('n') => out.push('\n'),
                        Some(o) => out.push(o),
                        None => {}
                    },
                    '"' => break,
                    c => out.push(c),
                }
            }
            // a marker inside another identifier (`ptr("`) isn't one
            let before = rest[..at].chars().last();
            if !before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                used.insert(out);
            }
            rest = &s[1..];
        }
    }
    let keys = |text: &str| -> std::collections::BTreeSet<String> {
        let strings = text.split("[strings]").nth(1).unwrap_or("");
        strings
            .lines()
            .filter(|l| l.starts_with('"'))
            .map(|l| {
                let mut out = String::new();
                let mut chars = l[1..].chars();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => match chars.next() {
                            Some('n') => out.push('\n'),
                            Some(o) => out.push(o),
                            None => {}
                        },
                        '"' => break,
                        c => out.push(c),
                    }
                }
                out
            })
            .collect()
    };
    let en = keys(include_str!("../lang/en.toml"));
    let cs = keys(include_str!("../lang/cs.toml"));
    let missing: Vec<_> = used.difference(&en).collect();
    let unused: Vec<_> = en.difference(&used).collect();
    assert!(missing.is_empty() && unused.is_empty(), "missing in en.toml: {missing:#?}\nnot used: {unused:#?}");
    let untranslated: Vec<_> = en.difference(&cs).collect();
    assert!(untranslated.is_empty(), "missing in cs.toml: {untranslated:#?}");
}

/// The question for a missing tape names the places it can go, and what
/// the standalone drives hold.
#[test]
fn the_question_says_where_to_put_the_tape() {
    let one_lib = Places { libraries: vec!["3573-TL".into()], ..Places::default() };
    assert!(ask_text("X", &one_lib).0.contains("mail slot"));
    let one_drive = Places { drives: vec![(r"\\.\Tape2".into(), Holds::Tape("Y".into()))], ..Places::default() };
    let (t, d) = ask_text("X", &one_drive);
    assert!(t.contains(r"Insert it into drive \\.\Tape2"), "{t}");
    assert_eq!(d, vec![r"Drive \\.\Tape2 holds tape Y.".to_string()]);
    let both = Places { libraries: vec!["A 1".into(), "A 2".into()], drives: vec![("d".into(), Holds::Unknown)], ejected: Some(("d".into(), Holds::Tape("Z".into()))) };
    let (t, d) = ask_text("X", &both);
    assert!(t.contains("A 1, A 2") && t.contains("drive d"), "{t}");
    assert!(d[0].contains("Z was ejected") && d[1].contains("can't be identified"), "{d:?}");
}

/// Only a standalone drive: the status says so, a new tape in it is
/// formatted through the form and ejected by Eject.
#[test]
fn standalone_drive_in_the_plugin() {
    let drive = SimLibrary::new(&Config::standalone(11));
    let archive = Archive::new(Box::new(drive.clone()), Catalog::in_memory().unwrap(), "test");
    let dir = std::env::temp_dir().join(format!("ltocatalog-plugin-sd-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let p = LtoCatalog::with(dir.clone(), Settings::default(), archive, None);
    drive.insert_drive(0, lto_sim::Cart::new("SD0001L5", Config::default().capacity)).unwrap();
    let mut v = Values::new();
    v.insert("barcode".into(), "SD0001L5".into());
    v.insert("confirm".into(), "SD0001L5".into());
    p.run("format_do", &v).unwrap();
    let Reply::Message(m) = p.run("status", &Values::new()).unwrap() else { panic!() };
    assert!(m.contains("Standalone drive sim11:tape0") && m.contains("SD0001L5") && m.contains("No tape library was found"), "{m}");
    let Reply::Message(m) = p.run("eject_do", &v).unwrap() else { panic!() };
    assert!(m.contains("came out of drive sim11:tape0"), "{m}");
    assert!(drive.take_drive(0).is_some());
    let Reply::Message(m) = p.run("inventory", &Values::new()).unwrap() else { panic!() };
    assert!(m.contains(": 0."), "{m}");
    fs::remove_dir_all(&dir).unwrap();
}

/// Hostile text in every form of the plugin: a message (or the error the
/// program shows), never a panic; a barcode that can't be one, a volume
/// name with control characters and a catalog that is a device are refused.
#[test]
fn hostile_input_in_every_form() {
    let (_sim, p, dir) = plugin(None);
    let v = |pairs: &[(&str, &str)]| -> Values { pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
    for s in sal_core::hostile::inputs() {
        let s = s.as_str();
        for (cmd, values) in [
            ("format_do", v(&[("barcode", s), ("confirm", s), ("name", "x")])),
            ("format_do", v(&[("barcode", "LTO001L5"), ("confirm", "LTO002L5"), ("name", s)])),
            ("eject_do", v(&[("barcode", s)])),
            ("note_do", v(&[("barcode", s), ("note", "x")])),
            ("note_do", v(&[("barcode", "LTO006L5"), ("note", s)])),
            ("forget_do", v(&[("barcode", s), ("confirm", "X")])),
            ("import_do", v(&[("which", "list"), ("barcodes", s)])),
            ("settings_do", v(&[("changer", ""), ("drives", ""), ("catalog", s)])),
            ("note", v(&[("selected", s)])),
            ("eject", v(&[("selected", s)])),
            ("format", v(&[("selected", s)])),
        ] {
            let _ = p.run(cmd, &values);
        }
        let bad = s.trim().to_uppercase();
        if !bad.is_empty() && !barcode_ok(&bad) {
            let e = p.run("eject_do", &v(&[("barcode", s)])).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{s:?}");
            assert!(e.to_string().contains("isn't a barcode"), "{e}");
        }
    }
    // a device is never the catalog
    for device in [r"\\.\PhysicalDrive0", r"\\.\C:", "NUL", r"C:\x\con.sqlite"] {
        let e = p.run("settings_do", &v(&[("catalog", device)])).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{device}: {e}");
    }
    // a volume name with a line break would make the index invalid XML
    let e = p.run("format_do", &v(&[("barcode", "LTO005L5"), ("confirm", "LTO005L5"), ("name", "a\nb")])).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    // the catalog is still whole and usable
    assert!(matches!(p.run("inventory", &Values::new()).unwrap(), Reply::Message(_)));
    drop(p);
    let _ = fs::remove_dir_all(&dir);
}

/// A long command says what it does (the program's status bar asks with
/// the empty root) and Cancel stops it between tapes.
#[test]
fn a_command_reports_its_steps_and_cancels() {
    static RELEASE: AtomicBool = AtomicBool::new(false);
    let (_sim, p, dir) = plugin(Some(Box::new(|_: &str, _, _: &Places| {
        // the missing tape's question waits until the test has looked
        while !RELEASE.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Answer::Skip
    })));
    let p = Arc::new(p);
    assert!(!p.activity(Path::new("")).running, "nothing runs yet");
    let p2 = p.clone();
    let job = std::thread::spawn(move || {
        let v: Values = [("which".to_string(), "list".to_string()), ("barcodes".to_string(), "MISS01L5 LTO001L5 LTO002L5".to_string())].into();
        p2.run("import_do", &v)
    });
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let seen = loop {
        let a = p.activity(Path::new(""));
        // "tape 1 of 3" comes first, then the step on it
        if a.running && a.text.contains("MISS01L5") && a.text.contains("getting it into a drive") {
            break a.text;
        }
        assert!(std::time::Instant::now() < until, "no step shown: {a:?}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    assert!(seen.contains("getting it into a drive"), "{seen}");
    p.stop(Path::new(""));
    assert!(p.activity(Path::new("")).text.contains("cancelling"));
    RELEASE.store(true, Ordering::Relaxed);
    let Reply::Message(m) = job.join().unwrap().unwrap() else { panic!("no message") };
    assert!(m.contains("Cancelled: 2 tapes weren't imported."), "{m}");
    assert!(!p.activity(Path::new("")).running, "done");
    drop(p);
    let _ = fs::remove_dir_all(&dir);
}

/// Values from pairs.
fn vals(pairs: &[(&str, &str)]) -> Values {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// The catalog is searched by a part of a name or a mask; the results open
/// in the program's results window, and a file found is copied out of them.
#[test]
fn search_the_catalog_and_copy_from_the_results() {
    let (_sim, p, dir) = plugin(None);
    let r = root();
    p.run("format_do", &vals(&[("barcode", "LTO001L5"), ("confirm", "LTO001L5")])).unwrap();
    let src = dir.join("src");
    fs::create_dir_all(src.join("filmy")).unwrap();
    fs::write(src.join("filmy").join("Arabesky 1080p.mkv"), vec![3u8; 300_000]).unwrap();
    fs::write(src.join("filmy").join("arabesky.srt"), b"titulky").unwrap();
    p.add(&r, &[(src.join("filmy"), "LTO001L5\\filmy".into())], &mut |_, _| true).unwrap();

    // the form, and a mistake: nothing typed
    assert!(matches!(p.run("search", &Values::new()).unwrap(), Reply::Form(f) if f.command == "search_do"));
    assert_eq!(p.run("search_do", &vals(&[("text", "  ")])).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    assert!(matches!(p.run("search_do", &vals(&[("text", "nowhere")])).unwrap(), Reply::Message(m) if m.contains("Nothing in the catalog")));

    // by a part of the name, any case
    let Reply::Results(res) = p.run("search_do", &vals(&[("text", "ARABESK")])).unwrap() else { panic!("no results") };
    assert!(res.title.contains("2 found"), "{}", res.title);
    let found = res.mount.root();
    let ix = p.list(&found, "").unwrap();
    assert!(ix.complete);
    let mut paths: Vec<&str> = ix.items.iter().map(|i| i.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, ["LTO001L5\\filmy\\Arabesky 1080p.mkv", "LTO001L5\\filmy\\arabesky.srt"]);
    // results can only be read
    assert!(!p.can_write(&found));
    // a file found, copied out (the library loads its tape)
    let out = dir.join("out");
    fs::create_dir_all(&out).unwrap();
    p.extract(&found, &["LTO001L5\\filmy\\arabesky.srt".into()], &out, &mut |_, _| true).unwrap();
    assert_eq!(fs::read(out.join("LTO001L5").join("filmy").join("arabesky.srt")).unwrap(), b"titulky");

    // by a mask, on one tape
    let Reply::Results(res) = p.run("search_do", &vals(&[("text", "*.mkv"), ("match", "mask"), ("tape", "lto001l5")])).unwrap() else { panic!() };
    assert_eq!(p.list(&res.mount.root(), "").unwrap().items.len(), 1);
    // closed: forgotten
    p.close(&res.mount.root());
    assert!(p.list(&res.mount.root(), "").is_err());
    drop(p);
    let _ = fs::remove_dir_all(&dir);
}

/// Several tapes formatted in one job: the form lists the tapes in the
/// library (those selected ticked); ERASE confirms.
#[test]
fn format_several_tapes_at_once() {
    let (_sim, p, dir) = plugin(None);
    let r = root();
    p.run("inventory", &Values::new()).unwrap();
    // LTO002L5 selected in the panel: ticked in the form
    let selected = sal_plugin_api::ui::encode(&[sal_plugin_api::ui::path_value(&r.join("LTO002L5"))]);
    let Reply::Form(f) = p.run("format", &vals(&[("selected", &selected)])).unwrap() else { panic!("no form") };
    let ticks: Vec<(&str, &str)> = f.fields.iter().filter(|f| f.id.starts_with("tape:")).map(|f| (f.id.as_str(), f.value.as_str())).collect();
    assert_eq!(ticks.len(), 6, "every tape in the library: {ticks:?}");
    assert_eq!(ticks.iter().filter(|(_, v)| !v.is_empty()).map(|(id, _)| *id).collect::<Vec<_>>(), ["tape:LTO002L5"]);
    assert!(f.fields.iter().any(|f| f.label.contains("LTO003L5") && f.label.contains("slot")), "where each tape is");

    // not confirmed: nothing happens
    let two = [("tape:LTO002L5", "1"), ("tape:LTO003L5", "1"), ("confirm", "LTO002L5")];
    assert!(matches!(p.run("format_do", &vals(&two)).unwrap(), Reply::Message(m) if m.contains("Nothing was done")));
    // ERASE: both formatted in one job
    let two = [("tape:LTO002L5", "1"), ("tape:LTO003L5", "1"), ("confirm", "erase"), ("name", "zálohy")];
    let Reply::Message(m) = p.run("format_do", &vals(&two)).unwrap() else { panic!() };
    assert_eq!(m.lines().count(), 2, "{m}");
    assert!(m.contains("LTO002L5: an empty LTFS volume") && m.contains("LTO003L5: an empty LTFS volume"), "{m}");
    assert!(p.list(&r, "LTO002L5").unwrap().items.is_empty() && p.list(&r, "LTO003L5").unwrap().items.is_empty());
    // the name given, on each
    assert_eq!(p.archive().catalog.lock().unwrap().tape("LTO003L5").unwrap().unwrap().name, "zálohy");
    // no name: each tape's barcode
    p.run("format_do", &vals(&[("tape:LTO004L5", "1"), ("confirm", "ERASE")])).unwrap();
    assert_eq!(p.archive().catalog.lock().unwrap().tape("LTO004L5").unwrap().unwrap().name, "LTO004L5");
    // the context menu on tapes: what is done to a tape, all run by the plugin
    let items: Vec<String> = p.item_commands().unwrap().into_iter().map(|c| c.id).collect();
    assert_eq!(items, ["search", "import", "format", "eject", "note", "forget"]);
    assert!(items.iter().all(|id| p.commands().iter().any(|c| &c.id == id)));
    // a barcode that can't be one, ticked by a crafted form
    assert_eq!(p.run("format_do", &vals(&[("tape:A\\B", "1"), ("confirm", "ERASE")])).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    drop(p);
    let _ = fs::remove_dir_all(&dir);
}

/// The program's update check finds the plugin's releases: tags
/// `ltocatalog-v<version>` in this repository, the changelog at its root
/// (package.ps1 and the release make `ltocatalog-<version>-api<n>.zip`).
#[test]
fn updates_are_published_where_the_releases_are() {
    let u = <LtoCatalog as Plugin>::UPDATES;
    assert_eq!(u.repo, "nocturneop15/RustySalamander-LTOCatalog");
    assert_eq!(u.tag, format!("{}-v", <LtoCatalog as Plugin>::ID));
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let log = fs::read_to_string(root.join(u.changelog)).unwrap();
    assert!(log.contains(&format!("\n## {} ", <LtoCatalog as Plugin>::VERSION)), "the changelog has this version");
}
