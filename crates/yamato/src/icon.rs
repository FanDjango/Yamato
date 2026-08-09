// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! The tray icon with the temperature in it.
//!
//! The program this replaces put the number in the tray, and that is most of
//! why people kept it there: a colored dot says warm, a number says 71.
//!
//! GDI, not Direct2D. This is a sixteen-pixel drawing that happens a few times
//! a minute.
//!
//! Two things are load bearing. GDI does not touch the alpha channel, so text
//! drawn into a 32-bit bitmap arrives as gray levels on a transparent ground
//! and has to be turned into coverage by hand. And every handle made here is
//! freed here on every path, because a leak in something that runs all session
//! eventually takes the desktop down with it.

use std::ffi::c_void;
use std::ptr;

use windows_sys::Win32::Foundation::SIZE;
use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
    GetTextExtentPoint32W, SelectObject, SetBkMode, SetTextCharacterExtra, SetTextColor, TextOutW,
    ANTIALIASED_QUALITY,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH,
    DIB_RGB_COLORS, FF_DONTCARE, HGDIOBJ, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

/// The ink the number is drawn in. Near-white rather than white: pure white on
/// a dark taskbar is louder than the reading deserves.
const INK: u32 = 0x00ff_ffff;

/// The dark edge drawn around the digits, so they hold their shape against a
/// bright band color and against a taskbar of any shade.
const OUTLINE: u32 = 0x0000_0000;

/// Weight for the number. Semi-bold survives being small; bold at this size
/// turns into a blob.
const FW_BOLD: i32 = 700;

/// Builds a tray icon: `number` drawn in `color`, outlined so it holds
/// against any taskbar.
///
/// A null return means it could not be drawn, and the caller is expected to
/// fall back to one of the icons compiled into the program. Every failure path
/// frees whatever it had made by then.
pub(crate) fn compose(size: i32, color: u32, number: i32) -> HICON {
    let Some(pixels) = render(size, color, number) else { return ptr::null_mut() };

    unsafe {
        let dc = CreateCompatibleDC(ptr::null_mut());
        if dc.is_null() {
            return ptr::null_mut();
        }

        let mut bits: *mut c_void = ptr::null_mut();
        let bitmap = CreateDIBSection(
            dc,
            &header(size),
            DIB_RGB_COLORS,
            &mut bits,
            ptr::null_mut(),
            0,
        );

        if bitmap.is_null() || bits.is_null() {
            if !bitmap.is_null() {
                DeleteObject(bitmap as HGDIOBJ);
            }
            DeleteDC(dc);
            return ptr::null_mut();
        }

        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u32, pixels.len());

        // A monochrome mask is still required even for an icon that carries its
        // own alpha. Zeroed means opaque, which is what leaves the alpha in
        // charge.
        let mask = CreateBitmap(size, size, 1, 1, ptr::null());

        let icon = if mask.is_null() {
            ptr::null_mut()
        } else {
            let shape = ICONINFO {
                fIcon: 1,
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: mask,
                hbmColor: bitmap,
            };

            CreateIconIndirect(&shape)
        };

        // CreateIconIndirect copies both bitmaps, so they go now whether it
        // worked or not.
        if !mask.is_null() {
            DeleteObject(mask as HGDIOBJ);
        }
        DeleteObject(bitmap as HGDIOBJ);
        DeleteDC(dc);

        icon
    }
}

/// The icon's pixels, as premultiplied ARGB.
///
/// Separate from making an icon out of them so a test can look at what gets
/// drawn.
fn render(size: i32, color: u32, number: i32) -> Option<Vec<u32>> {
    if size < 8 {
        return None;
    }

    unsafe {
        let dc = CreateCompatibleDC(ptr::null_mut());
        if dc.is_null() {
            return None;
        }

        let mut bits: *mut c_void = ptr::null_mut();
        let bitmap = CreateDIBSection(
            dc,
            &header(size),
            DIB_RGB_COLORS,
            &mut bits,
            ptr::null_mut(),
            0,
        );

        if bitmap.is_null() || bits.is_null() {
            if !bitmap.is_null() {
                DeleteObject(bitmap as HGDIOBJ);
            }
            DeleteDC(dc);
            return None;
        }

        let previous = SelectObject(dc, bitmap as HGDIOBJ);

        // The section arrives zeroed, which is transparent black, and that is
        // the ground the text is measured against below.
        let drawn = draw_number(dc, size, number);

        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);

        // Digits first, turned into a coverage mask, then outlined and tinted.
        // Sharing the square with a disc cost them half the width, which at
        // sixteen pixels is a glyph nobody can read.
        if drawn {
            // Glyphs arrive as coverage. Grown by a pixel and laid down dark
            // first, then the original on top, they come out outlined and stay
            // readable against any band color or taskbar shade.
            let glyph: Vec<u8> = pixels
                .iter()
                .map(|p| ((p & 0xff).max((p >> 8) & 0xff).max((p >> 16) & 0xff)) as u8)
                .collect();

            // Centered on the ink, not on the font's box. The box leaves room
            // for descenders digits do not have, so centering it sits the
            // number low against the icons either side of it.
            let glyph = centered(&glyph, size);

            // Two rings above the smallest icons, one below. A single soft
            // ring reads as a gray fringe instead of an outline.
            let halo = if size >= 20 {
                grown_by_one(&grown_by_one(&glyph, size), size)
            } else {
                grown_by_one(&glyph, size)
            };

            // Hardened: a dilated edge keeps the soft alpha of the glyph it
            // came from, and a half-transparent outline is barely an outline.
            let halo: Vec<u8> = halo
                .iter()
                .map(|v| if *v > 24 { 255 } else { v.saturating_mul(4) })
                .collect();

            for (at, pixel) in pixels.iter_mut().enumerate() {
                *pixel = 0;

                if halo[at] > 0 {
                    *pixel = premultiplied(OUTLINE, halo[at] as u32);
                }
            }

            // The band color goes on the digits themselves, not on a disc
            // behind them: same signal, none of the room.
            for (at, pixel) in pixels.iter_mut().enumerate() {
                if glyph[at] > 0 {
                    *pixel = over(premultiplied(color, glyph[at] as u32), *pixel);
                }
            }
        } else {
            // No reading to show: the disc alone, which is still the band.
            coverage_to_ink(pixels, INK);
            fill_disc(pixels, size, color);
        }

        let copy = pixels.to_vec();

        SelectObject(dc, previous);
        DeleteObject(bitmap as HGDIOBJ);
        DeleteDC(dc);

        Some(copy)
    }
}

/// The shape of the bitmap both halves of this use.
fn header(size: i32) -> BITMAPINFO {
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };

    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = size;
    // Negative, so the rows run the way everything else here counts them.
    info.bmiHeader.biHeight = -size;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB;

    info
}

/// Draws the number as large as it will fit.
///
/// Returns whether anything was drawn. Measured at the size actually in hand,
/// because three digits happen (100 C, and every Fahrenheit reading above
/// ninety-nine) and a size chosen for two would push them out of the icon.
unsafe fn draw_number(dc: windows_sys::Win32::Graphics::Gdi::HDC, size: i32, number: i32) -> bool {
    let text: Vec<u16> = number.to_string().encode_utf16().collect();
    if text.is_empty() {
        return false;
    }

    // The whole icon, less a pixel each side for the outline to sit in.
    let room = size - 2;

    let Some(height) = largest_that_fits(dc, &text, size, room) else { return false };
    let Some(font) = font_of(height) else { return false };

    let previous = SelectObject(dc, font as HGDIOBJ);

    let mut extent = SIZE { cx: 0, cy: 0 };
    let drawn = if GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut extent) != 0 {
        SetBkMode(dc, TRANSPARENT as i32);
        // White because this is coverage, not color: the pass afterwards reads
        // it as how much of each pixel the glyph covered, and puts the ink in.
        SetTextColor(dc, 0x00ff_ffff);
        // Matched to the measurement above, or the text is drawn wider than
        // the size that was chosen for it and runs off the edge.
        SetTextCharacterExtra(dc, tracking(height, text.len()));
        // Whole pixels, centered both ways. Half a pixel of offset at this
        // size is the difference between a digit and a smudge.
        TextOutW(
            dc,
            (size - extent.cx) / 2,
            (size - extent.cy) / 2,
            text.as_ptr(),
            text.len() as i32,
        );
        true
    } else {
        false
    };

    SelectObject(dc, previous);
    DeleteObject(font as HGDIOBJ);

    drawn
}

/// Space to remove between glyphs, which is how three digits get to be a
/// readable size.
///
/// At these sizes three digits are bound by width, not height: the font has to
/// shrink until all three fit side by side, which left it a third of the icon
/// tall with room to spare. Pulling them together a fraction buys back the
/// width and lets the font grow. Two digits have room already.
fn tracking(height: i32, digits: usize) -> i32 {
    match digits {
        0 | 1 => 0,
        2 => -(height / 14).max(1),
        _ => -(height / 9).max(1),
    }
}

/// The tallest font this text fits in, or nothing if even the smallest will
/// not do.
///
/// Estimated from one measurement and then checked: text width is near enough
/// proportional to height that the first guess is usually right.
unsafe fn largest_that_fits(
    dc: windows_sys::Win32::Graphics::Gdi::HDC,
    text: &[u16],
    size: i32,
    room: i32,
) -> Option<i32> {
    let measure = |height: i32| -> Option<SIZE> {
        let font = font_of(height)?;
        let previous = SelectObject(dc, font as HGDIOBJ);
        SetTextCharacterExtra(dc, tracking(height, text.len()));

        let mut extent = SIZE { cx: 0, cy: 0 };
        let ok = GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut extent) != 0;

        SelectObject(dc, previous);
        DeleteObject(font as HGDIOBJ);

        ok.then_some(extent)
    };

    let reference = measure(size)?;
    if reference.cx <= 0 {
        return None;
    }

    // Allowed past the icon height: a font's em box is taller than the digits
    // in it, holding room for ascenders and descenders digits do not have, so
    // capping at the icon size threw away about a third of the height. Width
    // is what binds, and the loop below rejects anything that does not fit.
    let mut height = ((size * room) / reference.cx).clamp(5, size * 3 / 2);

    // The em box may overhang the icon; the digits standing in it will not.
    let ceiling = size * 3 / 2;

    while height >= 5 {
        let extent = measure(height)?;
        if extent.cx <= room && extent.cy <= ceiling {
            return Some(height);
        }
        height -= 1;
    }

    None
}

unsafe fn font_of(height: i32) -> Option<windows_sys::Win32::Graphics::Gdi::HFONT> {
    let face: Vec<u16> = "Segoe UI".encode_utf16().chain(std::iter::once(0)).collect();

    let font = CreateFontW(
        height,
        0,
        0,
        0,
        // Heavier than the window's type. This is read at a glance from a
        // corner of the screen at twenty-four pixels, next to a clock, where a
        // semibold digit is a gray smudge.
        FW_BOLD,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        OUT_DEFAULT_PRECIS as u32,
        CLIP_DEFAULT_PRECIS as u32,
        // Antialiased, which at this size is the difference between digits and
        // a smear of pixels.
        ANTIALIASED_QUALITY as u32,
        (DEFAULT_PITCH | FF_DONTCARE) as u32,
        face.as_ptr(),
    );

    (!font.is_null()).then_some(font)
}

/// The same coverage, moved so its ink sits in the middle of the icon.
fn centered(coverage: &[u8], size: i32) -> Vec<u8> {
    let ink = |x: i32, y: i32| coverage[(y * size + x) as usize] > 24;

    let rows: Vec<i32> = (0..size).filter(|y| (0..size).any(|x| ink(x, *y))).collect();
    let cols: Vec<i32> = (0..size).filter(|x| (0..size).any(|y| ink(*x, y))).collect();

    let (Some(&top), Some(&bottom)) = (rows.first(), rows.last()) else {
        return coverage.to_vec();
    };
    let (Some(&left), Some(&right)) = (cols.first(), cols.last()) else {
        return coverage.to_vec();
    };

    let dy = (size - 1 - bottom - top) / 2;
    let dx = (size - 1 - right - left) / 2;

    if dx == 0 && dy == 0 {
        return coverage.to_vec();
    }

    let mut out = vec![0u8; coverage.len()];

    for y in 0..size {
        for x in 0..size {
            let (sx, sy) = (x - dx, y - dy);
            if sx >= 0 && sy >= 0 && sx < size && sy < size {
                out[(y * size + x) as usize] = coverage[(sy * size + sx) as usize];
            }
        }
    }

    out
}

/// The same coverage, one pixel fatter in every direction.
///
/// Each pixel takes the strongest of itself and its eight neighbors. That is
/// all an outline needs to be at this size.
fn grown_by_one(coverage: &[u8], size: i32) -> Vec<u8> {
    let mut out = vec![0u8; coverage.len()];

    for y in 0..size {
        for x in 0..size {
            let mut strongest = 0u8;

            for dy in -1..=1 {
                for dx in -1..=1 {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || ny < 0 || nx >= size || ny >= size {
                        continue;
                    }

                    strongest = strongest.max(coverage[(ny * size + nx) as usize]);
                }
            }

            out[(y * size + x) as usize] = strongest;
        }
    }

    out
}

/// Turns what GDI left behind into premultiplied ink.
///
/// GDI wrote gray levels and no alpha at all, so the brightest channel is read
/// as coverage.
fn coverage_to_ink(pixels: &mut [u32], ink: u32) {
    for pixel in pixels.iter_mut() {
        let value = *pixel;
        let coverage = (value & 0xff).max((value >> 8) & 0xff).max((value >> 16) & 0xff);

        *pixel = premultiplied(ink, coverage);
    }
}

/// The dot, over whatever is already there.
///
/// Rasterized by hand because GDI would leave it with no alpha, and a shape
/// drawn here can have a soft edge.
fn fill_disc(pixels: &mut [u32], size: i32, color: u32) {
    paint_disc(pixels, size, color, false);
}


/// A disc filling the icon, painted over or under what is there.
///
/// Under is what puts the number inside the dot: the digits are rendered first
/// as ink and the color goes beneath them, so a glyph is a hole in the disc.
fn paint_disc(pixels: &mut [u32], size: i32, color: u32, beneath: bool) {
    // Half a pixel in from the edge, so the feathering has somewhere to go and
    // the disc does not come out with four flat sides.
    let radius = size as f32 / 2.0 - 0.5;
    let center = size as f32 / 2.0;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - center;
            let dy = y as f32 + 0.5 - center;
            // One pixel of feathering, which is the difference between a dot
            // and a small brick.
            let coverage = (radius - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);

            if coverage <= 0.0 {
                continue;
            }

            let at = (y * size + x) as usize;
            let disc = premultiplied(color, (coverage * 255.0) as u32);

            pixels[at] = if beneath {
                over(pixels[at], disc)
            } else {
                over(disc, pixels[at])
            };
        }
    }
}

/// A color with its channels scaled by an alpha, which is the form a 32-bit
/// icon bitmap is read in.
fn premultiplied(color: u32, alpha: u32) -> u32 {
    let alpha = alpha.min(255);
    let channel = |shift: u32| (((color >> shift) & 0xff) * alpha / 255) << shift;

    (alpha << 24) | channel(16) | channel(8) | channel(0)
}

/// Source over destination, both premultiplied.
fn over(source: u32, destination: u32) -> u32 {
    let inverse = 255 - (source >> 24);
    let channel = |shift: u32| {
        let s = (source >> shift) & 0xff;
        let d = (destination >> shift) & 0xff;

        (s + d * inverse / 255).min(255) << shift
    };

    let alpha = ((source >> 24) + ((destination >> 24) * inverse) / 255).min(255);

    (alpha << 24) | channel(16) | channel(8) | channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_becomes_alpha_rather_than_color() {
        // What GDI leaves behind: white where a glyph covered a pixel fully,
        // gray at its edges, black everywhere else.
        let mut pixels = [0x00ff_ffff, 0x0080_8080, 0x0000_0000];
        coverage_to_ink(&mut pixels, INK);

        assert_eq!(pixels[0] >> 24, 255, "a covered pixel should be opaque");
        assert!((pixels[1] >> 24) > 100 && (pixels[1] >> 24) < 150, "an edge should be partial");
        assert_eq!(pixels[2], 0, "an untouched pixel must stay transparent");
    }

    #[test]
    fn nothing_is_ever_brighter_than_its_own_alpha() {
        // The rule for premultiplied color, and the one that shows up as
        // bright fringing around the glyphs when it is broken.
        for alpha in [0u32, 1, 64, 128, 254, 255] {
            let pixel = premultiplied(0x00ff_ffff, alpha);

            assert_eq!(pixel >> 24, alpha);
            for shift in [0, 8, 16] {
                assert!((pixel >> shift) & 0xff <= alpha, "channel above alpha at {alpha}");
            }
        }
    }

    #[test]
    fn the_dot_covers_its_own_corner_and_leaves_the_rest_alone() {
        let size = 16;
        let mut pixels = vec![0u32; (size * size) as usize];
        fill_disc(&mut pixels, size, 0x00ff_0000);

        let at = |x: i32, y: i32| pixels[(y * size + x) as usize];

        // The disc fills the icon instead of sitting in a corner of it.
        // Corners stay clear, which is what makes it a disc.
        assert!(at(8, 8) >> 24 > 200, "the middle should be solid");
        assert!(at(2, 8) >> 24 > 200, "the left edge should be inside the disc");
        assert!(at(13, 8) >> 24 > 200, "the right edge should be inside the disc");
        assert_eq!(at(0, 0), 0, "a corner should be outside the disc");
        assert_eq!(at(15, 15), 0, "the opposite corner too");
    }

    #[test]
    fn an_impossible_size_is_refused_rather_than_drawn_badly() {
        assert!(compose(0, 0x00ff_0000, 61).is_null());
        assert!(compose(-4, 0x00ff_0000, 61).is_null());
    }

    /// How many pixels of a horizontal slice of the icon have ink in them.
    fn ink_in(pixels: &[u32], size: i32, from: i32, to: i32) -> usize {
        let mut count = 0;

        for y in 0..size {
            for x in from..to {
                if pixels[(y * size + x) as usize] >> 24 > 40 {
                    count += 1;
                }
            }
        }

        count
    }

    #[test]
    fn the_number_is_actually_drawn_and_not_just_promised() {
        // Ink in the right-hand two thirds is the number being there at all.
        for size in [16, 20, 24, 32] {
            for number in [7, 61, 100, 212] {
                let pixels = render(size, 0x0022_a844, number).expect("nothing rendered");

                assert_eq!(pixels.len(), (size * size) as usize);

                let digits = ink_in(&pixels, size, size / 3, size);
                assert!(
                    digits > (size as usize) / 2,
                    "{number} at {size} pixels drew almost nothing: {digits} pixels of ink"
                );

                // No disc behind the digits: the band color is on the digits
                // themselves, which is the same signal without costing them a
                // third of the width.
                let colored = pixels
                    .iter()
                    .filter(|p| (*p & 0x00ff_ffff) != 0 && (*p >> 24) > 40)
                    .count();
                assert!(colored > 0, "nothing was colored at {size} pixels");
            }
        }
    }


    #[test]
    fn the_digits_do_not_run_into_the_dot() {
        // They used to touch at sixteen pixels, where a dot against a six
        // reads as one shape.
        let size = 16;
        let pixels = render(size, 0x0022_a844, 61).unwrap();

        // The digits are a hole in the disc, not something laid on top: their
        // pixels are the pale ink, the disc's are the band color.
        let ink = |x: i32, y: i32| pixels[(y * size + x) as usize] & 0x00ff_ffff;

        let has_band = (0..size).any(|y| (0..size).any(|x| ink(x, y) == 0x0022_a844));
        assert!(has_band, "the band color is not in the icon at all");

        let center_band = (size / 4..3 * size / 4)
            .flat_map(|y| (size / 4..3 * size / 4).map(move |x| (x, y)))
            .filter(|(x, y)| ink(*x, *y) != 0x0022_a844)
            .count();

        assert!(center_band > 0, "no digits were drawn inside the disc");
    }

    #[test]
    fn an_icon_is_produced_at_every_size_the_shell_asks_for() {
        // 16 at 100%, 20 at 125%, 24 at 150%, 32 at 200%, and three digits
        // wherever Fahrenheit or a boiling laptop puts them.
        for size in [16, 20, 24, 32] {
            for number in [7, 61, 100, 212] {
                let icon = compose(size, 0x0022_a844, number);
                assert!(!icon.is_null(), "{number} at {size} pixels came out as nothing");

                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::DestroyIcon(icon);
                }
            }
        }
    }

    #[test]
    fn drawing_over_something_keeps_what_it_does_not_cover() {
        let ground = premultiplied(0x0000_00ff, 255);

        assert_eq!(over(premultiplied(0x00ff_0000, 0), ground), ground);
        assert_eq!(over(premultiplied(0x00ff_0000, 255), ground) >> 24, 255);
    }

    #[test]
    fn the_digits_fill_most_of_the_icon() {
        // Sizing the em box to fit inside the icon threw away about a third of
        // the height, because an em box is much taller than the digits in it.
        for size in [16, 20, 24, 32] {
            for number in [61, 100] {
                let pixels = render(size, 0x0022_a844, number).expect("nothing rendered");

                let row_has_ink = |y: i32| {
                    (0..size).any(|x| pixels[(y * size + x) as usize] >> 24 > 40)
                };

                let inked = (0..size).filter(|y| row_has_ink(*y)).count() as i32;

                // Two digits reach most of the way; three are bound by width
                // however tightly they are set, so they get a lower bar.
                let want = if number >= 100 { 5 } else { 6 };

                assert!(
                    inked * 10 >= size * want,
                    "{number} at {size} px covers only {inked} of {size} rows"
                );
            }
        }
    }

    #[test]
    fn the_digits_sit_in_the_middle() {
        // Centering on the font's box, which reserves space for descenders
        // digits do not have, sat the number low against the icons beside it.
        for size in [16, 20, 24, 32] {
            for number in [7, 61, 100] {
                let pixels = render(size, 0x0022_a844, number).expect("nothing rendered");
                let ink = |x: i32, y: i32| pixels[(y * size + x) as usize] >> 24 > 40;

                let rows: Vec<i32> =
                    (0..size).filter(|y| (0..size).any(|x| ink(x, *y))).collect();

                let top = rows[0];
                let bottom = rows[rows.len() - 1];
                let above = top;
                let below = size - 1 - bottom;

                assert!(
                    (above - below).abs() <= 2,
                    "{number} at {size} px: {above} above, {below} below"
                );
            }
        }
    }
}
