//! Windows' SCSI pass-through: commands straight to a tape drive
//! (`\\.\Tape0`), a changer with a driver (`\\.\Changer0`), or a changer
//! without one, addressed through its host adapter (`\\.\Scsi2:` with path,
//! target and LUN). Needs administrator rights.

use std::ffi::c_void;
use std::io;
use std::mem::{offset_of, size_of, zeroed};

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::library::Devices;
use crate::{Data, Reply, Transport};

const IOCTL_SCSI_PASS_THROUGH: u32 = 0x0004_D004;
const IOCTL_SCSI_PASS_THROUGH_DIRECT: u32 = 0x0004_D014;
const IOCTL_SCSI_GET_ADDRESS: u32 = 0x0004_1018;
const SCSI_IOCTL_DATA_OUT: u8 = 0;
const SCSI_IOCTL_DATA_IN: u8 = 1;
const SCSI_IOCTL_DATA_UNSPECIFIED: u8 = 2;
const SENSE_LEN: usize = 64;
/// The most data a buffered pass-through (to an adapter) carries.
const BUFFERED_MAX: usize = 64 * 1024;

#[repr(C)]
struct ScsiPassThroughDirect {
    length: u16,
    scsi_status: u8,
    path_id: u8,
    target_id: u8,
    lun: u8,
    cdb_length: u8,
    sense_info_length: u8,
    data_in: u8,
    data_transfer_length: u32,
    timeout_value: u32,
    data_buffer: *mut c_void,
    sense_info_offset: u32,
    cdb: [u8; 16],
}

#[repr(C)]
struct DirectWithSense {
    spt: ScsiPassThroughDirect,
    filler: u32,
    sense: [u8; SENSE_LEN],
}

#[repr(C)]
struct ScsiPassThrough {
    length: u16,
    scsi_status: u8,
    path_id: u8,
    target_id: u8,
    lun: u8,
    cdb_length: u8,
    sense_info_length: u8,
    data_in: u8,
    data_transfer_length: u32,
    timeout_value: u32,
    data_buffer_offset: usize,
    sense_info_offset: u32,
    cdb: [u8; 16],
}

#[repr(C)]
struct BufferedWithSense {
    spt: ScsiPassThrough,
    filler: u32,
    sense: [u8; SENSE_LEN],
    data: [u8; BUFFERED_MAX],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ScsiAddress {
    length: u32,
    port_number: u8,
    path_id: u8,
    target_id: u8,
    lun: u8,
}

/// Where a device sits: adapter (port), bus (path), target, LUN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    pub port: u8,
    pub path: u8,
    pub target: u8,
    pub lun: u8,
}

/// An open device handle.
struct Handle(HANDLE);

unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn open(path: &str) -> io::Result<Handle> {
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let h = unsafe { CreateFileW(wide.as_ptr(), GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(e.kind(), format!("{path}: {e}")));
    }
    Ok(Handle(h))
}

fn ioctl(h: &Handle, code: u32, buf: *mut c_void, len: usize) -> io::Result<()> {
    let mut ret = 0u32;
    let ok = unsafe { DeviceIoControl(h.0, code, buf, len as u32, buf, len as u32, &mut ret, std::ptr::null_mut()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A buffer aligned for any adapter's DMA.
struct Aligned {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

// owned memory, used by one thread at a time
unsafe impl Send for Aligned {}

impl Aligned {
    fn new(len: usize) -> Aligned {
        let layout = std::alloc::Layout::from_size_align(len.max(1), 4096).unwrap();
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Aligned { ptr, layout }
    }
    fn slice(&mut self, len: usize) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, len) }
    }
}

impl Drop for Aligned {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

fn finish(status: u8, sense: &[u8], sense_len: u8, transferred: usize) -> Reply {
    let sense = if status != crate::GOOD { crate::Sense::parse(&sense[..(sense_len as usize).min(sense.len())]) } else { None };
    Reply { status, sense, transferred }
}

/// A device opened by its own name (`\\.\Tape0`, `\\.\Changer0`).
pub struct DeviceTransport {
    h: Handle,
    name: String,
    buf: Option<Aligned>,
}

impl DeviceTransport {
    pub fn open(path: &str) -> io::Result<DeviceTransport> {
        Ok(DeviceTransport { h: open(path)?, name: path.to_owned(), buf: None })
    }

    /// Where the device sits on its adapter.
    pub fn address(&self) -> io::Result<Address> {
        address_of(&self.h)
    }
}

fn address_of(h: &Handle) -> io::Result<Address> {
    let mut a = ScsiAddress { length: size_of::<ScsiAddress>() as u32, ..Default::default() };
    ioctl(h, IOCTL_SCSI_GET_ADDRESS, &mut a as *mut _ as *mut c_void, size_of::<ScsiAddress>())?;
    Ok(Address { port: a.port_number, path: a.path_id, target: a.target_id, lun: a.lun })
}

impl Transport for DeviceTransport {
    fn exec(&mut self, cdb: &[u8], data: Data<'_>, timeout_s: u32) -> io::Result<Reply> {
        let len = match &data {
            Data::None => 0,
            Data::In(b) => b.len(),
            Data::Out(b) => b.len(),
        };
        // one aligned buffer, kept for the next command of the same size or smaller
        if self.buf.as_ref().is_none_or(|b| b.layout.size() < len) {
            self.buf = Some(Aligned::new(len));
        }
        let buf = self.buf.as_mut().unwrap();
        if let Data::Out(b) = &data {
            buf.slice(len).copy_from_slice(b);
        }
        let mut s: DirectWithSense = unsafe { zeroed() };
        s.spt.length = size_of::<ScsiPassThroughDirect>() as u16;
        s.spt.cdb_length = cdb.len() as u8;
        s.spt.cdb[..cdb.len()].copy_from_slice(cdb);
        s.spt.sense_info_length = SENSE_LEN as u8;
        s.spt.sense_info_offset = offset_of!(DirectWithSense, sense) as u32;
        s.spt.data_in = match &data {
            Data::None => SCSI_IOCTL_DATA_UNSPECIFIED,
            Data::In(_) => SCSI_IOCTL_DATA_IN,
            Data::Out(_) => SCSI_IOCTL_DATA_OUT,
        };
        s.spt.data_transfer_length = len as u32;
        s.spt.data_buffer = if len > 0 { buf.ptr as *mut c_void } else { std::ptr::null_mut() };
        s.spt.timeout_value = timeout_s;
        ioctl(&self.h, IOCTL_SCSI_PASS_THROUGH_DIRECT, &mut s as *mut _ as *mut c_void, size_of::<DirectWithSense>()).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", self.name)))?;
        let got = (s.spt.data_transfer_length as usize).min(len);
        if let Data::In(b) = data {
            b[..got].copy_from_slice(&buf.slice(len)[..got]);
        }
        Ok(finish(s.spt.scsi_status, &s.sense, s.spt.sense_info_length, got))
    }

    fn describe(&self) -> String {
        self.name.clone()
    }
}

/// A logical unit reached through its host adapter (`\\.\Scsi2:`), for a
/// changer that has no driver of its own.
pub struct AdapterTransport {
    h: Handle,
    at: Address,
}

impl AdapterTransport {
    pub fn open(at: Address) -> io::Result<AdapterTransport> {
        Ok(AdapterTransport { h: open(&format!("\\\\.\\Scsi{}:", at.port))?, at })
    }
}

impl Transport for AdapterTransport {
    fn exec(&mut self, cdb: &[u8], data: Data<'_>, timeout_s: u32) -> io::Result<Reply> {
        let len = match &data {
            Data::None => 0,
            Data::In(b) => b.len(),
            Data::Out(b) => b.len(),
        };
        if len > BUFFERED_MAX {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "too much data for a command through the adapter"));
        }
        let mut s: Box<BufferedWithSense> = unsafe { Box::new(zeroed()) };
        s.spt.length = size_of::<ScsiPassThrough>() as u16;
        s.spt.path_id = self.at.path;
        s.spt.target_id = self.at.target;
        s.spt.lun = self.at.lun;
        s.spt.cdb_length = cdb.len() as u8;
        s.spt.cdb[..cdb.len()].copy_from_slice(cdb);
        s.spt.sense_info_length = SENSE_LEN as u8;
        s.spt.sense_info_offset = offset_of!(BufferedWithSense, sense) as u32;
        s.spt.data_buffer_offset = offset_of!(BufferedWithSense, data);
        s.spt.data_in = match &data {
            Data::None => SCSI_IOCTL_DATA_UNSPECIFIED,
            Data::In(_) => SCSI_IOCTL_DATA_IN,
            Data::Out(_) => SCSI_IOCTL_DATA_OUT,
        };
        s.spt.data_transfer_length = len as u32;
        s.spt.timeout_value = timeout_s;
        if let Data::Out(b) = &data {
            s.data[..len].copy_from_slice(b);
        }
        let size = offset_of!(BufferedWithSense, data) + len;
        ioctl(&self.h, IOCTL_SCSI_PASS_THROUGH, &mut *s as *mut _ as *mut c_void, size).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", self.describe())))?;
        let got = (s.spt.data_transfer_length as usize).min(len);
        if let Data::In(b) = data {
            b[..got].copy_from_slice(&s.data[..got]);
        }
        Ok(finish(s.spt.scsi_status, &s.sense, s.spt.sense_info_length, got))
    }

    fn describe(&self) -> String {
        format!("scsi:{}:{}:{}:{}", self.at.port, self.at.path, self.at.target, self.at.lun)
    }
}

/// Parse `scsi:<port>:<path>:<target>:<lun>`.
pub fn parse_address(s: &str) -> Option<Address> {
    let rest = s.strip_prefix("scsi:")?;
    let n: Vec<u8> = rest.split(':').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
    match n[..] {
        [port, path, target, lun] => Some(Address { port, path, target, lun }),
        _ => None,
    }
}

/// Open a device by a name the user can give: `\\.\Tape0`, `\\.\Changer0`
/// or `scsi:2:0:3:1`.
pub fn open_any(name: &str) -> io::Result<Box<dyn Transport>> {
    match parse_address(name) {
        Some(at) => Ok(Box::new(AdapterTransport::open(at)?)),
        None => Ok(Box::new(DeviceTransport::open(name)?)),
    }
}

/// The library's devices as Windows sees them, found by themselves unless
/// named.
#[derive(Debug, Clone, Default)]
pub struct WindowsDevices {
    /// The changer (`\\.\Changer0` or `scsi:2:0:3:1`); empty: find it.
    pub changer: String,
    /// The tape drives (`\\.\Tape0`…); empty: every one there is.
    pub drives: Vec<String>,
}

/// Tape devices that exist: `\\.\Tape0` … `\\.\Tape31`.
pub fn tape_names() -> Vec<String> {
    (0..32).map(|i| format!("\\\\.\\Tape{i}")).filter(|n| DeviceTransport::open(n).is_ok()).collect()
}

fn is_changer(t: &mut dyn Transport) -> bool {
    crate::inquiry(t).is_ok_and(|i| i.device_type == crate::TYPE_CHANGER)
}

impl WindowsDevices {
    /// Look for a changer: by its driver's name, then next to the tape
    /// drives on their adapters (the TS3100's changer is LUN 1 of the first
    /// drive's target). Every place tried, and what it was, for `ltocat devices`.
    pub fn find_changer(&self) -> (Option<String>, Vec<String>) {
        let mut tried = Vec::new();
        for i in 0..16 {
            let n = format!("\\\\.\\Changer{i}");
            if let Ok(mut t) = DeviceTransport::open(&n) {
                if is_changer(&mut t) {
                    return (Some(n), tried);
                }
                tried.push(format!("{n}: not a changer"));
            }
        }
        let tapes = if self.drives.is_empty() { tape_names() } else { self.drives.clone() };
        let mut seen = Vec::new();
        for n in tapes {
            let Ok(t) = DeviceTransport::open(&n) else { continue };
            let a = match t.address() {
                Ok(a) => a,
                Err(e) => {
                    tried.push(format!("{n}: no SCSI address ({e})"));
                    continue;
                }
            };
            drop(t);
            for lun in 0..8u8 {
                let at = Address { lun, ..a };
                if at == a || seen.contains(&at) {
                    continue;
                }
                seen.push(at);
                match AdapterTransport::open(at) {
                    Ok(mut t) => {
                        if is_changer(&mut t) {
                            return (Some(t.describe()), tried);
                        }
                    }
                    Err(e) => {
                        tried.push(format!("scsi:{}: {e}", at.port));
                        break;
                    }
                }
            }
        }
        (None, tried)
    }
}

impl Devices for WindowsDevices {
    fn open_changer(&self) -> io::Result<Box<dyn Transport>> {
        if !self.changer.is_empty() {
            return open_any(&self.changer);
        }
        match self.find_changer() {
            (Some(n), _) => open_any(&n),
            (None, tried) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no tape library (medium changer) was found{}", if tried.is_empty() { String::new() } else { format!(": {}", tried.join("; ")) }),
            )),
        }
    }

    fn open_drives(&self) -> io::Result<Vec<Box<dyn Transport>>> {
        let names = if self.drives.is_empty() { tape_names() } else { self.drives.clone() };
        let mut out: Vec<Box<dyn Transport>> = Vec::new();
        for n in names {
            match DeviceTransport::open(&n) {
                Ok(t) => out.push(Box::new(t)),
                Err(e) if !self.drives.is_empty() => return Err(e),
                Err(_) => {}
            }
        }
        if out.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no tape drive was found (\\\\.\\Tape0 …); is the tape driver installed, and does the program run as administrator?"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_windows() {
        // sizes from the Windows SDK, x64
        assert_eq!(size_of::<ScsiPassThroughDirect>(), 56);
        assert_eq!(size_of::<ScsiPassThrough>(), 56);
        assert_eq!(offset_of!(ScsiPassThroughDirect, data_buffer), 24);
        assert_eq!(offset_of!(ScsiPassThroughDirect, cdb), 36);
        assert_eq!(size_of::<ScsiAddress>(), 8);
    }

    #[test]
    fn addresses() {
        assert_eq!(parse_address("scsi:2:0:3:1"), Some(Address { port: 2, path: 0, target: 3, lun: 1 }));
        assert_eq!(parse_address("\\\\.\\Changer0"), None);
    }
}
