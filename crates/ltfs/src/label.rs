//! The label construct at the start of each partition: an ANSI VOL1 label
//! (80 bytes), a filemark, the LTFS label (XML), a filemark.

use crate::time::Time;
use crate::xml::{Reader, Tok, Writer};

/// The LTFS format version written.
pub const VERSION: &str = "2.4.0";
/// LTFS' usual block size.
pub const BLOCK_SIZE: u32 = 512 * 1024;

/// The 80-byte VOL1 label of an LTFS partition.
pub fn vol1(barcode: &str) -> [u8; 80] {
    let mut b = [b' '; 80];
    b[0..4].copy_from_slice(b"VOL1");
    for (i, c) in barcode.bytes().filter(|c| c.is_ascii_alphanumeric()).take(6).enumerate() {
        b[4 + i] = c.to_ascii_uppercase();
    }
    b[10] = b'L'; // accessibility: only LTFS may use it
    b[24..28].copy_from_slice(b"LTFS");
    b[79] = b'4';
    b
}

/// Whether a record is the VOL1 label of an LTFS partition.
pub fn is_ltfs_vol1(b: &[u8]) -> bool {
    b.len() == 80 && b.starts_with(b"VOL1") && &b[24..28] == b"LTFS"
}

/// The LTFS label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub version: String,
    pub creator: String,
    pub format_time: Time,
    pub volume_uuid: String,
    /// The partition this copy of the label is in.
    pub location: char,
    pub index_partition: char,
    pub data_partition: char,
    pub block_size: u32,
    pub compression: bool,
}

impl Label {
    pub fn to_xml(&self) -> String {
        let mut w = Writer::new();
        w.open("ltfslabel", &[("version", &self.version)]);
        w.leaf("creator", &self.creator);
        w.leaf("formattime", &self.format_time.format());
        w.leaf("volumeuuid", &self.volume_uuid);
        w.open("location", &[]);
        w.leaf("partition", &self.location.to_string());
        w.close("location");
        w.open("partitions", &[]);
        w.leaf("index", &self.index_partition.to_string());
        w.leaf("data", &self.data_partition.to_string());
        w.close("partitions");
        w.leaf("blocksize", &self.block_size.to_string());
        w.leaf("compression", if self.compression { "true" } else { "false" });
        w.close("ltfslabel");
        w.out
    }

    pub fn parse(xml: &str) -> Result<Label, String> {
        let mut r = Reader::new(xml);
        let version = loop {
            match r.next()? {
                Tok::Start { name: "ltfslabel", attrs, .. } => break attrs.into_iter().find(|(k, _)| *k == "version").map(|(_, v)| v).unwrap_or_default(),
                Tok::Start { name, .. } => return Err(format!("not an LTFS label (<{name}>)")),
                Tok::Eof => return Err("an empty LTFS label".into()),
                _ => {}
            }
        };
        let mut l = Label {
            version,
            creator: String::new(),
            format_time: Time::default(),
            volume_uuid: String::new(),
            location: 'a',
            index_partition: 'a',
            data_partition: 'b',
            block_size: BLOCK_SIZE,
            compression: false,
        };
        let one = |s: String| s.trim().chars().next().filter(|c| c.is_ascii_lowercase()).ok_or_else(|| format!("bad partition \"{s}\""));
        loop {
            match r.next()? {
                Tok::Start { name, .. } => match name {
                    "creator" => l.creator = r.text(name)?,
                    "formattime" => l.format_time = Time::parse(&r.text(name)?).ok_or("bad formattime")?,
                    "volumeuuid" => l.volume_uuid = r.text(name)?.trim().to_owned(),
                    "partition" => l.location = one(r.text(name)?)?,
                    "index" => l.index_partition = one(r.text(name)?)?,
                    "data" => l.data_partition = one(r.text(name)?)?,
                    "blocksize" => l.block_size = r.text(name)?.trim().parse().map_err(|_| "bad blocksize")?,
                    "compression" => l.compression = r.text(name)?.trim() == "true",
                    "location" | "partitions" => {}
                    _ => r.skip(name)?,
                },
                Tok::End { name: "ltfslabel", .. } | Tok::Eof => break,
                _ => {}
            }
        }
        if l.volume_uuid.is_empty() {
            return Err("the LTFS label has no volume UUID".into());
        }
        if l.block_size == 0 || l.block_size > 8 * 1024 * 1024 {
            return Err(format!("unsupported block size {}", l.block_size));
        }
        if l.index_partition == l.data_partition {
            return Err("the LTFS label names one partition for both index and data".into());
        }
        Ok(l)
    }
}

/// A new random (version 4) UUID, from the system's random generator
/// where there is one.
pub fn new_uuid() -> String {
    let mut b = [0u8; 16];
    fill_random(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn fill_random(b: &mut [u8]) {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    // RandomState is seeded from the system's random source; mix in time and counter
    for (i, chunk) in b.chunks_mut(8).enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_usize(i);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        let v = h.finish().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_round_trip() {
        let l = Label {
            version: VERSION.into(),
            creator: "test".into(),
            format_time: Time { secs: 1_794_399_630, nanos: 0 },
            volume_uuid: new_uuid(),
            location: 'b',
            index_partition: 'a',
            data_partition: 'b',
            block_size: BLOCK_SIZE,
            compression: true,
        };
        assert_eq!(Label::parse(&l.to_xml()).unwrap(), l);
        let v = vol1("abc123L5");
        assert!(is_ltfs_vol1(&v));
        assert_eq!(&v[4..10], b"ABC123");
        assert_ne!(new_uuid(), new_uuid());
        assert_eq!(new_uuid().len(), 36);
    }
}
