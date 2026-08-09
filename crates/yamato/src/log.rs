// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! The optional history file.
//!
//! Without it the program had no memory: a fan that surprised someone at four
//! in the afternoon left nothing behind to look at. One line per pass, in a
//! shape a spreadsheet opens without being asked twice.
//!
//! One rule governs the rest: cooling the machine is the job, writing about it
//! is not. A full disk, a deleted file or a folder gone read-only must never
//! become a failed pass, a failed handback, or a delayed one. So the file is
//! opened once and kept, the first failure of any kind turns logging off for
//! the session, and nothing here returns an error a caller has to think about.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

const FILE_NAME: &str = "yamato-log.csv";

/// The column names, written when a file is created and never again.
pub const HEADER: &str = "timestamp,sensor,temp_c,level,fan1_rpm,fan2_rpm,mode,profile";

/// `%ProgramData%\Yamato\yamato-log.csv`, beside the settings file.
///
/// One folder holds everything this program keeps, so there is one place to
/// look and one place to clear out.
pub fn default_path() -> PathBuf {
    let config = yamato_core::Config::default_path();

    match config.parent() {
        Some(dir) => dir.join(FILE_NAME),
        None => PathBuf::from(FILE_NAME),
    }
}

/// The local wall clock, ISO 8601, to the second.
///
/// Local rather than UTC because the only thing anyone does with this file is
/// line it up against something they remember happening.
fn timestamp() -> String {
    // A POD structure the call fills in entirely.
    let mut now: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut now) };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
    )
}

/// Quotes a field, but only when it would otherwise be misread.
///
/// Profile names are whatever somebody typed, and a comma in one would shift
/// every column after it by one for the rest of the file.
fn cell(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

/// One pass, as it will be written down.
///
/// The optional fields are the ones a failed pass does not have. Written as
/// empty cells, not zeros: a zero in the level column reads as "the fan was
/// stopped" and a zero in the speed columns as "the fan was not turning",
/// which is the opposite of what a failed pass means.
pub struct Record<'a> {
    pub hottest: Option<(usize, i8)>,
    /// What the fan register was asked for, or nothing when the controller
    /// could not be read at all.
    pub applied: Option<u8>,
    pub fan_rpm: Option<[u16; 2]>,
    pub mode: &'a str,
    pub profile: &'a str,
}

impl Record<'_> {
    /// The line as it will be written, stamped with the clock as it is now.
    pub fn line(&self) -> String {
        self.line_at(&timestamp())
    }

    /// Split from the above so the shape of a line can be tested against a
    /// clock that does not move.
    fn line_at(&self, stamp: &str) -> String {
        let (sensor, temp) = match self.hottest {
            Some((index, celsius)) => (index.to_string(), celsius.to_string()),
            None => (String::new(), String::new()),
        };

        let level = match self.applied {
            Some(yamato_ec::FAN_BIOS) => "bios".to_string(),
            Some(level) => level.to_string(),
            None => String::new(),
        };

        let (first, second) = match self.fan_rpm {
            Some([a, b]) => (a.to_string(), b.to_string()),
            None => (String::new(), String::new()),
        };

        format!(
            "{stamp},{sensor},{temp},{level},{first},{second},{},{}",
            cell(self.mode),
            cell(self.profile)
        )
    }
}

/// An append-only file that rotates once and gives up permanently on failure.
pub struct Logger {
    path: PathBuf,
    file: Option<File>,
    /// Bytes in the file that is open, counted as they are written rather than
    /// asked of the filesystem, so deciding whether to rotate is not a trip to
    /// the disk on every pass.
    size: u64,
    /// Set by the first failure of any kind and never cleared.
    ///
    /// A full disk stays full, and a folder that refused a write refuses the
    /// next one. Retrying every few seconds would tax the control loop.
    broken: bool,
}

impl Logger {
    pub fn new(path: PathBuf) -> Self {
        Logger { path, file: None, size: 0, broken: false }
    }

    /// Appends one line. Cannot fail, by construction.
    ///
    /// `max_bytes` is read on every call rather than remembered, because the
    /// setting behind it can change while the program is running.
    pub fn write_line(&mut self, line: &str, max_bytes: u64) {
        if self.broken {
            return;
        }

        if self.file.is_none() && !self.open() {
            return;
        }

        // One write rather than two, so a line and its terminator cannot end up
        // separated by anything else that has the file open.
        let mut text = String::with_capacity(line.len() + 2);
        text.push_str(line);
        text.push_str("\r\n");

        let Some(file) = self.file.as_mut() else { return };

        if file.write_all(text.as_bytes()).is_err() {
            self.fail();
            return;
        }

        self.size += text.len() as u64;

        if self.size >= max_bytes {
            self.rotate();
        }
    }

    /// Lets go of the file, for when logging has been turned off.
    ///
    /// With nothing open, the file can be moved, mailed or deleted without
    /// this program having an opinion about it, and turning logging back on
    /// starts cleanly.
    pub fn close(&mut self) {
        self.file = None;
        self.size = 0;
    }

    /// Whether logging has given up for the rest of the session.
    ///
    /// Only the tests ask. In the program, giving up is invisible to
    /// everything above it.
    #[cfg(test)]
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    fn fail(&mut self) {
        self.broken = true;
        self.file = None;
    }

    fn open(&mut self) -> bool {
        if let Some(dir) = self.path.parent() {
            if std::fs::create_dir_all(dir).is_err() {
                self.fail();
                return false;
            }
        }

        let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&self.path) else {
            self.fail();
            return false;
        };

        let mut size = file.metadata().map(|m| m.len()).unwrap_or(0);

        // A header for a file that is new, or that has just been rotated away,
        // and never for one that already has rows in it.
        if size == 0 {
            let header = format!("{HEADER}\r\n");

            if file.write_all(header.as_bytes()).is_err() {
                self.fail();
                return false;
            }

            size = header.len() as u64;
        }

        self.size = size;
        self.file = Some(file);

        true
    }

    /// One generation of history, and no more.
    ///
    /// Numbered files that accumulate are how a diagnostic aid becomes the
    /// reason a machine runs out of disk.
    fn rotate(&mut self) {
        // Closed first. Windows will rename a file that is still open, and the
        // handle follows it, so the fresh file would be the one nobody writes.
        self.file = None;

        if std::fs::rename(&self.path, rotated_path(&self.path)).is_err() {
            self.fail();
            return;
        }

        self.size = 0;
        self.open();
    }
}

/// `yamato-log.csv` becomes `yamato-log.1.csv`.
///
/// The generation goes before the extension rather than after it so the file
/// still opens in whatever reads a .csv.
fn rotated_path(path: &Path) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());

    match (stem, path.extension()) {
        (Some(stem), Some(extension)) => {
            path.with_file_name(format!("{stem}.1.{}", extension.to_string_lossy()))
        }
        (Some(stem), None) => path.with_file_name(format!("{stem}.1")),
        _ => path.with_file_name("yamato-log.1.csv"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("yamato-log-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();

        path
    }

    fn record<'a>(profile: &'a str, applied: Option<u8>) -> Record<'a> {
        Record {
            hottest: Some((0, 61)),
            applied,
            fan_rpm: applied.map(|_| [2100, 1900]),
            mode: "smart",
            profile,
        }
    }

    #[test]
    fn the_history_sits_beside_the_settings_file() {
        assert_eq!(default_path().parent(), yamato_core::Config::default_path().parent());
        assert!(default_path().ends_with(FILE_NAME));
    }

    #[test]
    fn a_line_has_one_cell_for_every_column() {
        let line = record("Balanced", Some(3)).line_at("2026-08-09T14:03:22");

        assert_eq!(line.matches(',').count(), HEADER.matches(',').count());
        assert_eq!(line, "2026-08-09T14:03:22,0,61,3,2100,1900,smart,Balanced");
    }

    #[test]
    fn firmware_control_is_a_word_and_not_a_number() {
        // 128 in the level column would read as a fan speed, and there is no
        // level 128.
        let line = record("Balanced", Some(yamato_ec::FAN_BIOS)).line_at("t");
        assert!(line.contains(",bios,"), "{line}");
    }

    #[test]
    fn a_pass_that_could_not_read_the_controller_leaves_the_cells_empty() {
        // Zeros here would say the fan was stopped and the machine was cold,
        // which is the opposite of what a failed pass means.
        let line = Record { hottest: None, applied: None, fan_rpm: None, mode: "bios", profile: "X" }
            .line_at("t");

        assert_eq!(line, "t,,,,,,bios,X");
        assert!(!line.contains(",0,"));
    }

    #[test]
    fn a_profile_name_cannot_shift_the_columns() {
        // Names are whatever somebody typed into the box.
        let line = record("Loud, and proud", Some(3)).line_at("t");
        assert!(line.ends_with("\"Loud, and proud\""), "{line}");

        let line = record("The \"quiet\" one", Some(3)).line_at("t");
        assert!(line.ends_with("\"The \"\"quiet\"\" one\""), "{line}");
    }

    #[test]
    fn a_fresh_file_gets_a_header_and_a_reopened_one_does_not() {
        let dir = temp_dir("header");
        let path = dir.join("yamato-log.csv");

        let mut log = Logger::new(path.clone());
        log.write_line("first", 1 << 20);
        drop(log);

        let mut log = Logger::new(path.clone());
        log.write_line("second", 1 << 20);
        drop(log);

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches(HEADER).count(), 1, "{text}");
        assert!(text.contains("first") && text.contains("second"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_rotates_once_and_keeps_exactly_one_generation() {
        let dir = temp_dir("rotate");
        let path = dir.join("yamato-log.csv");
        let older = dir.join("yamato-log.1.csv");

        // Small enough that a couple of lines pass it.
        let max = (HEADER.len() + 40) as u64;
        let mut log = Logger::new(path.clone());

        for i in 0..40 {
            log.write_line(&format!("line {i}"), max);
        }

        assert!(!log.is_broken(), "rotation should not have given up");
        assert!(older.exists(), "the previous generation was not kept");

        // The live file starts again with its own header, and neither file is
        // allowed to grow past the limit by more than the line that crossed it.
        let text = std::fs::read_to_string(&path).unwrap();
        let kept = std::fs::read_to_string(&older).unwrap();

        assert!(text.starts_with(HEADER), "{text}");
        assert!(kept.starts_with(HEADER), "{kept}");
        assert!(text.len() as u64 <= max + 32, "the live file outgrew its limit");
        assert!(kept.len() as u64 <= max + 32, "the kept file outgrew its limit");

        // The newest line is in one of the two, depending on whether it was
        // the one that tipped the file over.
        assert!(
            text.contains("line 39") || kept.contains("line 39"),
            "the newest line was thrown away"
        );

        // And exactly one generation: nothing numbered beyond it.
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 2, "more than one generation was kept");

        drop(log);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotating_replaces_the_previous_generation_rather_than_stacking_up() {
        let dir = temp_dir("replace");
        let path = dir.join("yamato-log.csv");
        let older = dir.join("yamato-log.1.csv");

        std::fs::write(&older, "an older generation").unwrap();

        let max = (HEADER.len() + 20) as u64;
        let mut log = Logger::new(path.clone());
        for i in 0..20 {
            log.write_line(&format!("line {i}"), max);
        }

        let kept = std::fs::read_to_string(&older).unwrap();
        assert!(!kept.contains("an older generation"), "the old file was not replaced");

        drop(log);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failure_turns_logging_off_for_the_session_rather_than_retrying() {
        // A full disk or a read-only folder will still be that way in five
        // seconds, and the control loop must not spend them finding out again.
        // Standing in for it: a folder that cannot be created because a file
        // of that name is already sitting where it would go.
        let dir = temp_dir("broken");
        let blocker = dir.join("in-the-way");
        std::fs::write(&blocker, "not a folder").unwrap();

        let mut log = Logger::new(blocker.join("yamato-log.csv"));

        log.write_line("first", 1 << 20);
        assert!(log.is_broken(), "a folder that cannot be made should have given up");

        // And it stays given up: nothing reopens on a later pass.
        for _ in 0..5 {
            log.write_line("again", 1 << 20);
        }
        assert!(log.is_broken());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_generation_keeps_the_extension_a_spreadsheet_knows() {
        assert_eq!(
            rotated_path(Path::new(r"C:\ProgramData\Yamato\yamato-log.csv")),
            PathBuf::from(r"C:\ProgramData\Yamato\yamato-log.1.csv")
        );
        assert_eq!(rotated_path(Path::new("plain")), PathBuf::from("plain.1"));
    }

    #[test]
    fn a_timestamp_is_ordinary_iso_8601() {
        let stamp = timestamp();

        assert_eq!(stamp.len(), 19, "{stamp}");
        assert_eq!(stamp.as_bytes()[10], b'T');
        assert!(stamp.starts_with("20"), "{stamp}");
    }
}
