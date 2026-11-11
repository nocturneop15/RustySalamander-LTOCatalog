//! A simulated tape library that answers SCSI commands like a TS3100 with
//! LTO drives: the same commands as the real devices go through
//! [`lto_scsi::Transport`], so everything above it is tested as it runs on
//! the hardware.
//!
//! Tapes hold records and filemarks per partition, with a capacity in bytes,
//! early warning near the end, write protection and medium attributes.
//! Partitioning follows the medium partition mode page and FORMAT MEDIUM.
//! The state can be saved to a file and loaded again, so the plugin can be
//! tried in the program without a library.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lto_scsi::changer::{build_element_status, Element, Kind};
use lto_scsi::library::Devices;
use lto_scsi::sense::{key, Sense};
use lto_scsi::{Data, Reply, Transport, CHECK_CONDITION, GOOD};

/// Element addresses, as a TS3100 numbers them.
pub const TRANSPORT: u16 = 0x0001;
pub const MAIL: u16 = 0x0010;
pub const DRIVES: u16 = 0x0100;
pub const SLOTS: u16 = 0x1000;

/// One record on tape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rec {
    Data(Vec<u8>),
    Mark,
}

/// A tape cartridge.
#[derive(Debug, Clone)]
pub struct Cart {
    pub barcode: String,
    /// The partitions' records.
    pub parts: Vec<Vec<Rec>>,
    /// The partitions' capacities, bytes.
    pub caps: Vec<u64>,
    pub write_protect: bool,
    /// Medium attributes by (partition, id): (format, value).
    pub attrs: BTreeMap<(u8, u16), (u8, Vec<u8>)>,
    /// Volume change reference: counts writes.
    pub vcr: u64,
}

impl Cart {
    /// A new, unpartitioned tape of `capacity` bytes.
    pub fn new(barcode: &str, capacity: u64) -> Cart {
        Cart { barcode: barcode.into(), parts: vec![Vec::new()], caps: vec![capacity], write_protect: false, attrs: BTreeMap::new(), vcr: 1 }
    }

    fn used(&self, p: usize) -> u64 {
        self.parts[p].iter().map(|r| if let Rec::Data(d) = r { d.len() as u64 } else { 1 }).sum()
    }

    fn total(&self) -> u64 {
        self.caps.iter().sum()
    }
}

/// A drive.
#[derive(Debug, Clone)]
struct Drive {
    serial: String,
    cart: Option<Cart>,
    /// Where its tape came from.
    from: Option<u16>,
    loaded: bool,
    part: usize,
    pos: usize,
    unit_attention: bool,
    compression: bool,
    /// Partitions asked for by MODE SELECT, applied by FORMAT MEDIUM:
    /// (additional partitions, unit exponent, sizes).
    pending: Option<(u8, u8, Vec<u16>)>,
}

/// The library's state.
#[derive(Debug, Clone)]
pub struct State {
    pub slots: Vec<Option<Cart>>,
    pub mail: Vec<Option<Cart>>,
    /// Whether the robot can reach the mail slot (its door is closed).
    pub mail_accessible: bool,
    drives: Vec<Drive>,
    /// Early warning this many bytes before the end of a partition.
    pub early_warning: u64,
    /// The smallest partition the "drive" makes, bytes (LTO-5: 37.5 GB).
    pub min_partition: u64,
    /// The next command with this operation code fails with this sense
    /// (tests of error handling).
    pub fail_next: Option<(u8, Sense)>,
    /// Commands sent, by operation code (for tests).
    pub counts: BTreeMap<u8, u64>,
}

/// A simulated library; clone it to share.
#[derive(Clone)]
pub struct SimLibrary {
    pub state: Arc<Mutex<State>>,
    /// Where the state is saved after every change that moves or writes a tape.
    file: Option<PathBuf>,
}

/// How big a simulated library is.
#[derive(Debug, Clone)]
pub struct Config {
    pub slots: usize,
    pub mail: usize,
    pub drives: usize,
    /// Capacity of each new tape, bytes.
    pub capacity: u64,
    /// Barcodes of the tapes in the first slots.
    pub tapes: Vec<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config { slots: 24, mail: 1, drives: 2, capacity: 256 * 1024 * 1024, tapes: (1..=6).map(|i| format!("LTO{i:03}L5")).collect() }
    }
}

impl SimLibrary {
    pub fn new(cfg: &Config) -> SimLibrary {
        let mut slots: Vec<Option<Cart>> = vec![None; cfg.slots];
        for (i, b) in cfg.tapes.iter().enumerate().take(cfg.slots) {
            slots[i] = Some(Cart::new(b, cfg.capacity));
        }
        let drives = (0..cfg.drives)
            .map(|i| Drive { serial: format!("10680000{i:02}"), cart: None, from: None, loaded: false, part: 0, pos: 0, unit_attention: false, compression: true, pending: None })
            .collect();
        let state = State {
            slots,
            mail: vec![None; cfg.mail],
            mail_accessible: true,
            drives,
            early_warning: (cfg.capacity / 100).max(2 * 1024 * 1024),
            min_partition: (cfg.capacity / 40).max(1024 * 1024),
            fail_next: None,
            counts: BTreeMap::new(),
        };
        SimLibrary { state: Arc::new(Mutex::new(state)), file: None }
    }

    /// A library kept in `file`: loaded from it if it exists, else made
    /// from `cfg`; saved there after changes.
    pub fn persistent(file: &Path, cfg: &Config) -> io::Result<SimLibrary> {
        let mut lib = if file.exists() { SimLibrary { state: Arc::new(Mutex::new(load(file)?)), file: None } } else { SimLibrary::new(cfg) };
        lib.file = Some(file.to_path_buf());
        lib.save()?;
        Ok(lib)
    }

    fn save(&self) -> io::Result<()> {
        match &self.file {
            Some(f) => save(&self.state.lock().unwrap(), f),
            None => Ok(()),
        }
    }

    /// Put a tape into the first free mail slot (as a person would).
    pub fn insert(&self, cart: Cart) -> Result<(), Cart> {
        let mut s = self.state.lock().unwrap();
        match s.mail.iter_mut().find(|m| m.is_none()) {
            Some(m) => {
                *m = Some(cart);
                drop(s);
                let _ = self.save();
                Ok(())
            }
            None => Err(cart),
        }
    }

    /// Take the tape out of the mail slot.
    pub fn take(&self) -> Option<Cart> {
        let t = self.state.lock().unwrap().mail.iter_mut().find_map(|m| m.take());
        let _ = self.save();
        t
    }

    /// Take a tape out of whichever slot it is in (an operator with the magazine).
    pub fn remove(&self, barcode: &str) -> Option<Cart> {
        let mut s = self.state.lock().unwrap();
        let s2 = &mut *s;
        let t = s2.slots.iter_mut().chain(s2.mail.iter_mut()).find(|c| c.as_ref().is_some_and(|c| c.barcode == barcode)).and_then(|c| c.take());
        drop(s);
        let _ = self.save();
        t
    }

    /// A copy of the tape with this barcode, wherever it is.
    pub fn cart(&self, barcode: &str) -> Option<Cart> {
        let s = self.state.lock().unwrap();
        s.slots.iter().chain(s.mail.iter()).flatten().chain(s.drives.iter().filter_map(|d| d.cart.as_ref())).find(|c| c.barcode == barcode).cloned()
    }

    /// Change the tape with this barcode, wherever it is.
    pub fn with_cart<R>(&self, barcode: &str, f: impl FnOnce(&mut Cart) -> R) -> Option<R> {
        let mut s = self.state.lock().unwrap();
        let s = &mut *s;
        let c = s.slots.iter_mut().chain(s.mail.iter_mut()).flatten().chain(s.drives.iter_mut().filter_map(|d| d.cart.as_mut())).find(|c| c.barcode == barcode)?;
        Some(f(c))
    }

    /// How many commands with this operation code were sent.
    pub fn count(&self, op: u8) -> u64 {
        self.state.lock().unwrap().counts.get(&op).copied().unwrap_or(0)
    }

    /// The changer's transport.
    pub fn changer(&self) -> Box<dyn Transport> {
        Box::new(SimChanger { lib: self.clone() })
    }

    /// The transport of drive `i`.
    pub fn drive(&self, i: usize) -> Box<dyn Transport> {
        Box::new(SimTape { lib: self.clone(), i })
    }
}

impl Devices for SimLibrary {
    fn open_changer(&self) -> io::Result<Box<dyn Transport>> {
        Ok(self.changer())
    }
    fn open_drives(&self) -> io::Result<Vec<Box<dyn Transport>>> {
        let n = self.state.lock().unwrap().drives.len();
        Ok((0..n).map(|i| self.drive(i)).collect())
    }
}

fn check(s: Sense) -> io::Result<Reply> {
    Ok(Reply { status: CHECK_CONDITION, sense: Some(s), transferred: 0 })
}

fn good(n: usize) -> io::Result<Reply> {
    Ok(Reply { status: GOOD, sense: None, transferred: n })
}

/// Copy `src` into an In buffer.
fn give(data: Data<'_>, src: &[u8]) -> usize {
    match data {
        Data::In(b) => {
            let n = b.len().min(src.len());
            b[..n].copy_from_slice(&src[..n]);
            n
        }
        _ => 0,
    }
}

fn inquiry_data(dtype: u8, vendor: &str, product: &str) -> Vec<u8> {
    let mut b = vec![b' '; 36];
    b[0] = dtype;
    b[1] = if dtype == lto_scsi::TYPE_TAPE { 0x80 } else { 0 };
    b[2] = 0x06;
    b[3] = 0x02;
    b[4] = 31;
    b[5] = 0;
    b[6] = 0;
    b[7] = 0;
    b[8..8 + vendor.len().min(8)].copy_from_slice(&vendor.as_bytes()[..vendor.len().min(8)]);
    b[16..16 + product.len().min(16)].copy_from_slice(&product.as_bytes()[..product.len().min(16)]);
    b[32..36].copy_from_slice(b"SIM1");
    b
}

fn serial_page(serial: &str) -> Vec<u8> {
    let mut b = vec![0, 0x80, 0, serial.len() as u8];
    b.extend_from_slice(serial.as_bytes());
    b
}

/// The changer.
struct SimChanger {
    lib: SimLibrary,
}

impl SimChanger {
    fn elements(s: &State, kind: Kind) -> Vec<Element> {
        let el = |kind, address, cart: Option<&Cart>, source, accessible, device_id| Element {
            kind,
            address,
            full: cart.is_some(),
            barcode: cart.map(|c| c.barcode.clone()),
            source,
            exception: false,
            accessible,
            device_id,
        };
        match kind {
            Kind::Transport => vec![el(Kind::Transport, TRANSPORT, None, None, true, None)],
            Kind::Slot => s.slots.iter().enumerate().map(|(i, c)| el(Kind::Slot, SLOTS + i as u16, c.as_ref(), None, true, None)).collect(),
            Kind::MailSlot => s.mail.iter().enumerate().map(|(i, c)| el(Kind::MailSlot, MAIL + i as u16, c.as_ref(), None, s.mail_accessible, None)).collect(),
            Kind::Drive => s
                .drives
                .iter()
                .enumerate()
                .map(|(i, d)| el(Kind::Drive, DRIVES + i as u16, d.cart.as_ref(), d.from, true, Some(format!("IBM     ULT3580-HH5     {}", d.serial))))
                .collect(),
        }
    }

    fn take(s: &mut State, addr: u16) -> Result<(Cart, Option<u16>), Sense> {
        let empty = Sense::new(key::ILLEGAL_REQUEST, 0x3B, 0x0E);
        let bad = Sense::new(key::ILLEGAL_REQUEST, 0x21, 0x01);
        if (SLOTS..SLOTS + s.slots.len() as u16).contains(&addr) {
            return s.slots[(addr - SLOTS) as usize].take().map(|c| (c, None)).ok_or(empty);
        }
        if (MAIL..MAIL + s.mail.len() as u16).contains(&addr) {
            if !s.mail_accessible {
                return Err(Sense::new(key::NOT_READY, 0x3B, 0x11));
            }
            return s.mail[(addr - MAIL) as usize].take().map(|c| (c, None)).ok_or(empty);
        }
        if (DRIVES..DRIVES + s.drives.len() as u16).contains(&addr) {
            let d = &mut s.drives[(addr - DRIVES) as usize];
            if d.cart.is_none() {
                return Err(empty);
            }
            if d.loaded {
                // the drive must unload the tape first
                return Err(Sense::new(key::ILLEGAL_REQUEST, 0x53, 0x02));
            }
            let from = d.from.take();
            return Ok((d.cart.take().unwrap(), from));
        }
        Err(bad)
    }

    fn put(s: &mut State, addr: u16, cart: Cart, from: u16) -> Result<(), (Sense, Cart)> {
        let full = Sense::new(key::ILLEGAL_REQUEST, 0x3B, 0x0D);
        let bad = Sense::new(key::ILLEGAL_REQUEST, 0x21, 0x01);
        if (SLOTS..SLOTS + s.slots.len() as u16).contains(&addr) {
            let v = &mut s.slots[(addr - SLOTS) as usize];
            return if v.is_some() { Err((full, cart)) } else { Ok(*v = Some(cart)) };
        }
        if (MAIL..MAIL + s.mail.len() as u16).contains(&addr) {
            if !s.mail_accessible {
                return Err((Sense::new(key::NOT_READY, 0x3B, 0x11), cart));
            }
            let v = &mut s.mail[(addr - MAIL) as usize];
            return if v.is_some() { Err((full, cart)) } else { Ok(*v = Some(cart)) };
        }
        if (DRIVES..DRIVES + s.drives.len() as u16).contains(&addr) {
            let d = &mut s.drives[(addr - DRIVES) as usize];
            if d.cart.is_some() {
                return Err((full, cart));
            }
            d.cart = Some(cart);
            d.from = Some(from);
            // drives in a library load a tape put into them
            d.loaded = true;
            d.part = 0;
            d.pos = 0;
            d.unit_attention = true;
            return Ok(());
        }
        Err((bad, cart))
    }
}

impl Transport for SimChanger {
    fn exec(&mut self, cdb: &[u8], data: Data<'_>, _timeout: u32) -> io::Result<Reply> {
        let mut s = self.lib.state.lock().unwrap();
        *s.counts.entry(cdb[0]).or_default() += 1;
        if let Some((op, sense)) = s.fail_next.clone() {
            if op == cdb[0] {
                s.fail_next = None;
                return check(sense);
            }
        }
        match cdb[0] {
            0x00 | 0x07 | 0x1E => good(0),
            0x12 if cdb[1] & 1 != 0 => good(give(data, &serial_page("SIM0001234"))),
            0x12 => good(give(data, &inquiry_data(lto_scsi::TYPE_CHANGER, "IBM", "3573-TL"))),
            0xB8 => {
                let Some(kind) = [Kind::Transport, Kind::Slot, Kind::MailSlot, Kind::Drive].into_iter().find(|k| *k as u8 == cdb[1] & 0x0f) else {
                    return check(Sense::new(key::ILLEGAL_REQUEST, 0x24, 0x00));
                };
                let els = SimChanger::elements(&s, kind);
                good(give(data, &build_element_status(kind, &els, cdb[6] & 1 != 0)))
            }
            0xA5 => {
                let (from, to) = (u16::from_be_bytes([cdb[4], cdb[5]]), u16::from_be_bytes([cdb[6], cdb[7]]));
                let (cart, origin) = match SimChanger::take(&mut s, from) {
                    Ok(c) => c,
                    Err(e) => return check(e),
                };
                if let Err((e, cart)) = SimChanger::put(&mut s, to, cart, origin.unwrap_or(from)) {
                    // back where it was
                    let _ = SimChanger::put(&mut s, from, cart, from);
                    return check(e);
                }
                drop(s);
                let _ = self.lib.save();
                good(0)
            }
            _ => check(Sense::new(key::ILLEGAL_REQUEST, 0x20, 0x00)),
        }
    }

    fn describe(&self) -> String {
        "sim:changer".into()
    }
}

/// A drive.
struct SimTape {
    lib: SimLibrary,
    i: usize,
}

impl Transport for SimTape {
    fn exec(&mut self, cdb: &[u8], data: Data<'_>, _timeout: u32) -> io::Result<Reply> {
        let (r, changed) = {
            let mut s = self.lib.state.lock().unwrap();
            *s.counts.entry(cdb[0]).or_default() += 1;
            if let Some((op, sense)) = s.fail_next.clone() {
                if op == cdb[0] {
                    s.fail_next = None;
                    return check(sense);
                }
            }
            let (ew, minp) = (s.early_warning, s.min_partition);
            let d = &mut s.drives[self.i];
            let changed = matches!(cdb[0], 0x0A | 0x10 | 0x04 | 0x8D | 0x1B);
            (tape_cmd(d, cdb, data, ew, minp), changed)
        };
        if changed {
            let _ = self.lib.save();
        }
        r
    }

    fn describe(&self) -> String {
        format!("sim:tape{}", self.i)
    }
}

fn tape_cmd(d: &mut Drive, cdb: &[u8], data: Data<'_>, ew: u64, min_part: u64) -> io::Result<Reply> {
    let illegal = || check(Sense::new(key::ILLEGAL_REQUEST, 0x24, 0x00));
    // commands that work without a tape
    match cdb[0] {
        0x12 if cdb[1] & 1 != 0 => return good(give(data, &serial_page(&d.serial))),
        0x12 => return good(give(data, &inquiry_data(lto_scsi::TYPE_TAPE, "IBM", "ULT3580-HH5"))),
        0x1E => return good(0),
        _ => {}
    }
    if d.cart.is_none() {
        return check(Sense::new(key::NOT_READY, 0x3A, 0x00));
    }
    if cdb[0] == 0x1B {
        d.loaded = cdb[4] & 1 != 0;
        d.part = 0;
        d.pos = 0;
        return good(0);
    }
    if !d.loaded {
        return check(Sense::new(key::NOT_READY, 0x04, 0x02));
    }
    if d.unit_attention {
        d.unit_attention = false;
        return check(Sense::new(key::UNIT_ATTENTION, 0x28, 0x00));
    }
    let part = d.part;
    let cart = d.cart.as_mut().unwrap();
    let len = cart.parts[part].len();
    match cdb[0] {
        0x00 => good(0),
        0x01 => {
            d.part = 0;
            d.pos = 0;
            good(0)
        }
        0x92 => {
            let p = if cdb[1] & 0x02 != 0 { cdb[3] as usize } else { part };
            if p >= cart.parts.len() {
                return check(Sense::new(key::ILLEGAL_REQUEST, 0x24, 0x00));
            }
            let block = u64::from_be_bytes(cdb[4..12].try_into().unwrap()) as usize;
            d.part = p;
            if block > cart.parts[p].len() {
                d.pos = cart.parts[p].len();
                return check(Sense { eom: true, ..Sense::new(key::BLANK_CHECK, 0x00, 0x05) });
            }
            d.pos = block;
            good(0)
        }
        0x34 => {
            let mut b = [0u8; 32];
            if d.pos == 0 {
                b[0] |= 0x80;
            }
            if cart.used(part) + ew >= cart.caps[part] {
                b[0] |= 0x40;
            }
            b[4..8].copy_from_slice(&(part as u32).to_be_bytes());
            b[8..16].copy_from_slice(&(d.pos as u64).to_be_bytes());
            let files = cart.parts[part][..d.pos].iter().filter(|r| **r == Rec::Mark).count() as u64;
            b[16..24].copy_from_slice(&files.to_be_bytes());
            good(give(data, &b))
        }
        0x08 => {
            if cdb[1] & 1 != 0 {
                return illegal();
            }
            let want = u32::from_be_bytes([0, cdb[2], cdb[3], cdb[4]]) as usize;
            if d.pos >= len {
                return check(Sense::new(key::BLANK_CHECK, 0x00, 0x05));
            }
            let rec = cart.parts[part][d.pos].clone();
            d.pos += 1;
            match rec {
                Rec::Mark => check(Sense { filemark: true, info: Some(want as i64), ..Sense::new(key::NO_SENSE, 0x00, 0x01) }),
                Rec::Data(v) => {
                    let n = give(data, &v[..v.len().min(want)]);
                    if v.len() == want {
                        good(n)
                    } else {
                        Ok(Reply { status: CHECK_CONDITION, sense: Some(Sense { ili: true, info: Some(want as i64 - v.len() as i64), ..Sense::new(key::NO_SENSE, 0, 0) }), transferred: n })
                    }
                }
            }
        }
        0x0A | 0x10 => {
            if cart.write_protect {
                return check(Sense::new(key::DATA_PROTECT, 0x27, 0x00));
            }
            let n = u32::from_be_bytes([0, cdb[2], cdb[3], cdb[4]]) as usize;
            cart.parts[part].truncate(d.pos);
            if cart.used(part) + n as u64 > cart.caps[part] {
                return check(Sense { eom: true, ..Sense::new(key::VOLUME_OVERFLOW, 0x00, 0x02) });
            }
            if cdb[0] == 0x0A {
                let Data::Out(b) = data else { return illegal() };
                cart.parts[part].push(Rec::Data(b[..n.min(b.len())].to_vec()));
                d.pos += 1;
            } else {
                for _ in 0..n {
                    cart.parts[part].push(Rec::Mark);
                    d.pos += 1;
                }
            }
            cart.vcr += 1;
            if cart.used(part) + ew >= cart.caps[part] {
                return check(Sense { eom: true, ..Sense::new(key::NO_SENSE, 0x00, 0x02) });
            }
            good(n)
        }
        0x11 => {
            let recs = &cart.parts[part];
            match cdb[1] & 0x07 {
                3 => {
                    d.pos = len;
                    good(0)
                }
                1 => {
                    let raw = u32::from_be_bytes([0, cdb[2], cdb[3], cdb[4]]);
                    let n = if raw & 0x80_0000 != 0 { raw as i32 - 0x100_0000 } else { raw as i32 };
                    if n >= 0 {
                        let mut left = n;
                        while left > 0 {
                            if d.pos >= len {
                                return check(Sense { eom: true, ..Sense::new(key::BLANK_CHECK, 0x00, 0x05) });
                            }
                            if recs[d.pos] == Rec::Mark {
                                left -= 1;
                            }
                            d.pos += 1;
                        }
                    } else {
                        let mut left = -n;
                        while left > 0 {
                            if d.pos == 0 {
                                return check(Sense { eom: true, ..Sense::new(key::MEDIUM_ERROR, 0x00, 0x04) });
                            }
                            d.pos -= 1;
                            if recs[d.pos] == Rec::Mark {
                                left -= 1;
                            }
                        }
                    }
                    good(0)
                }
                _ => illegal(),
            }
        }
        0x5A => {
            let dbd = cdb[1] & 0x08 != 0;
            let page = cdb[2] & 0x3f;
            let mut pages = Vec::new();
            if page == 0x0F || page == 0x3F {
                let mut p = vec![0u8; 16];
                p[0] = 0x0F;
                p[1] = 14;
                p[2] = 0x40 | if d.compression { 0x80 } else { 0 };
                p[4..8].copy_from_slice(&1u32.to_be_bytes());
                pages.extend_from_slice(&p);
            }
            if page == 0x11 || page == 0x3F {
                let mut p = vec![0u8; 8];
                p[0] = 0x11;
                p[2] = 1;
                p[3] = (cart.parts.len() - 1) as u8;
                p[4] = 0x18;
                p[6] = 0x09;
                for c in &cart.caps {
                    p.extend_from_slice(&((c / 1_000_000_000).min(0xFFFE) as u16).to_be_bytes());
                }
                p[1] = (p.len() - 2) as u8;
                pages.extend_from_slice(&p);
            }
            if pages.is_empty() {
                return illegal();
            }
            let mut b = vec![0u8; 8];
            b[3] = 0x10 | if cart.write_protect { 0x80 } else { 0 };
            if !dbd {
                b[7] = 8;
                b.extend_from_slice(&[0x58, 0, 0, 0, 0, 0, 0, 0]);
            }
            b.extend_from_slice(&pages);
            let n = (b.len() - 2) as u16;
            b[0..2].copy_from_slice(&n.to_be_bytes());
            good(give(data, &b))
        }
        0x55 => {
            let Data::Out(b) = data else { return illegal() };
            if b.len() < 8 {
                return illegal();
            }
            let bd = u16::from_be_bytes([b[6], b[7]]) as usize;
            if bd == 8 && b.len() >= 16 {
                let block = u32::from_be_bytes([0, b[13], b[14], b[15]]);
                if block != 0 {
                    // only variable blocks here
                    return check(Sense::new(key::ILLEGAL_REQUEST, 0x26, 0x00));
                }
            }
            let mut p = 8 + bd;
            while p + 2 <= b.len() {
                let (code, plen) = (b[p] & 0x3f, b[p + 1] as usize + 2);
                let page = &b[p..(p + plen).min(b.len())];
                if b[p] & 0x80 != 0 {
                    // PS is reserved in MODE SELECT
                    return check(Sense::new(key::ILLEGAL_REQUEST, 0x26, 0x00));
                }
                match code {
                    0x0F if page.len() >= 3 => d.compression = page[2] & 0x80 != 0,
                    0x11 if page.len() >= 8 => {
                        if page[4] & 0x20 == 0 || page[3] > 1 {
                            return check(Sense::new(key::ILLEGAL_REQUEST, 0x26, 0x00));
                        }
                        let sizes = page[8..].chunks(2).filter(|c| c.len() == 2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                        d.pending = Some((page[3], page[6] & 0x0f, sizes));
                    }
                    _ => return check(Sense::new(key::ILLEGAL_REQUEST, 0x26, 0x00)),
                }
                p += plen;
            }
            good(0)
        }
        0x04 => {
            if cart.write_protect {
                return check(Sense::new(key::DATA_PROTECT, 0x27, 0x00));
            }
            if d.part != 0 || d.pos != 0 {
                // FORMAT MEDIUM wants the beginning of partition 0
                return check(Sense::new(key::ILLEGAL_REQUEST, 0x00, 0x00));
            }
            let total = cart.total();
            match cdb[2] & 0x0f {
                0 => {
                    cart.parts = vec![Vec::new()];
                    cart.caps = vec![total];
                }
                1 => {
                    let Some((add, unit, sizes)) = d.pending.take() else { return illegal() };
                    if add == 0 {
                        cart.parts = vec![Vec::new()];
                        cart.caps = vec![total];
                    } else {
                        let first = sizes.first().copied().unwrap_or(0) as u64 * 10u64.pow(unit as u32);
                        let first = first.max(min_part).min(total / 2);
                        cart.parts = vec![Vec::new(), Vec::new()];
                        cart.caps = vec![first, total - first];
                    }
                }
                _ => return illegal(),
            }
            cart.attrs.retain(|(_, id), _| *id == lto_scsi::tape::attr::BARCODE);
            cart.vcr += 1;
            d.part = 0;
            d.pos = 0;
            good(0)
        }
        0x8C => {
            if cdb[1] & 0x1f != 0 {
                return illegal();
            }
            let p = cdb[7];
            let first = u16::from_be_bytes([cdb[8], cdb[9]]);
            if p as usize >= cart.parts.len() {
                return illegal();
            }
            let mib = 1024 * 1024;
            let mut attrs: BTreeMap<u16, (u8, Vec<u8>)> = BTreeMap::new();
            attrs.insert(0x0000, (0x80, ((cart.caps[p as usize].saturating_sub(cart.used(p as usize))) / mib).to_be_bytes().to_vec()));
            attrs.insert(0x0001, (0x80, (cart.caps[p as usize] / mib).to_be_bytes().to_vec()));
            attrs.insert(0x0009, (0x80, cart.vcr.to_be_bytes().to_vec()));
            for ((ap, id), v) in &cart.attrs {
                if *ap == p {
                    attrs.insert(*id, v.clone());
                }
            }
            let mut out = vec![0u8; 4];
            for (id, (fmt, v)) in attrs.range(first..) {
                out.extend_from_slice(&id.to_be_bytes());
                out.push(*fmt);
                out.extend_from_slice(&(v.len() as u16).to_be_bytes());
                out.extend_from_slice(v);
            }
            if out.len() == 4 {
                return illegal();
            }
            let n = (out.len() - 4) as u32;
            out[0..4].copy_from_slice(&n.to_be_bytes());
            good(give(data, &out))
        }
        0x8D => {
            let Data::Out(b) = data else { return illegal() };
            let p = cdb[7];
            let mut at = 4;
            while at + 5 <= b.len() {
                let id = u16::from_be_bytes([b[at], b[at + 1]]);
                let fmt = b[at + 2] & 0x03;
                let n = u16::from_be_bytes([b[at + 3], b[at + 4]]) as usize;
                let v = b[at + 5..(at + 5 + n).min(b.len())].to_vec();
                if id < 0x0800 {
                    // device attributes are read-only
                    return check(Sense::new(key::ILLEGAL_REQUEST, 0x26, 0x00));
                }
                cart.attrs.insert((p, id), (fmt, v));
                at += 5 + n;
            }
            good(0)
        }
        _ => check(Sense::new(key::ILLEGAL_REQUEST, 0x20, 0x00)),
    }
}

// ---- saving and loading the state ----

const MAGIC: &[u8; 8] = b"LTOSIM01";

fn w64(o: &mut Vec<u8>, v: u64) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn wbytes(o: &mut Vec<u8>, b: &[u8]) {
    w64(o, b.len() as u64);
    o.extend_from_slice(b);
}

fn write_cart(o: &mut Vec<u8>, c: &Option<Cart>) {
    let Some(c) = c else {
        o.push(0);
        return;
    };
    o.push(1);
    wbytes(o, c.barcode.as_bytes());
    o.push(c.write_protect as u8);
    w64(o, c.vcr);
    w64(o, c.parts.len() as u64);
    for (p, cap) in c.parts.iter().zip(&c.caps) {
        w64(o, *cap);
        w64(o, p.len() as u64);
        for r in p {
            match r {
                Rec::Mark => o.push(0),
                Rec::Data(d) => {
                    o.push(1);
                    wbytes(o, d);
                }
            }
        }
    }
    w64(o, c.attrs.len() as u64);
    for ((p, id), (f, v)) in &c.attrs {
        o.push(*p);
        o.extend_from_slice(&id.to_le_bytes());
        o.push(*f);
        wbytes(o, v);
    }
}

fn save(s: &State, file: &Path) -> io::Result<()> {
    let mut o = MAGIC.to_vec();
    w64(&mut o, s.slots.len() as u64);
    for c in &s.slots {
        write_cart(&mut o, c);
    }
    w64(&mut o, s.mail.len() as u64);
    for c in &s.mail {
        write_cart(&mut o, c);
    }
    o.push(s.mail_accessible as u8);
    w64(&mut o, s.drives.len() as u64);
    for d in &s.drives {
        wbytes(&mut o, d.serial.as_bytes());
        write_cart(&mut o, &d.cart);
        w64(&mut o, d.from.map(|f| f as u64 + 1).unwrap_or(0));
        o.push(d.loaded as u8);
    }
    w64(&mut o, s.early_warning);
    w64(&mut o, s.min_partition);
    let tmp = file.with_extension("tmp");
    let mut f = fs::File::create(&tmp)?;
    f.write_all(&o)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, file)
}

struct Rd<'a>(&'a [u8]);

impl Rd<'_> {
    fn bad() -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, "the simulated library's file is damaged")
    }
    fn u8(&mut self) -> io::Result<u8> {
        let (&b, rest) = self.0.split_first().ok_or_else(Self::bad)?;
        self.0 = rest;
        Ok(b)
    }
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.0.len() < n {
            return Err(Self::bad());
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let n = self.u64()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn string(&mut self) -> io::Result<String> {
        String::from_utf8(self.bytes()?).map_err(|_| Self::bad())
    }
    fn cart(&mut self) -> io::Result<Option<Cart>> {
        if self.u8()? == 0 {
            return Ok(None);
        }
        let barcode = self.string()?;
        let write_protect = self.u8()? != 0;
        let vcr = self.u64()?;
        let n = self.u64()? as usize;
        let (mut parts, mut caps) = (Vec::new(), Vec::new());
        for _ in 0..n {
            caps.push(self.u64()?);
            let m = self.u64()? as usize;
            let mut recs = Vec::with_capacity(m.min(1 << 20));
            for _ in 0..m {
                recs.push(if self.u8()? == 0 { Rec::Mark } else { Rec::Data(self.bytes()?) });
            }
            parts.push(recs);
        }
        let mut attrs = BTreeMap::new();
        for _ in 0..self.u64()? {
            let p = self.u8()?;
            let id = u16::from_le_bytes(self.take(2)?.try_into().unwrap());
            let f = self.u8()?;
            attrs.insert((p, id), (f, self.bytes()?));
        }
        Ok(Some(Cart { barcode, parts, caps, write_protect, attrs, vcr }))
    }
}

fn load(file: &Path) -> io::Result<State> {
    let mut data = Vec::new();
    fs::File::open(file)?.read_to_end(&mut data)?;
    if !data.starts_with(MAGIC) {
        return Err(Rd::bad());
    }
    let mut r = Rd(&data[8..]);
    let slots = (0..r.u64()?).map(|_| r.cart()).collect::<io::Result<Vec<_>>>()?;
    let mail = (0..r.u64()?).map(|_| r.cart()).collect::<io::Result<Vec<_>>>()?;
    let mail_accessible = r.u8()? != 0;
    let mut drives = Vec::new();
    for _ in 0..r.u64()? {
        let serial = r.string()?;
        let cart = r.cart()?;
        let from = match r.u64()? {
            0 => None,
            f => Some((f - 1) as u16),
        };
        let loaded = r.u8()? != 0;
        drives.push(Drive { serial, cart, from, loaded, part: 0, pos: 0, unit_attention: true, compression: true, pending: None });
    }
    let early_warning = r.u64()?;
    let min_partition = r.u64()?;
    Ok(State { slots, mail, mail_accessible, drives, early_warning, min_partition, fail_next: None, counts: BTreeMap::new() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lto_scsi::library::Library;
    use lto_scsi::tape::Read;

    #[test]
    fn load_write_read_move() {
        let sim = SimLibrary::new(&Config::default());
        let lib = Library::open(&sim).unwrap();
        assert_eq!(lib.drives.len(), 2);
        assert_eq!(lib.inventory().slots.iter().filter(|e| e.full).count(), 6);
        {
            let mut l = lib.acquire("LTO002L5", &|| false).unwrap();
            let d = l.drive();
            d.write(b"hello").unwrap();
            d.write_filemarks(1).unwrap();
            d.write(b"world!").unwrap();
            d.rewind().unwrap();
            let mut buf = vec![0u8; 64];
            assert_eq!(d.read(&mut buf).unwrap(), Read::Record(5));
            assert_eq!(&buf[..5], b"hello");
            assert_eq!(d.read(&mut buf).unwrap(), Read::Filemark);
            assert_eq!(d.read(&mut buf).unwrap(), Read::Record(6));
            assert_eq!(d.read(&mut buf).unwrap(), Read::EndOfData);
            d.space_filemarks(-1).unwrap();
            assert_eq!(d.position().unwrap().block, 1);
            d.space_to_eod().unwrap();
            assert_eq!(d.position().unwrap().block, 3);
        }
        // a second tape goes to the other drive, a third replaces the one used least recently
        let a = lib.acquire("LTO003L5", &|| false).unwrap();
        drop(a);
        let _b = lib.acquire("LTO004L5", &|| false).unwrap();
        let inv = lib.inventory();
        let in_drives: Vec<_> = inv.drives.iter().filter_map(|e| e.barcode.clone()).collect();
        assert_eq!(in_drives.len(), 2);
        assert!(in_drives.contains(&"LTO004L5".to_string()));
        // LTO002L5 went home to its slot
        assert_eq!(inv.find("LTO002L5").unwrap().address, SLOTS + 1);
        assert!(matches!(lib.acquire("NOPE01L5", &|| false), Err(lto_scsi::library::LoadError::NotInLibrary(_))));
    }

    #[test]
    fn inventory_matches_the_changer() {
        let sim = SimLibrary::new(&Config::default());
        let lib = Library::open(&sim).unwrap();
        drop(lib.acquire("LTO005L5", &|| false).unwrap());
        let known = lib.inventory();
        assert_eq!(lib.refresh(false).unwrap(), known);
        // a tape put into the mail slot is seen after a refresh, and goes to a slot
        sim.insert(Cart::new("NEW001L5", 1 << 26)).unwrap();
        assert!(!lib.has("NEW001L5"));
        lib.refresh(false).unwrap();
        assert!(lib.has("NEW001L5"));
        let slot = lib.import("NEW001L5").unwrap();
        assert_eq!(lib.inventory().find("NEW001L5").unwrap().address, slot);
        lib.export("LTO005L5").unwrap();
        assert_eq!(sim.take().unwrap().barcode, "LTO005L5");
    }

    #[test]
    fn partitions_and_attributes() {
        let sim = SimLibrary::new(&Config::default());
        let lib = Library::open(&sim).unwrap();
        let mut l = lib.acquire("LTO001L5", &|| false).unwrap();
        let d = l.drive();
        assert_eq!(d.partitions().unwrap(), 1);
        d.format_two_partitions(1).unwrap();
        assert_eq!(d.partitions().unwrap(), 2);
        d.locate(1, 0).unwrap();
        d.write(b"data").unwrap();
        assert_eq!(d.position().unwrap().partition, 1);
        d.write_attribute(1, lto_scsi::tape::attr::BARCODE, lto_scsi::tape::FORMAT_ASCII, b"LTO001L5").unwrap();
        assert_eq!(d.read_attribute(1, lto_scsi::tape::attr::BARCODE).unwrap().unwrap(), b"LTO001L5");
        assert_eq!(d.read_attribute(0, lto_scsi::tape::attr::BARCODE).unwrap(), None);
        let (rem, max) = d.capacity(1).unwrap();
        assert!(rem.unwrap() <= max.unwrap() && max.unwrap() > 0);
        d.set_compression(false).unwrap();
        d.set_variable_blocks().unwrap();
        assert!(!d.write_protected().unwrap());
    }

    #[test]
    fn saved_and_loaded() {
        let dir = std::env::temp_dir().join(format!("ltosim-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("lib.bin");
        let _ = fs::remove_file(&f);
        {
            let sim = SimLibrary::persistent(&f, &Config::default()).unwrap();
            let lib = Library::open(&sim).unwrap();
            let mut l = lib.acquire("LTO001L5", &|| false).unwrap();
            l.drive().write(b"kept").unwrap();
        }
        let sim = SimLibrary::persistent(&f, &Config::default()).unwrap();
        let c = sim.cart("LTO001L5").unwrap();
        assert_eq!(c.parts[0], vec![Rec::Data(b"kept".to_vec())]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
