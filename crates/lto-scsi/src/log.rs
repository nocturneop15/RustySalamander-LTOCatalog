//! The detailed log: one text file, a line per event, for finding out what
//! happened on real hardware afterwards. Off until [`open`] is called.
//!
//! Every SCSI command goes in, with its CDB, status, sense and time, except
//! READ(6) and WRITE(6) of data that went well: those are counted, and a
//! line sums them up when another command comes (or the job ends). The
//! LTFS layer, the catalog and the plugin add what they decide and why.
//!
//! The file is cut at [`MAX_BYTES`]: `name.log` becomes `name.1.log` (and
//! that one `name.2.log`…), [`KEEP`] old files are kept.

use std::cell::RefCell;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A log file is cut when it grows past this.
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Old log files kept.
pub const KEEP: usize = 4;

struct Sink {
    path: PathBuf,
    file: File,
    size: u64,
}

static ON: AtomicBool = AtomicBool::new(false);
static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Start writing the log to `path` (appended to). Another file already
/// open is closed first.
pub fn open(path: &Path) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut size = file.metadata().map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        size = start_utf8(&mut file);
    }
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Sink { path: path.to_path_buf(), file, size });
    ON.store(true, Ordering::Relaxed);
    Ok(())
}

/// Stop writing the log.
pub fn close() {
    flush_counts();
    ON.store(false, Ordering::Relaxed);
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Whether a log is being written.
pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

/// The file being written, if any.
pub fn path() -> Option<PathBuf> {
    SINK.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|s| s.path.clone())
}

/// Write one line (use the [`log!`](crate::log!) macro).
pub fn line(args: fmt::Arguments<'_>) {
    if !enabled() {
        return;
    }
    let text = format!("{} [{}] {args}\r\n", stamp(SystemTime::now()), thread());
    let mut g = SINK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = g.as_mut() else { return };
    if s.size + text.len() as u64 > MAX_BYTES {
        rotate(s);
    }
    if s.file.write_all(text.as_bytes()).is_ok() {
        s.size += text.len() as u64;
    }
}

/// Write a line to the detailed log, when one is open.
#[macro_export]
macro_rules! log {
    ($($a:tt)*) => {
        if $crate::log::enabled() {
            $crate::log::line(format_args!($($a)*))
        }
    };
}

fn rotate(s: &mut Sink) {
    let numbered = |n: usize| {
        let stem = s.path.file_stem().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
        let ext = s.path.extension().map(|x| format!(".{}", x.to_string_lossy())).unwrap_or_default();
        s.path.with_file_name(format!("{stem}.{n}{ext}"))
    };
    let _ = fs::remove_file(numbered(KEEP));
    for n in (1..KEEP).rev() {
        let _ = fs::rename(numbered(n), numbered(n + 1));
    }
    let _ = s.file.flush();
    let _ = fs::rename(&s.path, numbered(1));
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&s.path) {
        s.size = start_utf8(&mut f);
        s.file = f;
    }
}

/// Start a new log file with the UTF-8 byte order mark: without it, editors
/// that guess (and Windows tools) read the file in the system's code page,
/// and names with diacritics come out garbled (`PĹ™ĂlĂš` for `Příliš`).
/// The bytes written.
fn start_utf8(f: &mut File) -> u64 {
    const BOM: &[u8] = b"\xEF\xBB\xBF";
    if f.write_all(BOM).is_ok() {
        BOM.len() as u64
    } else {
        0
    }
}

fn thread() -> String {
    let t = std::thread::current();
    match t.name() {
        Some(n) => n.to_owned(),
        None => format!("{:?}", t.id()).trim_start_matches("ThreadId(").trim_end_matches(')').to_owned(),
    }
}

/// `2026-11-15 14:03:07.123Z` (UTC).
fn stamp(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // days since 1970-01-01 to a civil date (Howard Hinnant's algorithm)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{:03}Z", rest / 3600, rest / 60 % 60, rest % 60, d.subsec_millis())
}

/// READ(6) / WRITE(6) records that went well, counted per thread and
/// device until another command.
struct Counts {
    device: String,
    op: &'static str,
    records: u64,
    bytes: u64,
    first: Instant,
    took: Duration,
}

thread_local! {
    static COUNTS: RefCell<Option<Counts>> = const { RefCell::new(None) };
}

/// A data record moved without remark: counted, not written out.
pub(crate) fn count(device: &str, op: &'static str, bytes: usize, took: Duration) {
    COUNTS.with(|c| {
        let mut c = c.borrow_mut();
        if c.as_ref().is_some_and(|x| x.device != device || x.op != op) {
            if let Some(x) = c.take() {
                write_counts(&x);
            }
        }
        let x = c.get_or_insert_with(|| Counts { device: device.to_owned(), op, records: 0, bytes: 0, first: Instant::now(), took: Duration::ZERO });
        x.records += 1;
        x.bytes += bytes as u64;
        x.took += took;
    });
}

/// Write out what this thread counted.
pub fn flush_counts() {
    if let Some(x) = COUNTS.with(|c| c.borrow_mut().take()) {
        write_counts(&x);
    }
}

fn write_counts(x: &Counts) {
    let wall = x.first.elapsed().as_secs_f64();
    let mb_s = if wall > 0.0 { x.bytes as f64 / wall / 1e6 } else { 0.0 };
    line(format_args!(
        "{} {} x{} records, {} bytes, {:.1} s in the commands, {:.1} s in all, {:.1} MB/s",
        x.device,
        x.op,
        x.records,
        x.bytes,
        x.took.as_secs_f64(),
        wall,
        mb_s
    ));
}

/// A number of bytes for the log: exact, and readable when large
/// (`130024472576 bytes (121.1 GiB)`).
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} bytes");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    format!("{n} bytes ({v:.1} {})", UNITS[unit])
}

/// A value that may not be known, for the log: it, or `unknown`.
pub fn or_unknown<T: fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "unknown".to_owned(), |v| v.to_string())
}

/// Items for the log: `a, b, c`, or `none`.
pub fn list<T: fmt::Display>(items: impl IntoIterator<Item = T>) -> String {
    let s = items.into_iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    if s.is_empty() {
        "none".to_owned()
    } else {
        s
    }
}

/// A CDB as hex.
pub(crate) fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 3);
    for (i, x) in b.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{x:02X}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_civil_dates() {
        assert_eq!(stamp(UNIX_EPOCH), "1970-01-01 00:00:00.000Z");
        // 2024-02-29 12:34:56.789 UTC
        assert_eq!(stamp(UNIX_EPOCH + Duration::from_millis(1_709_210_096_789)), "2024-02-29 12:34:56.789Z");
    }

    /// A new log starts with the UTF-8 byte order mark, once; names keep
    /// their diacritics.
    #[test]
    fn a_new_log_says_it_is_utf8() {
        let file = std::env::temp_dir().join(format!("lto-log-bom-{}.log", std::process::id()));
        let _ = fs::remove_file(&file);
        open(&file).unwrap();
        line(format_args!("backup: \"Účetnictví\\Příliš žluťoučký kůň.txt\""));
        close();
        open(&file).unwrap();
        line(format_args!("again"));
        close();
        let bytes = fs::read(&file).unwrap();
        assert!(bytes.starts_with(b"\xEF\xBB\xBF"));
        assert_eq!(bytes.windows(3).filter(|w| *w == b"\xEF\xBB\xBF").count(), 1, "once, not on every open");
        let text = String::from_utf8(bytes[3..].to_vec()).unwrap();
        assert!(text.contains("Účetnictví\\Příliš žluťoučký kůň.txt") && text.contains("again"), "{text}");
        let _ = fs::remove_file(&file);
    }

    #[test]
    fn values_read_as_values() {
        assert_eq!(bytes(512), "512 bytes");
        assert_eq!(bytes(130_024_472_576), "130024472576 bytes (121.1 GiB)");
        assert_eq!(bytes(1_443_999_252_480), "1443999252480 bytes (1.3 TiB)");
        assert_eq!(or_unknown(Some(bytes(2048))), "2048 bytes (2.0 KiB)");
        assert_eq!(or_unknown(None::<u64>), "unknown");
        assert_eq!(list(["LTO001L5", "LTO002L5"]), "LTO001L5, LTO002L5");
        assert_eq!(list(Vec::<String>::new()), "none");
    }
}
