//! An LTFS volume on a tape in a drive: mounting (labels and the newest
//! index), reading files, appending files, writing the index back, and
//! formatting.
//!
//! Layout (LTFS 2.4), each partition:
//!
//! ```text
//! block 0  VOL1 label
//!       1  filemark
//!       2  LTFS label (XML)
//!       3  filemark
//!       4… index (XML, one or more records), filemark      ← right after formatting
//!          [data records…] filemark index filemark …        ← data partition, later
//! ```
//!
//! The index partition keeps one index, overwritten at each update; the data
//! partition keeps every generation, each pointing back to the one before.

use std::io::{self, Read};

use lto_scsi::tape::{attr, Read as TapeRead, TapeDrive, FORMAT_ASCII, FORMAT_BINARY};

use crate::index::{Extent, File, Index, Location};
use crate::label::{self, Label, BLOCK_SIZE};
use crate::time::Time;

/// Why an LTFS operation failed.
#[derive(Debug)]
pub enum Error {
    /// The drive or library failed.
    Scsi(lto_scsi::Error),
    /// The tape isn't formatted for LTFS.
    NotLtfs(String),
    /// The tape holds LTFS, but something on it is wrong.
    Corrupt(String),
    /// Reading the files to write failed.
    Io(io::Error),
    /// The caller stopped it.
    Cancelled,
    /// The tape is full (early warning).
    Full,
    /// The tape can't be written.
    ReadOnly(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Scsi(e) => write!(f, "{e}"),
            Error::NotLtfs(m) => write!(f, "the tape isn't formatted for LTFS: {m}"),
            Error::Corrupt(m) => write!(f, "the tape's LTFS volume is damaged: {m}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Cancelled => write!(f, "cancelled"),
            Error::Full => write!(f, "the tape is full"),
            Error::ReadOnly(m) => write!(f, "the tape can't be written: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<lto_scsi::Error> for Error {
    fn from(e: lto_scsi::Error) -> Error {
        Error::Scsi(e)
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> io::Error {
        match e {
            Error::Io(e) => e,
            Error::Scsi(e) => e.into(),
            Error::Cancelled => io::Error::new(io::ErrorKind::Interrupted, "cancelled"),
            Error::ReadOnly(m) => io::Error::new(io::ErrorKind::PermissionDenied, Error::ReadOnly(m).to_string()),
            Error::Full => io::Error::new(io::ErrorKind::StorageFull, Error::Full.to_string()),
            e @ (Error::NotLtfs(_) | Error::Corrupt(_)) => io::Error::new(io::ErrorKind::InvalidData, e.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The partition number of a partition letter.
pub fn part_number(letter: char) -> u32 {
    (letter as u32).saturating_sub('a' as u32)
}

fn letter(p: u32) -> char {
    char::from_u32('a' as u32 + p).unwrap_or('?')
}

/// The block where the first index follows the label construct.
const FIRST_INDEX: u64 = 4;

/// A mounted volume.
pub struct Volume<'d> {
    drive: &'d mut TapeDrive,
    pub barcode: String,
    pub label: Label,
    /// The newest index.
    pub index: Index,
    /// Where the newest index of the index partition starts.
    ip_index: u64,
    /// The newest index of the data partition.
    dp_index: Location,
    /// The block after that index's filemark.
    dp_after_index: u64,
    /// The end of data of the data partition.
    dp_end: u64,
    /// Why the volume can't be written, if it can't.
    pub read_only: Option<String>,
    /// Appending: the next block in the data partition.
    append: Option<u64>,
    /// Where the tape is, when known.
    pos: Option<(u32, u64)>,
    buf: Vec<u8>,
    creator: String,
    /// Things worth knowing that didn't stop the work (medium attributes
    /// that couldn't be written).
    pub warnings: Vec<String>,
}

/// Read the label construct of partition `p`.
pub fn read_label(drive: &mut TapeDrive, p: u32) -> Result<Label> {
    let mut buf = vec![0u8; 1024 * 1024];
    drive.locate(p, 0)?;
    match drive.read(&mut buf)? {
        TapeRead::Record(n) if label::is_ltfs_vol1(&buf[..n]) => {}
        TapeRead::Record(_) => return Err(Error::NotLtfs(format!("partition {} doesn't start with an LTFS VOL1 label", letter(p)))),
        TapeRead::Filemark | TapeRead::EndOfData => return Err(Error::NotLtfs(format!("partition {} is empty", letter(p)))),
    }
    if drive.read(&mut buf)? != TapeRead::Filemark {
        return Err(Error::NotLtfs("no filemark after the VOL1 label".into()));
    }
    let TapeRead::Record(n) = drive.read(&mut buf)? else { return Err(Error::NotLtfs("no LTFS label after the VOL1 label".into())) };
    let text = std::str::from_utf8(&buf[..n]).map_err(|_| Error::NotLtfs("the LTFS label isn't text".into()))?;
    let l = Label::parse(text.trim_end_matches('\0')).map_err(Error::NotLtfs)?;
    if l.location != letter(p) {
        return Err(Error::Corrupt(format!("the label of partition {} says it is in partition {}", letter(p), l.location)));
    }
    Ok(l)
}

/// Whether the tape in the drive holds LTFS: its label, if it does.
pub fn probe(drive: &mut TapeDrive) -> Result<Option<Label>> {
    drive.set_variable_blocks()?;
    if drive.partitions()? < 2 {
        return Ok(None);
    }
    match read_label(drive, 0) {
        Ok(l) => Ok(Some(l)),
        Err(Error::NotLtfs(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// What tells the LTFS volume on the tape in `drive` apart: the volume
/// serial of its VOL1 label (six characters, usually the start of the
/// barcode) and its LTFS label (with the volume's UUID). `None` for a tape
/// without LTFS.
pub fn identify(drive: &mut TapeDrive) -> Result<Option<(String, Label)>> {
    drive.set_variable_blocks()?;
    if drive.partitions()? < 2 {
        return Ok(None);
    }
    let mut buf = vec![0u8; 1024 * 1024];
    drive.locate(0, 0)?;
    let volser = match drive.read(&mut buf)? {
        TapeRead::Record(n) if label::is_ltfs_vol1(&buf[..n]) => String::from_utf8_lossy(&buf[4..10]).trim().to_owned(),
        _ => return Ok(None),
    };
    match read_label(drive, 0) {
        Ok(l) => Ok(Some((volser, l))),
        Err(Error::NotLtfs(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Read records up to the next filemark, as text.
fn read_text(drive: &mut TapeDrive, buf: &mut [u8]) -> Result<String> {
    let mut out = Vec::new();
    loop {
        match drive.read(buf)? {
            TapeRead::Record(n) => out.extend_from_slice(&buf[..n]),
            TapeRead::Filemark | TapeRead::EndOfData => break,
        }
        if out.len() > 4 << 30 {
            return Err(Error::Corrupt("an index larger than 4 GiB".into()));
        }
    }
    while out.last() == Some(&0) {
        out.pop();
    }
    String::from_utf8(out).map_err(|_| Error::Corrupt("an index that isn't UTF-8 text".into()))
}

/// The last index of partition `p`: (its first block, it, the block after
/// its closing filemark). `None` when none is found there. Data written
/// after the last index without one of its own (writing cut short) is
/// passed over: the index before it is found.
fn last_index(drive: &mut TapeDrive, p: u32, buf: &mut [u8]) -> Result<Option<(u64, Index, u64)>> {
    drive.locate(p, 0)?;
    drive.space_to_eod()?;
    let eod = drive.position()?.block;
    if eod < FIRST_INDEX + 2 {
        return Ok(None);
    }
    // the filemark before the last index: back over two, forward over one
    drive.space_filemarks(-2)?;
    drive.space_filemarks(1)?;
    let start = drive.position()?.block;
    let text = read_text(drive, buf)?;
    let after = drive.position()?.block;
    match Index::parse(&text) {
        Ok(ix) => Ok(Some((start, ix, after))),
        Err(_) => Ok(None),
    }
}

impl<'d> Volume<'d> {
    /// Mount the LTFS volume on the tape in `drive`.
    pub fn mount(drive: &'d mut TapeDrive, barcode: &str, creator: &str) -> Result<Volume<'d>> {
        drive.set_variable_blocks()?;
        if drive.partitions()? < 2 {
            return Err(Error::NotLtfs("the tape has a single partition".into()));
        }
        let la = read_label(drive, 0)?;
        let lb = read_label(drive, 1)?;
        if la.volume_uuid != lb.volume_uuid {
            return Err(Error::Corrupt("the labels of the two partitions belong to different volumes".into()));
        }
        let label = la;
        let (ip, dp) = (part_number(label.index_partition), part_number(label.data_partition));
        let mut buf = vec![0u8; label.block_size as usize];
        let ipi = last_index(drive, ip, &mut buf)?;
        let dpi = last_index(drive, dp, &mut buf)?;
        let dp_eod = {
            drive.locate(dp, 0)?;
            drive.space_to_eod()?;
            drive.position()?.block
        };
        let (index, ip_index, dp_index, dp_after_index) = match (ipi, dpi) {
            (Some((ia, ix_i, _)), Some((da, ix_d, after))) => {
                let dloc = Location { partition: label.data_partition, block: da };
                if ix_i.generation > ix_d.generation {
                    (ix_i, ia, dloc, after)
                } else {
                    (ix_d, ia, dloc, after)
                }
            }
            (Some((ia, ix, _)), None) => {
                // no index found in the data partition: the index partition's
                // points at the generation before it; the next index there
                // starts with a filemark whatever was written last
                let dloc = Location { partition: label.data_partition, block: ix.previous.filter(|p| p.partition == label.data_partition).map(|p| p.block).unwrap_or(FIRST_INDEX) };
                (ix, ia, dloc, u64::MAX)
            }
            (None, Some((da, ix, after))) => (ix, FIRST_INDEX, Location { partition: label.data_partition, block: da }, after),
            (None, None) => return Err(Error::Corrupt("no index was found in either partition".into())),
        };
        if index.volume_uuid != label.volume_uuid {
            return Err(Error::Corrupt("the index belongs to another volume than the label".into()));
        }
        let mut read_only = None;
        if drive.write_protected()? {
            read_only = Some("the tape is write-protected".to_string());
        } else if matches!(index.volume_lock_state.as_deref(), Some("locked" | "permlocked")) {
            read_only = Some("the volume is locked".to_string());
        } else if index.has_data_in(label.index_partition) {
            read_only = Some("files are stored in the index partition (by another LTFS program), which this program doesn't write".to_string());
        }
        Ok(Volume {
            drive,
            barcode: barcode.to_owned(),
            label,
            index,
            ip_index,
            dp_index,
            dp_after_index,
            dp_end: dp_eod,
            read_only,
            append: None,
            pos: None,
            buf,
            creator: creator.to_owned(),
            warnings: Vec::new(),
        })
    }

    /// The drive, for anything else.
    pub fn drive(&mut self) -> &mut TapeDrive {
        self.drive
    }

    /// Remaining space in the data partition, bytes, if the drive says.
    pub fn remaining(&mut self) -> Result<Option<u64>> {
        Ok(self.drive.capacity(part_number(self.label.data_partition) as u8)?.0)
    }

    fn locate(&mut self, p: u32, block: u64) -> Result<()> {
        if self.pos != Some((p, block)) {
            self.drive.locate(p, block)?;
            self.pos = Some((p, block));
        }
        Ok(())
    }

    /// Read the file `f`, giving its bytes to `out` in order; `out`
    /// returning false stops it (`Cancelled`).
    pub fn read_file(&mut self, f: &File, out: &mut dyn FnMut(&[u8]) -> io::Result<bool>) -> Result<()> {
        let zeros = vec![0u8; 64 * 1024];
        let mut written = 0u64;
        let fill = |written: &mut u64, upto: u64, out: &mut dyn FnMut(&[u8]) -> io::Result<bool>| -> Result<()> {
            while *written < upto {
                let n = (upto - *written).min(zeros.len() as u64) as usize;
                if !out(&zeros[..n])? {
                    return Err(Error::Cancelled);
                }
                *written += n as u64;
            }
            Ok(())
        };
        let mut extents = f.extents.clone();
        extents.sort_by_key(|e| e.file_offset);
        for e in &extents {
            if e.file_offset > written {
                // a hole: zeros
                fill(&mut written, e.file_offset.min(f.length), out)?;
            }
            self.locate(part_number(e.partition), e.start_block)?;
            let (mut skip, mut left) = (e.byte_offset as usize, e.byte_count);
            while left > 0 {
                match self.drive.read(&mut self.buf)? {
                    TapeRead::Record(n) => {
                        if let Some((p, b)) = self.pos {
                            self.pos = Some((p, b + 1));
                        }
                        let from = skip.min(n);
                        skip -= from;
                        let take = ((n - from) as u64).min(left) as usize;
                        if take > 0 {
                            if !out(&self.buf[from..from + take])? {
                                return Err(Error::Cancelled);
                            }
                            left -= take as u64;
                            written = written.max(e.file_offset + e.byte_count - left);
                        }
                    }
                    TapeRead::Filemark | TapeRead::EndOfData => {
                        self.pos = None;
                        return Err(Error::Corrupt(format!("the data of \"{}\" ends early on the tape", f.name)));
                    }
                }
            }
        }
        fill(&mut written, f.length, out)
    }

    /// Whether the volume may be written.
    pub fn writable(&self) -> Result<()> {
        match &self.read_only {
            Some(why) => Err(Error::ReadOnly(why.clone())),
            None => Ok(()),
        }
    }

    /// Start appending files at the end of the data partition.
    pub fn begin_append(&mut self) -> Result<()> {
        self.writable()?;
        if self.append.is_none() {
            let dp = part_number(self.label.data_partition);
            self.drive.locate(dp, self.dp_end)?;
            self.pos = Some((dp, self.dp_end));
            self.append = Some(self.dp_end);
        }
        Ok(())
    }

    /// Write a file's data from `src`, record by record; `progress` gets the
    /// bytes of each record and returning false stops (`Cancelled`). The
    /// extents and the length written come back; put them into the index
    /// with [`Volume::index`]'s methods, then [`Volume::commit`].
    pub fn write_data(&mut self, src: &mut dyn Read, progress: &mut dyn FnMut(u64) -> bool) -> Result<(Vec<Extent>, u64)> {
        self.begin_append()?;
        let start = self.append.unwrap();
        let bs = self.label.block_size as usize;
        let mut total = 0u64;
        loop {
            let mut n = 0;
            while n < bs {
                let got = src.read(&mut self.buf[n..bs])?;
                if got == 0 {
                    break;
                }
                n += got;
            }
            if n == 0 {
                break;
            }
            let ew = self.drive.write(&self.buf[..n])?;
            let next = self.append.unwrap() + 1;
            self.append = Some(next);
            self.pos = Some((part_number(self.label.data_partition), next));
            self.dp_end = next;
            total += n as u64;
            if !progress(n as u64) {
                return Err(Error::Cancelled);
            }
            if ew {
                return Err(Error::Full);
            }
            if n < bs {
                break;
            }
        }
        let extents = if total > 0 { vec![Extent { file_offset: 0, partition: self.label.data_partition, start_block: start, byte_offset: 0, byte_count: total }] } else { Vec::new() };
        Ok((extents, total))
    }

    /// Write the index (a new generation): to the data partition after
    /// everything there, then over the index partition's, and record it in
    /// the medium attributes.
    pub fn commit(&mut self) -> Result<()> {
        self.writable()?;
        let dp = part_number(self.label.data_partition);
        let ip = part_number(self.label.index_partition);
        let at = match self.append {
            Some(a) => a,
            None => {
                self.drive.locate(dp, self.dp_end)?;
                self.dp_end
            }
        };
        // a filemark ends the data before the index, unless the last thing
        // written is the previous index's filemark
        let start = if at == self.dp_after_index {
            at
        } else {
            self.drive.write_filemarks(1)?;
            at + 1
        };
        self.index.generation += 1;
        self.index.update_time = Time::now();
        self.index.previous = Some(self.dp_index);
        self.index.location = Location { partition: self.label.data_partition, block: start };
        self.index.creator = self.creator.clone();
        let xml = self.index.to_xml();
        let blocks = self.write_records(xml.as_bytes())?;
        self.drive.write_filemarks(1)?;
        self.dp_index = self.index.location;
        self.dp_end = start + blocks + 1;
        self.dp_after_index = self.dp_end;
        self.append = None;
        // the index partition: the same generation over the last index there
        let mut copy = self.index.clone();
        copy.location = Location { partition: self.label.index_partition, block: self.ip_index };
        self.drive.locate(ip, self.ip_index)?;
        self.write_records(copy.to_xml().as_bytes())?;
        self.drive.write_filemarks(1)?;
        self.pos = None;
        for (p, block) in [(dp, self.dp_index.block), (ip, self.ip_index)] {
            if let Err(e) = write_coherency(self.drive, p as u8, self.index.generation, block, &self.label.volume_uuid) {
                self.warnings.push(format!("the volume coherency attribute of partition {} couldn't be written: {e}", letter(p)));
            }
        }
        Ok(())
    }

    fn write_records(&mut self, data: &[u8]) -> Result<u64> {
        let mut n = 0;
        for chunk in data.chunks(self.label.block_size as usize) {
            // the index may go past early warning: there is room for it
            self.drive.write(chunk)?;
            n += 1;
        }
        Ok(n)
    }
}

/// Write the volume coherency information attribute (0x080C) of a partition,
/// as LTFS does: the volume change reference, the index generation, the
/// index's block and the volume UUID.
fn write_coherency(drive: &mut TapeDrive, p: u8, generation: u64, block: u64, uuid: &str) -> lto_scsi::Result<()> {
    let vcr = drive.read_number_attribute(0, attr::VOLUME_CHANGE_REFERENCE)?.unwrap_or(0);
    let mut v = Vec::with_capacity(70);
    v.push(8);
    v.extend_from_slice(&vcr.to_be_bytes());
    v.extend_from_slice(&generation.to_be_bytes());
    v.extend_from_slice(&block.to_be_bytes());
    v.extend_from_slice(&43u16.to_be_bytes());
    v.extend_from_slice(b"LTFS\0");
    let mut u = [0u8; 37];
    for (i, b) in uuid.bytes().take(36).enumerate() {
        u[i] = b;
    }
    v.extend_from_slice(&u);
    v.push(1);
    drive.write_attribute(p, attr::VOLUME_COHERENCY_INFORMATION, FORMAT_BINARY, &v)
}

/// What [`format`] makes.
pub struct Formatted {
    pub label: Label,
    pub index: Index,
    pub warnings: Vec<String>,
}

/// Format the tape in `drive` as an empty LTFS volume: two partitions, the
/// labels and an empty index in each. **Everything on the tape is lost.**
pub fn format(drive: &mut TapeDrive, barcode: &str, volume_name: &str, creator: &str) -> Result<Formatted> {
    if drive.write_protected()? {
        return Err(Error::ReadOnly("the tape is write-protected".into()));
    }
    drive.format_two_partitions(1)?;
    drive.set_variable_blocks()?;
    let mut warnings = Vec::new();
    if let Err(e) = drive.set_compression(true) {
        warnings.push(format!("compression couldn't be turned on: {e}"));
    }
    let now = Time::now();
    let uuid = label::new_uuid();
    let mut l = Label {
        version: label::VERSION.into(),
        creator: creator.into(),
        format_time: now,
        volume_uuid: uuid.clone(),
        location: 'a',
        index_partition: 'a',
        data_partition: 'b',
        block_size: BLOCK_SIZE,
        compression: true,
    };
    let mut index = Index::new(&uuid, creator, volume_name);
    index.update_time = now;
    // the data partition first, then the index partition, as mkltfs does
    for p in [1u32, 0] {
        l.location = letter(p);
        drive.locate(p, 0)?;
        drive.write(&label::vol1(barcode))?;
        drive.write_filemarks(1)?;
        drive.write(l.to_xml().as_bytes())?;
        drive.write_filemarks(1)?;
        let mut ix = index.clone();
        ix.location = Location { partition: letter(p), block: FIRST_INDEX };
        for chunk in ix.to_xml().as_bytes().chunks(BLOCK_SIZE as usize) {
            drive.write(chunk)?;
        }
        drive.write_filemarks(1)?;
    }
    index.location = Location { partition: 'b', block: FIRST_INDEX };
    let mut tag = [b' '; 32];
    for (i, b) in barcode.bytes().take(32).enumerate() {
        tag[i] = b;
    }
    let attrs: [(u16, u8, Vec<u8>); 4] = [
        (attr::BARCODE, FORMAT_ASCII, tag.to_vec()),
        (attr::APPLICATION_VENDOR, FORMAT_ASCII, pad(b"RSALAMAN", 8)),
        (attr::APPLICATION_NAME, FORMAT_ASCII, pad(b"LTO Catalog", 32)),
        (attr::APPLICATION_VERSION, FORMAT_ASCII, pad(env!("CARGO_PKG_VERSION").as_bytes(), 8)),
    ];
    for (id, fmt, v) in attrs {
        if let Err(e) = drive.write_attribute(0, id, fmt, &v) {
            warnings.push(format!("medium attribute {id:#06x} couldn't be written: {e}"));
        }
    }
    for p in [1u8, 0] {
        if let Err(e) = write_coherency(drive, p, 1, FIRST_INDEX, &uuid) {
            warnings.push(format!("the volume coherency attribute couldn't be written: {e}"));
        }
    }
    l.location = 'a';
    Ok(Formatted { label: l, index, warnings })
}

fn pad(b: &[u8], n: usize) -> Vec<u8> {
    let mut v = b[..b.len().min(n)].to_vec();
    v.resize(n, b' ');
    v
}
