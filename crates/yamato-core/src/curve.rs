// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! Fan curves and how they are evaluated.
//!
//! Evaluation is a pure function of temperature and the point we last settled
//! on. TPFanControl also decided from the fan register's current contents, so
//! two controllers, or a firmware adjusting the level itself, each read a value
//! the other had just changed and the fan hunted. Nothing here reads the
//! hardware to decide.

use yamato_ec::{FAN_BIOS, FAN_LEVEL_MAX};

/// One step of a curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurvePoint {
    /// Degrees Celsius at or above which this step applies.
    pub temp: i8,
    /// Fan level. 0 is off, 1..=7 are the ordinary speeds, and `FAN_BIOS`
    /// hands the fan back to the firmware. Use it at the top of a curve, where
    /// firmware reacts faster than any polling loop.
    pub level: u8,
    /// Extra degrees required before stepping *up* to this point. Damps a
    /// noisy sensor without slowing the response much.
    pub hyst_up: i8,
    /// Degrees below this point's threshold before stepping *down* off it.
    /// Decides whether the fan sounds steady or audibly hunts between two
    /// speeds.
    pub hyst_down: i8,
}

impl CurvePoint {
    pub const fn new(temp: i8, level: u8) -> Self {
        CurvePoint { temp, level, hyst_up: 0, hyst_down: 4 }
    }

    pub const fn with_hysteresis(mut self, up: i8, down: i8) -> Self {
        self.hyst_up = up;
        self.hyst_down = down;
        self
    }

    /// True when this step gives the fan back to the firmware.
    pub fn is_bios(&self) -> bool {
        self.level == FAN_BIOS
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurveError {
    Empty,
    /// Points must climb in temperature; an out-of-order curve almost always
    /// means a typo, and silently sorting it hides the mistake.
    NotAscending { index: usize },
    /// A step that runs the fan slower than the one below it.
    Backwards { index: usize },
    /// One point, which is one level held at every temperature there is.
    TooFewPoints { count: usize },
    /// Levels above 7 that are not the firmware handoff. In practice, 0x40,
    /// which disengages the fan governor and is documented as potentially
    /// damaging.
    IllegalLevel { index: usize, level: u8 },
}

impl std::fmt::Display for CurveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CurveError::Empty => write!(f, "a curve needs at least one point"),
            CurveError::TooFewPoints { count } => write!(
                f,
                "a curve of {count} point holds one fan level at every temperature, with the firmware's own control switched off; it needs at least two"
            ),
            CurveError::Backwards { index } => write!(
                f,
                "point {index} runs the fan slower than the point below it, so the fan                  would ease off as the machine gets hotter"
            ),
            CurveError::NotAscending { index } => {
                write!(f, "point {index} is not warmer than the one before it")
            }
            CurveError::IllegalLevel { index, level } => write!(
                f,
                "point {index} asks for level {level}; only 0 to {} and the firmware handoff are allowed",
                FAN_LEVEL_MAX
            ),
        }
    }
}

impl std::error::Error for CurveError {}

/// An ascending list of steps, plus the memory of which one we are on.
#[derive(Debug, Clone)]
pub struct Curve {
    points: Vec<CurvePoint>,
}

impl Curve {
    pub fn new(points: Vec<CurvePoint>) -> Result<Self, CurveError> {
        if points.is_empty() {
            return Err(CurveError::Empty);
        }

        // A curve of one point is not a curve. Below its first step a curve
        // holds that step's level and above its last it holds that one, so a
        // single point applies one level at every temperature there is, with
        // the firmware's own management switched off because a level is set.
        // Make that point level 0 and the fan is off at 100 degrees; the
        // shipped Balanced curve starts at level 0, so deleting its other
        // points is all it takes.
        //
        // Refused here rather than in the editor, because the editor is not
        // the only way in: a hand-edited config.json and an imported ini reach
        // the engine without passing through it. A file like that is reported
        // and the engine will not start on it, which is the loud failure this
        // trades for a silent one.
        //
        // Checked after the per-point rules below, not before, so that a lone
        // point asking for the disengaged level is reported as what it is
        // rather than as a curve that happens to be short.

        for (i, p) in points.iter().enumerate() {
            if p.level > FAN_LEVEL_MAX && !p.is_bios() {
                return Err(CurveError::IllegalLevel { index: i, level: p.level });
            }

            if i > 0 && p.temp <= points[i - 1].temp {
                return Err(CurveError::NotAscending { index: i });
            }

            // A hotter step may not run the fan slower. Only temperatures were
            // checked before, so a curve holding level 4 at 76 degrees and
            // level 1 at 82 was accepted: the machine hotter and the fan
            // nearly idle. The 80 degree escape belongs to manual mode and
            // does not apply here, so nothing downstream would have caught it.
            //
            // Checked here rather than in the editor because the editor is not
            // the only way in. A hand-edited file and an imported curve reach
            // the engine without passing through it.
            //
            // The firmware step is exempt. It is a handoff, not a speed.
            if i > 0 && !p.is_bios() && !points[i - 1].is_bios()
                && p.level < points[i - 1].level
            {
                return Err(CurveError::Backwards { index: i });
            }
        }

        if points.len() < 2 {
            return Err(CurveError::TooFewPoints { count: points.len() });
        }

        Ok(Curve { points })
    }

    pub fn points(&self) -> &[CurvePoint] {
        &self.points
    }

    /// Picks the step for a temperature, given the step we are currently on.
    ///
    /// Returns the index into `points`. Pass `None` when nothing has been
    /// applied yet, which skips hysteresis so the first decision lands
    /// directly on the right step instead of climbing to it.
    pub fn evaluate(&self, temp: i8, current: Option<usize>) -> usize {
        // Highest step whose plain threshold the temperature has reached.
        let bare = self
            .points
            .iter()
            .enumerate()
            .filter(|(_, p)| temp >= p.temp)
            .map(|(i, _)| i)
            .next_back()
            .unwrap_or(0);

        let Some(current) = current.filter(|c| *c < self.points.len()) else {
            return bare;
        };

        if bare > current {
            // Going up: the target has to be met by its own margin as well.
            let target = &self.points[bare];
            if temp >= target.temp.saturating_add(target.hyst_up) {
                bare
            } else {
                current
            }
        } else if bare < current {
            // Coming down: stay put until we are clear of where we are, not of
            // where we are heading. Falling off the step we occupy is what
            // stops the fan oscillating between two neighboring speeds.
            let here = &self.points[current];
            if temp <= here.temp.saturating_sub(here.hyst_down) {
                bare
            } else {
                current
            }
        } else {
            current
        }
    }

    /// The level a step asks for.
    pub fn level_at(&self, index: usize) -> u8 {
        self.points[index.min(self.points.len() - 1)].level
    }
}

impl Default for Curve {
    /// Silent below 46 C, decisive above it, and the firmware takes over at
    /// the top instead of us chasing a spike we cannot win.
    fn default() -> Self {
        Curve::new(vec![
            CurvePoint::new(46, 0).with_hysteresis(0, 3),
            CurvePoint::new(52, 1).with_hysteresis(0, 5),
            CurvePoint::new(60, 2).with_hysteresis(0, 5),
            CurvePoint::new(68, 3).with_hysteresis(0, 6),
            CurvePoint::new(76, 4).with_hysteresis(0, 6),
            CurvePoint::new(84, 5).with_hysteresis(0, 6),
            CurvePoint::new(90, FAN_BIOS).with_hysteresis(0, 7),
        ])
        .expect("the default curve is valid")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve() -> Curve {
        Curve::new(vec![
            CurvePoint::new(50, 0).with_hysteresis(0, 5),
            CurvePoint::new(60, 2).with_hysteresis(0, 5),
            CurvePoint::new(70, 4).with_hysteresis(0, 5),
            CurvePoint::new(80, FAN_BIOS).with_hysteresis(0, 5),
        ])
        .unwrap()
    }

    #[test]
    fn cold_start_lands_directly_on_the_right_step() {
        // No climbing through intermediate levels on the first decision.
        assert_eq!(curve().evaluate(75, None), 2);
    }

    #[test]
    fn ramps_up_as_soon_as_the_threshold_is_met() {
        let c = curve();
        assert_eq!(c.evaluate(60, Some(0)), 1);
        assert_eq!(c.evaluate(70, Some(1)), 2);
    }

    #[test]
    fn holds_its_step_until_clear_of_hysteresis() {
        let c = curve();
        // On step 2 (70 C, drop at 65). 66 is not far enough down.
        assert_eq!(c.evaluate(66, Some(2)), 2);
        assert_eq!(c.evaluate(65, Some(2)), 1);
    }

    #[test]
    fn a_single_point_is_refused_however_it_arrives() {
        // Not a curve: one level applied at every temperature there is, with
        // the firmware's own management switched off because a level is set.
        // The shipped Balanced curve starts at level 0, so deleting its other
        // points would leave the fan off at 100 degrees.
        //
        // Checked here rather than only in the editor, because a hand-edited
        // config.json and an imported ini both reach the engine without
        // passing through it.
        assert!(matches!(
            Curve::new(vec![CurvePoint::new(50, 0)]),
            Err(CurveError::TooFewPoints { count: 1 })
        ));
        assert!(matches!(
            Curve::new(vec![CurvePoint::new(50, FAN_BIOS)]),
            Err(CurveError::TooFewPoints { count: 1 })
        ));

        // Two is enough to be a curve, and still has to satisfy the rest.
        assert!(Curve::new(vec![CurvePoint::new(50, 0), CurvePoint::new(80, FAN_BIOS)]).is_ok());
    }

    #[test]
    fn the_firmware_takes_over_above_the_ceiling_whatever_the_curve_says() {
        // A curve is not required to end by handing the fan back, and above
        // its last step it holds that step's level. The three that ship hand
        // over at 88, 90 and 93, so none of them ever reaches the ceiling.
        for profile in [Curve::default()] {
            let hottest = profile.points().last().expect("a curve has points");
            assert!(
                hottest.is_bios() || hottest.temp < crate::engine::SMART_CEILING_C,
                "a shipped curve is still deciding at the ceiling"
            );
        }

        assert!(crate::engine::SMART_CEILING_C > crate::engine::MANUAL_ESCAPE_C);
    }

    #[test]
    fn does_not_hunt_between_neighboring_steps() {
        // The failure this hysteresis exists to prevent: a temperature sitting
        // just under a threshold must not flip back and forth.
        let c = curve();
        let mut at = c.evaluate(72, None);
        for temp in [69, 71, 68, 72, 67, 70] {
            let next = c.evaluate(temp, Some(at));
            assert_eq!(next, at, "stepped at {temp} C when it should have held");
            at = next;
        }
    }

    #[test]
    fn evaluation_ignores_the_hardware_entirely() {
        // Same inputs, same answer, no matter what the fan register says.
        // This is the property TPFanControl lacked.
        let c = curve();
        assert_eq!(c.evaluate(75, Some(1)), 2);
        assert_eq!(c.evaluate(75, Some(1)), 2);
    }

    #[test]
    fn hands_over_to_firmware_at_the_top() {
        let c = curve();
        let idx = c.evaluate(85, None);
        assert_eq!(c.level_at(idx), FAN_BIOS);
        assert!(c.points()[idx].is_bios());
    }

    #[test]
    fn below_the_first_point_stays_on_the_first_step() {
        assert_eq!(curve().evaluate(20, None), 0);
    }

    #[test]
    fn rejects_curves_that_do_not_climb() {
        let e = Curve::new(vec![CurvePoint::new(60, 1), CurvePoint::new(50, 2)]);
        assert_eq!(e.unwrap_err(), CurveError::NotAscending { index: 1 });
    }

    #[test]
    fn rejects_the_disengaged_level() {
        // 0x40 runs the blower unregulated. No curve gets to ask for it.
        let e = Curve::new(vec![CurvePoint::new(90, yamato_ec::FAN_DISENGAGED)]);
        assert_eq!(e.unwrap_err(), CurveError::IllegalLevel { index: 0, level: 0x40 });
    }

    #[test]
    fn rejects_empty_curves() {
        assert_eq!(Curve::new(vec![]).unwrap_err(), CurveError::Empty);
    }

    #[test]
    fn the_default_curve_is_quiet_at_idle_and_defers_high_up() {
        let c = Curve::default();
        assert_eq!(c.level_at(c.evaluate(40, None)), 0);
        assert_eq!(c.level_at(c.evaluate(95, None)), FAN_BIOS);
    }

    #[test]
    fn a_curve_cannot_ease_the_fan_off_as_it_heats() {
        // Found by audit: the editor clamped a dragged point but not an added
        // one, and nothing downstream compared levels at all. A hand-edited
        // file and an imported curve reach the engine the same way, so the
        // rule belongs here rather than in the window.
        let backwards = Curve::new(vec![
            CurvePoint::new(60, 2),
            CurvePoint::new(76, 4),
            CurvePoint::new(82, 1),
        ]);

        assert_eq!(backwards.unwrap_err(), CurveError::Backwards { index: 2 });

        // Level steps at the same height are fine: that is a plateau.
        assert!(Curve::new(vec![
            CurvePoint::new(60, 2),
            CurvePoint::new(70, 2),
            CurvePoint::new(80, 5),
        ])
        .is_ok());

        // The firmware step is a handoff, not a slower fan, so it is exempt
        // both above and below.
        assert!(Curve::new(vec![
            CurvePoint::new(60, 5),
            CurvePoint::new(90, yamato_ec::FAN_BIOS),
        ])
        .is_ok());
    }
}
