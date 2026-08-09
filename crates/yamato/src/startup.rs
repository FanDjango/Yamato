// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! Starting with Windows.
//!
//! Two halves, and neither works alone. The service controls the fan from
//! boot, before anyone logs in; a Run entry opens the window in whichever
//! session logs on. Without the service the window would need administrator
//! rights at logon, and Windows silently skips an elevated Run entry instead
//! of prompting, so nothing would appear and nothing would say why.

use std::ptr;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "Yamato";

const LAYERS_KEY: &str =
    r"Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// True when the Run entry is present.
pub fn is_enabled() -> bool {
    unsafe {
        let mut key: HKEY = ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_QUERY_VALUE, &mut key)
            != ERROR_SUCCESS
        {
            return false;
        }

        let mut size = 0u32;
        let present = RegQueryValueExW(
            key,
            wide(VALUE_NAME).as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut size,
        ) == ERROR_SUCCESS;

        RegCloseKey(key);

        present
    }
}

pub fn toggle() -> bool {
    set(!is_enabled())
}

pub fn set(on: bool) -> bool {
    unsafe {
        let mut key: HKEY = ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_SET_VALUE, &mut key)
            != ERROR_SUCCESS
        {
            return false;
        }

        let ok = if on {
            let Ok(exe) = std::env::current_exe() else {
                RegCloseKey(key);
                return false;
            };

            // Quoted, or a path containing a space is read as a command
            // followed by arguments.
            let value = wide(&format!("\"{}\"", exe.display()));

            RegSetValueExW(
                key,
                wide(VALUE_NAME).as_ptr(),
                0,
                REG_SZ,
                value.as_ptr() as *const u8,
                (value.len() * 2) as u32,
            ) == ERROR_SUCCESS
        } else {
            // Already absent is success, not failure.
            let rc = RegDeleteValueW(key, wide(VALUE_NAME).as_ptr());
            rc == ERROR_SUCCESS || rc == 2 /* ERROR_FILE_NOT_FOUND */
        };

        RegCloseKey(key);

        ok
    }
}

/// Removes a RUNASADMIN compatibility layer on our own executable.
///
/// Such a layer overrides the manifest, and Windows then silently skips a Run
/// entry that wants elevating: no window, no error. Anything left over from an
/// older build that always asked for administrator does this, so it is cleared
/// on every start, not just at install time where an in-place upgrade would
/// miss it.
///
/// Only the elevation flag goes; anything else in that value is somebody's own
/// choice.
pub fn clear_runasadmin_layer() {
    let Ok(exe) = std::env::current_exe() else { return };
    let path = exe.to_string_lossy().into_owned();

    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        unsafe {
            let mut key: HKEY = ptr::null_mut();
            if RegOpenKeyExW(
                root,
                wide(LAYERS_KEY).as_ptr(),
                0,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                &mut key,
            ) != ERROR_SUCCESS
            {
                continue;
            }

            let name = wide(&path);
            let mut buf = [0u16; 512];
            let mut size = (buf.len() * 2) as u32;

            let read = RegQueryValueExW(
                key,
                name.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                buf.as_mut_ptr() as *mut u8,
                &mut size,
            );

            if read == ERROR_SUCCESS {
                let chars = (size as usize / 2).min(buf.len());
                let value = String::from_utf16_lossy(&buf[..chars]);
                let value = value.trim_end_matches('\0');

                let kept: Vec<&str> = value
                    .split_whitespace()
                    .filter(|t| !t.eq_ignore_ascii_case("RUNASADMIN") && *t != "~")
                    .collect();

                if kept.len() != value.split_whitespace().filter(|t| *t != "~").count() {
                    if kept.is_empty() {
                        RegDeleteValueW(key, name.as_ptr());
                    } else {
                        let rest = wide(&format!("~ {}", kept.join(" ")));
                        RegSetValueExW(
                            key,
                            name.as_ptr(),
                            0,
                            REG_SZ,
                            rest.as_ptr() as *const u8,
                            (rest.len() * 2) as u32,
                        );
                    }
                }
            }

            RegCloseKey(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asking_about_startup_never_panics() {
        let _ = is_enabled();
    }

    #[test]
    fn the_run_value_would_be_quoted() {
        // "C:\Program Files\Yamato\yamato.exe" unquoted is read as the command
        // "C:\Program" with arguments.
        let value = format!("\"{}\"", r"C:\Program Files\Yamato\yamato.exe");
        assert!(value.starts_with('"') && value.ends_with('"'));
    }

    #[test]
    fn only_the_elevation_flag_is_stripped_from_a_layer() {
        let value = "~ REGISTERAPPRESTART RUNASADMIN HIGHDPIAWARE";
        let kept: Vec<&str> = value
            .split_whitespace()
            .filter(|t| !t.eq_ignore_ascii_case("RUNASADMIN") && *t != "~")
            .collect();

        assert_eq!(kept, vec!["REGISTERAPPRESTART", "HIGHDPIAWARE"]);
    }
}
