//! TapeAlert (SSC-4 annex A): the flags a tape drive raises about its tape
//! and itself, read from its TapeAlert log page (0x2E). Reading the page
//! clears them, so they say what happened since the last look.

/// What a flag is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concern {
    /// The tape: worn, damaged, or failing to be read or written.
    Tape,
    /// The drive needs cleaning (or the cleaning cartridge is spent).
    Cleaning,
    /// The drive itself, or its connection.
    Drive,
}

/// A flag the drive raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alert {
    /// Its number, 1 to 64.
    pub flag: u8,
    pub concern: Concern,
    /// What it means, in English (for the log and `ltocat`).
    pub text: &'static str,
}

/// Flags that only tell how things are, not that something is wrong: a
/// write-protected tape, the eject button locked by a program.
const INFORMATIONAL: [u8; 2] = [9, 10];

/// What flag `flag` means. Flags that only inform are `None`.
pub fn describe(flag: u8) -> Option<Alert> {
    use Concern::*;
    if INFORMATIONAL.contains(&flag) {
        return None;
    }
    let (concern, text) = match flag {
        1 => (Tape, "the drive has severe trouble reading this tape"),
        2 => (Tape, "the drive has severe trouble writing this tape"),
        3 => (Tape, "an error that couldn't be recovered while reading, writing or positioning"),
        4 => (Tape, "the tape is damaged, or the drive is faulty: copy the data off it"),
        5 => (Tape, "read failure: the tape is damaged, or the drive is faulty"),
        6 => (Tape, "write failure: the tape is damaged, or the drive is faulty"),
        7 => (Tape, "the tape has reached the end of its life: copy the data onto another tape"),
        8 => (Tape, "the tape isn't data grade"),
        11 => (Drive, "a cleaning cartridge is in the drive"),
        12 => (Tape, "the tape's format isn't supported by this drive"),
        13 => (Tape, "a mechanical failure of the cartridge (recovered)"),
        14 => (Tape, "a mechanical failure of the cartridge: it may be stuck in the drive"),
        15 => (Tape, "the cartridge memory chip has failed"),
        16 => (Drive, "the tape was ejected by hand while it was in use"),
        17 => (Tape, "the tape is in a format the drive can only read"),
        18 => (Tape, "the tape's directory was damaged when it was loaded: reading may be slow"),
        19 => (Tape, "the tape is nearing the end of its life"),
        20 => (Cleaning, "the drive needs cleaning now"),
        21 => (Cleaning, "the drive is due for its routine cleaning"),
        22 => (Cleaning, "the cleaning cartridge is used up"),
        23 => (Cleaning, "the cleaning cartridge isn't a valid one"),
        24 => (Drive, "the drive asks for the tape to be retensioned"),
        25 => (Drive, "a dual-port interface error"),
        26 => (Drive, "the drive's cooling fan has failed"),
        27 => (Drive, "the drive's power supply has failed"),
        28 => (Drive, "the drive draws more power than it should"),
        29 => (Drive, "the drive needs maintenance"),
        30 => (Drive, "the drive has a hardware fault: switch it off and on again"),
        31 => (Drive, "the drive has a hardware fault: run its diagnostics"),
        32 => (Drive, "a fault of the connection to the drive"),
        33 => (Drive, "the drive asks for the tape to be taken out and put in again"),
        34 => (Drive, "a firmware download failed"),
        35 => (Drive, "the drive is too humid"),
        36 => (Drive, "the drive is too hot"),
        37 => (Drive, "the drive's voltage is out of range"),
        38 => (Drive, "the drive predicts that it will fail"),
        39 => (Drive, "the drive asks for its diagnostics to be run"),
        49 => (Tape, "the tape's native capacity has diminished"),
        50 => (Drive, "the drive lost the statistics of the tape"),
        51 => (Tape, "the tape's directory couldn't be written at unload: reading may be slow"),
        52 => (Tape, "the tape's system area couldn't be written"),
        53 => (Tape, "the tape's system area couldn't be read"),
        54 => (Tape, "the start of data can't be found on the tape"),
        55 => (Tape, "the tape couldn't be loaded"),
        56 => (Tape, "the tape couldn't be unloaded"),
        57 => (Drive, "a fault of the library's connection to the drive"),
        58 => (Drive, "the drive's firmware failed"),
        59 => (Tape, "the WORM tape's integrity check failed"),
        60 => (Tape, "an attempt to overwrite a WORM tape"),
        _ => (Drive, "a flag this program doesn't know"),
    };
    Some(Alert { flag, concern, text })
}

/// The flags raised in a TapeAlert log page, by number.
pub fn parse_page(page: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if page.len() < 4 || page[0] & 0x3F != 0x2E {
        return out;
    }
    let end = (4 + u16::from_be_bytes([page[2], page[3]]) as usize).min(page.len());
    let mut at = 4;
    while at + 4 <= end {
        let code = u16::from_be_bytes([page[at], page[at + 1]]);
        let len = page[at + 3] as usize;
        if at + 4 + len > end {
            break;
        }
        if (1..=64).contains(&code) && len >= 1 && page[at + 4] & 1 != 0 {
            out.push(code as u8);
        }
        at += 4 + len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_says_its_flags() {
        let mut page = vec![0x2E, 0, 0, 0];
        for n in 1..=64u16 {
            page.extend_from_slice(&n.to_be_bytes());
            page.extend_from_slice(&[0x03, 1, u8::from(matches!(n, 4 | 20 | 64))]);
        }
        let len = (page.len() - 4) as u16;
        page[2..4].copy_from_slice(&len.to_be_bytes());
        assert_eq!(parse_page(&page), [4, 20, 64]);
        // cut short, or another page: nothing
        assert_eq!(parse_page(&page[..12]), Vec::<u8>::new());
        assert!(parse_page(&[0x31, 0, 0, 0]).is_empty());
        assert_eq!(describe(4).unwrap().concern, Concern::Tape);
        assert_eq!(describe(20).unwrap().concern, Concern::Cleaning);
        assert_eq!(describe(30).unwrap().concern, Concern::Drive);
        assert!(describe(9).is_none(), "write protection is no problem");
    }
}
