//! Each file written onto a tape gets the SHA-256 digest of its data in
//! its LTFS extended attribute `ltfs.hash.sha256sum` (the name IBM's LTFS
//! uses), so a check of the tape ([`crate::Archive::verify`]) shows the
//! data read back is the data written, not only that it could be read.

use std::io::{self, Read};

use ltfs::index::Xattr;
use ltfs::File;
use sha2::{Digest, Sha256};

/// The extended attribute holding a file's SHA-256, in hex.
pub const SHA256_KEY: &str = "ltfs.hash.sha256sum";

/// Reads through `inner`, hashing what it gives.
pub(crate) struct Hashing<'a> {
    inner: &'a mut dyn Read,
    hash: Sha256,
    read: u64,
}

impl<'a> Hashing<'a> {
    pub(crate) fn new(inner: &'a mut dyn Read) -> Hashing<'a> {
        Hashing { inner, hash: Sha256::new(), read: 0 }
    }

    /// The digest attribute for a file of `length` bytes, when that much
    /// went through (else none: the file isn't what was hashed).
    pub(crate) fn attribute(self, length: u64) -> Option<Xattr> {
        (self.read == length).then(|| Xattr { key: SHA256_KEY.into(), value: hex(&self.hash.finalize()), base64: false })
    }
}

impl Read for Hashing<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        self.read += n as u64;
        Ok(n)
    }
}

/// Hashes data handed over in pieces.
#[derive(Default)]
pub(crate) struct Hasher(Sha256);

impl Hasher {
    pub(crate) fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
    pub(crate) fn hex(self) -> String {
        hex(&self.0.finalize())
    }
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// The SHA-256 stored with the file (lowercase hex), if any.
pub(crate) fn stored(f: &File) -> Option<String> {
    f.xattrs.iter().find(|x| x.key == SHA256_KEY && !x.base64).map(|x| x.value.trim().to_ascii_lowercase()).filter(|v| v.len() == 64 && v.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_digests() {
        let mut data: &[u8] = b"abc";
        let mut h = Hashing::new(&mut data);
        let mut out = Vec::new();
        h.read_to_end(&mut out).unwrap();
        assert_eq!(h.attribute(3).unwrap().value, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let mut empty: &[u8] = b"";
        assert_eq!(Hashing::new(&mut empty).attribute(0).unwrap().value, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        let mut short: &[u8] = b"ab";
        let mut h = Hashing::new(&mut short);
        h.read_to_end(&mut Vec::new()).unwrap();
        assert!(h.attribute(3).is_none(), "not all of it went through");
    }
}
