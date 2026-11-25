//! ltocat's exit status, as a script sees it: the simulated library, a
//! catalog and a log of their own.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Run ltocat on the scratch library with `input` on stdin: its exit code
/// and output.
fn ltocat(dir: &Path, args: &[&str], input: &str) -> (i32, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ltocat"))
        .args(["--sim", &dir.join("sim.bin").display().to_string(), "--catalog", &dir.join("cat.sqlite").display().to_string(), "--log", &dir.join("ltocat.log").display().to_string()])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = c.wait_with_output().unwrap();
    (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

#[test]
fn exit_status_says_whether_the_job_is_complete() {
    let dir = std::env::temp_dir().join(format!("ltocat-exit-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    let _scratch = Scratch(dir.clone());
    fs::write(dir.join("src").join("a.txt"), b"alpha").unwrap();
    let src = dir.join("src").display().to_string();

    // a wrong command line: 2
    assert_eq!(ltocat(&dir, &["--read-ahead", "lots", "tapes"], "").0, 2);
    // a new tape: the barcode typed, and again for the blank cartridge
    let (code, out) = ltocat(&dir, &["format", "LTO001L5"], "LTO001L5\nLTO001L5\n");
    assert_eq!(code, 0, "{out}");
    // a backup that writes everything: 0
    let (code, out) = ltocat(&dir, &["backup", &src, "LTO001L5", "--check"], "");
    assert_eq!(code, 0, "{out}");
    // an archive declined at its question: nothing done, not complete: 3
    let (code, out) = ltocat(&dir, &["archive", &src, "LTO001L5"], "n\n");
    assert_eq!(code, 3, "{out}");
    assert!(dir.join("src").join("a.txt").exists());
    // "only Archive" with nothing changed: nothing to do, done: 0
    let quiet = dir.join("quiet");
    fs::create_dir_all(&quiet).unwrap();
    let (code, out) = ltocat(&dir, &["backup", &quiet.display().to_string(), "LTO001L5", "--archive-only"], "");
    assert_eq!(code, 0, "{out}");
    // "only Archive" with a changed (empty) folder, its tape not put in: 3
    let changed = dir.join("changed");
    fs::create_dir_all(changed.join("new")).unwrap();
    lto_archive::set_archive(&changed.join("new")).unwrap();
    let (code, out) = ltocat(&dir, &["format", "LTO002L5"], "LTO002L5\nLTO002L5\n");
    assert_eq!(code, 0, "{out}");
    let (code, out) = ltocat(&dir, &["sim-move", "LTO002L5", "shelf"], "");
    assert_eq!(code, 0, "{out}");
    let (code, out) = ltocat(&dir, &["backup", &changed.display().to_string(), "LTO002L5", "--archive-only"], "q\n");
    assert_eq!(code, 3, "{out}");
    assert!(!out.contains("Nothing to back up"), "{out}");
    // "only Archive" in a folder that can't be listed: nothing found, but
    // not "nothing changed": 3
    let locked = dir.join("locked");
    fs::create_dir_all(locked.join("inside")).unwrap();
    let icacls = |args: &[&str]| Command::new("icacls").arg(&locked).args(args).output().is_ok_and(|o| o.status.success());
    // Everyone may not list it
    if icacls(&["/deny", "*S-1-1-0:(RD)"]) && fs::read_dir(&locked).is_err() {
        let (code, out) = ltocat(&dir, &["backup", &locked.display().to_string(), "LTO002L5", "--archive-only"], "q\n");
        icacls(&["/remove:d", "*S-1-1-0"]);
        assert_eq!(code, 3, "{out}");
        assert!(!out.contains("Nothing to back up") && out.contains("couldn't be read"), "{out}");
    } else {
        icacls(&["/remove:d", "*S-1-1-0"]);
    }
    // a tape that isn't an LTFS volume in the catalog: it can't start: 1
    let (code, out) = ltocat(&dir, &["backup", &src, "NOSUCH1L5"], "q\n");
    assert_eq!(code, 1, "{out}");
    // a check of a tape with a damaged index: it ran, but isn't complete: 3
    let (code, out) = ltocat(&dir, &["sim-damage", "LTO001L5", "b"], "");
    assert_eq!(code, 0, "{out}");
    let (code, out) = ltocat(&dir, &["verify", "LTO001L5"], "");
    assert_eq!(code, 3, "{out}");
    assert!(out.contains("not complete"), "{out}");
    // a backup onto it (read-only now): the tape fails, nothing written: 3
    let (code, out) = ltocat(&dir, &["backup", &src, "LTO001L5", "--no-date", "--name", "again"], "q\n");
    assert_eq!(code, 3, "{out}");
    assert!(out.contains("0 of 1 files"), "{out}");
}
