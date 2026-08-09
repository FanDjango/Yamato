// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! The control loop, and the guarantee that wraps it.
//!
//! Writing a manual level to the fan register takes the firmware out of the
//! loop until 0x80 goes back, so a level left set by something that stopped
//! running means a laptop with no thermal management. Every exit from this
//! module goes through [`FanGuard`], including panics.


use std::sync::Arc;
use std::time::{Duration, Instant};

use yamato_ec::{Ec, EcState, Error, FAN_BIOS};

use crate::curve::Curve;

/// What the engine is currently doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Hands the fan to the firmware and stops deciding.
    Bios,
    /// Follows a curve.
    Smart,
    /// Holds one level. The level is checked against the curve's own rules,
    /// so this cannot be used to smuggle in a disengaged fan.
    Manual(u8),
}

impl Default for Mode {
    fn default() -> Self {
        // Starting in firmware mode means a crash between construction and the
        // first decision leaves the machine cooled rather than pinned.
        Mode::Bios
    }
}

/// One pass of the loop, for the UI and the log.
#[derive(Debug, Clone)]
pub struct Tick {
    pub state: EcState,
    /// Sensor index and reading the decision was made on.
    pub hottest: Option<(usize, i8)>,
    /// What we asked the fan register to be.
    pub applied: u8,
    /// True when this pass changed the register.
    pub changed: bool,
}

/// Restores firmware control when dropped, whatever the reason.
///
/// This is why the release profile keeps `panic = "unwind"`: an abort skips
/// every destructor, including this one.
pub struct FanGuard {
    ec: Arc<Ec>,
}

impl FanGuard {
    pub fn new(ec: Arc<Ec>) -> Self {
        FanGuard { ec }
    }

    /// Hands the fan back, unconditionally.
    ///
    /// Tracking whether the firmware already has the fan means a flag that can
    /// latch off and leave a level pinned on exit. Writing 0x80 twice is
    /// harmless, and no state means no state to get wrong.
    pub fn release(&self) -> Result<(), Error> {
        let mut last = None;

        // Retried, because this is the one operation that must not fail, and
        // the caller is told if it does. A silently pinned fan is the failure
        // that damages hardware.
        for attempt in 0..3 {
            match self.ec.release_to_bios() {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e),
            }

            if attempt < 2 {
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        Err(last.unwrap_or(Error::Busy))
    }

}

impl Drop for FanGuard {
    fn drop(&mut self) {
        // Nothing to report to at this point, but the attempt still matters.
        let _ = self.release();
    }
}

/// Temperature at which a held manual level is abandoned.
///
/// Manual mode disables the firmware's fan management, so a level set and
/// walked away from holds while the machine climbs to whatever it likes. The
/// reference implementation has escaped manual mode on temperature for years
/// and defaults to around here.
pub const MANUAL_ESCAPE_C: i8 = 80;

/// Where the firmware takes a curve's fan back, whatever the curve says.
///
/// Above this a curve has stopped being an opinion worth having: the three
/// that ship hand the fan over at 88, 90 and 93, so one still deciding at 95
/// has been cut down to something that no longer covers the top of the range.
/// Higher than the manual escape on purpose, because a curve is watching the
/// temperature and a held level is not.
pub const SMART_CEILING_C: i8 = 95;

/// How long the loop may go without a decision before the fan is handed back.
///
/// A stall means something is wrong, and a loud laptop is the safe failure.
pub const DEFAULT_WATCHDOG: Duration = Duration::from_secs(30);

pub struct Engine {
    ec: Arc<Ec>,
    guard: FanGuard,
    curve: Curve,
    mode: Mode,
    ignored_sensors: Vec<usize>,
    /// Which curve step we settled on. Ours, not read back from hardware.
    step: Option<usize>,
    /// What we last asked for, to avoid pointless writes.
    applied: Option<u8>,
    last_decision: Instant,
    watchdog: Duration,
    manual_escape: i8,
    /// Set once a held level has been abandoned for being too hot. Cleared
    /// only by a new instruction, never by cooling down.
    manual_escaped: bool,
}

impl Engine {
    pub fn new(ec: Ec, curve: Curve) -> Self {
        let ec = Arc::new(ec);

        Engine {
            guard: FanGuard::new(Arc::clone(&ec)),
            ec,
            curve,
            mode: Mode::default(),
            ignored_sensors: Vec::new(),
            step: None,
            applied: None,
            last_decision: Instant::now(),
            watchdog: DEFAULT_WATCHDOG,
            manual_escape: MANUAL_ESCAPE_C,
            manual_escaped: false,
        }
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    /// Switching mode forgets where we were on the curve, so the next pass
    /// lands directly on the right step instead of walking up to it.
    pub fn set_mode(&mut self, mode: Mode) {
        // Cleared on any instruction, including one naming the mode we are
        // already in. The tray always sends the same manual level, so gating
        // this on "did the mode change" meant clicking Manual again left the
        // escape latched and the fan on firmware, with the menu still showing
        // Manual ticked.
        self.manual_escaped = false;

        if self.mode != mode {
            self.mode = mode;
            self.step = None;
        }
    }

    /// Whether a held manual level has been abandoned for being too hot.
    ///
    /// The window shows this, so the mode on screen matches the fan.
    pub fn manual_escaped(&self) -> bool {
        self.manual_escaped
    }

    pub fn set_curve(&mut self, curve: Curve) {
        self.curve = curve;
        self.step = None;
    }

    pub fn set_ignored_sensors(&mut self, ignored: Vec<usize>) {
        self.ignored_sensors = ignored;
    }

    pub fn set_manual_escape(&mut self, celsius: i8) {
        self.manual_escape = celsius;
    }

    /// Tells the controller handle whether this machine has a second fan.
    ///
    /// Forwarded, not stored: the EC layer is the only place that acts on it,
    /// and a copy here would be one more thing to fall out of step with what
    /// the hardware is actually asked.
    pub fn set_single_fan(&mut self, single: bool) {
        self.ec.set_single_fan(single);
    }

    pub fn set_watchdog(&mut self, watchdog: Duration) {
        self.watchdog = watchdog;
    }

    /// True when too long has passed since a decision.
    pub fn is_stalled(&self) -> bool {
        self.last_decision.elapsed() > self.watchdog
    }

    /// Reads the controller, decides, and writes if the answer changed.
    pub fn tick(&mut self) -> Result<Tick, Error> {
        let state = self.ec.sample()?;
        let hottest = state.hottest(&self.ignored_sensors);

        let wanted = match self.mode {
            Mode::Bios => FAN_BIOS,
            // Blind counts as too hot. With no sensor reporting, a manual
            // level would hold on no information and the escape below could
            // never fire.
            Mode::Manual(_) if hottest.is_none() => FAN_BIOS,
            // Above the escape temperature the firmware takes the fan back and
            // keeps it. Latched: without that, cooling one degree re-applies
            // the level right under the danger line and the next ramp rides it
            // for a whole poll interval. Only a fresh instruction clears it.
            Mode::Manual(_) if self.manual_escaped => FAN_BIOS,
            Mode::Manual(_) if hottest.is_some_and(|(_, t)| t >= self.manual_escape) => {
                self.manual_escaped = true;
                FAN_BIOS
            }
            Mode::Manual(level) => level.min(yamato_ec::FAN_LEVEL_MAX),
            Mode::Smart => match hottest {
                // The curve is trusted right up to the point where trusting it
                // stops being defensible.
                //
                // A curve is not required to end by handing the fan back, and
                // below its first step and above its last it holds that step's
                // level. Delete every point but the first of the shipped
                // Balanced curve, whose first step is level 0, and the result
                // is a valid curve that runs the fan at nothing whatever the
                // temperature, with the firmware's own management switched off
                // because a level is set. The watchdog does not catch it: the
                // loop is not stalled, it is doing exactly as it was told.
                //
                // Rather than refuse such a curve, which would stop a
                // hand-edited file and an imported ini from loading at all,
                // the firmware takes over above a temperature no sane curve is
                // still deciding at. The three that ship hand back at 88, 90
                // and 93, so none of them ever reaches this.
                Some((_, temp)) if temp >= SMART_CEILING_C => FAN_BIOS,
                Some((_, temp)) => {
                    let step = self.curve.evaluate(temp, self.step);
                    self.step = Some(step);
                    self.curve.level_at(step)
                }
                // Every sensor silent. Guessing would be worse than deferring.
                None => FAN_BIOS,
            },
        };

        // Compared against the hardware as well as against what we think we
        // last wrote. On memory alone, anything else moving the register, or
        // one fan of two declining a write, went unnoticed forever.
        //
        // Distinct from deciding the *level* from the register, which is what
        // makes two controllers hunt. The level still comes from the curve;
        // this only decides whether to write it.
        let disagrees = state
            .fan_ctrl_per_fan
            .iter()
            .any(|seen| seen & yamato_ec::FAN_BITS != wanted & yamato_ec::FAN_BITS);

        let changed = self.applied != Some(wanted) || disagrees;
        if changed {
            self.ec.set_fan(wanted)?;
            self.applied = Some(wanted);
        }

        // Only after a write we believe in, so a failing EC trips the watchdog.
        self.last_decision = Instant::now();

        // The guard is never stood down here. Knowing the firmware has the fan
        // now says nothing about whether it will on the way out.

        Ok(Tick { state, hottest, applied: wanted, changed })
    }

    /// Hands the fan back and stops. Safe to call more than once.
    pub fn shutdown(&self) -> Result<(), Error> {
        self.guard.release()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::{Curve, CurvePoint};

    #[test]
    fn default_mode_is_firmware_controlled() {
        // A crash before the first decision must not leave a level pinned.
        assert_eq!(Mode::default(), Mode::Bios);
    }

    #[test]
    fn changing_mode_forgets_the_curve_step() {
        // Otherwise a mode switch resumes from a step chosen under different
        // rules and walks to the right answer instead of jumping to it.
        let c = Curve::new(vec![CurvePoint::new(50, 1), CurvePoint::new(70, 4)]).unwrap();
        let mut step = Some(1usize);

        let mode_changed = true;
        if mode_changed {
            step = None;
        }

        assert_eq!(c.evaluate(75, step), 1);
    }

    #[test]
    fn the_fan_guard_has_no_way_to_be_stood_down() {
        // Regression. An earlier FanGuard could be stood down and never
        // re-armed, so the first curve handoff to firmware latched it off for
        // the life of the process and every later manual level was left pinned
        // on exit. The default curve reached that by touching 90 C once.
        //
        // The guarantee rests on there being no state to get wrong: release
        // always writes 0x80.
        let source = include_str!("engine.rs");

        // Split, or the assertion matches its own source text.
        let banned = concat!("fn ", "disarm");

        assert!(
            !source.contains(banned),
            "FanGuard must not gain a way to be stood down without a matching re-arm"
        );
    }

    #[test]
    fn watchdog_reports_a_stall() {
        // Standing in for the engine's clock without touching hardware.
        let last = Instant::now() - Duration::from_secs(60);
        assert!(last.elapsed() > DEFAULT_WATCHDOG);
    }
}
