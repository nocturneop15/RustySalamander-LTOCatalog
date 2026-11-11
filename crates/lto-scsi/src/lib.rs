//! SCSI commands for LTO tape drives (SSC) and the medium changers of tape
//! libraries (SMC), sent through a [`Transport`]: Windows' SCSI pass-through
//! for real devices ([`windows`]), or a simulator in tests.
//!
//! - [`tape::TapeDrive`]: positioning, reading and writing records and
//!   filemarks, partitions, medium attributes (MAM).
//! - [`changer::Changer`]: element status (slots, drives, mail slot, with
//!   barcodes) and moving tapes.
//! - [`library::Library`]: both together, with a tape loaded on demand into
//!   a free drive, each drive used by one job at a time.
//! - [`site::Site`]: every library and standalone drive there is.

pub mod changer;
pub mod library;
pub mod sense;
pub mod site;
pub mod tape;
#[cfg(windows)]
pub mod windows;

use std::fmt;
use std::io;

pub use sense::Sense;

/// SCSI status GOOD.
pub const GOOD: u8 = 0x00;
/// SCSI status CHECK CONDITION: sense data says why.
pub const CHECK_CONDITION: u8 = 0x02;
/// SCSI status BUSY.
pub const BUSY: u8 = 0x08;
/// SCSI status RESERVATION CONFLICT.
pub const RESERVATION_CONFLICT: u8 = 0x18;

/// Which way a command's data goes.
pub enum Data<'a> {
    /// No data.
    None,
    /// From the device into this buffer.
    In(&'a mut [u8]),
    /// From this buffer to the device.
    Out(&'a [u8]),
}

/// How a command ended at the transport level.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    /// SCSI status byte.
    pub status: u8,
    /// Sense data, when the status is CHECK CONDITION and the device sent some.
    pub sense: Option<Sense>,
    /// Bytes actually moved, as far as the transport knows; the buffer's
    /// length when it doesn't.
    pub transferred: usize,
}

/// Sends commands to one device (a logical unit).
pub trait Transport: Send {
    /// Send `cdb` with `data`, waiting at most `timeout_s` seconds.
    /// Transport failures are errors; a SCSI status other than GOOD is not.
    fn exec(&mut self, cdb: &[u8], data: Data<'_>, timeout_s: u32) -> io::Result<Reply>;
    /// Where the device is, for messages (`\\.\Tape0`).
    fn describe(&self) -> String;
}

/// Why a SCSI command failed.
#[derive(Debug)]
pub enum Error {
    /// The transport failed (the device is gone, access denied…).
    Io(io::Error),
    /// The device refused or failed the command.
    Check { op: &'static str, sense: Sense },
    /// An unexpected status byte (BUSY, RESERVATION CONFLICT…).
    Status { op: &'static str, status: u8 },
    /// The answer made no sense.
    Protocol(String),
}

impl Error {
    /// The sense data, if the device sent some.
    pub fn sense(&self) -> Option<&Sense> {
        match self {
            Error::Check { sense, .. } => Some(sense),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Check { op, sense } => write!(f, "{op}: {sense}"),
            Error::Status { op, status } => write!(f, "{op}: SCSI status {status:#04x}"),
            Error::Protocol(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> io::Error {
        match e {
            Error::Io(e) => e,
            Error::Check { ref sense, .. } if sense.write_protected() => io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()),
            other => io::Error::other(other.to_string()),
        }
    }
}

/// Result of SCSI operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Run a command and turn a status other than GOOD into an error, except
/// sense the caller accepts (`ok` returns true for it): then the sense comes
/// back with the reply.
pub(crate) fn run(t: &mut dyn Transport, op: &'static str, cdb: &[u8], data: Data<'_>, timeout_s: u32, ok: impl Fn(&Sense) -> bool) -> Result<Reply> {
    let r = t.exec(cdb, data, timeout_s)?;
    match r.status {
        GOOD => Ok(r),
        CHECK_CONDITION => match &r.sense {
            Some(s) if s.harmless() || ok(s) => Ok(r),
            Some(s) => Err(Error::Check { op, sense: s.clone() }),
            None => Err(Error::Protocol(format!("{op}: CHECK CONDITION without sense data"))),
        },
        status => Err(Error::Status { op, status }),
    }
}

/// Big-endian helpers.
pub(crate) fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
pub(crate) fn be24(b: &[u8]) -> u32 {
    u32::from_be_bytes([0, b[0], b[1], b[2]])
}
pub(crate) fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
pub(crate) fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}

/// ASCII text from a fixed-width SCSI field, without padding.
pub fn ascii(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim_matches(|c: char| c == ' ' || c == '\0').to_owned()
}

/// What INQUIRY says about a device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inquiry {
    /// Peripheral device type: 0x01 tape, 0x08 medium changer.
    pub device_type: u8,
    pub vendor: String,
    pub product: String,
    pub revision: String,
}

/// Peripheral device type of tape drives.
pub const TYPE_TAPE: u8 = 0x01;
/// Peripheral device type of medium changers.
pub const TYPE_CHANGER: u8 = 0x08;

/// Standard INQUIRY.
pub fn inquiry(t: &mut dyn Transport) -> Result<Inquiry> {
    let mut buf = [0u8; 96];
    let cdb = [0x12, 0, 0, 0, buf.len() as u8, 0];
    run(t, "INQUIRY", &cdb, Data::In(&mut buf), 30, |_| false)?;
    Ok(Inquiry { device_type: buf[0] & 0x1f, vendor: ascii(&buf[8..16]), product: ascii(&buf[16..32]), revision: ascii(&buf[32..36]) })
}

/// The unit serial number (VPD page 0x80).
pub fn serial_number(t: &mut dyn Transport) -> Result<String> {
    let mut buf = [0u8; 64];
    let cdb = [0x12, 0x01, 0x80, 0, buf.len() as u8, 0];
    run(t, "INQUIRY (serial number)", &cdb, Data::In(&mut buf), 30, |_| false)?;
    let n = (buf[3] as usize).min(buf.len() - 4);
    Ok(ascii(&buf[4..4 + n]))
}

/// TEST UNIT READY: `Ok(None)` when ready, else the sense saying why not.
pub fn test_unit_ready(t: &mut dyn Transport) -> Result<Option<Sense>> {
    let r = t.exec(&[0x00, 0, 0, 0, 0, 0], Data::None, 30)?;
    match r.status {
        GOOD => Ok(None),
        CHECK_CONDITION => Ok(Some(r.sense.unwrap_or_else(|| Sense::new(sense::key::NOT_READY, 0, 0)))),
        status => Err(Error::Status { op: "TEST UNIT READY", status }),
    }
}

/// PREVENT/ALLOW MEDIUM REMOVAL.
pub fn prevent_removal(t: &mut dyn Transport, prevent: bool) -> Result<()> {
    run(t, "PREVENT/ALLOW MEDIUM REMOVAL", &[0x1E, 0, 0, 0, prevent as u8, 0], Data::None, 60, |_| false).map(|_| ())
}
