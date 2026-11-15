//! LTFS on the simulated library: format, write, read back, damage.

use lto_scsi::library::Library;
use lto_sim::{Config, Rec, SimLibrary};

use crate::index::{File, Node, Times};
use crate::volume::{format, probe, Error, Volume};
use crate::Time;

fn lib() -> (SimLibrary, Library) {
    let sim = SimLibrary::new(&Config::default());
    let lib = Library::open(&sim).unwrap();
    (sim, lib)
}

/// Write `files` (path, bytes) and commit.
fn write(v: &mut Volume, files: &[(&str, Vec<u8>)]) {
    for (path, data) in files {
        let (extents, length) = v.write_data(&mut &data[..], &mut |_| true).unwrap();
        let (dir, name) = path.rsplit_once('\\').unwrap_or(("", path));
        let now = Time::now();
        let uid = v.index.next_uid();
        let f = File { name: name.into(), length, readonly: false, open_for_write: false, times: Times::all(now), uid, xattrs: vec![], extents, symlink: None, extra: vec![] };
        v.index.put(dir, Node::File(f), now).unwrap();
    }
    v.commit().unwrap();
}

fn read(v: &mut Volume, path: &str) -> Vec<u8> {
    let Some(Node::File(f)) = v.index.find(path).cloned() else { panic!("{path} missing") };
    let mut out = Vec::new();
    v.read_file(&f, &mut |b| {
        out.extend_from_slice(b);
        Ok(true)
    })
    .unwrap();
    out
}

fn big(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

#[test]
fn format_write_remount_read() {
    let (sim, lib) = lib();
    let mut l = lib.acquire("LTO001L5", &|| false).unwrap();
    assert!(probe(l.drive()).unwrap().is_none(), "a new tape isn't LTFS");
    let f = format(l.drive(), "LTO001L5", "Backup", &crate::creator()).unwrap();
    assert!(f.warnings.is_empty(), "{:?}", f.warnings);
    assert_eq!(probe(l.drive()).unwrap().unwrap().volume_uuid, f.label.volume_uuid);
    {
        let mut v = Volume::mount(l.drive(), "LTO001L5", "test").unwrap();
        assert_eq!(v.index.generation, 1);
        assert_eq!(v.index.root.name, "Backup");
        assert!(v.read_only.is_none());
        write(&mut v, &[("docs\\a.txt", b"hello".to_vec()), ("docs\\big.bin", big(1_300_000, 7)), ("empty.txt", vec![]), ("exact.bin", big(512 * 1024, 3))]);
        assert_eq!(v.index.generation, 2);
        write(&mut v, &[("docs\\c.txt", b"second batch".to_vec())]);
        assert_eq!(v.index.generation, 3);
    }
    let cart = sim.cart("LTO001L5").unwrap();
    // index partition: label construct and one index, overwritten each time
    assert_eq!(cart.parts[0].len(), 6);
    assert_eq!(cart.parts[0][3], Rec::Mark);
    // the coherency attribute points at the newest index
    let vci = &cart.attrs[&(1u8, 0x080Cu16)].1;
    assert_eq!(u64::from_be_bytes(vci[9..17].try_into().unwrap()), 3);
    drop(l);
    // mounted again, as another program would: everything is there
    let mut l = lib.acquire("LTO001L5", &|| false).unwrap();
    let mut v = Volume::mount(l.drive(), "LTO001L5", "test").unwrap();
    assert_eq!(v.index.generation, 3);
    assert_eq!(read(&mut v, "docs\\a.txt"), b"hello");
    assert_eq!(read(&mut v, "docs\\big.bin"), big(1_300_000, 7));
    assert_eq!(read(&mut v, "exact.bin"), big(512 * 1024, 3));
    assert_eq!(read(&mut v, "empty.txt"), b"");
    assert_eq!(read(&mut v, "DOCS\\C.TXT"), b"second batch");
    // generation 3 points back at 2
    let prev = v.index.previous.unwrap();
    assert_eq!(prev.partition, 'b');
    assert!(prev.block < v.index.location.block);
}

#[test]
fn index_only_changes_and_cut_short_writes() {
    let (sim, lib) = lib();
    let mut l = lib.acquire("LTO002L5", &|| false).unwrap();
    format(l.drive(), "LTO002L5", "", "test").unwrap();
    {
        let mut v = Volume::mount(l.drive(), "LTO002L5", "test").unwrap();
        write(&mut v, &[("a.txt", b"aaa".to_vec())]);
        // a rename writes only an index, right after the last one
        v.index.rename("a.txt", "b.txt", Time::now()).unwrap();
        v.commit().unwrap();
        // data written but no index (the program stopped): passed over
        v.write_data(&mut &b"orphan"[..], &mut |_| true).unwrap();
    }
    let dp_len = sim.cart("LTO002L5").unwrap().parts[1].len();
    {
        let mut v = Volume::mount(l.drive(), "LTO002L5", "test").unwrap();
        assert_eq!(v.index.generation, 3);
        assert_eq!(read(&mut v, "b.txt"), b"aaa");
        // the next index after the orphan data starts with a filemark
        write(&mut v, &[("c.txt", b"ccc".to_vec())]);
    }
    let recs = &sim.cart("LTO002L5").unwrap().parts[1];
    assert_eq!(recs[dp_len], Rec::Data(b"ccc".to_vec()));
    assert_eq!(recs[dp_len + 1], Rec::Mark);
    let mut v = Volume::mount(l.drive(), "LTO002L5", "test").unwrap();
    assert_eq!(v.index.generation, 4);
    assert_eq!(read(&mut v, "c.txt"), b"ccc");
    assert_eq!(read(&mut v, "b.txt"), b"aaa");
}

#[test]
fn cancelled_full_and_protected() {
    let mut cfg = Config::default();
    cfg.capacity = 64 * 1024 * 1024;
    let sim = SimLibrary::new(&cfg);
    let lib = Library::open(&sim).unwrap();
    let mut l = lib.acquire("LTO003L5", &|| false).unwrap();
    format(l.drive(), "LTO003L5", "", "test").unwrap();
    {
        let mut v = Volume::mount(l.drive(), "LTO003L5", "test").unwrap();
        write(&mut v, &[("kept.bin", big(100_000, 1))]);
        // cancelled in the middle of a file: it isn't in the index
        let data = big(3_000_000, 2);
        let mut n = 0;
        let r = v.write_data(&mut &data[..], &mut |_| {
            n += 1;
            n < 3
        });
        assert!(matches!(r, Err(Error::Cancelled)));
        v.commit().unwrap();
        // until the tape is full
        let data = big(200 * 1024 * 1024, 3);
        assert!(matches!(v.write_data(&mut &data[..], &mut |_| true), Err(Error::Full)));
        v.commit().unwrap();
    }
    let mut v = Volume::mount(l.drive(), "LTO003L5", "test").unwrap();
    assert_eq!(read(&mut v, "kept.bin"), big(100_000, 1));
    assert_eq!(v.index.totals().0, 1);
    drop(v);
    drop(l);
    sim.with_cart("LTO003L5", |c| c.write_protect = true);
    let mut l = lib.acquire("LTO003L5", &|| false).unwrap();
    let mut v = Volume::mount(l.drive(), "LTO003L5", "test").unwrap();
    assert!(v.read_only.is_some());
    assert!(matches!(v.write_data(&mut &b"x"[..], &mut |_| true), Err(Error::ReadOnly(_))));
    assert!(matches!(format(l.drive(), "LTO003L5", "", "t"), Err(Error::ReadOnly(_))));
}

/// An index written to the data partition only (a backup's interim one):
/// the index partition keeps the one before, mounting still finds the
/// newer one, and the next full commit brings the index partition up to
/// date.
#[test]
fn an_index_in_the_data_partition_only_counts() {
    let (_sim, lib) = lib();
    let mut l = lib.acquire("LTO003L5", &|| false).unwrap();
    format(l.drive(), "LTO003L5", "", "test").unwrap();
    let ip_generation = |d: &mut lto_scsi::tape::TapeDrive| {
        let mut buf = vec![0u8; crate::label::BLOCK_SIZE as usize];
        crate::volume::last_index(d, 0, &mut buf).unwrap().unwrap().1.generation
    };
    {
        let mut v = Volume::mount(l.drive(), "LTO003L5", "test").unwrap();
        let (extents, length) = v.write_data(&mut &b"synced"[..], &mut |_| true).unwrap();
        let now = Time::now();
        let uid = v.index.next_uid();
        let f = File { name: "s.txt".into(), length, readonly: false, open_for_write: false, times: Times::all(now), uid, xattrs: vec![], extents, symlink: None, extra: vec![] };
        v.index.put("", Node::File(f), now).unwrap();
        v.sync().unwrap();
        assert_eq!(v.index.generation, 2);
        // the program stops here: no full commit
    }
    assert_eq!(ip_generation(l.drive()), 1, "the index partition wasn't written");
    {
        let mut v = Volume::mount(l.drive(), "LTO003L5", "test").unwrap();
        assert_eq!(v.index.generation, 2, "the newer index, from the data partition");
        assert_eq!(read(&mut v, "s.txt"), b"synced");
        write(&mut v, &[("t.txt", b"committed".to_vec())]);
    }
    assert_eq!(ip_generation(l.drive()), 3);
    let mut v = Volume::mount(l.drive(), "LTO003L5", "test").unwrap();
    assert_eq!(v.index.generation, 3);
    assert_eq!(read(&mut v, "s.txt"), b"synced");
    assert_eq!(read(&mut v, "t.txt"), b"committed");
}

/// One partition's index damaged: the other partition's is used, the
/// volume is read-only and says what couldn't be read; its files still read,
/// and a damaged place in a file fails that file only.
#[test]
fn a_damaged_index_is_made_up_for_by_the_other() {
    let (sim, lib) = lib();
    let mut l = lib.acquire("LTO003L5", &|| false).unwrap();
    format(l.drive(), "LTO003L5", "", "test").unwrap();
    {
        let mut v = Volume::mount(l.drive(), "LTO003L5", "test").unwrap();
        write(&mut v, &[("a.bin", big(300_000, 1)), ("b.bin", big(300_000, 2))]);
        assert!(v.damage.is_empty());
    }
    // the data partition's newest index
    sim.damage_last_index("LTO003L5", 1).expect("an index in b");
    {
        let mut v = Volume::mount(l.drive(), "LTO003L5", "test").unwrap();
        assert_eq!(v.damage.len(), 1);
        assert_eq!((v.damage[0].partition, v.damage[0].label), ('b', false));
        assert!(v.damage[0].error.contains("11/00") || v.damage[0].error.contains("unrecovered read error"), "{}", v.damage[0].error);
        assert_eq!(v.index.generation, 2, "the index partition's copy is as new");
        assert!(v.read_only.is_some());
        assert!(matches!(v.begin_append(), Err(Error::ReadOnly(_))));
        assert_eq!(read(&mut v, "a.bin"), big(300_000, 1));
    }
    // the drive raised its flags: media, read failure
    let alerts = l.drive().tape_alerts().unwrap();
    assert!(alerts.contains(&4) && alerts.contains(&5), "{alerts:?}");
    assert!(l.drive().tape_alerts().unwrap().is_empty(), "cleared once read");
    // the index partition's too: nothing to mount
    sim.damage_last_index("LTO003L5", 0).expect("an index in a");
    match Volume::mount(l.drive(), "LTO003L5", "test") {
        Err(Error::Corrupt(m)) => assert!(m.contains("neither index"), "{m}"),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("mounted without an index"),
    }
}

/// A damaged place inside a file: that file fails, the next one reads.
#[test]
fn a_damaged_file_fails_alone() {
    let (sim, lib) = lib();
    let mut l = lib.acquire("LTO004L5", &|| false).unwrap();
    format(l.drive(), "LTO004L5", "", "test").unwrap();
    let mut v = Volume::mount(l.drive(), "LTO004L5", "test").unwrap();
    write(&mut v, &[("one.bin", big(200_000, 1)), ("two.bin", big(200_000, 2))]);
    let Some(Node::File(one)) = v.index.find("one.bin").cloned() else { panic!() };
    let Some(Node::File(two)) = v.index.find("two.bin").cloned() else { panic!() };
    sim.damage("LTO004L5", 1, one.extents[0].start_block as usize);
    assert!(v.read_file(&one, &mut |_| Ok(true)).is_err());
    let mut out = Vec::new();
    v.read_file(&two, &mut |b| {
        out.extend_from_slice(b);
        Ok(true)
    })
    .unwrap();
    assert_eq!(out, big(200_000, 2));
}
