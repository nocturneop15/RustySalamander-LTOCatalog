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
    assert_eq!(p.commands().len(), 10);
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
