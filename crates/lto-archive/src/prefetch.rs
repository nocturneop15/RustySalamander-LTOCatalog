//! Reading ahead: a thread reads the next files of a backup from disk into
//! a queue of records while the tape writes the one before, so a slow disk
//! (a network share, many small files) doesn't stop the tape between
//! records. The queue holds up to the buffer chosen in the settings; with
//! none, each file is read when it is written, as before. While it runs,
//! the log says every [`STATUS_EVERY`] how full the buffer is and which
//! side waited for the other.

use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lto_scsi::log;

/// The records read ahead: LTFS' block size, as the tape writes them.
pub const CHUNK: usize = ltfs::label::BLOCK_SIZE as usize;

/// How often the buffer's state goes into the log while files are written.
pub const STATUS_EVERY: Duration = Duration::from_secs(5);

/// What the reading thread and the writer share: bytes in the queue, read
/// from disk in all, and the time the reading thread waited for room (the
/// queue full: the tape is the slower side).
#[derive(Default)]
struct Stats {
    queued: AtomicU64,
    read: AtomicU64,
    disk_waited_ns: AtomicU64,
}

/// The queue's writer side: the records read ahead, and the counts for the
/// log.
struct Queue {
    rx: Receiver<Msg>,
    done: bool,
    stats: Arc<Stats>,
    /// The buffer's size, in bytes.
    capacity: u64,
    /// Bytes handed to the tape, and the time the tape waited for the disk
    /// (the queue empty).
    taken: u64,
    tape_waited: Duration,
    started: Instant,
    /// The last status: when, and the counts then (read, taken, the
    /// waits), for the rates since.
    last: Instant,
    last_counts: (u64, u64, Duration, Duration),
}

impl Queue {
    /// The next message; `None` once the reading thread is gone. While it
    /// waits (the tape waiting for the disk), the status still goes into
    /// the log.
    fn recv(&mut self) -> Option<Msg> {
        if self.done {
            return None;
        }
        loop {
            let left = STATUS_EVERY.saturating_sub(self.last.elapsed());
            let began = Instant::now();
            let got = self.rx.recv_timeout(left.max(Duration::from_millis(1)));
            self.tape_waited += began.elapsed();
            if self.last.elapsed() >= STATUS_EVERY {
                self.status();
            }
            match got {
                Ok(m) => {
                    if let Msg::Data(d) = &m {
                        self.stats.queued.fetch_sub(d.len() as u64, Ordering::Relaxed);
                    }
                    return Some(m);
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    self.done = true;
                    return None;
                }
            }
        }
    }

    fn disk_waited(&self) -> Duration {
        Duration::from_nanos(self.stats.disk_waited_ns.load(Ordering::Relaxed))
    }

    /// The buffer's state into the log: how full, and since the last time
    /// what was read and written and who waited.
    fn status(&mut self) {
        let queued = self.stats.queued.load(Ordering::Relaxed);
        let read = self.stats.read.load(Ordering::Relaxed);
        let disk_waited = self.disk_waited();
        let (r0, t0, tw0, dw0) = self.last_counts;
        let secs = self.last.elapsed().as_secs_f64().max(0.001);
        let rate = |b: u64| mib(b as f64 / secs);
        log!(
            "read ahead: {} of {} queued ({}%); last {:.0} s: disk read {} ({}/s), tape took {} ({}/s); the tape waited for the disk {:.1} s, the disk for the tape {:.1} s",
            mib(queued as f64),
            mib(self.capacity as f64),
            queued * 100 / self.capacity.max(1),
            secs,
            mib((read - r0) as f64),
            rate(read - r0),
            mib((self.taken - t0) as f64),
            rate(self.taken - t0),
            (self.tape_waited - tw0).as_secs_f64(),
            disk_waited.saturating_sub(dw0).as_secs_f64(),
        );
        self.last = Instant::now();
        self.last_counts = (read, self.taken, self.tape_waited, disk_waited);
    }
}

impl Drop for Queue {
    /// The whole run, once it ends.
    fn drop(&mut self) {
        let secs = self.started.elapsed().as_secs_f64();
        log!(
            "read ahead done: {} read, {} written in {:.1} s; the tape waited for the disk {:.1} s, the disk for the tape {:.1} s",
            mib(self.stats.read.load(Ordering::Relaxed) as f64),
            mib(self.taken as f64),
            secs,
            self.tape_waited.as_secs_f64(),
            self.disk_waited().as_secs_f64(),
        );
    }
}

/// Bytes in MiB, for the log.
fn mib(b: f64) -> String {
    format!("{:.1} MiB", b / (1024.0 * 1024.0))
}

/// What the reading thread sends, in order: a file opened (or why not),
/// its data, its end (or why it couldn't be read to the end).
enum Msg {
    Open(usize, io::Result<fs::Metadata>),
    Data(Vec<u8>),
    End(io::Result<()>),
}

/// The files to read ahead, and how they are read.
pub struct Prefetch {
    how: How,
}

enum How {
    /// Read when written: the files left, and the one open.
    Direct { files: std::vec::IntoIter<(usize, PathBuf)>, open: Option<fs::File> },
    /// Read ahead by a thread (which stops when this is dropped).
    Ahead(Box<Queue>),
}

impl Prefetch {
    /// Read the files `files` (their place in the backup, their path), in
    /// this order, holding up to `buffer` bytes read ahead (0: none).
    pub fn start(files: Vec<(usize, PathBuf)>, buffer: u64) -> Prefetch {
        if buffer == 0 || files.is_empty() {
            return Prefetch { how: How::Direct { files: files.into_iter(), open: None } };
        }
        let slots = ((buffer / CHUNK as u64) as usize).max(1);
        let (tx, rx) = sync_channel(slots);
        let stats = Arc::new(Stats::default());
        let theirs = stats.clone();
        let spawned = std::thread::Builder::new().name("read ahead".into()).spawn(move || read_all(files, &tx, &theirs));
        let now = Instant::now();
        let queue = Queue { rx, done: false, stats, capacity: (slots * CHUNK) as u64, taken: 0, tape_waited: Duration::ZERO, started: now, last: now, last_counts: Default::default() };
        let mut queue = Box::new(queue);
        if let Err(e) = spawned {
            // no thread: an empty queue, each file then fails to open and
            // the backup notes it; can't happen in practice
            log!("write: no thread to read ahead: {e}");
            queue.done = true;
        }
        Prefetch { how: How::Ahead(queue) }
    }

    /// The next file: its place in the backup and its metadata (or why it
    /// can't be opened). Its data comes from [`Self::reader`]. Data of the
    /// file before not read to its end is skipped. `None`: no files left.
    pub fn next_file(&mut self) -> Option<(usize, io::Result<fs::Metadata>)> {
        match &mut self.how {
            How::Direct { files, open } => {
                let (i, path) = files.next()?;
                *open = None;
                Some((
                    i,
                    fs::File::open(&path).and_then(|f| {
                        let m = f.metadata()?;
                        *open = Some(f);
                        Ok(m)
                    }),
                ))
            }
            How::Ahead(q) => loop {
                match q.recv()? {
                    Msg::Open(i, m) => return Some((i, m)),
                    _ => continue,
                }
            },
        }
    }

    /// The data of the file [`Self::next_file`] gave, to its end.
    pub fn reader(&mut self) -> FileData<'_> {
        FileData { p: self, chunk: Vec::new(), pos: 0, ended: false }
    }
}

/// The reading thread: each file in turn, record by record, until the
/// queue's other end is gone.
fn read_all(files: Vec<(usize, PathBuf)>, tx: &SyncSender<Msg>, stats: &Stats) {
    for (i, path) in files {
        let opened = fs::File::open(&path).and_then(|f| f.metadata().map(|m| (f, m)));
        let mut f = match opened {
            Ok((f, m)) => {
                if tx.send(Msg::Open(i, Ok(m))).is_err() {
                    return;
                }
                f
            }
            Err(e) => {
                if tx.send(Msg::Open(i, Err(e))).is_err() {
                    return;
                }
                continue;
            }
        };
        loop {
            let mut buf = vec![0u8; CHUNK];
            let mut n = 0;
            let mut failed = None;
            while n < CHUNK {
                match f.read(&mut buf[n..]) {
                    Ok(0) => break,
                    Ok(got) => n += got,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            if let Some(e) = failed {
                if tx.send(Msg::End(Err(e))).is_err() {
                    return;
                }
                break;
            }
            buf.truncate(n);
            if n > 0 {
                // counted before it goes: the writer takes it off when it gets it
                stats.queued.fetch_add(n as u64, Ordering::Relaxed);
                stats.read.fetch_add(n as u64, Ordering::Relaxed);
                let began = Instant::now();
                if tx.send(Msg::Data(buf)).is_err() {
                    return;
                }
                stats.disk_waited_ns.fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
            if n < CHUNK {
                if tx.send(Msg::End(Ok(()))).is_err() {
                    return;
                }
                break;
            }
        }
    }
}

/// One file's data, from the queue (or the disk).
pub struct FileData<'a> {
    p: &'a mut Prefetch,
    chunk: Vec<u8>,
    pos: usize,
    ended: bool,
}

impl Read for FileData<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let How::Direct { open, .. } = &mut self.p.how {
            return match open {
                Some(f) => f.read(buf),
                None => Ok(0),
            };
        }
        loop {
            if self.pos < self.chunk.len() {
                let n = buf.len().min(self.chunk.len() - self.pos);
                buf[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if self.ended {
                return Ok(0);
            }
            let How::Ahead(q) = &mut self.p.how else { unreachable!() };
            match q.recv() {
                Some(Msg::Data(d)) => {
                    q.taken += d.len() as u64;
                    self.chunk = d;
                    self.pos = 0;
                }
                Some(Msg::End(r)) => {
                    self.ended = true;
                    r?;
                }
                Some(Msg::Open(..)) => return Err(io::Error::other("read ahead: a file's data is missing")),
                None => return Err(io::Error::other("read ahead: the reading stopped")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Files read ahead come back whole and in order, with a file that
    /// can't be opened in between and a file left unread skipped; with no
    /// buffer, the same.
    #[test]
    fn files_come_back_whole_and_in_order() {
        let dir = std::env::temp_dir().join(format!("lto-prefetch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let sizes = [0usize, 10, CHUNK, CHUNK * 2 + 7, 3];
        let mut files = Vec::new();
        for (i, n) in sizes.iter().enumerate() {
            let p = dir.join(format!("f{i}"));
            fs::write(&p, (0..*n).map(|j| (j * 31 + i) as u8).collect::<Vec<_>>()).unwrap();
            files.push((i * 2, p));
        }
        files.insert(2, (99, dir.join("missing")));
        for buffer in [0u64, CHUNK as u64, 64 << 20] {
            let mut p = Prefetch::start(files.clone(), buffer);
            let mut seen = Vec::new();
            while let Some((i, m)) = p.next_file() {
                if i == 99 {
                    assert!(m.is_err());
                    seen.push(i);
                    continue;
                }
                assert!(m.is_ok());
                // the third file (CHUNK long) is left unread: skipped
                if i == 4 {
                    seen.push(i);
                    continue;
                }
                let mut data = Vec::new();
                p.reader().read_to_end(&mut data).unwrap();
                assert_eq!(data, fs::read(dir.join(format!("f{}", i / 2))).unwrap(), "buffer {buffer}, file {i}");
                seen.push(i);
            }
            assert_eq!(seen, [0, 2, 99, 4, 6, 8], "buffer {buffer}");
            // the counts: all read, all handed out but the file skipped,
            // nothing left queued; the status line can be written
            if let How::Ahead(q) = &mut p.how {
                let all: u64 = sizes.iter().map(|n| *n as u64).sum();
                assert_eq!(q.stats.read.load(Ordering::Relaxed), all);
                assert_eq!(q.taken, all - CHUNK as u64);
                assert_eq!(q.stats.queued.load(Ordering::Relaxed), 0);
                q.status();
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
