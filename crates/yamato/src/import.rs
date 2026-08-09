// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! Bringing a curve over from TPFanControl.
//!
//! The parsing lives in `yamato_core::import`, where it can be tested without
//! a file or a window. This part asks which file, asks what to call it, and
//! says what happened. Shared between the tray and the settings window, so the
//! two cannot disagree about what one of those files means.

use std::path::PathBuf;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONWARNING, MB_OK,
};
use windows_sys::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError,
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};

/// Where TPFanControl conventionally lives, and where the file will therefore
/// be if the person importing it has not moved anything.
const CUSTOMARY_HOME: &str = r"C:\TPFC";

/// How it went, in a sentence fit to put in a message box.
pub(crate) struct Outcome {
    pub summary: String,
    /// The settings file as saved, so the caller can announce the switch and
    /// show the new curve without reading it again.
    pub config: yamato_core::Config,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Asks for a file. `None` when the dialog was dismissed.
fn choose_file(owner: HWND) -> Option<PathBuf> {
    // The filter is a run of NUL-terminated pairs ending in a second NUL, not
    // a string with separators in it.
    let filter: Vec<u16> = "TPFanControl settings (*.ini)\0*.ini\0All files\0*.*\0\0"
        .encode_utf16()
        .collect();

    let title = wide("Import a curve from TPFanControl");

    // Started where the file usually is, and only if that is really there:
    // pointing the dialog at a folder that does not exist gets it ignored, and
    // on some builds it lands somewhere less useful than the default.
    let start = std::path::Path::new(CUSTOMARY_HOME);
    let initial = start.is_dir().then(|| wide(CUSTOMARY_HOME));

    let mut path = vec![0u16; 32_768];

    let mut open: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    open.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    open.hwndOwner = owner;
    open.lpstrFilter = filter.as_ptr();
    open.nFilterIndex = 1;
    open.lpstrFile = path.as_mut_ptr();
    open.nMaxFile = path.len() as u32;
    open.lpstrTitle = title.as_ptr();
    open.lpstrInitialDir = initial.as_ref().map_or(std::ptr::null(), |d| d.as_ptr());
    open.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY;

    if unsafe { GetOpenFileNameW(&mut open) } == 0 {
        // Zero covers both "they pressed Cancel" and "the dialog could not be
        // shown". Cancel reports no error; anything else is worth saying.
        let failed = unsafe { CommDlgExtendedError() };

        if failed != 0 {
            unsafe {
                MessageBoxW(
                    owner,
                    wide(&format!(
                        "Yamato could not open the file chooser (error {failed}).\n\n\
                         You can still import by putting the file somewhere simple, \
                         such as C:\\TPFC, and trying again."
                    ))
                    .as_ptr(),
                    wide("Yamato").as_ptr(),
                    MB_OK | MB_ICONWARNING,
                );
            }
        }

        return None;
    }

    let end = path.iter().position(|c| *c == 0).unwrap_or(path.len());

    Some(PathBuf::from(String::from_utf16_lossy(&path[..end])))
}

/// The whole thing: pick a file, read it, name the profile, save it.
///
/// `None` means the person changed their mind, which is not a failure and gets
/// nothing said about it. Everything else comes back as a sentence to show.
pub(crate) fn run(owner: HWND) -> Option<Result<Outcome, String>> {
    let path = choose_file(owner)?;

    // Read as bytes and converted loosely. These files are ancient, hand
    // edited, and frequently have a stray byte in a comment somewhere; failing
    // an import over one would be absurd.
    let Ok(bytes) = std::fs::read(&path) else {
        return Some(Err(format!("Yamato could not read {}.", path.display())));
    };
    let text = String::from_utf8_lossy(&bytes);

    let found = match yamato_core::parse_tpfancontrol_ini(&text) {
        Ok(found) => found,
        Err(e) => return Some(Err(format!("{e}"))),
    };

    if found.is_empty() {
        return Some(Err(
            "there are no Level= lines in that file, so there is no curve in it to import".into(),
        ));
    }

    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("tpfancontrol"))
        .unwrap_or_else(|| "Imported".to_string());

    let name = crate::prompt::ask(owner, "Name the imported profile", &stem)?;

    // Every curve in the file, not just the first. These files can carry a
    // second smart profile under Level2, and importing the file a second time
    // to get it would only have produced the first curve again.
    let mut config = None;
    let mut imported: Vec<(String, usize)> = Vec::new();

    for (i, curve) in found.iter().enumerate() {
        // The first keeps the name that was asked for; the rest are numbered
        // after it, so a pair arrives as "T14" and "T14 2" rather than one of
        // them silently losing to a name that is already taken.
        let this = if i == 0 { name.clone() } else { format!("{name} {}", i + 1) };

        match crate::tray::add_profile_from_curve(&this, &curve.curve) {
            Ok(saved) => {
                imported.push((this, curve.curve.points().len()));
                config = Some(saved);
            }
            // One bad name should not throw away the curves that did arrive,
            // so this is reported at the end rather than aborting the lot.
            Err(message) => {
                if imported.is_empty() {
                    return Some(Err(message.to_string()));
                }
            }
        }
    }

    let Some(mut config) = config else {
        return Some(Err("none of the curves in that file could be added".into()));
    };

    // The one that was named is the one to end up on. Each addition makes
    // itself active as it goes, so without this the last curve would win.
    if let Some((first_name, _)) = imported.first() {
        if config.active_profile != *first_name {
            let path = yamato_core::Config::default_path();
            config.active_profile = first_name.clone();
            let _ = config.save(&path);
        }
    }

    let first = &found[0];

    let mut summary = if imported.len() == 1 {
        format!("Imported {} points into \"{name}\".", imported[0].1)
    } else {
        let list: Vec<String> = imported
            .iter()
            .map(|(n, points)| format!("\"{n}\" with {points} points"))
            .collect();

        format!(
            "That file had {} curves in it. Imported {}, and switched to \"{name}\".",
            imported.len(),
            list.join(", and ")
        )
    };

    // Said plainly, because the fan will behave differently there than it did
    // in the program this came from.
    if !first.disengaged_at.is_empty() {
        let temps: Vec<String> = first
            .disengaged_at
            .iter()
            .map(|t| format!("{t} \u{00b0}C"))
            .collect();

        summary.push_str(&format!(
            "\n\nThe disengaged fan setting at {} is not something Yamato will run, so it came \
             across as the firmware step, which hands the fan back to the BIOS.",
            temps.join(", ")
        ));
    }

    if first.hysteresis_clamped {
        summary.push_str(
            "\n\nSome of the delays in that file were larger than Yamato will run, and came \
             across as the largest it will. You can see them on the graph as the shaded bands.",
        );
    }

    if found.len() > 1 {
        summary.push_str(
            "\n\nThat file has a second curve in it. Import it again and pick the same file to \
             bring the other one over.",
        );
    }

    summary.push_str("\n\nOnly the curve was imported. Everything else in that file is either \
                      something Yamato decides for itself or something it does differently.");

    Some(Ok(Outcome { summary, config }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_filter_is_a_run_of_terminated_pairs() {
        // A common way to get this wrong is to write it as one string with
        // separators, which the dialog reads as a single unusable filter.
        let filter = "TPFanControl settings (*.ini)\0*.ini\0All files\0*.*\0\0";

        assert!(filter.ends_with("\0\0"));
        assert_eq!(filter.matches('\0').count(), 5);
    }
}
