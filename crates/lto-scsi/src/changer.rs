//! A medium changer (SMC-3): the robot of a tape library. It knows its
//! elements (slots, drives, the mail slot), which hold a tape and its
//! barcode, and moves tapes between them.

use crate::{be16, be24, run, Data, Error, Inquiry, Result, Transport};

const T_SHORT: u32 = 60;
const T_MOVE: u32 = 15 * 60;
const T_INIT: u32 = 60 * 60;

/// How many times a robot's command is sent again after UNIT ATTENTION.
const ATTENTION_RETRIES: usize = 5;

/// Run a command of the robot's. UNIT ATTENTION (a mail slot opened or
/// closed, tapes taken out or put in: 28/01, a reset, changed settings)
/// means the command wasn't carried out, only that the library has news:
/// it is sent again. So is a command that met the library still becoming
/// ready, a second later. (Not for tape drives: there, a unit attention can
/// mean the position on the tape is lost.)
fn run_robot(t: &mut dyn Transport, op: &'static str, cdb: &[u8], mut buf: Option<&mut [u8]>, timeout_s: u32) -> Result<crate::Reply> {
    let mut tries = 0;
    loop {
        let data = match buf.as_deref_mut() {
            Some(b) => Data::In(b),
            None => Data::None,
        };
        match run(t, op, cdb, data, timeout_s, |_| false) {
            Err(Error::Check { sense, .. }) if tries < ATTENTION_RETRIES && (sense.key == crate::sense::key::UNIT_ATTENTION || sense.becoming_ready()) => {
                tries += 1;
                if sense.becoming_ready() {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
            r => return r,
        }
    }
}

/// The kinds of elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// The robot's hand.
    Transport = 1,
    /// A storage slot.
    Slot = 2,
    /// The mail slot (import/export): where tapes go in and out.
    MailSlot = 3,
    /// A tape drive.
    Drive = 4,
}

impl Kind {
    fn from_code(c: u8) -> Option<Kind> {
        Some(match c {
            1 => Kind::Transport,
            2 => Kind::Slot,
            3 => Kind::MailSlot,
            4 => Kind::Drive,
            _ => return None,
        })
    }
}

/// One element and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub kind: Kind,
    pub address: u16,
    /// It holds a tape.
    pub full: bool,
    /// The tape's barcode, when it has a readable one.
    pub barcode: Option<String>,
    /// Where the tape came from (its home slot, for a tape in a drive).
    pub source: Option<u16>,
    /// The element is in an abnormal state.
    pub exception: bool,
    /// The robot can reach it (a mail slot whose door is open is not).
    pub accessible: bool,
    /// A drive's identification (vendor, product and serial number).
    pub device_id: Option<String>,
}

/// Everything the changer knows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    pub transports: Vec<Element>,
    pub slots: Vec<Element>,
    pub mail_slots: Vec<Element>,
    pub drives: Vec<Element>,
}

impl Inventory {
    /// Every element.
    pub fn all(&self) -> impl Iterator<Item = &Element> {
        self.transports.iter().chain(&self.slots).chain(&self.mail_slots).chain(&self.drives)
    }

    /// A place in the library, for people: slot 3, drive 1, mail slot.
    pub fn place(&self, address: u16) -> String {
        let Some(el) = self.at(address) else { return String::new() };
        let nth = |list: &[Element]| list.iter().position(|e| e.address == address).map(|i| i + 1).unwrap_or(0);
        match el.kind {
            Kind::Slot => format!("slot {}", nth(&self.slots)),
            Kind::Drive => format!("drive {}", nth(&self.drives)),
            Kind::MailSlot if self.mail_slots.len() > 1 => format!("mail slot {}", nth(&self.mail_slots)),
            Kind::MailSlot => "mail slot".into(),
            Kind::Transport => "robot".into(),
        }
    }

    /// Where the tape with this barcode is (case-insensitive).
    pub fn find(&self, barcode: &str) -> Option<&Element> {
        self.all().find(|e| e.full && e.barcode.as_deref().is_some_and(|b| b.eq_ignore_ascii_case(barcode)))
    }

    /// The element at this address.
    pub fn at(&self, address: u16) -> Option<&Element> {
        self.all().find(|e| e.address == address)
    }

    pub fn at_mut(&mut self, address: u16) -> Option<&mut Element> {
        self.transports.iter_mut().chain(&mut self.slots).chain(&mut self.mail_slots).chain(&mut self.drives).find(|e| e.address == address)
    }

    /// The first empty storage slot.
    pub fn free_slot(&self) -> Option<u16> {
        self.slots.iter().find(|e| !e.full && !e.exception).map(|e| e.address)
    }

    /// Record a move done.
    pub fn moved(&mut self, from: u16, to: u16) {
        let (barcode, source) = match self.at_mut(from) {
            Some(e) => {
                e.full = false;
                let b = e.barcode.take();
                let s = e.source.take();
                (b, s)
            }
            None => (None, None),
        };
        let from_kind = self.at(from).map(|e| e.kind);
        if let Some(e) = self.at_mut(to) {
            e.full = true;
            e.barcode = barcode;
            // a drive remembers where the tape came from; a slot is its home
            e.source = if from_kind == Some(Kind::Slot) { Some(from) } else { source.or(Some(from)) };
        }
    }
}

/// The robot.
pub struct Changer {
    t: Box<dyn Transport>,
}

impl Changer {
    pub fn new(t: Box<dyn Transport>) -> Changer {
        Changer { t }
    }

    pub fn describe(&self) -> String {
        self.t.describe()
    }

    pub fn inquiry(&mut self) -> Result<Inquiry> {
        crate::inquiry(&mut *self.t)
    }

    pub fn serial_number(&mut self) -> Result<String> {
        crate::serial_number(&mut *self.t)
    }

    /// Wait for the changer to be ready, clearing a pending unit attention.
    pub fn ready(&mut self) -> Result<()> {
        for _ in 0..10 {
            match crate::test_unit_ready(&mut *self.t)? {
                None => return Ok(()),
                Some(s) if s.key == crate::sense::key::UNIT_ATTENTION => continue,
                Some(s) if s.becoming_ready() => std::thread::sleep(std::time::Duration::from_secs(1)),
                Some(s) => return Err(Error::Check { op: "TEST UNIT READY", sense: s }),
            }
        }
        Ok(())
    }

    /// INITIALIZE ELEMENT STATUS: scan every slot's barcode again (slow).
    pub fn rescan(&mut self) -> Result<()> {
        run_robot(&mut *self.t, "INITIALIZE ELEMENT STATUS", &[0x07, 0, 0, 0, 0, 0], None, T_INIT).map(|_| ())
    }

    /// READ ELEMENT STATUS of every element, kind by kind.
    pub fn inventory(&mut self) -> Result<Inventory> {
        self.ready()?;
        let mut inv = Inventory::default();
        for kind in [Kind::Transport, Kind::Slot, Kind::MailSlot, Kind::Drive] {
            let els = self.element_status(kind, kind == Kind::Drive)?;
            match kind {
                Kind::Transport => inv.transports = els,
                Kind::Slot => inv.slots = els,
                Kind::MailSlot => inv.mail_slots = els,
                Kind::Drive => inv.drives = els,
            }
        }
        if inv.transports.is_empty() {
            return Err(Error::Protocol("the changer reports no robot (medium transport element)".into()));
        }
        Ok(inv)
    }

    /// READ ELEMENT STATUS of the elements of one kind; with drives'
    /// identifications when `dvcid` (asked again without when the changer
    /// doesn't support it).
    pub fn element_status(&mut self, kind: Kind, dvcid: bool) -> Result<Vec<Element>> {
        let mut buf = vec![0u8; 64 * 1024];
        let n = buf.len() as u32;
        let mut cdb = [0u8; 12];
        cdb[0] = 0xB8;
        cdb[1] = 0x10 | kind as u8; // VOLTAG
        cdb[4..6].copy_from_slice(&0xFFFFu16.to_be_bytes());
        cdb[6] = if dvcid { 0x01 } else { 0 };
        cdb[7] = (n >> 16) as u8;
        cdb[8] = (n >> 8) as u8;
        cdb[9] = n as u8;
        let r = match run_robot(&mut *self.t, "READ ELEMENT STATUS", &cdb, Some(&mut buf), T_SHORT) {
            Err(Error::Check { sense, .. }) if dvcid && sense.key == crate::sense::key::ILLEGAL_REQUEST => return self.element_status(kind, false),
            r => r?,
        };
        let len = r.transferred.min(buf.len());
        parse_element_status(&buf[..len])
    }

    /// MOVE MEDIUM from one element to another, with the robot `transport`.
    pub fn move_medium(&mut self, transport: u16, from: u16, to: u16) -> Result<()> {
        let mut cdb = [0u8; 12];
        cdb[0] = 0xA5;
        cdb[2..4].copy_from_slice(&transport.to_be_bytes());
        cdb[4..6].copy_from_slice(&from.to_be_bytes());
        cdb[6..8].copy_from_slice(&to.to_be_bytes());
        run_robot(&mut *self.t, "MOVE MEDIUM", &cdb, None, T_MOVE).map(|_| ())
    }

    /// Lock or unlock the library's doors and mail slot.
    pub fn prevent_removal(&mut self, prevent: bool) -> Result<()> {
        crate::prevent_removal(&mut *self.t, prevent)
    }
}

/// Decode READ ELEMENT STATUS data.
pub fn parse_element_status(b: &[u8]) -> Result<Vec<Element>> {
    let bad = || Error::Protocol("READ ELEMENT STATUS: malformed answer".into());
    if b.len() < 8 {
        return Err(bad());
    }
    let total = (be24(&b[5..8]) as usize + 8).min(b.len());
    let mut out = Vec::new();
    let mut p = 8;
    while p + 8 <= total {
        let kind = Kind::from_code(b[p] & 0x0f);
        let (pvoltag, avoltag) = (b[p + 1] & 0x80 != 0, b[p + 1] & 0x40 != 0);
        let dlen = be16(&b[p + 2..p + 4]) as usize;
        let bytes = be24(&b[p + 5..p + 8]) as usize;
        let start = p + 8;
        let end = (start + bytes).min(total);
        if dlen == 0 {
            return Err(bad());
        }
        let mut d = start;
        while d + dlen <= end {
            let e = &b[d..d + dlen];
            if let Some(kind) = kind {
                out.push(parse_descriptor(kind, e, pvoltag, avoltag));
            }
            d += dlen;
        }
        p = end;
    }
    Ok(out)
}

fn parse_descriptor(kind: Kind, e: &[u8], pvoltag: bool, avoltag: bool) -> Element {
    let flags = e.get(2).copied().unwrap_or(0);
    let svalid = e.get(9).is_some_and(|f| f & 0x80 != 0);
    let mut el = Element {
        kind,
        address: be16(&e[0..2]),
        full: flags & 0x01 != 0,
        exception: flags & 0x04 != 0,
        // the ACCESS bit is defined for mail slots and drives; slots are always reachable
        accessible: kind == Kind::Slot || kind == Kind::Transport || flags & 0x08 != 0,
        barcode: None,
        source: if svalid && e.len() >= 12 { Some(be16(&e[10..12])) } else { None },
        device_id: None,
    };
    let mut p = 12;
    if pvoltag {
        if e.len() >= p + 36 {
            let tag = crate::ascii(&e[p..p + 32]);
            if !tag.is_empty() {
                el.barcode = Some(tag);
            }
        }
        p += 36;
    }
    if avoltag {
        p += 36;
    }
    // device identifier: code set, identifier type, reserved, length, identifier
    if e.len() >= p + 4 {
        let n = e[p + 3] as usize;
        if n > 0 && e.len() >= p + 4 + n {
            let id = crate::ascii(&e[p + 4..p + 4 + n]);
            if !id.is_empty() {
                el.device_id = Some(id);
            }
        }
    }
    el
}

/// Build READ ELEMENT STATUS data (the simulator answers with it). Each
/// element: address, full, barcode, source, device id.
pub fn build_element_status(kind: Kind, elements: &[Element], dvcid: bool) -> Vec<u8> {
    let dlen = 12 + 36 + if dvcid { 4 + 34 } else { 0 };
    let mut page = vec![kind as u8, 0x80, 0, 0, 0, 0, 0, 0];
    page[2..4].copy_from_slice(&(dlen as u16).to_be_bytes());
    let bytes = dlen * elements.len();
    page[5..8].copy_from_slice(&(bytes as u32).to_be_bytes()[1..]);
    for el in elements {
        let mut d = vec![0u8; dlen];
        d[0..2].copy_from_slice(&el.address.to_be_bytes());
        d[2] = (el.full as u8) | if el.exception { 0x04 } else { 0 } | if el.accessible { 0x08 } else { 0 };
        if let Some(s) = el.source {
            d[9] = 0x80;
            d[10..12].copy_from_slice(&s.to_be_bytes());
        }
        let mut tag = [b' '; 32];
        if let Some(b) = el.barcode.as_ref().filter(|_| el.full) {
            for (i, c) in b.bytes().take(32).enumerate() {
                tag[i] = c;
            }
        }
        d[12..44].copy_from_slice(&tag);
        if dvcid {
            if let Some(id) = &el.device_id {
                let id = id.as_bytes();
                let n = id.len().min(34);
                d[48] = 0x02; // ASCII
                d[49] = 0x01; // T10 vendor identification
                d[51] = n as u8;
                d[52..52 + n].copy_from_slice(&id[..n]);
            }
        }
        page.extend_from_slice(&d);
    }
    let first = elements.first().map(|e| e.address).unwrap_or(0);
    let mut out = vec![0u8; 8];
    out[0..2].copy_from_slice(&first.to_be_bytes());
    out[2..4].copy_from_slice(&(elements.len() as u16).to_be_bytes());
    out[5..8].copy_from_slice(&(page.len() as u32).to_be_bytes()[1..]);
    out.extend_from_slice(&page);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_status_round_trip() {
        let els = vec![
            Element { kind: Kind::Drive, address: 0x100, full: true, barcode: Some("ABC123L5".into()), source: Some(0x1003), exception: false, accessible: true, device_id: Some("IBM     ULT3580-HH5     1068000001".into()) },
            Element { kind: Kind::Drive, address: 0x101, full: false, barcode: None, source: None, exception: false, accessible: true, device_id: Some("IBM     ULT3580-HH5     1068000002".into()) },
        ];
        let b = build_element_status(Kind::Drive, &els, true);
        assert_eq!(parse_element_status(&b).unwrap(), els);
    }
}
