// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! Telling someone their fan is not being controlled because PawnIO is missing.
//!
//! The engine's own diagnosis is good and unreachable. It runs as a service in
//! session zero, where there is nobody to show a message box to; its console
//! output goes nowhere in a windows subsystem binary; and the tray never opens
//! the port driver, so it cannot discover the problem by trying.
//!
//! Without this, the tray said the service was stopped, the user started it,
//! it died in under a second, and the tray said the service was stopped.
//!
//! So the tray asks the installer's question from outside the engine: is the
//! driver registered, and is the module file where we expect it. A guess, but
//! a good one, and better than silence.

use std::path::PathBuf;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS};
use windows_sys::Win32::System::Threading::OpenMutexW;

/// The right to wait on an object, which is the least that will open one.
/// Spelled out because windows-sys files it under the file system, where
/// nobody looking for a mutex would think to find it.
const SYNCHRONIZE: u32 = 0x0010_0000;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
};

/// Where a driver's service registration lives. The installer checks the same
/// key, so the two cannot disagree about what "installed" means.
const PAWNIO_SERVICE_KEY: &str = r"SYSTEM\CurrentControlSet\Services\PawnIO";

/// The download page. The driver is not ours to bundle: it is GPL-2.0, and
/// shipping it would oblige us to ship its source.
pub const DOWNLOAD_URL: &str = "https://pawnio.eu";

/// What is missing, if anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    /// The driver is not registered on this machine.
    Driver,
    /// The driver is there, but the module Yamato hands it is not beside the
    /// executable. An incomplete copy rather than an incomplete install.
    Module(PathBuf),
    /// Nothing obviously missing. The engine's failure, if there is one, is
    /// something this check cannot see.
    Nothing,
}

impl Missing {
    /// A sentence for a tooltip. Kept short: the tray's tip has a hard limit
    /// and the profile name is already competing for it.
    pub fn short(&self) -> &'static str {
        match self {
            Missing::Driver => "PawnIO is not installed. Right-click to fix it.",
            Missing::Module(_) => "A file is missing beside Yamato. Right-click for details.",
            Missing::Nothing => "Starting up, or the engine could not reach the controller.",
        }
    }

    /// The longer version, for a message box, where there is room to explain
    /// what PawnIO is rather than just naming it.
    pub fn explain(&self) -> String {
        match self {
            Missing::Driver => format!(
                "Yamato cannot control the fan because PawnIO is not installed.\n\n\
                 PawnIO is a small signed driver, written by someone else, that Yamato \
                 uses to reach the embedded controller. It is not bundled with Yamato, \
                 so it is a separate download.\n\n\
                 Open the PawnIO download page now?"
            ),
            Missing::Module(path) => format!(
                "Yamato cannot control the fan because a file it needs is missing:\n\n{}\n\n\
                 This file is installed alongside Yamato. Reinstalling should restore it.",
                path.display()
            ),
            Missing::Nothing => String::from(
                "PawnIO is installed and the module file is where it should be.\n\n\
                 PawnIO is a small signed driver, written by someone else, that Yamato \
                 uses to reach the embedded controller. It is not bundled with Yamato \
                 and is not ours to sign for, so it lives at pawnio.eu.\n\n\
                 Open the download page anyway, to reinstall it or to read what it is?",
            ),
        }
    }
}

/// Looks for the reasons the engine most often cannot start.
///
/// Cheap and side effect free: it opens a registry key and asks whether a file
/// exists. Nothing here touches the driver, so calling it from a client cannot
/// interfere with an engine running elsewhere.
pub fn diagnose() -> Missing {
    if !driver_registered() {
        return Missing::Driver;
    }

    match first_missing_module() {
        Some(path) => Missing::Module(path),
        None => Missing::Nothing,
    }
}

/// Whether TPFanControl or TPFanCtrl2 appears to be running.
///
/// Both programs take `Global\Access_EC` before touching the controller, so
/// neither corrupts the other's transactions. That is not the same as being
/// safe to run together: two programs with opinions about register 0x2f take
/// turns writing different levels into it, and a manual level switches the
/// firmware out of the fan loop, so the fan chases two curves with nothing
/// underneath it.
///
/// The name is TPFanControl's own single-instance mutex, session local rather
/// than global, which suits us: the tray runs in the same session as anything
/// a person launched.
pub fn another_fan_tool_running() -> bool {
    let name: Vec<u16> = "TPFanControlMutex01"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let handle = OpenMutexW(SYNCHRONIZE, 0, name.as_ptr());

        if handle.is_null() {
            false
        } else {
            // Opened only to ask whether it exists. Never waited on: taking
            // this would be claiming to be TPFanControl.
            CloseHandle(handle);
            true
        }
    }
}

/// Whether the PawnIO driver is registered as a service.
///
/// Presence of the key means installed, not necessarily running. That is the
/// right question here: a stopped driver is a different problem from an absent
/// one, and the absent one is what a new user hits.
fn driver_registered() -> bool {
    let name: Vec<u16> = PAWNIO_SERVICE_KEY
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        let opened = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            name.as_ptr(),
            0,
            KEY_READ,
            &mut key,
        );

        if opened == ERROR_SUCCESS {
            RegCloseKey(key);
            true
        } else {
            false
        }
    }
}

/// The first PawnIO module that is not beside the executable, if any.
///
/// Next to the binary, not the working directory: a service and a Run-key
/// launch both start somewhere else. Both modules are checked, because both
/// ship: which one the engine ends up needing depends on where this machine
/// keeps its EC, and the machine that needs the second file is exactly the
/// one where its absence is fatal.
fn first_missing_module() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;

    yamato_ec::MODULE_FILES
        .iter()
        .map(|file| exe.with_file_name(file))
        .find(|path| !path.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_says_something_a_tooltip_can_hold() {
        // The tray's tip field is a fixed 128 UTF-16 units and shares it with
        // the temperature, the mode and the profile name. A sentence that
        // overran would be truncated into nonsense rather than refused.
        for m in [
            Missing::Driver,
            Missing::Module(PathBuf::from("x")),
            Missing::Nothing,
        ] {
            assert!(!m.short().is_empty());
            assert!(m.short().len() < 64, "too long for a shared tooltip");
        }
    }

    #[test]
    fn the_long_form_names_the_missing_file() {
        let m = Missing::Module(PathBuf::from(r"C:\Program Files\Yamato\LpcACPIEC.bin"));

        // Naming the path is the whole point: "a file is missing" without
        // saying which one leaves someone no better off than silence did.
        assert!(m.explain().contains("LpcACPIEC.bin"));
    }

    #[test]
    fn diagnosing_is_safe_to_call_anywhere() {
        // No panic, whatever this machine happens to have installed. The tray
        // calls it from a menu handler, where a panic would take the icon out.
        let _ = diagnose();
    }
}
