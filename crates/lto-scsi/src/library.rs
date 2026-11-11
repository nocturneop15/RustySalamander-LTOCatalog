//! A tape library: the changer and its drives together. A job asks for a
//! tape by barcode ([`Library::acquire`]); the library finds a drive that
//! already holds it, or a free one (unloading the tape used least recently
//! back to its slot) and loads it. Each drive serves one job at a time;
//! with two drives, two tapes are used at once.

use std::io;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::changer::{Changer, Inventory, Kind};
use crate::tape::TapeDrive;
use crate::{Error, Inquiry, Transport};

/// Opens the devices of a library.
pub trait Devices: Send + Sync {
    /// The changer.
    fn open_changer(&self) -> io::Result<Box<dyn Transport>>;
    /// Every tape drive that can be found (the library's and others).
    fn open_drives(&self) -> io::Result<Vec<Box<dyn Transport>>>;
}

/// Why a tape couldn't be loaded.
#[derive(Debug)]
pub enum LoadError {
    /// No slot, drive or mail slot of the library holds the tape.
    NotInLibrary(String),
    /// The caller gave up waiting.
    Cancelled,
    /// The library failed.
    Scsi(Error),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::NotInLibrary(b) => write!(f, "tape {b} is not in the library"),
            LoadError::Cancelled => write!(f, "cancelled"),
            LoadError::Scsi(e) => write!(f, "{e}"),
        }
    }
}

impl From<Error> for LoadError {
    fn from(e: Error) -> LoadError {
        LoadError::Scsi(e)
    }
}

/// A drive and what it holds.
pub struct DriveState {
    pub drive: TapeDrive,
    last_used: Instant,
}

/// One drive of the library.
pub struct DriveCell {
    /// Its element address in the changer.
    pub element: u16,
    /// Its serial number.
    pub serial: String,
    /// Its device (`\\.\Tape0`).
    pub name: String,
    /// The barcode of the tape in it, readable without waiting for the drive.
    tape: Mutex<Option<String>>,
    state: Mutex<DriveState>,
}

impl DriveCell {
    /// The barcode of the tape in it.
    pub fn tape(&self) -> Option<String> {
        self.tape.lock().unwrap().clone()
    }
}

struct ChangerState {
    changer: Changer,
    inv: Inventory,
}

/// A tape loaded in a drive, for one job. Dropping it frees the drive; the
/// tape stays loaded for the next job that wants it.
pub struct Lease<'a> {
    pub barcode: String,
    pub drive_name: String,
    guard: MutexGuard<'a, DriveState>,
}

impl Lease<'_> {
    pub fn drive(&mut self) -> &mut TapeDrive {
        &mut self.guard.drive
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.guard.last_used = Instant::now();
    }
}

/// A changer with its drives.
pub struct Library {
    changer: Mutex<ChangerState>,
    pub drives: Vec<DriveCell>,
    /// The changer's identification, for messages.
    pub identity: Inquiry,
    /// The changer's device, for messages.
    pub changer_name: String,
    /// Things noticed while opening (a drive whose serial number didn't
    /// match), for the status line.
    pub notes: Vec<String>,
}

/// How long a tape may take to become ready after it is put into a drive.
const LOAD_LIMIT: Duration = Duration::from_secs(5 * 60);

impl Library {
    /// Open the changer and its drives, and match the drives to the
    /// changer's drive elements by serial number.
    pub fn open(devices: &dyn Devices) -> Result<Library, Error> {
        let mut changer = Changer::new(devices.open_changer()?);
        let identity = changer.inquiry()?;
        let changer_name = changer.describe();
        let inv = changer.inventory()?;
        let mut notes = Vec::new();
        let mut found: Vec<(String, TapeDrive)> = Vec::new();
        for t in devices.open_drives()? {
            let mut d = TapeDrive::new(t);
            match d.serial_number() {
                Ok(s) => found.push((s, d)),
                Err(e) => notes.push(format!("{}: {e}", d.describe())),
            }
        }
        let mut drives = Vec::new();
        let mut unmatched: Vec<&crate::changer::Element> = Vec::new();
        for el in &inv.drives {
            let at = el.device_id.as_deref().and_then(|id| found.iter().position(|(s, _)| !s.is_empty() && id.trim_end().ends_with(s.as_str())));
            match at {
                Some(i) => {
                    let (serial, drive) = found.remove(i);
                    drives.push(cell(el.address, serial, drive, el.barcode.clone().filter(|_| el.full)));
                }
                None => unmatched.push(el),
            }
        }
        // a changer that doesn't report drive serial numbers: match by order,
        // when that leaves no doubt
        if !unmatched.is_empty() {
            if unmatched.len() == found.len() {
                notes.push("the changer doesn't identify its drives; they are matched in order".into());
                for (el, (serial, drive)) in unmatched.into_iter().zip(found.drain(..)) {
                    drives.push(cell(el.address, serial, drive, el.barcode.clone().filter(|_| el.full)));
                }
            } else {
                for el in unmatched {
                    notes.push(format!("drive element {:#06x} ({}) has no tape device", el.address, el.device_id.as_deref().unwrap_or("?")));
                }
            }
        }
        if drives.is_empty() {
            return Err(Error::Protocol("none of the library's drives was found as a tape device".into()));
        }
        drives.sort_by_key(|d| d.element);
        Ok(Library { changer: Mutex::new(ChangerState { changer, inv }), drives, identity, changer_name, notes })
    }

    /// The changer's inventory as last read or updated.
    pub fn inventory(&self) -> Inventory {
        self.changer.lock().unwrap().inv.clone()
    }

    /// Read the inventory from the changer again (after the mail slot or a
    /// magazine was used); `rescan` also has it scan every barcode (slow).
    pub fn refresh(&self, rescan: bool) -> Result<Inventory, Error> {
        let mut c = self.changer.lock().unwrap();
        if rescan {
            c.changer.rescan()?;
        }
        c.inv = c.changer.inventory()?;
        for d in &self.drives {
            let el = c.inv.drives.iter().find(|e| e.address == d.element);
            *d.tape.lock().unwrap() = el.filter(|e| e.full).and_then(|e| e.barcode.clone());
        }
        Ok(c.inv.clone())
    }

    /// Whether the tape is in the library (slot, drive or mail slot).
    pub fn has(&self, barcode: &str) -> bool {
        self.changer.lock().unwrap().inv.find(barcode).is_some()
    }

    /// Load the tape `barcode` into a drive and hand it over. Waits while
    /// every drive is busy, asking `cancelled` now and then.
    pub fn acquire(&self, barcode: &str, cancelled: &dyn Fn() -> bool) -> Result<Lease<'_>, LoadError> {
        let mut stale = 0;
        loop {
            if cancelled() {
                return Err(LoadError::Cancelled);
            }
            // a drive that holds it already: wait for it
            if let Some(cell) = self.drives.iter().find(|d| d.tape().is_some_and(|t| t.eq_ignore_ascii_case(barcode))) {
                if let Ok(mut g) = cell.state.try_lock() {
                    g.drive.wait_ready(LOAD_LIMIT)?;
                    return Ok(Lease { barcode: barcode.to_owned(), drive_name: cell.name.clone(), guard: g });
                }
                std::thread::sleep(Duration::from_millis(300));
                continue;
            }
            let Some(from) = self.changer.lock().unwrap().inv.find(barcode).map(|e| e.address) else {
                return Err(LoadError::NotInLibrary(barcode.to_owned()));
            };
            // a free drive: an empty one, else the one used least recently
            let mut free: Vec<(&DriveCell, MutexGuard<'_, DriveState>)> = self.drives.iter().filter_map(|d| d.state.try_lock().ok().map(|g| (d, g))).collect();
            free.sort_by_key(|(d, g)| (d.tape().is_some(), g.last_used));
            let Some((cell, mut g)) = free.into_iter().next() else {
                std::thread::sleep(Duration::from_millis(300));
                continue;
            };
            if cell.tape().is_some() {
                self.unload_locked(cell, &mut g)?;
            }
            {
                let mut c = self.changer.lock().unwrap();
                let transport = c.inv.transports[0].address;
                let r = c.changer.move_medium(transport, from, cell.element);
                if let Err(e) = r {
                    // what is where now? (tapes taken out or put in by hand)
                    if let Ok(inv) = c.changer.inventory() {
                        c.inv = inv;
                    }
                    let moved_by_hand = e.sense().is_some_and(|s| s.asc == 0x3B && matches!(s.ascq, 0x0D | 0x0E));
                    if moved_by_hand && stale < 2 {
                        stale += 1;
                        drop(c);
                        drop(g);
                        continue;
                    }
                    return Err(e.into());
                }
                c.inv.moved(from, cell.element);
            }
            *cell.tape.lock().unwrap() = Some(barcode.to_owned());
            // drives in libraries load a tape put into them by themselves; LOAD is harmless if so
            let _ = g.drive.load();
            g.drive.wait_ready(LOAD_LIMIT)?;
            return Ok(Lease { barcode: barcode.to_owned(), drive_name: cell.name.clone(), guard: g });
        }
    }

    /// Unload the tape of a drive held by the caller and put it back: into
    /// the slot it came from if that is free, else the first free slot.
    fn unload_locked(&self, cell: &DriveCell, g: &mut DriveState) -> Result<(), Error> {
        g.drive.unload()?;
        let mut c = self.changer.lock().unwrap();
        let home = c.inv.at(cell.element).and_then(|e| e.source).filter(|&s| c.inv.at(s).is_some_and(|e| e.kind == Kind::Slot && !e.full));
        let to = home.or_else(|| c.inv.free_slot()).ok_or_else(|| Error::Protocol("the library has no free slot to put a tape back into".into()))?;
        let transport = c.inv.transports[0].address;
        c.changer.move_medium(transport, cell.element, to)?;
        c.inv.moved(cell.element, to);
        *cell.tape.lock().unwrap() = None;
        Ok(())
    }

    /// Put every tape in the drives back into the slots (waits for jobs).
    pub fn unload_all(&self) -> Result<(), Error> {
        for cell in &self.drives {
            let mut g = cell.state.lock().unwrap();
            if cell.tape().is_some() {
                self.unload_locked(cell, &mut g)?;
            }
        }
        Ok(())
    }

    /// Move the tape to the mail slot, so it can be taken out.
    pub fn export(&self, barcode: &str) -> Result<(), LoadError> {
        // out of a drive first
        if let Some(cell) = self.drives.iter().find(|d| d.tape().is_some_and(|t| t.eq_ignore_ascii_case(barcode))) {
            let mut g = cell.state.lock().unwrap();
            self.unload_locked(cell, &mut g)?;
        }
        let mut c = self.changer.lock().unwrap();
        let from = c.inv.find(barcode).map(|e| (e.address, e.kind)).ok_or_else(|| LoadError::NotInLibrary(barcode.to_owned()))?;
        if from.1 == Kind::MailSlot {
            return Ok(());
        }
        let to = c
            .inv
            .mail_slots
            .iter()
            .find(|e| !e.full && e.accessible)
            .map(|e| e.address)
            .ok_or_else(|| LoadError::Scsi(Error::Protocol("the mail slot is full or not accessible; empty it and close it first".into())))?;
        let transport = c.inv.transports[0].address;
        c.changer.move_medium(transport, from.0, to)?;
        c.inv.moved(from.0, to);
        Ok(())
    }

    /// Move a tape from the mail slot into a free storage slot.
    pub fn import(&self, barcode: &str) -> Result<u16, LoadError> {
        let mut c = self.changer.lock().unwrap();
        let from = c.inv.find(barcode).filter(|e| e.kind == Kind::MailSlot).map(|e| e.address).ok_or_else(|| LoadError::NotInLibrary(barcode.to_owned()))?;
        let to = c.inv.free_slot().ok_or_else(|| LoadError::Scsi(Error::Protocol("the library has no free slot".into())))?;
        let transport = c.inv.transports[0].address;
        c.changer.move_medium(transport, from, to)?;
        c.inv.moved(from, to);
        Ok(to)
    }
}

fn cell(element: u16, serial: String, drive: TapeDrive, tape: Option<String>) -> DriveCell {
    DriveCell { element, name: drive.describe(), serial, tape: Mutex::new(tape), state: Mutex::new(DriveState { drive, last_used: Instant::now() - Duration::from_secs(3600) }) }
}
