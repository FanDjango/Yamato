// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! The fan curve, as something you drag rather than something you type.
//!
//! The reason for the rewrite. The program this replaces made you edit
//! `Level=50 0 0 3` lines in an ini file and restart to see what you had done.
//!
//! The geometry and interaction live here, separate from the drawing, because
//! the fiddly parts are the parts worth testing: mapping between temperatures
//! and pixels, deciding what the mouse grabbed, and keeping the curve valid
//! while a point is being dragged.

use yamato_core::{Curve, CurveError, CurvePoint, HYST_DOWN_MAX, HYST_UP_MAX};
use yamato_ec::{FAN_BIOS, FAN_LEVEL_MAX};

use crate::theme;

/// A rectangle in pixels. Kept local so the logic does not depend on Win32.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Rect {
    pub fn width(&self) -> f32 {
        self.right - self.left
    }

    pub fn height(&self) -> f32 {
        self.bottom - self.top
    }
}

/// What the pointer is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grab {
    None,
    /// Index of the point being dragged.
    Point(usize),
}

/// Levels a point can hold, in the order the axis shows them.
///
/// `FAN_BIOS` sits one step above level 7. It is not a faster fan but a
/// handback to the firmware, which reacts faster than any polling loop.
/// `0x40`, the disengaged level, is absent and cannot be chosen.
pub fn level_for_axis(slot: u8) -> u8 {
    if slot > FAN_LEVEL_MAX {
        FAN_BIOS
    } else {
        slot
    }
}

/// Inverse of [`level_for_axis`], for placing a stored point on the graph.
pub fn axis_for_level(level: u8) -> f32 {
    if level == FAN_BIOS {
        (FAN_LEVEL_MAX + 1) as f32
    } else {
        level.min(FAN_LEVEL_MAX) as f32
    }
}

pub struct Editor {
    points: Vec<CurvePoint>,
    grab: Grab,
    /// Latest reading, drawn on the graph so you can see where you sit.
    live_temp: Option<i8>,
    dirty: bool,
    /// Which point the hysteresis rows are talking about.
    ///
    /// The only way the two per-point settings are reached: the graph already
    /// answers to drag, double-click and right-click, and a fourth gesture on
    /// it would be one too many.
    selected: Option<usize>,
}

impl Editor {
    pub fn new(curve: &Curve) -> Self {
        Editor {
            points: curve.points().to_vec(),
            grab: Grab::None,
            live_temp: None,
            dirty: false,
            selected: None,
        }
    }

    pub fn points(&self) -> &[CurvePoint] {
        &self.points
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn set_live_temp(&mut self, temp: Option<i8>) {
        self.live_temp = temp;
    }

    pub fn live_temp(&self) -> Option<i8> {
        self.live_temp
    }

    /// Validates what is on screen. The editor allows a curve to be *arranged*
    /// invalidly for a moment while dragging, but never saved that way.
    pub fn validate(&self) -> Result<Curve, CurveError> {
        Curve::new(self.points.clone())
    }

    // Coordinates -----------------------------------------------------------

    pub fn temp_to_x(&self, area: Rect, temp: f32) -> f32 {
        let t = (temp - theme::TEMP_MIN) / (theme::TEMP_MAX - theme::TEMP_MIN);

        area.left + t.clamp(0.0, 1.0) * area.width()
    }

    pub fn level_to_y(&self, area: Rect, slot: f32) -> f32 {
        let t = slot / theme::LEVEL_MAX;

        // Inverted: level 0 sits at the bottom, which is how anyone reading a
        // fan curve expects it.
        area.bottom - t.clamp(0.0, 1.0) * area.height()
    }

    pub fn x_to_temp(&self, area: Rect, x: f32) -> f32 {
        let t = ((x - area.left) / area.width()).clamp(0.0, 1.0);

        theme::TEMP_MIN + t * (theme::TEMP_MAX - theme::TEMP_MIN)
    }

    pub fn y_to_slot(&self, area: Rect, y: f32) -> f32 {
        let t = ((area.bottom - y) / area.height()).clamp(0.0, 1.0);

        t * theme::LEVEL_MAX
    }

    // Interaction -----------------------------------------------------------

    /// Which point, if any, is under the pointer.
    pub fn hit_test(&self, area: Rect, x: f32, y: f32) -> Option<usize> {
        let mut best: Option<(usize, f32)> = None;

        for (i, p) in self.points.iter().enumerate() {
            let px = self.temp_to_x(area, p.temp as f32);
            let py = self.level_to_y(area, axis_for_level(p.level));
            let d = ((px - x).powi(2) + (py - y).powi(2)).sqrt();

            if d <= theme::GRAB_RADIUS && best.map_or(true, |(_, bd)| d < bd) {
                best = Some((i, d));
            }
        }

        best.map(|(i, _)| i)
    }

    pub fn begin_drag(&mut self, area: Rect, x: f32, y: f32) -> bool {
        match self.hit_test(area, x, y) {
            Some(i) => {
                self.grab = Grab::Point(i);
                // Grabbing a point is also how it is chosen, so the rows that
                // show its hysteresis follow the pointer without needing a
                // gesture of their own.
                self.selected = Some(i);
                true
            }
            None => false,
        }
    }

    /// The point the hysteresis rows are about, if any.
    pub fn selected(&self) -> Option<usize> {
        self.selected.filter(|i| *i < self.points.len())
    }

    pub fn select(&mut self, index: Option<usize>) {
        self.selected = index.filter(|i| *i < self.points.len());
    }

    /// The chosen point, for reading its numbers out.
    pub fn selected_point(&self) -> Option<CurvePoint> {
        self.selected().map(|i| self.points[i])
    }

    /// Steps how far the machine must cool below this point before the curve
    /// drops off it, wrapping round at the top of its range.
    ///
    /// A generous one holds the fan higher for longer, which is the safe
    /// direction to be wrong in, so it has room to move.
    pub fn step_hyst_down(&mut self) {
        let Some(i) = self.selected() else { return };

        let next = self.points[i].hyst_down + 1;
        self.points[i].hyst_down = if next > HYST_DOWN_MAX { 0 } else { next };
        self.dirty = true;
    }

    /// Steps how far *above* this point's threshold the machine must get
    /// before the curve steps up to it.
    ///
    /// Held to a much shorter range than its opposite: this one delays the fan
    /// on a machine that is already climbing, which is the dangerous
    /// direction.
    pub fn step_hyst_up(&mut self) {
        let Some(i) = self.selected() else { return };

        let next = self.points[i].hyst_up + 1;
        self.points[i].hyst_up = if next > HYST_UP_MAX { 0 } else { next };
        self.dirty = true;
    }

    /// Moves the held point, keeping the curve in ascending order.
    ///
    /// Clamping between the neighbors instead of reordering means a point
    /// never jumps past another while you are dragging it, which would swap
    /// two rows out from under the pointer.
    pub fn drag_to(&mut self, area: Rect, x: f32, y: f32) {
        let Grab::Point(i) = self.grab else { return };

        let temp = self.x_to_temp(area, x).round();
        let slot = self.y_to_slot(area, y).round();

        // At least one degree of daylight either side, so two points can never
        // land on the same temperature.
        let low = if i == 0 {
            theme::TEMP_MIN
        } else {
            self.points[i - 1].temp as f32 + 1.0
        };
        let high = if i + 1 >= self.points.len() {
            theme::TEMP_MAX
        } else {
            self.points[i + 1].temp as f32 - 1.0
        };

        let temp = temp.clamp(low.min(high), high.max(low));

        self.points[i].temp = temp as i8;
        self.points[i].level = self.level_between_neighbors(i, slot);
        self.dirty = true;
    }

    /// A level that cannot make the curve run the fan slower as the machine
    /// gets hotter.
    ///
    /// Checking only the temperatures allowed level four at seventy-six
    /// degrees and level one at seventy-nine: the machine hotter and the fan
    /// nearly idle. The eighty degree escape belongs to manual mode and does
    /// not apply to a curve, so nothing else would have caught it. Held
    /// between its neighbors for the same reason its temperature already is:
    /// the curve stays sensible while it is dragged, instead of being refused
    /// once it is finished.
    fn level_between_neighbors(&self, i: usize, slot: f32) -> u8 {
        let wanted = slot.max(0.0) as u8;

        // The firmware step is the top of the axis and is not a slower fan, so
        // it bounds nothing below it.
        let floor = i
            .checked_sub(1)
            .and_then(|below| self.points.get(below))
            .filter(|p| p.level != FAN_BIOS)
            .map_or(0, |p| p.level);

        let ceiling = self
            .points
            .get(i + 1)
            .filter(|p| p.level != FAN_BIOS)
            .map_or(FAN_LEVEL_MAX + 1, |p| p.level);

        level_for_axis(wanted.clamp(floor, ceiling.max(floor)))
    }

    pub fn end_drag(&mut self) {
        self.grab = Grab::None;
    }

    pub fn is_dragging(&self) -> bool {
        self.grab != Grab::None
    }

    /// Adds a point where the pointer is, keeping the list ordered.
    pub fn add_point(&mut self, area: Rect, x: f32, y: f32) -> bool {
        let temp = self.x_to_temp(area, x).round() as i8;

        // A curve cannot hold two points at one temperature, but refusing on
        // the spot meant a double-click near an existing step did nothing and
        // said nothing about why, and near a step is where somebody aims when
        // putting a point back. Moved to the nearest free degree instead.
        let taken = |t: i8, points: &[CurvePoint]| points.iter().any(|p| p.temp == t);

        let temp = if taken(temp, &self.points) {
            let mut found = None;

            'search: for step in 1..=6i8 {
                for candidate in [temp.saturating_sub(step), temp.saturating_add(step)] {
                    if candidate >= theme::TEMP_MIN as i8
                        && candidate <= theme::TEMP_MAX as i8
                        && !taken(candidate, &self.points)
                    {
                        found = Some(candidate);
                        break 'search;
                    }
                }
            }

            // Nowhere to go: every degree within reach is already a step, and
            // one more would be indistinguishable from its neighbors anyway.
            match found {
                Some(t) => t,
                None => return false,
            }
        } else {
            temp
        };

        let at = self
            .points
            .iter()
            .position(|p| p.temp > temp)
            .unwrap_or(self.points.len());

        // Hysteresis is inherited from the neighbor below, or from the one
        // above when this is the new first point. A tuned or imported curve
        // has its own idea of how eagerly the fan should change speed, and a
        // new point carrying the generic defaults would behave differently
        // from everything around it with nothing on screen saying why.
        let neighbor = at.checked_sub(1).or(Some(at)).and_then(|i| self.points.get(i));

        // Held between its neighbors, exactly as dragging one is. Without
        // this a point could be dropped below the step under it, giving a
        // curve that eases the fan off as the machine heats. Computed at the
        // insertion point, since that is what decides the neighbors.
        let level = {
            let wanted = self.y_to_slot(area, y).round().max(0.0) as u8;

            let floor = at
                .checked_sub(1)
                .and_then(|below| self.points.get(below))
                .filter(|p| p.level != FAN_BIOS)
                .map_or(0, |p| p.level);

            let ceiling = self
                .points
                .get(at)
                .filter(|p| p.level != FAN_BIOS)
                .map_or(FAN_LEVEL_MAX + 1, |p| p.level);

            level_for_axis(wanted.clamp(floor, ceiling.max(floor)))
        };

        let mut fresh = CurvePoint::new(temp, level);
        if let Some(neighbor) = neighbor {
            fresh.hyst_up = neighbor.hyst_up;
            fresh.hyst_down = neighbor.hyst_down;
        }

        self.points.insert(at, fresh);
        self.selected = Some(at);
        self.dirty = true;

        true
    }

    /// Removes a point. The last one cannot go: a curve with no points is not
    /// a curve, and the engine would have nothing to follow.
    pub fn remove_point(&mut self, index: usize) -> bool {
        if self.points.len() <= 1 || index >= self.points.len() {
            return false;
        }

        self.points.remove(index);
        self.dirty = true;

        // The selection is an index into a list that just got shorter, so it
        // moves with the points or is dropped. Left alone it would name a
        // different point and the hysteresis rows would edit that one.
        self.selected = match self.selected {
            Some(i) if i == index => None,
            Some(i) if i > index => Some(i - 1),
            other => other,
        };

        true
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect { left: 0.0, top: 0.0, right: 700.0, bottom: 350.0 }
    }

    fn editor() -> Editor {
        Editor::new(&Curve::default())
    }

    #[test]
    fn temperature_maps_to_pixels_and_back() {
        let e = editor();
        let a = area();

        for temp in [35.0f32, 50.0, 72.5, 99.0] {
            let x = e.temp_to_x(a, temp);
            let back = e.x_to_temp(a, x);
            assert!((back - temp).abs() < 0.5, "{temp} came back as {back}");
        }
    }

    #[test]
    fn level_zero_is_at_the_bottom_of_the_graph() {
        // Anyone reading a fan curve expects up to mean faster.
        let e = editor();
        let a = area();
        assert!(e.level_to_y(a, 0.0) > e.level_to_y(a, 7.0));
    }

    #[test]
    fn the_firmware_step_sits_above_level_seven() {
        assert!(axis_for_level(FAN_BIOS) > axis_for_level(FAN_LEVEL_MAX));
        assert_eq!(level_for_axis(8), FAN_BIOS);
        assert_eq!(level_for_axis(7), 7);
    }

    #[test]
    fn the_disengaged_level_cannot_be_chosen_from_the_axis() {
        // 0x40 runs the blower unregulated and is documented as potentially
        // damaging. No slot on the axis produces it.
        for slot in 0..=255u8 {
            assert_ne!(level_for_axis(slot), yamato_ec::FAN_DISENGAGED);
        }
    }

    #[test]
    fn clicking_a_point_grabs_it() {
        let mut e = editor();
        let a = area();
        let p = e.points()[2];
        let x = e.temp_to_x(a, p.temp as f32);
        let y = e.level_to_y(a, axis_for_level(p.level));

        assert_eq!(e.hit_test(a, x, y), Some(2));
        assert!(e.begin_drag(a, x, y));
        assert!(e.is_dragging());
    }

    #[test]
    fn clicking_empty_space_grabs_nothing() {
        let mut e = editor();
        let a = area();
        assert!(!e.begin_drag(a, 5.0, 5.0));
        assert!(!e.is_dragging());
    }

    #[test]
    fn a_dragged_point_cannot_cross_its_neighbors() {
        // Crossing would swap two rows out from under the pointer and produce
        // a curve that no longer ascends.
        let mut e = editor();
        let a = area();
        let before = e.points()[1].temp;
        let after = e.points()[3].temp;

        let p = e.points()[2];
        e.begin_drag(a, e.temp_to_x(a, p.temp as f32), e.level_to_y(a, axis_for_level(p.level)));

        // Yank it hard to the left, past its left neighbor.
        e.drag_to(a, 0.0, 100.0);
        assert!(e.points()[2].temp > before, "point crossed the one below it");

        // And hard to the right.
        e.drag_to(a, 10_000.0, 100.0);
        assert!(e.points()[2].temp < after, "point crossed the one above it");

        assert!(e.validate().is_ok(), "dragging produced an invalid curve");
    }

    #[test]
    fn dragging_never_produces_a_curve_that_will_not_save() {
        let mut e = editor();
        let a = area();

        for i in 0..e.points().len() {
            let p = e.points()[i];
            e.begin_drag(a, e.temp_to_x(a, p.temp as f32), e.level_to_y(a, axis_for_level(p.level)));

            for (x, y) in [(0.0, 0.0), (700.0, 350.0), (350.0, 0.0), (350.0, 350.0)] {
                e.drag_to(a, x, y);
                assert!(e.validate().is_ok(), "point {i} at ({x},{y}) broke the curve");
            }

            e.end_drag();
        }
    }

    #[test]
    fn adding_a_point_keeps_the_curve_ordered_and_valid() {
        let mut e = editor();
        let a = area();
        let before = e.points().len();

        assert!(e.add_point(a, e.temp_to_x(a, 63.0), e.level_to_y(a, 2.0)));
        assert_eq!(e.points().len(), before + 1);
        assert!(e.validate().is_ok());

        let temps: Vec<i8> = e.points().iter().map(|p| p.temp).collect();
        let mut sorted = temps.clone();
        sorted.sort_unstable();
        assert_eq!(temps, sorted, "adding a point left the curve out of order");
    }

    #[test]
    fn a_point_dropped_on_an_existing_one_lands_beside_it() {
        // Refusing outright made a double-click near a step do nothing, with
        // nothing said about why. Two points at one temperature is still
        // impossible; the new one moves.
        let mut e = editor();
        let a = area();
        let existing = e.points()[0].temp;
        let before = e.points().len();

        assert!(e.add_point(a, e.temp_to_x(a, existing as f32), e.level_to_y(a, 3.0)));
        assert_eq!(e.points().len(), before + 1, "nothing was added");

        // Still a valid curve, which is what refusing was protecting.
        let temps: Vec<i8> = e.points().iter().map(|p| p.temp).collect();
        let mut unique = temps.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(temps.len(), unique.len(), "two points share a temperature");
        assert!(e.validate().is_ok(), "the curve no longer validates");
    }

    #[test]
    fn the_last_point_cannot_be_removed() {
        // An empty curve would leave the engine with nothing to follow.
        let mut e = Editor::new(&Curve::new(vec![CurvePoint::new(50, 1)]).unwrap());
        assert!(!e.remove_point(0));
        assert_eq!(e.points().len(), 1);
    }

    #[test]
    fn removing_a_point_leaves_a_valid_curve() {
        let mut e = editor();
        assert!(e.remove_point(2));
        assert!(e.validate().is_ok());
    }

    #[test]
    fn a_new_point_takes_after_its_neighbor_rather_than_the_defaults() {
        // A tuned or imported curve has its own idea of how eagerly the fan
        // should change speed. A point added into the middle of it carrying
        // the generic 0 and 4 would behave differently from everything around
        // it, with nothing on screen saying so.
        let curve = Curve::new(vec![
            CurvePoint::new(50, 1).with_hysteresis(2, 7),
            CurvePoint::new(80, 5).with_hysteresis(1, 9),
        ])
        .unwrap();

        let mut e = Editor::new(&curve);
        let a = area();

        assert!(e.add_point(a, e.temp_to_x(a, 65.0), e.level_to_y(a, 3.0)));

        let added = e.points()[1];
        assert_eq!(added.temp, 65);
        assert_eq!((added.hyst_up, added.hyst_down), (2, 7), "took the defaults, not the curve");
    }

    #[test]
    fn a_new_first_point_takes_after_the_one_above_it() {
        // There is nothing below it to copy, and the alternative is the same
        // silent mixing at the other end of the curve.
        let curve = Curve::new(vec![CurvePoint::new(60, 2).with_hysteresis(3, 8)]).unwrap();
        let mut e = Editor::new(&curve);
        let a = area();

        assert!(e.add_point(a, e.temp_to_x(a, 40.0), e.level_to_y(a, 1.0)));

        let added = e.points()[0];
        assert_eq!(added.temp, 40);
        assert_eq!((added.hyst_up, added.hyst_down), (3, 8));
    }

    #[test]
    fn dragging_a_point_leaves_its_hysteresis_alone() {
        let curve = Curve::new(vec![
            CurvePoint::new(50, 1).with_hysteresis(2, 7),
            CurvePoint::new(80, 5).with_hysteresis(1, 9),
        ])
        .unwrap();

        let mut e = Editor::new(&curve);
        let a = area();

        e.begin_drag(a, e.temp_to_x(a, 50.0), e.level_to_y(a, 1.0));
        e.drag_to(a, e.temp_to_x(a, 55.0), e.level_to_y(a, 3.0));
        e.end_drag();

        let moved = e.points()[0];
        assert_eq!(moved.temp, 55);
        assert_eq!(moved.level, 3);
        assert_eq!((moved.hyst_up, moved.hyst_down), (2, 7));
    }

    #[test]
    fn stepping_hysteresis_stays_inside_what_the_loader_allows() {
        // The two ranges are different sizes on purpose: a late drop is a
        // quiet fan, a late climb is a hot machine.
        let mut e = editor();
        e.select(Some(0));

        for _ in 0..(HYST_DOWN_MAX as usize + 2) * 3 {
            e.step_hyst_down();
            assert!((0..=HYST_DOWN_MAX).contains(&e.points()[0].hyst_down));
        }

        let mut highest_up = 0;
        for _ in 0..(HYST_UP_MAX as usize + 2) * 3 {
            e.step_hyst_up();
            let up = e.points()[0].hyst_up;
            assert!((0..=HYST_UP_MAX).contains(&up));
            highest_up = highest_up.max(up);
        }

        // The climb has less room than the drop, and this is where that shows.
        assert!(highest_up < HYST_DOWN_MAX, "the two ranges should not be the same size");
    }

    #[test]
    fn stepping_wraps_round_so_a_value_can_be_walked_back_down() {
        // The only gesture is a click, so the way back to zero is round the
        // top. A control you can only ever increase is a trap.
        let mut e = editor();
        e.select(Some(0));

        for _ in 0..=HYST_UP_MAX {
            e.step_hyst_up();
        }

        assert_eq!(e.points()[0].hyst_up, 0);
    }

    #[test]
    fn nothing_selected_means_nothing_is_edited() {
        let mut e = editor();
        let before = e.points()[0];

        e.select(None);
        e.step_hyst_down();
        e.step_hyst_up();

        assert_eq!(e.points()[0].hyst_down, before.hyst_down);
        assert!(!e.is_dirty());
    }

    #[test]
    fn removing_a_point_does_not_leave_the_selection_pointing_at_another() {
        // Otherwise the rows quietly start editing a point nobody chose.
        let mut e = editor();

        e.select(Some(3));
        e.remove_point(3);
        assert_eq!(e.selected(), None, "the selection outlived the point");

        e.select(Some(3));
        e.remove_point(1);
        assert_eq!(e.selected(), Some(2), "the selection did not move with the list");
    }

    #[test]
    fn clicking_a_point_selects_it() {
        let mut e = editor();
        let a = area();
        let p = e.points()[2];

        e.begin_drag(a, e.temp_to_x(a, p.temp as f32), e.level_to_y(a, axis_for_level(p.level)));

        assert_eq!(e.selected(), Some(2));
        assert_eq!(e.selected_point().map(|p| p.temp), Some(p.temp));
    }

    #[test]
    fn edits_mark_the_editor_dirty_until_saved() {
        let mut e = editor();
        assert!(!e.is_dirty());

        e.add_point(area(), e.temp_to_x(area(), 55.0), e.level_to_y(area(), 1.0));
        assert!(e.is_dirty());

        e.mark_saved();
        assert!(!e.is_dirty());
    }

    #[test]
    fn a_step_removed_from_the_middle_can_be_put_back() {
        // Reported from the machine: a point could be taken out but not added
        // again between its neighbors.
        let curve = Curve::new(vec![
            CurvePoint::new(46, 0),
            CurvePoint::new(60, 2),
            CurvePoint::new(76, 4),
        ])
        .unwrap();

        let mut editor = Editor::new(&curve);
        let area = Rect { left: 0.0, top: 0.0, right: 800.0, bottom: 400.0 };

        // Where the middle point sits on screen, before it goes.
        let x = editor.temp_to_x(area, 60.0);
        let y = editor.level_to_y(area, axis_for_level(2));

        let middle = editor.hit_test(area, x, y).expect("the middle point should be hittable");
        editor.remove_point(middle);
        assert_eq!(editor.points().len(), 2);

        // Then back again, at the very same place.
        assert!(editor.add_point(area, x, y), "a removed point could not be added back");
        assert_eq!(editor.points().len(), 3);

        let put_back = editor.points().iter().find(|p| p.temp == 60).expect("not at 60 C");
        assert_eq!(put_back.level, 2, "it came back at the wrong level");
    }

    #[test]
    fn a_point_cannot_be_dragged_into_running_the_fan_slower_when_hotter() {
        // Seen on the machine: level four at seventy-six and level one at
        // seventy-nine, so the fan went nearly idle as the CPU passed eighty.
        // Only the temperatures were ever checked.
        let curve = Curve::new(vec![
            CurvePoint::new(60, 2),
            CurvePoint::new(76, 4),
            CurvePoint::new(84, 5),
        ])
        .unwrap();

        let mut e = Editor::new(&curve);
        let a = area();

        // Take hold of the middle one and drag it to the floor.
        let x = e.temp_to_x(a, 76.0);
        let y = e.level_to_y(a, axis_for_level(4));
        assert!(e.begin_drag(a, x, y));

        e.drag_to(a, e.temp_to_x(a, 79.0), e.level_to_y(a, 1.0));
        e.end_drag();

        let levels: Vec<u8> = e.points().iter().map(|p| p.level).collect();
        assert!(
            levels.windows(2).all(|w| w[0] <= w[1] || w[1] == FAN_BIOS),
            "the curve runs the fan slower as it gets hotter: {levels:?}"
        );
    }
}
