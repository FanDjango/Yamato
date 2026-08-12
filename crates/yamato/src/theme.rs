// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! Colors and metrics, in one place so the window and the canvas agree.
//!
//! Dark by default and not configurable. A fan utility that lives in the tray
//! and gets opened for thirty seconds at a time does not need a theme engine.

use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;

const fn rgb(hex: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

const fn rgba(hex: u32, a: f32) -> D2D1_COLOR_F {
    let c = rgb(hex);

    D2D1_COLOR_F { a, ..c }
}

/// Window ground. A shade deeper than the panels, so raised surfaces read as
/// raised without needing drop shadows.
pub const BACKGROUND: D2D1_COLOR_F = rgb(0x0f1116);
/// Raised surfaces: the graph, panels.
pub const SURFACE: D2D1_COLOR_F = rgb(0x161921);
/// A faint top-light laid over the ground. Alpha does the work; anything
/// stronger and the window starts looking like a gradient, not a room.
pub const GROUND_SHEEN: D2D1_COLOR_F = rgba(0xffffff, 0.03);
/// 1px panel edges. Low-alpha white reads as a lit edge on any dark ground,
/// which is what separates a panel from a flat rectangle.
pub const BORDER: D2D1_COLOR_F = rgba(0xffffff, 0.08);
/// Hairlines between sections inside a panel. Quieter than the panel edge.
pub const DIVIDER: D2D1_COLOR_F = rgba(0xffffff, 0.06);
/// The grid. It is furniture, not content, so it sits just above invisible
/// and lets the curve carry the canvas.
pub const GRID: D2D1_COLOR_F = rgba(0xffffff, 0.04);
pub const GRID_STRONG: D2D1_COLOR_F = rgba(0xffffff, 0.10);
/// Body text.
pub const TEXT: D2D1_COLOR_F = rgb(0xe8eaf0);
/// Secondary text: captions, axis labels, row labels.
pub const TEXT_DIM: D2D1_COLOR_F = rgb(0x99a0ae);
/// Tertiary text: section labels, footnotes. Present, but not asking.
pub const TEXT_FAINT: D2D1_COLOR_F = rgb(0x5f6674);

/// The accent, used sparingly so it means something.
pub const ACCENT: D2D1_COLOR_F = rgb(0xce1b22);
pub const ACCENT_BRIGHT: D2D1_COLOR_F = rgb(0xff6e6e);

/// Bright yellow for user-changeable settings.
pub const YELLOW: D2D1_COLOR_F = rgb(0xffeb3b);

/// Thermal bands, matching the tray tints exactly: green while things are
/// fine, amber warming, red hot.
pub const COOL: D2D1_COLOR_F = rgb(COOL_HEX);
pub const WARM: D2D1_COLOR_F = rgb(WARM_HEX);
pub const HOT: D2D1_COLOR_F = rgb(HOT_HEX);

/// The same three as plain hex, plus the gray for "not driving the fan".
///
/// The tray icon is drawn with GDI, which has never heard of a Direct2D color,
/// and two lists of the same colors would not stay the same list.
pub const COOL_HEX: u32 = 0x22a844;
pub const WARM_HEX: u32 = 0xe07c10;
pub const HOT_HEX: u32 = 0xd62820;
pub const IDLE_HEX: u32 = 0x8a8f98;

/// Where the bands change. Shared with the tray so the graph and the icon
/// never disagree about what "warm" means.
pub const WARM_AT: f32 = 70.0;
pub const HOT_AT: f32 = 85.0;

/// The band color for a temperature, so the curve is readable at a glance.
pub fn band(temp: f32) -> D2D1_COLOR_F {
    if temp >= HOT_AT {
        HOT
    } else if temp >= WARM_AT {
        WARM
    } else {
        COOL
    }
}

/// Graph extents. Wider than any sane curve so points never sit on the edge.
pub const TEMP_MIN: f32 = 30.0;
pub const TEMP_MAX: f32 = 100.0;

/// Fan levels 0..=7, with the firmware handoff drawn as one step above 7 so it
/// has somewhere to live on the axis.
pub const LEVEL_MAX: f32 = 8.0;

/// Radius of a draggable point, and how close the mouse has to be to grab it.
/// The grab radius stays generous while the drawn point slims down; a target
/// and a mark are different jobs.
pub const POINT_RADIUS: f32 = 6.0;
pub const GRAB_RADIUS: f32 = 14.0;

/// One corner radius for every panel, so nothing argues about roundness.
pub const RADIUS: f32 = 8.0;

/// The spacing scale. Every gap in the window is one of these, which is most
/// of what makes a layout feel deliberate rather than nudged into place.
pub const SPACE_XS: f32 = 4.0;
pub const SPACE_SM: f32 = 8.0;
pub const SPACE_MD: f32 = 12.0;
pub const SPACE_LG: f32 = 16.0;
pub const SPACE_XL: f32 = 24.0;
pub const SPACE_XXL: f32 = 32.0;

/// Window margin, on the same scale as everything else.
pub const PADDING: f32 = SPACE_XL;

/// Room to the left of the graph for the fan-level labels, and below it for
/// the temperature scale. Cramped axis labels were the first thing that read
/// as unfinished.
pub const AXIS_GUTTER: f32 = 56.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_decode_from_hex_correctly() {
        let white = rgb(0xffffff);
        assert!((white.r - 1.0).abs() < f32::EPSILON);
        assert!((white.g - 1.0).abs() < f32::EPSILON);
        assert!((white.b - 1.0).abs() < f32::EPSILON);
        assert!((white.a - 1.0).abs() < f32::EPSILON);

        let accent = ACCENT;
        assert!(accent.r > accent.g && accent.r > accent.b, "the accent should read as red");
    }

    #[test]
    fn thermal_bands_agree_with_the_tray() {
        // The graph and the tray icon must never disagree about "warm".
        assert_eq!(WARM_AT as i8, crate::tray::WARM_AT);
        assert_eq!(HOT_AT as i8, crate::tray::HOT_AT);
    }

    #[test]
    fn band_selection_is_ordered() {
        let cool = band(50.0);
        let warm = band(75.0);
        let hot = band(95.0);

        assert!(cool.g > cool.r, "cool should be green");
        assert!(hot.r > hot.g, "hot should be red");
        assert!(warm.r > warm.b && warm.g > warm.b, "warm should be amber");
    }

    #[test]
    fn the_graph_covers_every_reachable_temperature() {
        // A curve point outside the axes would be undraggable.
        assert!(TEMP_MIN < 40.0 && TEMP_MAX >= 100.0);
    }
}
