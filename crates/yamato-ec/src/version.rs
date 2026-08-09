// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein
//
// Which PawnIO is actually installed, and the floor Yamato holds it to.
//
// PawnIO before 2.2.0 can blue-screen Windows 10 1809, 19H1 and 19H2 on port
// access, and 1809 is a version Yamato supports. The floor is 2.2.0 on every
// Windows build, not just the ones that crash: one rule is simpler than a
// build check, and there is no reason to run an older PawnIO anywhere.
//
// The version measured is a *file* version, deliberately. PawnIO also
// reports a version of its own through its API, and that is the wrong
// signal: it tracks the API, not the release, and has answered 2.0.0 ever
// since 2.0.0. Gating on it would refuse a machine with a current 2.2.0
// correctly installed. Measured on exactly such a machine: every file
// carries 2.2.0, the API answers 2.0.0.

use std::ffi::c_void;
use std::fmt;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FFI_SIGNATURE,
    VS_FIXEDFILEINFO,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

use crate::pawnio::Error;

/// The oldest PawnIO Yamato will drive. Below this, reaching the embedded
/// controller can blue-screen Windows 10 1809, 19H1 and 19H2, and 1809 is a
/// version Yamato supports.
pub const MIN_DRIVER_VERSION: DriverVersion = DriverVersion::new(2, 2, 0, 0);

/// Where the driver's service registration lives. The same key the installer
/// and the tray check, so nothing disagrees about what "installed" means.
const SERVICE_KEY: &str = r"SYSTEM\CurrentControlSet\Services\PawnIO";

/// Where PawnIO's own installer records itself, which is the way to its
/// install directory when the driver binary cannot be read.
const UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO";

/// A four-part Windows file version, ordered the way versions order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DriverVersion([u16; 4]);

impl DriverVersion {
    /// The four fields of a VS_FIXEDFILEINFO, most significant first.
    pub const fn new(major: u16, minor: u16, build: u16, revision: u16) -> Self {
        DriverVersion([major, minor, build, revision])
    }
}

impl fmt::Display for DriverVersion {
    /// Three parts, as PawnIO's releases are numbered; the fourth only when
    /// it carries anything. "2.2.0.0" in an error message reads like a
    /// different number than the "2.2.0" on the download page.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, build, revision] = self.0;
        write!(f, "{major}.{minor}.{build}")?;
        if revision != 0 {
            write!(f, ".{revision}")?;
        }

        Ok(())
    }
}

/// The installed PawnIO's file version, and the file it was read from.
///
/// The driver binary first: PawnIO.sys is the file the old crash lives in,
/// so it is the most correct thing to measure, and its location comes from
/// the service registration. Failing that, PawnIOLib.dll out of PawnIO's
/// install directory, which carries the same release version.
pub fn installed_driver_version() -> Option<(DriverVersion, PathBuf)> {
    candidates()
        .into_iter()
        .find_map(|path| file_version(&path).map(|version| (version, path)))
}

/// One clause for the startup log: what is installed and where that was
/// read, or the fact that no version could be read and the floor therefore
/// went unchecked.
pub fn driver_version_report() -> String {
    match installed_driver_version() {
        Some((version, path)) => format!("PawnIO {} (from {})", version, path.display()),
        None => format!(
            "PawnIO's file version could not be read, so the {MIN_DRIVER_VERSION} floor \
             was not checked"
        ),
    }
}

/// Refuses a PawnIO that is readable and below the floor. Called after the
/// driver has been opened, so an absent driver reports as absent rather than
/// as a version problem it does not have.
pub(crate) fn enforce_minimum() -> Result<(), Error> {
    verdict(installed_driver_version().map(|(version, _)| version))
}

/// The rule itself, separated from the reading so it can be pinned down.
fn verdict(read: Option<DriverVersion>) -> Result<(), Error> {
    match read {
        Some(found) if found < MIN_DRIVER_VERSION => Err(Error::DriverTooOld { found }),
        // No version is not evidence of an old one. Refusing here would
        // strand a working machine over a failed registry read, so the
        // machine runs, and the startup log says the check did not happen.
        _ => Ok(()),
    }
}

/// The files worth asking, most authoritative first.
fn candidates() -> Vec<PathBuf> {
    let mut found = Vec::new();

    if let Some(raw) = registry_string(SERVICE_KEY, "ImagePath") {
        found.push(expand_image_path(&raw, &windows_dir()));
    }

    let install_dir = registry_string(UNINSTALL_KEY, "InstallLocation")
        .map(PathBuf::from)
        .unwrap_or_else(default_install_dir);
    found.push(install_dir.join("PawnIOLib.dll"));

    found
}

/// A service ImagePath as the file system can open it.
///
/// The value usually reads `\SystemRoot\System32\DriverStore\...`, which is
/// the kernel's spelling of the Windows directory and not a path CreateFile
/// understands. Kernel image paths also come as `\??\C:\...` and,
/// historically, bare and relative to the Windows directory, so all three
/// forms are translated.
fn expand_image_path(raw: &str, windows_dir: &Path) -> PathBuf {
    let lower = raw.to_ascii_lowercase();

    if lower.starts_with(r"\systemroot\") {
        return windows_dir.join(&raw[r"\SystemRoot\".len()..]);
    }

    if lower.starts_with(r"\??\") {
        return PathBuf::from(&raw[r"\??\".len()..]);
    }

    if lower.starts_with(r"system32\") {
        return windows_dir.join(raw);
    }

    PathBuf::from(raw)
}

/// From the environment rather than an API call, because the variable is set
/// for every process, services included, and it keeps this file inside the
/// features the crate already needs.
fn windows_dir() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

/// Where PawnIO installs when nothing says otherwise.
fn default_install_dir() -> PathBuf {
    std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
        .join("PawnIO")
}

/// A string value under HKLM, or None for any failure at all. Failures are
/// not told apart, because an unreadable value and an absent one call for
/// the same behavior upstream.
fn registry_string(subkey: &str, value: &str) -> Option<String> {
    let subkey_w = wide(subkey);
    let value_w = wide(value);

    // Size first, in bytes. RRF_RT_REG_SZ also accepts REG_EXPAND_SZ and
    // expands any environment strings on the way through.
    let mut bytes: u32 = 0;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey_w.as_ptr(),
            value_w.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut bytes,
        )
    };

    if status != ERROR_SUCCESS || bytes < 2 {
        return None;
    }

    let mut buffer = vec![0u16; bytes.div_ceil(2) as usize];
    let mut got = bytes;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey_w.as_ptr(),
            value_w.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut got,
        )
    };

    if status != ERROR_SUCCESS {
        return None;
    }

    // Terminated by the API; keep what comes before the NUL.
    let text: Vec<u16> = buffer.into_iter().take_while(|&c| c != 0).collect();

    Some(String::from_utf16_lossy(&text))
}

/// The file's fixed version resource: the language-independent numbers that
/// the properties dialog's FileVersion is rendered from.
fn file_version(path: &Path) -> Option<DriverVersion> {
    let name: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut ignored: u32 = 0;
    let size = unsafe { GetFileVersionInfoSizeW(name.as_ptr(), &mut ignored) };
    if size == 0 {
        return None;
    }

    let mut data = vec![0u8; size as usize];
    let ok = unsafe { GetFileVersionInfoW(name.as_ptr(), 0, size, data.as_mut_ptr().cast()) };
    if ok == 0 {
        return None;
    }

    // The root block is the VS_FIXEDFILEINFO, the one place the version is
    // numbers rather than a string in some translation.
    let mut info: *mut c_void = ptr::null_mut();
    let mut len: u32 = 0;
    let root = wide(r"\");
    let ok = unsafe {
        VerQueryValueW(data.as_ptr() as *const c_void, root.as_ptr(), &mut info, &mut len)
    };

    if ok == 0 || info.is_null() || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>() {
        return None;
    }

    let fixed = unsafe { &*(info as *const VS_FIXEDFILEINFO) };
    if fixed.dwSignature != VS_FFI_SIGNATURE as u32 {
        return None;
    }

    Some(DriverVersion([
        (fixed.dwFileVersionMS >> 16) as u16,
        (fixed.dwFileVersionMS & 0xffff) as u16,
        (fixed.dwFileVersionLS >> 16) as u16,
        (fixed.dwFileVersionLS & 0xffff) as u16,
    ]))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_readable_old_version_refuses() {
        // The floor exists to stop a crash, not to strand machines: a
        // version that cannot be read is not evidence of an old one, and a
        // registry read failing is not a reason to give up fan control.
        assert!(verdict(None).is_ok());
        assert!(verdict(Some(DriverVersion::new(2, 2, 0, 0))).is_ok());
        assert!(verdict(Some(DriverVersion::new(2, 3, 1, 0))).is_ok());

        assert!(matches!(
            verdict(Some(DriverVersion::new(2, 1, 9, 0))),
            Err(Error::DriverTooOld { .. })
        ));
        assert!(matches!(
            verdict(Some(DriverVersion::new(2, 0, 0, 0))),
            Err(Error::DriverTooOld { .. })
        ));
    }

    #[test]
    fn versions_order_numerically_not_textually() {
        // "10" sorts before "2" as text. A future PawnIO 2.10 or 10.0 must
        // clear a 2.2 floor, so the comparison has to be on numbers.
        assert!(DriverVersion::new(2, 10, 0, 0) > MIN_DRIVER_VERSION);
        assert!(DriverVersion::new(10, 0, 0, 0) > MIN_DRIVER_VERSION);
        assert!(DriverVersion::new(2, 2, 0, 1) > MIN_DRIVER_VERSION);
        assert!(DriverVersion::new(1, 9, 9, 9) < MIN_DRIVER_VERSION);
    }

    #[test]
    fn display_matches_the_download_page() {
        // The error names a version somebody will look for on pawnio.eu, so
        // it prints the way releases are numbered there.
        assert_eq!(DriverVersion::new(2, 2, 0, 0).to_string(), "2.2.0");
        assert_eq!(DriverVersion::new(2, 2, 0, 5).to_string(), "2.2.0.5");
        assert_eq!(MIN_DRIVER_VERSION.to_string(), "2.2.0");
    }

    #[test]
    fn image_paths_expand_to_files_the_filesystem_can_open() {
        let windir = Path::new(r"C:\Windows");

        // The form on a real machine: the kernel's own spelling of the
        // Windows directory, which CreateFile does not understand.
        assert_eq!(
            expand_image_path(r"\SystemRoot\System32\DriverStore\x\PawnIO.sys", windir),
            PathBuf::from(r"C:\Windows\System32\DriverStore\x\PawnIO.sys")
        );
        // Registry values are hand-typed often enough that case cannot be
        // assumed.
        assert_eq!(
            expand_image_path(r"\systemroot\System32\PawnIO.sys", windir),
            PathBuf::from(r"C:\Windows\System32\PawnIO.sys")
        );
        assert_eq!(
            expand_image_path(r"\??\C:\somewhere\PawnIO.sys", windir),
            PathBuf::from(r"C:\somewhere\PawnIO.sys")
        );
        assert_eq!(
            expand_image_path(r"System32\drivers\PawnIO.sys", windir),
            PathBuf::from(r"C:\Windows\System32\drivers\PawnIO.sys")
        );
        assert_eq!(
            expand_image_path(r"C:\already\plain\PawnIO.sys", windir),
            PathBuf::from(r"C:\already\plain\PawnIO.sys")
        );
    }

    #[test]
    fn reading_the_installed_version_is_safe_anywhere() {
        // Whatever this machine has installed, asking must not panic: the
        // tray asks from a menu handler, where a panic takes the icon out.
        let _ = installed_driver_version();
        let _ = driver_version_report();
    }
}
