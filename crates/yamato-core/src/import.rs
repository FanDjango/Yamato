// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! Reading a curve out of a TPFanControl ini.
//!
//! Anyone arriving here has probably spent an evening at some point getting a
//! curve the way they like it, in a file that looks like this:
//!
//! ```text
//! Level=46 0 0 3
//! Level=52 1 0 5
//! Level=90 128 0 7
//! ```
//!
//! Making them do that again by dragging points is a poor welcome, and both
//! formats describe the same thing: a temperature, a fan level, and the two
//! hysteresis figures.
//!
//! Only the curves are read. Polling, sensor names, hotkeys and the
//! manual-mode exit either have no counterpart here or are decided differently
//! on purpose, and importing settings nobody asked for turns a migration into
//! a surprise.
//!
//! Pure: text in, curves out. No file, no window, no clock.

use yamato_ec::{FAN_BIOS, FAN_DISENGAGED, FAN_LEVEL_MAX};

use crate::curve::{Curve, CurveError, CurvePoint};

/// The level TPFanControl writes for "give the fan back to the firmware".
const INI_BIOS: u8 = 128;

#[derive(Debug, PartialEq, Eq)]
pub enum ImportError {
    /// Not one `Level=` line anywhere. Almost always the wrong file.
    NoCurve,
    /// The lines were there but do not describe a curve this program would
    /// accept. Reported, not repaired: quietly changing somebody's curve on
    /// the way in is the surprise this feature exists to avoid.
    Curve(CurveError),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::NoCurve => write!(
                f,
                "there are no Level= lines in that file, so there is no curve in it to import"
            ),
            ImportError::Curve(e) => write!(f, "the curve in that file is not usable: {e}"),
        }
    }
}

impl std::error::Error for ImportError {}

/// One curve found in the file, plus what changed on the way in.
#[derive(Debug)]
pub struct Imported {
    /// `Level` or `Level2`, which is what the second smart profile is called
    /// in the files that have one.
    pub key: String,
    pub curve: Curve,
    /// Temperatures whose level was the disengaged byte, which this program
    /// will not run at any speed.
    ///
    /// Substituted, since refusing a whole file over one line helps nobody.
    /// Reported, since the fan will behave differently there than it used to.
    pub disengaged_at: Vec<i8>,
    /// Whether any hysteresis figure had to be brought inside the range this
    /// program will run.
    ///
    /// Reported for the same reason: somebody tuned those numbers.
    pub hysteresis_clamped: bool,
}

/// Pulls every curve out of a TPFanControl ini.
///
/// The order they came in is kept: `Level` first, then `Level2` if the file
/// has one, which is that program's second smart profile.
pub fn parse_tpfancontrol_ini(text: &str) -> Result<Vec<Imported>, ImportError> {
    // Name, points, temperatures that asked for the disengaged byte, and
    // whether any hysteresis had to be brought into range.
    let mut found: Vec<(String, Vec<CurvePoint>, Vec<i8>, bool)> = Vec::new();

    for line in text.lines() {
        // `//` starts a comment in these files, and it is used on the same
        // line as a setting often enough to matter.
        let line = match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        };

        let Some((key, value)) = line.split_once('=') else { continue };

        // Matched case insensitively, then named one way. These files are
        // hand-edited, and taking the case as written turned one curve written
        // both `level=` and `LEVEL=` into two curves.
        let key = match key.trim().to_ascii_lowercase().as_str() {
            "level" => "Level",
            "level2" => "Level2",
            _ => continue,
        };

        let mut fields = value.split_whitespace();
        let (Some(temp), Some(level)) = (fields.next(), fields.next()) else { continue };

        // A line that does not parse is skipped, not fatal. Only finding no
        // curve at all is worth failing over.
        let (Ok(temp), Ok(level)) = (temp.parse::<i32>(), level.parse::<i32>()) else { continue };

        let hyst_up = fields.next().and_then(|f| f.parse::<i32>().ok()).unwrap_or(0);
        let hyst_down = fields.next().and_then(|f| f.parse::<i32>().ok()).unwrap_or(4);

        // Dropped on the way in instead of wrapping into something plausible.
        // A temperature above 127 does not fit in an i8.
        let Ok(temp) = i8::try_from(temp) else { continue };

        let entry = match found.iter_mut().find(|(name, _, _, _)| *name == key) {
            Some(entry) => entry,
            None => {
                found.push((key.to_string(), Vec::new(), Vec::new(), false));
                found.last_mut().expect("just pushed")
            }
        };

        let (level, disengaged) = translate_level(level);
        if disengaged {
            entry.2.push(temp);
        }

        // The same bounds the settings file is held to, applied here so an
        // imported curve is not a way around them.
        let (hyst_up, up_clamped) = clamp_hysteresis(hyst_up, crate::config::HYST_UP_MAX);
        let (hyst_down, down_clamped) = clamp_hysteresis(hyst_down, crate::config::HYST_DOWN_MAX);
        entry.3 |= up_clamped || down_clamped;

        entry.1.push(CurvePoint { temp, level, hyst_up, hyst_down });
    }

    if found.iter().all(|(_, points, _, _)| points.is_empty()) {
        return Err(ImportError::NoCurve);
    }

    let mut curves = Vec::new();

    for (key, mut points, disengaged_at, hysteresis_clamped) in found {
        if points.is_empty() {
            continue;
        }

        // Usually in order already, but nothing guarantees it and Curve::new
        // refuses a list that does not climb. Sorting is not repairing: two
        // points at one temperature is a contradiction, and that still comes
        // back as an error.
        points.sort_by_key(|p| p.temp);

        let curve = Curve::new(points).map_err(ImportError::Curve)?;
        curves.push(Imported { key, curve, disengaged_at, hysteresis_clamped });
    }

    Ok(curves)
}

/// TPFanControl's fan byte, as a level this program will run.
///
/// Returns whether the disengaged byte had to be substituted. That changes how
/// the machine behaves, so the person importing should hear about it.
fn translate_level(level: i32) -> (u8, bool) {
    match level {
        n if n == INI_BIOS as i32 => (FAN_BIOS, false),
        // 0x40 runs the blower with no governor. Level 7 is the fastest this
        // program will ask for, and it is what somebody reaching for the
        // disengaged byte was after.
        n if n == FAN_DISENGAGED as i32 => (FAN_LEVEL_MAX, true),
        n if (0..=FAN_LEVEL_MAX as i32).contains(&n) => (n as u8, false),
        // Anything else is not a level. The firmware step is the safe reading
        // of a number nobody can explain.
        _ => (FAN_BIOS, false),
    }
}

/// Hysteresis as a number of degrees this program will run, and whether it had
/// to be changed to get there.
fn clamp_hysteresis(degrees: i32, ceiling: i8) -> (i8, bool) {
    let held = degrees.clamp(0, ceiling as i32);

    (held as i8, held != degrees)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The owner's own file, which is the one case that has to be exactly
    /// right.
    const REAL: &str = "\
Level=46 0 0 3
Level=52 1 0 5
Level=60 2 0 5
Level=68 3 0 6
Level=76 4 0 6
Level=84 5 0 6
Level=90 128 0 7
";

    #[test]
    fn a_real_file_comes_across_point_for_point() {
        let found = parse_tpfancontrol_ini(REAL).unwrap();
        assert_eq!(found.len(), 1);

        let points = found[0].curve.points();
        assert_eq!(points.len(), 7);

        let expected = [
            (46i8, 0u8, 0i8, 3i8),
            (52, 1, 0, 5),
            (60, 2, 0, 5),
            (68, 3, 0, 6),
            (76, 4, 0, 6),
            (84, 5, 0, 6),
            (90, FAN_BIOS, 0, 7),
        ];

        for (point, (temp, level, up, down)) in points.iter().zip(expected) {
            assert_eq!(point.temp, temp);
            assert_eq!(point.level, level, "level at {temp} C");
            assert_eq!(point.hyst_up, up);
            assert_eq!(point.hyst_down, down);
        }

        assert!(found[0].disengaged_at.is_empty());
    }

    #[test]
    fn a_silent_idle_survives_the_trip() {
        // Level 0 is legal in a curve and is the point of the first step in
        // most of these files. Importing it as anything else would be
        // importing a different curve.
        let found = parse_tpfancontrol_ini(REAL).unwrap();
        assert_eq!(found[0].curve.points()[0].level, 0);
    }

    #[test]
    fn the_firmware_step_is_recognized() {
        // Two steps, because one is not a curve: a lone point holds its level
        // at every temperature with the firmware switched off, and the loader
        // refuses it however it arrived.
        let found = parse_tpfancontrol_ini("Level=50 1 0 4\nLevel=80 128 0 4\n").unwrap();
        assert!(found[0].curve.points()[1].is_bios());
    }

    #[test]
    fn the_disengaged_byte_is_substituted_and_said_out_loud() {
        // 0x40 is refused everywhere else in this program, and importing one
        // is not a way around that.
        let found = parse_tpfancontrol_ini("Level=50 1 0 4\nLevel=88 64 0 5\n").unwrap();

        assert_eq!(found[0].curve.points()[1].level, FAN_LEVEL_MAX);
        assert_eq!(found[0].disengaged_at, vec![88]);

        for point in found[0].curve.points() {
            assert_ne!(point.level, FAN_DISENGAGED);
        }
    }

    #[test]
    fn trailing_comments_are_not_part_of_the_numbers() {
        let found =
            parse_tpfancontrol_ini("Level=46 0 0 3 //was 44\nLevel=60 2 0 5\n").unwrap();

        assert_eq!(found[0].curve.points()[0].temp, 46);
        assert_eq!(found[0].curve.points()[0].hyst_down, 3);
    }

    #[test]
    fn the_second_smart_profile_comes_across_as_its_own_curve() {
        let text = "\
Level=46 0 0 3
Level=90 128 0 7

Level2=55 2 0 4
Level2=95 128 0 6
";
        let found = parse_tpfancontrol_ini(text).unwrap();

        assert_eq!(found.len(), 2);
        assert_eq!(found[0].key, "Level");
        assert_eq!(found[1].key, "Level2");
        assert_eq!(found[1].curve.points()[0].temp, 55);
    }

    #[test]
    fn the_untidiness_of_a_hand_edited_file_is_tolerated() {
        // Windows line endings, blank lines, leading and inner whitespace,
        // lower case keys, and settings that are none of our business.
        let text = "Cycle=5\r\n\r\n  level = 50   1  0   4  \r\nLEVEL=70 3 1 6\r\nManModeExit=78 //x\r\n";
        let found = parse_tpfancontrol_ini(text).unwrap();

        assert_eq!(found.len(), 1);
        let points = found[0].curve.points();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].temp, 50);
        assert_eq!(points[1].hyst_up, 1);
    }

    #[test]
    fn nothing_but_curves_is_taken() {
        // Each of these has a counterpart Yamato decides for itself. Importing
        // them silently would change how the program behaves on the strength
        // of a file somebody only meant to lift a curve out of.
        let text = "Cycle=2\nManModeExit=78\nSensorName=CPU\nLevel=46 0 0 3\nLevel=90 128 0 7\n";
        let found = parse_tpfancontrol_ini(text).unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].curve.points().len(), 2);
    }

    #[test]
    fn a_file_with_no_curve_in_it_says_so() {
        assert_eq!(
            parse_tpfancontrol_ini("Cycle=5\nManModeExit=78\n").unwrap_err(),
            ImportError::NoCurve
        );
        assert_eq!(parse_tpfancontrol_ini("").unwrap_err(), ImportError::NoCurve);
    }

    #[test]
    fn a_curve_that_does_not_climb_is_reported_rather_than_repaired() {
        // Two points at one temperature is a contradiction, not a sorting
        // problem, and it is the user's file to fix.
        let err = parse_tpfancontrol_ini("Level=50 1 0 4\nLevel=50 3 0 4\n").unwrap_err();

        assert!(matches!(err, ImportError::Curve(CurveError::NotAscending { .. })), "{err:?}");
        assert!(err.to_string().contains("not usable"));
    }

    #[test]
    fn points_out_of_order_are_sorted_rather_than_refused() {
        // The order in the file is presentation; the curve is the same one.
        let found = parse_tpfancontrol_ini("Level=90 128 0 7\nLevel=46 0 0 3\n").unwrap();

        assert_eq!(found[0].curve.points()[0].temp, 46);
        assert_eq!(found[0].curve.points()[1].temp, 90);
    }

    #[test]
    fn a_line_that_is_not_a_curve_point_is_skipped_not_fatal() {
        let found = parse_tpfancontrol_ini("Level=oops\nLevel=46 0 0 3\nLevel=90 128\n").unwrap();

        let points = found[0].curve.points();
        assert_eq!(points.len(), 2);
        // Missing hysteresis figures fall back to the same defaults a new
        // point gets here.
        assert_eq!(points[1].hyst_down, 4);
    }

    #[test]
    fn tuned_hysteresis_comes_across_untouched() {
        // The reason anyone imports one of these files rather than dragging a
        // new curve: the numbers in it were arrived at over an evening.
        let found = parse_tpfancontrol_ini(REAL).unwrap();

        assert!(!found[0].hysteresis_clamped);
        let downs: Vec<i8> = found[0].curve.points().iter().map(|p| p.hyst_down).collect();
        assert_eq!(downs, vec![3, 5, 5, 6, 6, 6, 7]);
    }

    #[test]
    fn hysteresis_beyond_what_this_program_runs_is_brought_in_and_admitted() {
        // Not silently: somebody chose those numbers, and changing them
        // without a word is the surprise this feature exists to avoid.
        let found = parse_tpfancontrol_ini("Level=50 1 40 40\nLevel=70 3 0 4\n").unwrap();

        assert!(found[0].hysteresis_clamped);
        assert_eq!(found[0].curve.points()[0].hyst_up, crate::config::HYST_UP_MAX);
        assert_eq!(found[0].curve.points()[0].hyst_down, crate::config::HYST_DOWN_MAX);
    }

    #[test]
    fn an_imported_curve_is_one_the_engine_would_accept() {
        // The same rules as any other curve, applied on the way in instead of
        // discovered later.
        for (_, level) in parse_tpfancontrol_ini(REAL)
            .unwrap()
            .iter()
            .flat_map(|f| f.curve.points())
            .map(|p| (p.temp, p.level))
        {
            assert!(level <= FAN_LEVEL_MAX || level == FAN_BIOS);
            assert_ne!(level, FAN_DISENGAGED);
        }
    }
}
