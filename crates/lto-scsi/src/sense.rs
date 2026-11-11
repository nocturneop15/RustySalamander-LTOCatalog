//! SCSI sense data (SPC-4 4.5): why a command ended with CHECK CONDITION.

use std::fmt;

/// Sense keys this crate tells apart.
pub mod key {
    pub const NO_SENSE: u8 = 0x0;
    pub const RECOVERED_ERROR: u8 = 0x1;
    pub const NOT_READY: u8 = 0x2;
    pub const MEDIUM_ERROR: u8 = 0x3;
    pub const HARDWARE_ERROR: u8 = 0x4;
    pub const ILLEGAL_REQUEST: u8 = 0x5;
    pub const UNIT_ATTENTION: u8 = 0x6;
    pub const DATA_PROTECT: u8 = 0x7;
    pub const BLANK_CHECK: u8 = 0x8;
    pub const ABORTED_COMMAND: u8 = 0xB;
    pub const VOLUME_OVERFLOW: u8 = 0xD;
}

/// Decoded sense data, fixed or descriptor format.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sense {
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
    /// A filemark was read (stream commands).
    pub filemark: bool,
    /// End of medium, or early warning when writing.
    pub eom: bool,
    /// Incorrect length: the record read was shorter or longer than asked.
    pub ili: bool,
    /// The INFORMATION field, when valid: for READ with ILI, the requested
    /// length minus the record's length (negative when the record was longer).
    pub info: Option<i64>,
}

impl Sense {
    /// Decode sense bytes; `None` when they are empty or of an unknown format.
    pub fn parse(b: &[u8]) -> Option<Sense> {
        let code = *b.first()? & 0x7f;
        match code {
            0x70 | 0x71 if b.len() >= 3 => {
                let valid = b[0] & 0x80 != 0;
                let info = if valid && b.len() >= 7 { Some(i32::from_be_bytes([b[3], b[4], b[5], b[6]]) as i64) } else { None };
                Some(Sense {
                    key: b[2] & 0x0f,
                    filemark: b[2] & 0x80 != 0,
                    eom: b[2] & 0x40 != 0,
                    ili: b[2] & 0x20 != 0,
                    info,
                    asc: b.get(12).copied().unwrap_or(0),
                    ascq: b.get(13).copied().unwrap_or(0),
                })
            }
            0x72 | 0x73 if b.len() >= 4 => {
                let mut s = Sense { key: b[1] & 0x0f, asc: b[2], ascq: b[3], ..Sense::default() };
                let len = b.get(7).copied().unwrap_or(0) as usize;
                let mut d = &b[8.min(b.len())..(8 + len).min(b.len())];
                while d.len() >= 2 {
                    let (kind, dlen) = (d[0], d[1] as usize);
                    let body = &d[..(2 + dlen).min(d.len())];
                    match kind {
                        // information
                        0x00 if body.len() >= 12 && body[2] & 0x80 != 0 => {
                            s.info = Some(i64::from_be_bytes(body[4..12].try_into().unwrap()));
                        }
                        // stream commands
                        0x04 if body.len() >= 4 => {
                            s.filemark = body[3] & 0x80 != 0;
                            s.eom = body[3] & 0x40 != 0;
                            s.ili = body[3] & 0x20 != 0;
                        }
                        _ => {}
                    }
                    d = &d[(2 + dlen).min(d.len())..];
                }
                Some(s)
            }
            _ => None,
        }
    }

    /// Fixed-format sense bytes for these values (the simulator answers with them).
    pub fn to_fixed(&self) -> Vec<u8> {
        let mut b = vec![0u8; 18];
        b[0] = 0x70 | if self.info.is_some() { 0x80 } else { 0 };
        b[2] = self.key | if self.filemark { 0x80 } else { 0 } | if self.eom { 0x40 } else { 0 } | if self.ili { 0x20 } else { 0 };
        if let Some(i) = self.info {
            b[3..7].copy_from_slice(&(i as i32).to_be_bytes());
        }
        b[7] = 10;
        b[12] = self.asc;
        b[13] = self.ascq;
        b
    }

    /// Sense with only a key and an additional sense code.
    pub fn new(key: u8, asc: u8, ascq: u8) -> Sense {
        Sense { key, asc, ascq, ..Sense::default() }
    }

    /// The medium isn't loaded or present (NOT READY, 3A/xx).
    pub fn no_medium(&self) -> bool {
        self.key == key::NOT_READY && self.asc == 0x3A
    }

    /// The device is getting ready (NOT READY, 04/01 becoming ready, or a
    /// load in progress); worth waiting for.
    pub fn becoming_ready(&self) -> bool {
        self.key == key::NOT_READY && self.asc == 0x04 && matches!(self.ascq, 0x00 | 0x01 | 0x07 | 0x08)
    }

    /// Write protected (DATA PROTECT, 27/xx).
    pub fn write_protected(&self) -> bool {
        self.key == key::DATA_PROTECT && self.asc == 0x27
    }

    /// End of data reached (BLANK CHECK).
    pub fn blank_check(&self) -> bool {
        self.key == key::BLANK_CHECK
    }

    /// Something only worth noting: the command did its work.
    pub fn harmless(&self) -> bool {
        matches!(self.key, key::NO_SENSE | key::RECOVERED_ERROR)
    }

    /// The name of the sense key.
    pub fn key_name(&self) -> &'static str {
        match self.key {
            key::NO_SENSE => "no sense",
            key::RECOVERED_ERROR => "recovered error",
            key::NOT_READY => "not ready",
            key::MEDIUM_ERROR => "medium error",
            key::HARDWARE_ERROR => "hardware error",
            key::ILLEGAL_REQUEST => "illegal request",
            key::UNIT_ATTENTION => "unit attention",
            key::DATA_PROTECT => "data protect",
            key::BLANK_CHECK => "blank check",
            key::ABORTED_COMMAND => "aborted command",
            key::VOLUME_OVERFLOW => "volume overflow",
            _ => "other",
        }
    }

    /// A short explanation of common additional sense codes, for people.
    pub fn explain(&self) -> Option<&'static str> {
        Some(match (self.asc, self.ascq) {
            (0x00, 0x02) => "end of partition or medium detected (early warning)",
            (0x04, 0x01) => "becoming ready",
            (0x04, 0x02) => "an initializing command is required (load the medium)",
            (0x04, 0x03) => "manual intervention required",
            (0x11, _) => "unrecovered read error",
            (0x14, _) => "recorded entity not found",
            (0x20, 0x00) => "invalid command operation code",
            (0x24, 0x00) => "invalid field in the command",
            (0x26, _) => "invalid field in the parameter list",
            (0x27, _) => "write protected",
            (0x28, _) => "the medium may have changed",
            (0x29, _) => "power on or reset",
            (0x30, _) => "incompatible medium",
            (0x3A, _) => "no medium present",
            (0x3B, 0x0D) => "the destination element is full",
            (0x3B, 0x0E) => "the source element is empty",
            (0x3B, 0x11) => "the medium magazine is not accessible",
            (0x3B, 0x12) => "the medium magazine was removed",
            (0x3B, 0x1A) => "the data transfer element was removed",
            (0x50, _) => "write append error",
            (0x53, 0x00) => "media load or eject failed",
            (0x53, 0x02) => "medium removal prevented",
            _ => return None,
        })
    }
}

impl fmt::Display for Sense {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({:02X}/{:02X}", self.key_name(), self.asc, self.ascq)?;
        if let Some(e) = self.explain() {
            write!(f, ": {e}")?;
        }
        write!(f, ")")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_round_trip() {
        let s = Sense { key: key::NO_SENSE, filemark: true, ili: true, info: Some(-5), asc: 0, ascq: 1, eom: false };
        assert_eq!(Sense::parse(&s.to_fixed()), Some(s));
    }

    #[test]
    fn descriptor_format() {
        // NOT READY 3A/00, with an information descriptor of 7 and stream flags EOM
        let mut b = vec![0x72, 0x02, 0x3A, 0x00, 0, 0, 0, 16];
        b.extend_from_slice(&[0x00, 0x0A, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 7]);
        b.extend_from_slice(&[0x04, 0x02, 0, 0x40]);
        let s = Sense::parse(&b).unwrap();
        assert!(s.no_medium() && s.eom && !s.filemark);
        assert_eq!(s.info, Some(7));
    }
}
