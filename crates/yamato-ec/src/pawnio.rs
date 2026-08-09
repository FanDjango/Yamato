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

/// Module that permits the ACPI EC ports. Shipped alongside us, unmodified,
/// under LGPL-2.1-or-later. See NOTICE.md.
pub const MODULE_FILE: &str = "LpcACPIEC.bin";

/// Ports the stock module allows. Anything else is refused, and the refusal
/// reads back as 0xff, which is indistinguishable from a stuck EC.
const EC_STATUS_PORT: u16 = 0x66;
const EC_DATA_PORT: u16 = 0x62;

#[derive(Debug)]
pub enum Error {
    /// The driver is not installed or not running. The one case where we offer
    /// a download.
    DriverUnavailable,
    /// Found the driver but could not open it. Almost always means we are not
    /// elevated.
    AccessDenied,
    /// The module file is not next to the executable.
    ModuleMissing(PathBuf),
    /// The driver rejected the module. A damaged or unsigned blob does this.
    ModuleRejected(u32),
    /// A call into a loaded module failed.
    Call { function: &'static str, code: u32 },
    /// Port outside what the loaded module permits.
    PortNotPermitted(u16),
    /// Another tool is mid-transaction on the controller and did not finish in
    /// time. Refusing is safer than interleaving with it.
    Busy,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::DriverUnavailable => write!(
                f,
                "PawnIO is not installed or not running. Yamato needs it to reach the \
                 embedded controller."
            ),
            Error::AccessDenied => write!(
                f,
                "PawnIO refused the connection. Reaching the embedded controller needs \
                 administrator rights."
            ),
            Error::ModuleMissing(p) => {
                write!(f, "{} is missing. Expected it at {}", MODULE_FILE, p.display())
            }
            Error::ModuleRejected(code) => write!(
                f,
                "PawnIO rejected {} (error {}). The file may be damaged.",
                MODULE_FILE, code
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
        }
    }
}

impl std::error::Error for Error {}

/// An open handle to PawnIO with our module loaded.
pub struct PawnIo {
    device: HANDLE,
}

// The handle is ours alone and every call is a synchronous ioctl.
unsafe impl Send for PawnIo {}

impl PawnIo {
    /// Opens the driver and loads the EC module sitting next to the executable.
    pub fn open() -> Result<Self, Error> {
        let module_path = module_path()?;
        let module = std::fs::read(&module_path)
            .map_err(|_| Error::ModuleMissing(module_path.clone()))?;

        let device = open_device()?;
        let this = PawnIo { device };
        this.load_module(&module)?;

        Ok(this)
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
            return Err(Error::ModuleRejected(unsafe { GetLastError() }));
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

    /// True for ports the stock module permits. Ask before probing: a refused
    /// read returns 0xff, which looks like an EC with both status bits stuck
    /// set, and costs a full timeout.
    pub fn port_permitted(port: u16) -> bool {
        matches!(port, EC_STATUS_PORT | EC_DATA_PORT)
    }

    pub fn read_port(&self, port: u16) -> Result<u8, Error> {
        if !Self::port_permitted(port) {
            return Err(Error::PortNotPermitted(port));
        }

        let mut out = [0u64; 1];
        self.execute("ioctl_pio_read", &[port as u64], &mut out)?;

        Ok(out[0] as u8)
    }

    pub fn write_port(&self, port: u16, value: u8) -> Result<(), Error> {
        if !Self::port_permitted(port) {
            return Err(Error::PortNotPermitted(port));
        }

        self.execute("ioctl_pio_write", &[port as u64, value as u64], &mut [])
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
fn module_path() -> Result<PathBuf, Error> {
    let exe = std::env::current_exe().map_err(|_| Error::ModuleMissing(PathBuf::from(MODULE_FILE)))?;
    let dir: &Path = exe.parent().unwrap_or_else(|| Path::new("."));

    Ok(dir.join(MODULE_FILE))
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
    fn only_the_acpi_ec_ports_are_permitted() {
        assert!(PawnIo::port_permitted(0x62));
        assert!(PawnIo::port_permitted(0x66));
        // The H8S window some ThinkPads expose. The stock module refuses it.
        assert!(!PawnIo::port_permitted(0x1600));
        assert!(!PawnIo::port_permitted(0x1610));
    }
}
