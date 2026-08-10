// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein
//
// Talks to the PawnIO driver directly over DeviceIoControl. PawnIOLib would be
// the other route, but it is x64 only and LGPL. The IOCTL interface is the
// documented boundary and PawnIO's license has an explicit exception for it.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
    HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::ec::Probe;
use crate::version::{self, DriverVersion, MIN_DRIVER_VERSION};

const DEVICE_PATH: &str = r"\\?\GLOBALROOT\Device\PawnIO";

/// PawnIO's device type. Function codes are its own, not Microsoft-assigned.
const DEVICE_TYPE: u32 = 41394;

const METHOD_BUFFERED: u32 = 0;
const FILE_ANY_ACCESS: u32 = 0;

const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

const IOCTL_LOAD_BINARY: u32 = ctl_code(DEVICE_TYPE, 0x821, METHOD_BUFFERED, FILE_ANY_ACCESS);
const IOCTL_EXECUTE_FN: u32 = ctl_code(DEVICE_TYPE, 0x841, METHOD_BUFFERED, FILE_ANY_ACCESS);

/// Execute payload is a fixed-width NUL-padded name followed by u64 cells.
const FN_NAME_LEN: usize = 32;

/// Every module Yamato ships. Both travel with the executable whatever layout
/// the machine turns out to have, so an install can be checked for
/// completeness without opening the driver. Shipped unmodified, under
/// LGPL-2.1-or-later. See NOTICE.md.
pub const MODULE_FILES: [&str; 2] =
    [Layout::Standard.module_file(), Layout::Alternate.module_file()];

/// The two port layouts ThinkPads keep their embedded controller at, and
/// everything that differs between them.
///
/// Most machines put the ACPI EC at the specified 0x62/0x66. P53-class
/// machines put it at 0x1600/0x1604 instead, inside an LPC base address
/// window, and answer nothing at the standard ports. One PawnIO module cannot
/// serve both: the stock `LpcACPIEC` whitelists exactly 0x62 and 0x66, and
/// `LpcIO` discovers BAR windows at runtime but discards anything below
/// 0x100, so it can never permit the standard pair. Two modules, one layout
/// each, chosen by probing.
///
/// The command set is the same on both. Only the addresses and the module
/// differ, which is why the handshake in ec.rs is written once and
/// parameterized on this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// The ACPI-specified EC ports, 0x62 data and 0x66 status, through the
    /// stock `LpcACPIEC` module. The path that has been in production.
    Standard,
    /// The P53-class window, 0x1600 data and 0x1604 status, through `LpcIO`.
    Alternate,
}

impl Layout {
    /// The module file that permits this layout's ports, looked for beside
    /// the executable.
    pub const fn module_file(self) -> &'static str {
        match self {
            Layout::Standard => "LpcACPIEC.bin",
            Layout::Alternate => "LpcIO.bin",
        }
    }

    pub const fn status_port(self) -> u16 {
        match self {
            Layout::Standard => 0x66,
            Layout::Alternate => 0x1604,
        }
    }

    pub const fn data_port(self) -> u16 {
        match self {
            Layout::Standard => 0x62,
            Layout::Alternate => 0x1600,
        }
    }

    /// The module entry points for a byte in and a byte out. Same input and
    /// output shapes on both modules, different names, and a name the module
    /// does not export fails the call outright.
    const fn read_fn(self) -> &'static str {
        match self {
            Layout::Standard => "ioctl_pio_read",
            Layout::Alternate => "ioctl_pio_inb",
        }
    }

    const fn write_fn(self) -> &'static str {
        match self {
            Layout::Standard => "ioctl_pio_write",
            Layout::Alternate => "ioctl_pio_outb",
        }
    }

    /// Whether driving the EC over this layout also has to hold LpcIO's own
    /// cross-process mutex. The module documents one for all its operations,
    /// and OpenRGB and LibreHardwareMonitor honor it, so on the alternate
    /// path both that and the EC lock apply.
    pub(crate) const fn uses_isa_lock(self) -> bool {
        matches!(self, Layout::Alternate)
    }

    /// The ports this layout is allowed to touch: its own two and nothing
    /// else. LpcIO itself permits every discovered BAR window, which on a
    /// P53-class machine is dozens of ports, but nothing in this crate has
    /// any business outside the EC pair, so the narrower rule is enforced
    /// here before a request ever reaches the driver.
    pub fn permits(self, port: u16) -> bool {
        port == self.status_port() || port == self.data_port()
    }

    /// One line for logs and error messages. Names the ports and the module,
    /// which is what a bug report from a machine nobody can test needs to
    /// contain. "Compatibility mode" is the name the settings row uses for
    /// the alternate layout, so a log line and the control it points at
    /// cannot drift apart.
    pub const fn describe(self) -> &'static str {
        match self {
            Layout::Standard => "standard mode (EC at 0x62/0x66 via LpcACPIEC)",
            Layout::Alternate => "compatibility mode (EC at 0x1600/0x1604 via LpcIO)",
        }
    }
}

#[derive(Debug)]
pub enum Error {
    /// The driver is not installed or not running. One of the two cases where
    /// we offer a download.
    DriverUnavailable,
    /// The driver is installed, its file version was readable, and it is
    /// older than Yamato can safely drive. The other case where the fix is a
    /// download, so the message points at the same page. Never produced for
    /// a version that merely could not be read: see version::verdict.
    DriverTooOld { found: DriverVersion },
    /// Found the driver but could not open it. Almost always means we are not
    /// elevated.
    AccessDenied,
    /// The module file is not next to the executable.
    ModuleMissing(PathBuf),
    /// The driver rejected the module. A damaged or unsigned blob does this.
    ModuleRejected { file: &'static str, code: u32 },
    /// A call into a loaded module failed.
    Call { function: &'static str, code: u32 },
    /// Port outside what the loaded module permits.
    PortNotPermitted(u16),
    /// Another tool is mid-transaction on the controller and did not finish in
    /// time. Refusing is safer than interleaving with it.
    Busy,
    /// No controller handle could be produced at all. Deliberately not
    /// returned for a machine that merely failed validation: that machine
    /// starts on a fallback layout and lets the engine retry, see Ec::open.
    /// Carries both probes, because the useful diagnostic from a machine
    /// nobody can test is what each layout returned, not just that both
    /// failed.
    NoController(Vec<Probe>),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::DriverUnavailable => write!(
                f,
                "PawnIO is not installed or not running. Yamato needs it to reach the \
                 embedded controller."
            ),
            Error::DriverTooOld { found } => write!(
                f,
                "PawnIO {} is installed, and Yamato needs {} or later: older versions \
                 can crash some Windows 10 machines while reaching the embedded \
                 controller. Update PawnIO from https://pawnio.eu.",
                found, MIN_DRIVER_VERSION
            ),
            Error::AccessDenied => write!(
                f,
                "PawnIO refused the connection. Reaching the embedded controller needs \
                 administrator rights."
            ),
            Error::ModuleMissing(p) => {
                write!(f, "{} is missing. It is installed alongside Yamato.", p.display())
            }
            Error::ModuleRejected { file, code } => write!(
                f,
                "PawnIO rejected {} (error {}). The file may be damaged.",
                file, code
            ),
            Error::Call { function, code } => {
                write!(f, "PawnIO call {} failed with error {}", function, code)
            }
            Error::PortNotPermitted(port) => write!(
                f,
                "port {:#06x} is not permitted by the loaded PawnIO module",
                port
            ),
            Error::Busy => write!(
                f,
                "another program is using the embedded controller and did not finish in time"
            ),
            Error::NoController(probes) => {
                write!(f, "cannot reach the embedded controller at either port layout.")?;
                for probe in probes {
                    write!(f, " {}.", probe)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Error {}

/// An open handle to PawnIO with one layout's module loaded.
///
/// Each handle carries its own module instance inside the driver, so two of
/// these can exist at once during probing without either seeing the other's
/// state.
pub struct PawnIo {
    device: HANDLE,
    layout: Layout,
}

// The handle is ours alone and every call is a synchronous ioctl.
unsafe impl Send for PawnIo {}

impl PawnIo {
    /// Opens the driver and loads the layout's module sitting next to the
    /// executable.
    pub fn open_module(layout: Layout) -> Result<Self, Error> {
        let module_path = module_path(layout.module_file())?;
        let module = std::fs::read(&module_path)
            .map_err(|_| Error::ModuleMissing(module_path.clone()))?;

        let device = open_device()?;
        let this = PawnIo { device, layout };

        // Only after the driver opened, so an absent driver reports as
        // absent rather than as a version problem it does not have. Checked
        // before any module runs, because the crash the floor exists to stop
        // happens on port access.
        version::enforce_minimum()?;

        this.load_module(&module)?;

        Ok(this)
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// What LpcIO requires before it will touch a port, in the order it
    /// requires it: pick the SuperIO register pair, then walk the logical
    /// devices for base address windows. Port access before both returns
    /// STATUS_DEVICE_NOT_READY. On the standard layout there is nothing to
    /// prepare and this is a no-op.
    ///
    /// Slot 1, the 0x4e/0x4f pair, not slot 0. Measured on real hardware:
    /// slot 0 answers with chip ID 0xff, nothing there, while slot 1 carries
    /// the chip whose BARs include the 0x1600 window. Both EC ports sit in
    /// that one 8-byte window, so one discovery covers the pair.
    ///
    /// The caller must hold the EC lock, which on this layout includes the
    /// ISA mutex the module documents for these calls.
    pub(crate) fn prepare(&self) -> Result<(), Error> {
        if self.layout != Layout::Alternate {
            return Ok(());
        }

        self.execute("ioctl_select_slot", &[1], &mut [])?;
        self.execute("ioctl_find_bars", &[], &mut [])
    }

    fn load_module(&self, blob: &[u8]) -> Result<(), Error> {
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.device,
                IOCTL_LOAD_BINARY,
                blob.as_ptr() as *const c_void,
                blob.len() as u32,
                ptr::null_mut(),
                0,
                &mut returned,
                ptr::null_mut(),
            )
        };

        if ok == 0 {
            return Err(Error::ModuleRejected {
                file: self.layout.module_file(),
                code: unsafe { GetLastError() },
            });
        }

        Ok(())
    }

    /// Calls a module entry point. `input` and `output` are u64 cells, which is
    /// how the Pawn VM sees everything.
    fn execute(
        &self,
        function: &'static str,
        input: &[u64],
        output: &mut [u64],
    ) -> Result<(), Error> {
        debug_assert!(function.len() < FN_NAME_LEN);

        let mut payload = vec![0u8; FN_NAME_LEN + input.len() * 8];
        payload[..function.len()].copy_from_slice(function.as_bytes());
        for (i, cell) in input.iter().enumerate() {
            let at = FN_NAME_LEN + i * 8;
            payload[at..at + 8].copy_from_slice(&cell.to_le_bytes());
        }

        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.device,
                IOCTL_EXECUTE_FN,
                payload.as_ptr() as *const c_void,
                payload.len() as u32,
                output.as_mut_ptr() as *mut c_void,
                (output.len() * 8) as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };

        if ok == 0 {
            return Err(Error::Call {
                function,
                code: unsafe { GetLastError() },
            });
        }

        Ok(())
    }

    pub fn read_port(&self, port: u16) -> Result<u8, Error> {
        // Ask before probing: a refused read costs a driver round trip and, on
        // the standard module, reads back as 0xff, which looks like an EC with
        // both status bits stuck set and costs a full timeout.
        if !self.layout.permits(port) {
            return Err(Error::PortNotPermitted(port));
        }

        let mut out = [0u64; 1];
        self.execute(self.layout.read_fn(), &[port as u64], &mut out)?;

        Ok(out[0] as u8)
    }

    pub fn write_port(&self, port: u16, value: u8) -> Result<(), Error> {
        if !self.layout.permits(port) {
            return Err(Error::PortNotPermitted(port));
        }

        self.execute(self.layout.write_fn(), &[port as u64, value as u64], &mut [])
    }
}

impl Drop for PawnIo {
    fn drop(&mut self) {
        if self.device != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.device) };
        }
    }
}

fn open_device() -> Result<HANDLE, Error> {
    let wide: Vec<u16> = std::ffi::OsStr::new(DEVICE_PATH)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0xC000_0000, // GENERIC_READ | GENERIC_WRITE
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        let code = unsafe { GetLastError() };

        return Err(match code {
            ERROR_ACCESS_DENIED => Error::AccessDenied,
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Error::DriverUnavailable,
            _ => Error::DriverUnavailable,
        });
    }

    Ok(handle)
}

/// Next to the executable, not the working directory. Anything launched from a
/// run key or by the service manager starts somewhere else.
fn module_path(file: &str) -> Result<PathBuf, Error> {
    let exe = std::env::current_exe().map_err(|_| Error::ModuleMissing(PathBuf::from(file)))?;
    let dir: &Path = exe.parent().unwrap_or_else(|| Path::new("."));

    Ok(dir.join(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_codes_match_pawnio() {
        // Verified against a working C++ client.
        assert_eq!(IOCTL_LOAD_BINARY, 0xA1B2_2084);
        assert_eq!(IOCTL_EXECUTE_FN, 0xA1B2_2104);
    }

    #[test]
    fn the_too_old_error_names_the_version_and_the_fix() {
        // Whoever reads this error is going to a download page to look for
        // a number, so both numbers and the page have to be in it.
        let message = Error::DriverTooOld { found: DriverVersion::new(2, 0, 0, 0) }.to_string();

        assert!(message.contains("2.0.0"), "{message}");
        assert!(message.contains("2.2.0"), "{message}");
        assert!(message.contains("https://pawnio.eu"), "{message}");
    }

    #[test]
    fn each_layout_permits_its_own_ports_and_nothing_else() {
        // The standard pair, and not the H8S window.
        assert!(Layout::Standard.permits(0x62));
        assert!(Layout::Standard.permits(0x66));
        assert!(!Layout::Standard.permits(0x1600));
        assert!(!Layout::Standard.permits(0x1604));

        // The other way around on the alternate layout. LpcIO itself would
        // permit every BAR window it found; this crate does not.
        assert!(Layout::Alternate.permits(0x1600));
        assert!(Layout::Alternate.permits(0x1604));
        assert!(!Layout::Alternate.permits(0x62));
        assert!(!Layout::Alternate.permits(0x66));
        // Inside a real discovered window, still not the EC pair, still no.
        assert!(!Layout::Alternate.permits(0x1610));
    }

    #[test]
    fn the_layouts_disagree_only_where_the_hardware_does() {
        // Ports and entry point names differ; both must, together. Crossing
        // them, the standard ioctl names against the alternate ports or the
        // reverse, is the mistake this pins down: the call would not fail
        // loudly, it would fail as an unknown function or a refused port at
        // the first tick.
        assert_eq!(Layout::Standard.read_fn(), "ioctl_pio_read");
        assert_eq!(Layout::Standard.write_fn(), "ioctl_pio_write");
        assert_eq!(Layout::Alternate.read_fn(), "ioctl_pio_inb");
        assert_eq!(Layout::Alternate.write_fn(), "ioctl_pio_outb");

        assert_eq!(Layout::Standard.module_file(), "LpcACPIEC.bin");
        assert_eq!(Layout::Alternate.module_file(), "LpcIO.bin");

        // Only the LpcIO path takes the module's own mutex.
        assert!(!Layout::Standard.uses_isa_lock());
        assert!(Layout::Alternate.uses_isa_lock());
    }
}
