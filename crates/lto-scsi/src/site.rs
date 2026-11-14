//! Every tape device the computer has: any number of libraries (a changer
//! with its drives) and standalone drives, which a person loads by hand.
//!
//! A drive belongs to the library whose changer reports its serial number;
//! drives no changer claims are standalone. A job asks for a tape by
//! barcode ([`Site::acquire`]):
//!
//! 1. a drive that holds it already, in a library or standalone;
//! 2. a library that has it in a slot or its mail slot: the robot loads it;
//! 3. else [`LoadError::NotInLibrary`]: the caller asks a person to put it
//!    into a library's mail slot or a standalone drive ([`Site::places`]
//!    says which there are), and tries again.
//!
//! A standalone drive has no barcode reader, so the tape in it is
//! identified by the caller's [`Identify`] (the cartridge memory's barcode,
//! the LTFS volume in the catalog…). What was found is remembered per
//! cartridge (its serial number in the cartridge memory), so a tape is
//! identified once, not at every job. While a job uses a standalone drive,
//! its eject button is locked.

use std::sync::Mutex;
use std::time::Duration;

use crate::library::{DriveState, Lease, Library, LoadError, LOAD_LIMIT};
use crate::tape::{attr, TapeDrive};
use crate::{Error, Transport};

pub use crate::library::Devices;

/// Names the tape in a standalone drive (loaded and ready): its barcode,
/// or `None` when it can't be told.
pub type Identify<'a> = &'a dyn Fn(&mut TapeDrive) -> Option<String>;

/// What a standalone drive holds, as last seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Holds {
    /// Nothing, or a tape not yet looked at.
    Empty,
    /// A tape identified as this barcode.
    Tape(String),
    /// A tape that couldn't be identified (no barcode in its memory, not
    /// in the catalog).
    Unknown,
}

/// A drive outside any library.
pub struct Standalone {
    /// Its device (`\\.\Tape2`).
    pub name: String,
    pub serial: String,
    holds: Mutex<Holds>,
    /// The cartridge last identified: (its serial number, what it is).
    known: Mutex<Option<(String, Holds)>>,
    /// The tape was ejected here and may still sit in the drive's mouth:
    /// it isn't loaded again until a person takes it or puts one in.
    ejected: Mutex<bool>,
    state: Mutex<DriveState>,
}

impl Standalone {
    /// What it holds, as last seen (without asking the drive).
    pub fn holds(&self) -> Holds {
        self.holds.lock().unwrap().clone()
    }

    /// Whether a job is using it.
    pub fn busy(&self) -> bool {
        self.state.try_lock().is_err()
    }

    fn set(&self, h: Holds) -> Holds {
        *self.holds.lock().unwrap() = h.clone();
        h
    }

    /// Look at the drive (held by the caller): is there a tape, and which?
    /// A tape just put in is waited for while it loads.
    fn look(&self, g: &mut DriveState, identify: Identify) -> Result<Holds, Error> {
        match g.drive.test_unit_ready()? {
            None => {}
            Some(s) if s.no_medium() => {
                *self.known.lock().unwrap() = None;
                *self.ejected.lock().unwrap() = false;
                return Ok(self.set(Holds::Empty));
            }
            // ejected by us and not taken: it counts as out of the drive
            Some(s) if s.needs_load() && *self.ejected.lock().unwrap() => return Ok(self.set(Holds::Empty)),
            Some(s) if s.becoming_ready() => g.drive.wait_ready(LOAD_LIMIT)?,
            // in the drive but not threaded (ejected and not taken out, or
            // a drive that doesn't load by itself)
            Some(s) if s.needs_load() => {
                g.drive.load()?;
                g.drive.wait_ready(LOAD_LIMIT)?;
            }
            Some(s) => return Err(Error::Check { op: "TEST UNIT READY", sense: s }),
        }
        *self.ejected.lock().unwrap() = false;
        let serial = g.drive.read_attribute(0, attr::MEDIUM_SERIAL_NUMBER).ok().flatten().map(|v| crate::ascii(&v)).filter(|s| !s.is_empty());
        if let (Some(s), Some((k, h))) = (&serial, &*self.known.lock().unwrap()) {
            if s == k {
                return Ok(self.set(h.clone()));
            }
        }
        let h = match identify(&mut g.drive) {
            Some(b) => Holds::Tape(b),
            None => Holds::Unknown,
        };
        // only a tape that was identified is remembered: an unknown one is
        // looked at again (a Format may take it, the catalog may learn it)
        *self.known.lock().unwrap() = serial.filter(|_| h != Holds::Unknown).map(|s| (s, h.clone()));
        Ok(self.set(h))
    }

    /// Eject the tape (held by the caller), so a person can take it.
    fn eject(&self, g: &mut DriveState) -> Result<(), Error> {
        let _ = g.drive.prevent_removal(false);
        g.drive.unload()?;
        *self.known.lock().unwrap() = None;
        *self.ejected.lock().unwrap() = true;
        self.set(Holds::Empty);
        Ok(())
    }
}

/// Where a person can put a tape, for the question that asks for one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Places {
    /// The libraries (by name) whose mail slot the robot can take it from.
    pub libraries: Vec<String>,
    /// The standalone drives and what they hold.
    pub drives: Vec<(String, Holds)>,
    /// A tape ejected from a standalone drive to make room: (drive, tape).
    pub ejected: Option<(String, Holds)>,
}

/// What [`Site::eject`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ejected {
    /// The tape is in the mail slot of this library.
    MailSlot(String),
    /// The tape came out of this standalone drive.
    Drive(String),
}

/// Every library and standalone drive.
pub struct Site {
    pub libraries: Vec<Library>,
    pub standalone: Vec<Standalone>,
    /// Things noticed while opening, for the status.
    pub notes: Vec<String>,
}

/// Drives with their serial numbers. A device that doesn't say it is a
/// tape drive (a disk named in the settings by mistake) is left out and
/// noted: a tape command such as FORMAT means something else to a disk.
/// One that doesn't answer is left out too. A device with the serial number
/// of one already found is the same drive seen a second time (a SAS
/// drive's second port, a second path): it is left out and noted, or two
/// "drives" would be one, and a job for the tape in one library drive would
/// be done to the tape in the other.
pub(crate) fn drives_with_serials(drives: Vec<Box<dyn Transport>>, notes: &mut Vec<String>) -> Vec<(String, TapeDrive)> {
    let mut found: Vec<(String, TapeDrive)> = Vec::new();
    for t in drives {
        let mut d = TapeDrive::new(t);
        match d.inquiry() {
            Ok(i) if i.device_type == crate::TYPE_TAPE => {}
            Ok(i) => {
                notes.push(format!("{} is not a tape drive (device type {:#04x}, {} {}); it isn't used", d.describe(), i.device_type, i.vendor, i.product));
                continue;
            }
            Err(e) => {
                notes.push(format!("{}: {e}", d.describe()));
                continue;
            }
        }
        match d.serial_number() {
            Ok(s) => {
                if let Some((_, first)) = found.iter().find(|(f, _)| !s.trim().is_empty() && f.trim() == s.trim()) {
                    notes.push(format!("{} is the same drive as {} (serial number {}), seen a second time (a second port or path); it is used once", d.describe(), first.describe(), s.trim()));
                    continue;
                }
                found.push((s, d))
            }
            Err(e) => notes.push(format!("{}: {e}", d.describe())),
        }
    }
    found
}

impl Site {
    /// Open every changer and drive: each changer takes its drives, the
    /// rest are standalone. Fails only when there is nothing at all.
    pub fn open(devices: &dyn Devices) -> Result<Site, Error> {
        let mut notes = Vec::new();
        let changers = devices.open_changers()?;
        let drives = match devices.open_drives() {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                notes.push(e.to_string());
                Vec::new()
            }
            Err(e) => return Err(e.into()),
        };
        let mut found = drives_with_serials(drives, &mut notes);
        // matching drives by order is only safe with one changer and no
        // other drives
        let by_order = changers.len() == 1;
        let mut libraries = Vec::new();
        for t in changers {
            let where_ = t.describe();
            match Library::with_drives(t, &mut found, by_order) {
                Ok(lib) => libraries.push(lib),
                Err(e) => notes.push(format!("the changer at {where_} can't be used: {e}")),
            }
        }
        if libraries.len() > 1 {
            for (i, lib) in libraries.iter_mut().enumerate() {
                lib.name = format!("{} {}", lib.name, i + 1);
            }
        }
        for lib in &libraries {
            notes.extend(lib.notes.iter().cloned());
        }
        let standalone: Vec<Standalone> = found
            .into_iter()
            .map(|(serial, drive)| Standalone { name: drive.describe(), serial, holds: Mutex::new(Holds::Empty), known: Mutex::new(None), ejected: Mutex::new(false), state: Mutex::new(DriveState::new(drive)) })
            .collect();
        if libraries.iter().all(|l| l.drives.is_empty()) && standalone.is_empty() {
            let why = if notes.is_empty() { String::new() } else { format!(": {}", notes.join("; ")) };
            return Err(Error::Protocol(format!("no tape drive was found{why}")));
        }
        let site = Site { libraries, standalone, notes };
        site.note_duplicates();
        Ok(site)
    }

    /// The same barcode in two libraries: only the first one's tape is used.
    fn note_duplicates(&self) -> Vec<String> {
        let mut seen: Vec<(String, &str)> = Vec::new();
        let mut dup = Vec::new();
        for lib in &self.libraries {
            for b in lib.inventory().all().filter(|e| e.full).filter_map(|e| e.barcode.clone()) {
                match seen.iter().find(|(s, _)| s.eq_ignore_ascii_case(&b)) {
                    Some((_, other)) if *other != lib.name => dup.push(format!("tape {b} is in both {other} and {}; only the one in {other} is used", lib.name)),
                    _ => seen.push((b, &lib.name)),
                }
            }
        }
        dup
    }

    /// Notes about the devices and their tapes now (duplicates included).
    pub fn all_notes(&self) -> Vec<String> {
        let mut n = self.notes.clone();
        n.extend(self.note_duplicates());
        n
    }

    /// Every drive, library and standalone.
    pub fn drive_count(&self) -> usize {
        self.libraries.iter().map(|l| l.drives.len()).sum::<usize>() + self.standalone.len()
    }

    /// Read the libraries' inventories again (`rescan`: scan barcodes) and
    /// look into the standalone drives no job is using.
    pub fn refresh(&self, rescan: bool, identify: Identify) -> Result<(), Error> {
        for lib in &self.libraries {
            lib.refresh(rescan)?;
        }
        for sd in &self.standalone {
            if let Ok(mut g) = sd.state.try_lock() {
                if let Err(e) = sd.look(&mut g, identify) {
                    // a drive that fails doesn't stop the others
                    sd.set(Holds::Unknown);
                    if self.standalone.len() == 1 && self.libraries.is_empty() {
                        return Err(e);
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether a library is still taking in its mail slot ([`Library::settling`]).
    pub fn settling(&self) -> bool {
        self.libraries.iter().any(|l| l.settling())
    }

    /// Tapes a person just put in, as last read: in the libraries' mail
    /// slots (where the robot can reach them), and in the standalone drives.
    pub fn offered(&self) -> Vec<String> {
        let mut out: Vec<String> = self.libraries.iter().flat_map(|l| l.in_mail_slots()).collect();
        out.extend(self.standalone.iter().filter_map(|s| if let Holds::Tape(b) = s.holds() { Some(b) } else { None }));
        out
    }

    /// Whether a library has the tape where its robot can reach it, as last read.
    pub fn reachable(&self, barcode: &str) -> bool {
        self.libraries.iter().any(|l| l.reachable(barcode))
    }

    /// Where every tape is, for people: (barcode, place). With several
    /// libraries, the place says which.
    pub fn locations(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let named = self.libraries.len() > 1 || !self.standalone.is_empty();
        for lib in &self.libraries {
            let inv = lib.inventory();
            for e in inv.all().filter(|e| e.full && e.kind != crate::changer::Kind::Transport) {
                let Some(b) = e.barcode.clone() else { continue };
                if out.iter().any(|(o, _): &(String, String)| o.eq_ignore_ascii_case(&b)) {
                    continue;
                }
                let place = inv.place(e.address);
                out.push((b, if named { format!("{}: {place}", lib.name) } else { place }));
            }
        }
        for sd in &self.standalone {
            if let Holds::Tape(b) = sd.holds() {
                if !out.iter().any(|(o, _)| o.eq_ignore_ascii_case(&b)) {
                    out.push((b, format!("drive {}", sd.name)));
                }
            }
        }
        out
    }

    /// Barcodes in drives now (read first, saving robot moves).
    pub fn loaded(&self) -> Vec<String> {
        let mut out: Vec<String> = self.libraries.iter().flat_map(|l| l.drives.iter().filter_map(|d| d.tape())).collect();
        out.extend(self.standalone.iter().filter_map(|s| if let Holds::Tape(b) = s.holds() { Some(b) } else { None }));
        out
    }

    /// Where a person can put a tape.
    pub fn places(&self) -> Places {
        Places {
            libraries: self.libraries.iter().filter(|l| !l.drives.is_empty()).map(|l| l.name.clone()).collect(),
            drives: self.standalone.iter().map(|s| (s.name.clone(), s.holds())).collect(),
            ejected: None,
        }
    }

    /// Before asking for a tape: when no standalone drive is empty, eject
    /// the tape of the one used least recently (no job is using it), so
    /// the person can put the wanted one in. What came out, if anything.
    pub fn make_room(&self) -> Option<(String, Holds)> {
        if self.standalone.is_empty() || self.standalone.iter().any(|s| s.holds() == Holds::Empty && !s.busy()) {
            return None;
        }
        let mut idle: Vec<(&Standalone, std::sync::MutexGuard<'_, DriveState>)> = self.standalone.iter().filter_map(|s| s.state.try_lock().ok().map(|g| (s, g))).collect();
        idle.sort_by_key(|(_, g)| g.last_used);
        let (sd, mut g) = idle.into_iter().next()?;
        let was = sd.holds();
        sd.eject(&mut g).ok()?;
        Some((sd.name.clone(), was))
    }

    /// The tape `barcode` in a drive, for one job: one that holds it
    /// already, else loaded by a library's robot. Waits while the drive
    /// that has it is busy. [`LoadError::NotInLibrary`] when no library and
    /// no standalone drive has it.
    pub fn acquire(&self, barcode: &str, identify: Identify, cancelled: &dyn Fn() -> bool) -> Result<Lease<'_>, LoadError> {
        loop {
            if cancelled() {
                return Err(LoadError::Cancelled);
            }
            // a library with it in a drive, then one with it anywhere
            let lib = self.libraries.iter().find(|l| l.holds(barcode)).or_else(|| self.libraries.iter().find(|l| l.has(barcode)));
            if let Some(lib) = lib {
                match lib.acquire(barcode, cancelled) {
                    // taken out by hand meanwhile: look everywhere again
                    Err(LoadError::NotInLibrary(_)) => {}
                    r => return r,
                }
            }
            let mut wait = false;
            for sd in &self.standalone {
                let Ok(mut g) = sd.state.try_lock() else {
                    // a job is using it: wait if that is our tape
                    wait |= matches!(sd.holds(), Holds::Tape(b) if b.eq_ignore_ascii_case(barcode));
                    continue;
                };
                let h = sd.look(&mut g, identify)?;
                if matches!(&h, Holds::Tape(b) if b.eq_ignore_ascii_case(barcode)) {
                    // a person mustn't take it out while the job runs
                    let _ = g.drive.prevent_removal(true);
                    return Ok(Lease { barcode: barcode.to_owned(), drive_name: sd.name.clone(), guard: g, standalone: true });
                }
            }
            if !wait {
                return Err(LoadError::NotInLibrary(barcode.to_owned()));
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// Forget what standalone drive `name` holds: it is looked at again
    /// before the next job (after a Format that failed, for one).
    pub fn forget(&self, name: &str) {
        if let Some(sd) = self.standalone.iter().find(|s| s.name == name) {
            *sd.known.lock().unwrap() = None;
            sd.set(Holds::Empty);
        }
    }

    /// Hand the tape to a person: a library moves it to its mail slot, a
    /// standalone drive ejects it.
    pub fn eject(&self, barcode: &str, cancelled: &dyn Fn() -> bool) -> Result<Ejected, LoadError> {
        if let Some(lib) = self.libraries.iter().find(|l| l.has(barcode)) {
            lib.export(barcode, cancelled)?;
            return Ok(Ejected::MailSlot(lib.name.clone()));
        }
        for sd in &self.standalone {
            if matches!(sd.holds(), Holds::Tape(b) if b.eq_ignore_ascii_case(barcode)) {
                let mut g = sd.state.lock().unwrap();
                sd.eject(&mut g)?;
                return Ok(Ejected::Drive(sd.name.clone()));
            }
        }
        Err(LoadError::NotInLibrary(barcode.to_owned()))
    }

    /// Move a tape from a library's mail slot into a free slot.
    pub fn import(&self, barcode: &str) -> Result<(String, u16), LoadError> {
        for lib in &self.libraries {
            lib.refresh(false)?;
            if lib.inventory().find(barcode).is_some_and(|e| e.kind == crate::changer::Kind::MailSlot) {
                return Ok((lib.name.clone(), lib.import(barcode)?));
            }
        }
        Err(LoadError::NotInLibrary(barcode.to_owned()))
    }

    /// Put every tape in the libraries' drives back into the slots, and
    /// eject the tapes of the standalone drives (waits for jobs).
    pub fn unload_all(&self) -> Result<(), Error> {
        for lib in &self.libraries {
            lib.unload_all()?;
        }
        for sd in &self.standalone {
            let mut g = sd.state.lock().unwrap();
            if sd.holds() != Holds::Empty {
                sd.eject(&mut g)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Data, Reply};
    use std::sync::{Arc, Mutex};

    /// A disk: answers INQUIRY as one, records every command it gets.
    struct Disk(Arc<Mutex<Vec<u8>>>);

    impl Transport for Disk {
        fn exec(&mut self, cdb: &[u8], data: Data<'_>, _: u32) -> std::io::Result<Reply> {
            self.0.lock().unwrap().push(cdb[0]);
            if let (0x12, Data::In(b)) = (cdb[0], data) {
                b.fill(b' ');
                b[0] = 0x00; // direct-access block device
                b[4] = 31;
                b[8..12].copy_from_slice(b"DISK");
                return Ok(Reply { status: crate::GOOD, sense: None, transferred: 36 });
            }
            Ok(Reply { status: crate::GOOD, sense: None, transferred: 0 })
        }
        fn describe(&self) -> String {
            r"\\.\PhysicalDrive0".into()
        }
    }

    struct Named {
        changers: Vec<Arc<Mutex<Vec<u8>>>>,
        drives: Vec<Arc<Mutex<Vec<u8>>>>,
    }

    impl Devices for Named {
        fn open_changers(&self) -> std::io::Result<Vec<Box<dyn Transport>>> {
            Ok(self.changers.iter().map(|c| Box::new(Disk(c.clone())) as Box<dyn Transport>).collect())
        }
        fn open_drives(&self) -> std::io::Result<Vec<Box<dyn Transport>>> {
            Ok(self.drives.iter().map(|c| Box::new(Disk(c.clone())) as Box<dyn Transport>).collect())
        }
    }

    /// A disk named as a tape drive or as a changer (a typing mistake in
    /// the settings) gets INQUIRY only, never a tape or changer command.
    #[test]
    fn a_disk_is_never_used_as_a_drive_or_changer() {
        let (as_changer, as_drive) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
        let e = Site::open(&Named { changers: vec![as_changer.clone()], drives: vec![as_drive.clone()] }).err().expect("nothing to use");
        assert!(e.to_string().contains("not a tape drive") || e.to_string().contains("not a medium changer"), "{e}");
        assert_eq!(*as_changer.lock().unwrap(), vec![0x12], "the changer that is a disk: INQUIRY only");
        assert_eq!(*as_drive.lock().unwrap(), vec![0x12], "the drive that is a disk: INQUIRY only");
    }
}
