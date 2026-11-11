//! A tape drive (SSC-4): positioning, records and filemarks, partitions,
//! medium auxiliary memory (MAM).
//!
//! Records are read and written in variable-block mode: each WRITE is one
//! record of its own length, as LTFS needs.

use std::time::{Duration, Instant};

use crate::sense::{key, Sense};
use crate::{be16, be32, be64, run, Data, Error, Inquiry, Reply, Result, Transport};

/// Timeouts, seconds.
const T_SHORT: u32 = 60;
const T_RW: u32 = 15 * 60;
const T_LOCATE: u32 = 30 * 60;
const T_LOAD: u32 = 15 * 60;
const T_FORMAT: u32 = 60 * 60;

/// Where the tape is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Position {
    pub partition: u32,
    /// Logical object (block) number: records and filemarks count alike.
    pub block: u64,
    /// The number of filemarks before it in the partition.
    pub file: u64,
    /// At the beginning of the partition.
    pub bop: bool,
    /// Between early warning and the end of the partition.
    pub eop: bool,
}

/// What a read found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    /// A record of this many bytes.
    Record(usize),
    /// A filemark; the tape is now after it.
    Filemark,
    /// End of data: nothing more is written in this partition.
    EndOfData,
}

/// Medium auxiliary memory attributes (SPC-4 7.4.2.x).
pub mod attr {
    /// Remaining capacity in the partition, MiB.
    pub const REMAINING_CAPACITY: u16 = 0x0000;
    /// Maximum capacity of the partition, MiB.
    pub const MAXIMUM_CAPACITY: u16 = 0x0001;
    /// Volume change reference: changes whenever the medium is written.
    pub const VOLUME_CHANGE_REFERENCE: u16 = 0x0009;
    /// Medium type and density (WORM, cleaning…).
    pub const MEDIUM_TYPE: u16 = 0x0408;
    /// Application vendor.
    pub const APPLICATION_VENDOR: u16 = 0x0800;
    /// Application name.
    pub const APPLICATION_NAME: u16 = 0x0801;
    /// Application version.
    pub const APPLICATION_VERSION: u16 = 0x0802;
    /// User medium text label.
    pub const USER_MEDIUM_TEXT_LABEL: u16 = 0x0803;
    /// Barcode.
    pub const BARCODE: u16 = 0x0806;
    /// Volume coherency information (LTFS: index generation and position).
    pub const VOLUME_COHERENCY_INFORMATION: u16 = 0x080C;
}

/// Attribute value formats.
pub const FORMAT_BINARY: u8 = 0;
pub const FORMAT_ASCII: u8 = 1;
pub const FORMAT_TEXT: u8 = 2;

/// One tape drive.
pub struct TapeDrive {
    t: Box<dyn Transport>,
}

impl TapeDrive {
    pub fn new(t: Box<dyn Transport>) -> TapeDrive {
        TapeDrive { t }
    }

    /// Where the drive is, for messages.
    pub fn describe(&self) -> String {
        self.t.describe()
    }

    /// The transport, for commands this type doesn't have.
    pub fn transport(&mut self) -> &mut dyn Transport {
        &mut *self.t
    }

    /// A command; a pending UNIT ATTENTION (the tape was changed, a reset)
    /// is cleared by sending it again.
    fn cmd(&mut self, op: &'static str, cdb: &[u8], mut data: Data<'_>, timeout: u32, ok: impl Fn(&Sense) -> bool) -> Result<Reply> {
        let mut tries = 0;
        loop {
            let d = match &mut data {
                Data::None => Data::None,
                Data::In(b) => Data::In(b),
                Data::Out(b) => Data::Out(b),
            };
            match run(&mut *self.t, op, cdb, d, timeout, &ok) {
                Err(Error::Check { sense, .. }) if sense.key == key::UNIT_ATTENTION && tries < 3 => tries += 1,
                r => return r,
            }
        }
    }

    pub fn inquiry(&mut self) -> Result<Inquiry> {
        crate::inquiry(&mut *self.t)
    }

    pub fn serial_number(&mut self) -> Result<String> {
        crate::serial_number(&mut *self.t)
    }

    /// `None` when ready, else why not.
    pub fn test_unit_ready(&mut self) -> Result<Option<Sense>> {
        for _ in 0..3 {
            match crate::test_unit_ready(&mut *self.t)? {
                Some(s) if s.key == key::UNIT_ATTENTION => continue,
                other => return Ok(other),
            }
        }
        crate::test_unit_ready(&mut *self.t)
    }

    /// Wait until a tape is loaded and ready, at most `limit`.
    pub fn wait_ready(&mut self, limit: Duration) -> Result<()> {
        let start = Instant::now();
        loop {
            match self.test_unit_ready()? {
                None => return Ok(()),
                Some(s) if (s.becoming_ready() || s.key == key::UNIT_ATTENTION) && start.elapsed() < limit => std::thread::sleep(Duration::from_millis(500)),
                Some(s) => return Err(Error::Check { op: "TEST UNIT READY", sense: s }),
            }
        }
    }

    /// LOAD: thread the tape and go to the beginning of partition 0.
    pub fn load(&mut self) -> Result<()> {
        self.cmd("LOAD", &[0x1B, 0, 0, 0, 0x01, 0], Data::None, T_LOAD, |_| false).map(|_| ())
    }

    /// UNLOAD: rewind and unthread, so the changer can take the tape.
    pub fn unload(&mut self) -> Result<()> {
        self.cmd("UNLOAD", &[0x1B, 0, 0, 0, 0x00, 0], Data::None, T_LOAD, |s| s.no_medium()).map(|_| ())
    }

    pub fn rewind(&mut self) -> Result<()> {
        self.cmd("REWIND", &[0x01, 0, 0, 0, 0, 0], Data::None, T_LOCATE, |_| false).map(|_| ())
    }

    /// LOCATE(16) to `block` of `partition`.
    pub fn locate(&mut self, partition: u32, block: u64) -> Result<()> {
        let mut cdb = [0u8; 16];
        cdb[0] = 0x92;
        cdb[1] = 0x02; // CP: change partition
        cdb[3] = partition as u8;
        cdb[4..12].copy_from_slice(&block.to_be_bytes());
        self.cmd("LOCATE", &cdb, Data::None, T_LOCATE, |_| false).map(|_| ())
    }

    /// READ POSITION, long form.
    pub fn position(&mut self) -> Result<Position> {
        let mut buf = [0u8; 32];
        let cdb = [0x34, 0x06, 0, 0, 0, 0, 0, 0, 0, 0];
        self.cmd("READ POSITION", &cdb, Data::In(&mut buf), T_SHORT, |_| false)?;
        if buf[0] & 0x04 != 0 {
            return Err(Error::Protocol("READ POSITION: the position is unknown".into()));
        }
        Ok(Position { bop: buf[0] & 0x80 != 0, eop: buf[0] & 0x40 != 0, partition: be32(&buf[4..8]), block: be64(&buf[8..16]), file: be64(&buf[16..24]) })
    }

    /// Read the next record into `buf` (which should be as long as the
    /// longest record expected).
    pub fn read(&mut self, buf: &mut [u8]) -> Result<Read> {
        let n = buf.len();
        if n > 0xFF_FFFF {
            return Err(Error::Protocol("READ: buffer too large".into()));
        }
        let cdb = [0x08, 0x00, (n >> 16) as u8, (n >> 8) as u8, n as u8, 0];
        let r = self.cmd("READ", &cdb, Data::In(buf), T_RW, |s| s.filemark || s.ili || s.blank_check() || s.eom)?;
        let Some(s) = r.sense else { return Ok(Read::Record(n)) };
        if s.filemark {
            return Ok(Read::Filemark);
        }
        if s.blank_check() {
            return Ok(Read::EndOfData);
        }
        if s.ili {
            return match s.info {
                Some(residue) if residue >= 0 && residue as usize <= n => Ok(Read::Record(n - residue as usize)),
                Some(residue) => Err(Error::Protocol(format!("READ: a record of {} bytes is longer than the buffer of {n}", n as i64 - residue))),
                None => Ok(Read::Record(r.transferred.min(n))),
            };
        }
        if s.eom && !s.harmless() {
            return Err(Error::Check { op: "READ", sense: s });
        }
        Ok(Read::Record(r.transferred.min(n)))
    }

    /// Write one record. `Ok(true)`: written, and the tape is past early
    /// warning (stop writing data soon).
    pub fn write(&mut self, data: &[u8]) -> Result<bool> {
        let n = data.len();
        if n > 0xFF_FFFF {
            return Err(Error::Protocol("WRITE: record too large".into()));
        }
        let cdb = [0x0A, 0x00, (n >> 16) as u8, (n >> 8) as u8, n as u8, 0];
        let r = self.cmd("WRITE", &cdb, Data::Out(data), T_RW, |s| s.eom && s.key == key::NO_SENSE)?;
        Ok(r.sense.is_some_and(|s| s.eom))
    }

    /// Write `n` filemarks (synchronously: buffered data reaches the tape).
    /// `Ok(true)`: past early warning.
    pub fn write_filemarks(&mut self, n: u32) -> Result<bool> {
        let cdb = [0x10, 0x00, (n >> 16) as u8, (n >> 8) as u8, n as u8, 0];
        let r = self.cmd("WRITE FILEMARKS", &cdb, Data::None, T_RW, |s| s.eom && s.key == key::NO_SENSE)?;
        Ok(r.sense.is_some_and(|s| s.eom))
    }

    /// SPACE over `n` filemarks; negative goes back, stopping before
    /// (on the beginning side of) the last one crossed.
    pub fn space_filemarks(&mut self, n: i32) -> Result<()> {
        let c = (n as u32) & 0xFF_FFFF;
        let cdb = [0x11, 0x01, (c >> 16) as u8, (c >> 8) as u8, c as u8, 0];
        self.cmd("SPACE", &cdb, Data::None, T_LOCATE, |_| false).map(|_| ())
    }

    /// SPACE to the end of data of the current partition.
    pub fn space_to_eod(&mut self) -> Result<()> {
        self.cmd("SPACE (end of data)", &[0x11, 0x03, 0, 0, 0, 0], Data::None, T_LOCATE, |_| false).map(|_| ())
    }

    /// MODE SENSE(10) of `page` (0x3F: all), without block descriptors when
    /// `dbd`; the whole answer, header included.
    pub fn mode_sense(&mut self, page: u8, dbd: bool) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 1024];
        let n = buf.len();
        let cdb = [0x5A, if dbd { 0x08 } else { 0 }, page & 0x3f, 0, 0, 0, 0, (n >> 8) as u8, n as u8, 0];
        let r = self.cmd("MODE SENSE", &cdb, Data::In(&mut buf), T_SHORT, |_| false)?;
        let len = (be16(&buf[0..2]) as usize + 2).min(r.transferred.max(8)).min(n);
        buf.truncate(len);
        Ok(buf)
    }

    /// MODE SELECT(10) with `params` (a mode parameter header and pages).
    pub fn mode_select(&mut self, params: &[u8]) -> Result<()> {
        let n = params.len();
        let cdb = [0x55, 0x10, 0, 0, 0, 0, 0, (n >> 8) as u8, n as u8, 0];
        self.cmd("MODE SELECT", &cdb, Data::Out(params), T_SHORT, |_| false).map(|_| ())
    }

    /// Whether the loaded tape is write-protected (the WP bit of the mode
    /// parameter header).
    pub fn write_protected(&mut self) -> Result<bool> {
        let b = self.mode_sense(0x3F, true)?;
        Ok(b.len() > 3 && b[3] & 0x80 != 0)
    }

    /// Variable-length records (block length 0 in the block descriptor).
    pub fn set_variable_blocks(&mut self) -> Result<()> {
        // header (8) + one short block descriptor (8): density 0 (keep), block length 0
        let mut p = [0u8; 16];
        p[7] = 8;
        self.mode_select(&p)
    }

    /// Turn the drive's data compression on or off (mode page 0x0F).
    pub fn set_compression(&mut self, on: bool) -> Result<()> {
        let cur = self.mode_sense(0x0F, true)?;
        let page_at = 8 + be16(&cur[6..8]) as usize;
        if cur.len() < page_at + 16 || cur[page_at] & 0x3f != 0x0F {
            return Err(Error::Protocol("MODE SENSE: no data compression page".into()));
        }
        let mut p = cur[..page_at + 16].to_vec();
        p[0] = 0;
        p[1] = 0;
        p[page_at] &= 0x3f; // PS is reserved in MODE SELECT
        if on {
            p[page_at + 2] |= 0x80; // DCE
        } else {
            p[page_at + 2] &= !0x80;
        }
        self.mode_select(&p)
    }

    /// The number of partitions of the loaded tape, from the medium
    /// partition page (0x11): additional partitions defined + 1.
    pub fn partitions(&mut self) -> Result<u32> {
        let b = self.mode_sense(0x11, true)?;
        let at = 8 + be16(&b[6..8]) as usize;
        if b.len() < at + 4 {
            return Err(Error::Protocol("MODE SENSE: no medium partition page".into()));
        }
        Ok(b[at + 3] as u32 + 1)
    }

    /// Divide the loaded tape into two partitions, the first `first_gb`
    /// (10⁹ bytes; the drive rounds it up to what it can do, on LTO-5 about
    /// 37.5 GB) and the second the rest, and format it (FORMAT MEDIUM). Every
    /// bit of data on the tape is lost.
    pub fn format_two_partitions(&mut self, first_gb: u16) -> Result<()> {
        self.rewind()?;
        let cur = self.mode_sense(0x11, true)?;
        let at = 8 + be16(&cur[6..8]) as usize;
        if cur.len() < at + 8 || cur[at] & 0x3f != 0x11 {
            return Err(Error::Protocol("MODE SENSE: no medium partition page".into()));
        }
        let page_len = cur[at + 1] as usize + 2;
        let mut page = cur[at..(at + page_len).min(cur.len())].to_vec();
        // room for two partition sizes
        if page.len() < 12 {
            page.resize(12, 0);
        }
        page[0] &= 0x3f;
        page[1] = (page.len() - 2) as u8;
        page[3] = 1; // additional partitions defined
        page[4] = 0x20 | 0x18; // IDP, PSUM = units given by the partition units field
        page[6] = 0x09; // partition units: 10^9 bytes
        page[8..10].copy_from_slice(&first_gb.to_be_bytes());
        page[10..12].copy_from_slice(&0xFFFFu16.to_be_bytes()); // the rest
        let mut params = vec![0u8; 8];
        params.extend_from_slice(&page);
        self.mode_select(&params)?;
        // FORMAT MEDIUM, format field 1: partition as the medium partition page says
        self.cmd("FORMAT MEDIUM", &[0x04, 0, 0x01, 0, 0, 0], Data::None, T_FORMAT, |_| false)?;
        Ok(())
    }

    /// Read one attribute of the medium auxiliary memory of `partition`;
    /// `Ok(None)` when it isn't set.
    pub fn read_attribute(&mut self, partition: u8, id: u16) -> Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; 4096];
        let n = buf.len() as u32;
        let mut cdb = [0u8; 16];
        cdb[0] = 0x8C;
        cdb[1] = 0x00; // attribute values
        cdb[7] = partition;
        cdb[8..10].copy_from_slice(&id.to_be_bytes());
        cdb[10..14].copy_from_slice(&n.to_be_bytes());
        let r = match self.cmd("READ ATTRIBUTE", &cdb, Data::In(&mut buf), T_SHORT, |_| false) {
            // not set (or not supported)
            Err(Error::Check { sense, .. }) if sense.key == key::ILLEGAL_REQUEST => return Ok(None),
            r => r?,
        };
        let avail = (be32(&buf[0..4]) as usize + 4).min(buf.len()).min(r.transferred.max(4));
        let mut p = 4;
        while p + 5 <= avail {
            let (aid, len) = (be16(&buf[p..p + 2]), be16(&buf[p + 3..p + 5]) as usize);
            let end = (p + 5 + len).min(avail);
            if aid == id {
                return Ok(Some(buf[p + 5..end].to_vec()));
            }
            p = end;
        }
        Ok(None)
    }

    /// A number attribute (capacity, volume change reference).
    pub fn read_number_attribute(&mut self, partition: u8, id: u16) -> Result<Option<u64>> {
        Ok(self.read_attribute(partition, id)?.map(|v| v.iter().fold(0u64, |a, &b| (a << 8) | b as u64)))
    }

    /// Write one attribute to the medium auxiliary memory of `partition`.
    pub fn write_attribute(&mut self, partition: u8, id: u16, format: u8, value: &[u8]) -> Result<()> {
        let mut data = vec![0u8; 4];
        data.extend_from_slice(&id.to_be_bytes());
        data.push(format);
        data.extend_from_slice(&(value.len() as u16).to_be_bytes());
        data.extend_from_slice(value);
        let len = (data.len() - 4) as u32;
        data[0..4].copy_from_slice(&len.to_be_bytes());
        let n = data.len() as u32;
        let mut cdb = [0u8; 16];
        cdb[0] = 0x8D;
        cdb[1] = 0x01; // write-through cache
        cdb[7] = partition;
        cdb[10..14].copy_from_slice(&n.to_be_bytes());
        self.cmd("WRITE ATTRIBUTE", &cdb, Data::Out(&data), T_SHORT, |_| false).map(|_| ())
    }

    /// Remaining and maximum capacity of `partition`, in bytes (from MAM).
    pub fn capacity(&mut self, partition: u8) -> Result<(Option<u64>, Option<u64>)> {
        let mib = 1024 * 1024;
        Ok((
            self.read_number_attribute(partition, attr::REMAINING_CAPACITY)?.map(|v| v * mib),
            self.read_number_attribute(partition, attr::MAXIMUM_CAPACITY)?.map(|v| v * mib),
        ))
    }

    pub fn prevent_removal(&mut self, prevent: bool) -> Result<()> {
        crate::prevent_removal(&mut *self.t, prevent)
    }
}
