// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! The About box: the version, the warnings, and every attribution owed.
//!
//! Built by hand like the name box, and for the same reason: one read-only
//! edit and three buttons do not justify a resource script. A message box was
//! considered and turned down, because the text below is a page and not a
//! sentence, and a MessageBoxW tall enough to hold it has no scrollbar and no
//! way to copy a URL out of it. The edit control gives both.
//!
//! The order of the text is deliberate. The disclaimer and the hardware
//! warning sit directly under the name, where they are on screen before
//! anyone scrolls, because they are the part somebody should actually read.
//! The attributions follow, and the license housekeeping comes last.

use std::ptr;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint,
    FillRect, RoundRect, SelectObject, SetBkColor, SetBkMode, SetTextColor, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, DEFAULT_PITCH, DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE,
    FW_NORMAL, FW_SEMIBOLD, HBRUSH, HDC, HFONT, OPAQUE, OUT_TT_PRECIS, PAINTSTRUCT, PS_SOLID,
    TRANSPARENT,
};
use windows_sys::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_DEFAULT, ODS_FOCUS, ODS_SELECTED};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ID_TEXT: isize = 100;
const ID_FOLDER: isize = 101;
const ID_PAGE: isize = 102;
/// IDCANCEL, so the WM_COMMAND that IsDialogMessageW turns the Escape key
/// into lands on the same arm as the button.
const ID_CLOSE: isize = 2;

const WIDTH: i32 = 660;
const HEIGHT: i32 = 640;

/// Room around everything, the header the icon and title sit in, and the row
/// the buttons sit on. Derived from the window height so resizing the box is
/// one edit rather than five.
const PAD: i32 = 20;
const ICON: i32 = 48;
/// Where the header's words start: past the icon, plus room to breathe.
const HEAD_X: i32 = PAD + ICON + 16;
/// The hairline under the header, and the top of the text below it.
const RULE_Y: i32 = 100;
const TEXT_Y: i32 = RULE_Y + 18;
const BUTTON_H: i32 = 32;

/// The window's colors, matching the settings window rather than the shell's
/// defaults.
///
/// The rest of this program is dark and drawn by hand, and a stock gray box
/// with a sunken border in the middle of it looks like a different decade.
/// GDI wants COLORREF, which is BGR rather than the RGB the theme is written
/// in, so these are spelled out here instead of converted at every use.
const BACK: u32 = 0x16_11_0f; // theme::BACKGROUND, 0x0f1116
const FIELD: u32 = 0x21_19_16; // theme::SURFACE, 0x161921
const INK: u32 = 0xf0_ea_e8; // theme::TEXT, 0xe8eaf0
const DIM: u32 = 0xae_a0_99; // theme::TEXT_DIM, 0x99a0ae
const FAINT: u32 = 0x74_66_5f; // theme::TEXT_FAINT, 0x5f6674
/// theme::BORDER is white at 8% over the ground; GDI has no alpha here, so
/// these are that blend already done.
const EDGE: u32 = 0x28_21_1f;
/// The same idea over the field rather than the ground, for a button that is
/// focused or pressed.
const EDGE_LIT: u32 = 0x50_46_42;
const RAISED: u32 = 0x2b_23_1f;

/// Sets the edit control's formatting rectangle, which is how its text is
/// given a margin. Not in windows-sys' constants for a plain EDIT.
const EM_SETRECT: u32 = 0x00b3;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Everything the box says, as one block of text.
///
/// Free-standing so a test can hold it to the facts that matter. Four things
/// live here that a later edit could drop without anything else breaking:
/// the attributions, the no-warranty disclaimer, the hardware warning, and
/// the no-support note. A test names each of them.
///
/// The version and the project page come from Cargo.toml through the
/// environment, the same way the usage text gets its version, so a release
/// bump or a moved repository cannot leave this box telling an old story.
///
/// Every URL and license claim below restates NOTICE.md and README.md, and
/// changes here should keep all three telling the same one.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn text() -> String {
    let [name, tagline, lenovo] = heading();

    format!("{name}\n{tagline}\n{lenovo}\n\n{}", body())
}

/// The three identity lines, which are painted at the top of the box rather
/// than set in the text field.
///
/// They are the one part nobody should have to scroll to, and a title drawn
/// beside the program's own icon says what it is faster than the same words
/// as the first paragraph of a page. `text()` still joins them to the body,
/// so the tests below read the box as a whole and neither half can quietly
/// lose the name, the license or the Lenovo line.
fn heading() -> [String; 3] {
    [
        format!("Yamato {}", env!("CARGO_PKG_VERSION")),
        "Fan control software for ThinkPads. MIT licensed.".into(),
        "Not affiliated with, endorsed by, or supported by Lenovo.".into(),
    ]
}

fn body() -> String {
    format!(
        "Yamato writes directly to your ThinkPad's embedded controller. Setting a \
         manual fan level switches the firmware's own thermal management off, which \
         is what makes fan control possible and also what makes it worth taking \
         seriously. Yamato hands the fan back to the firmware on exit, on crash, on \
         shutdown, on sleep, and when a watchdog notices the control loop has \
         stalled, and it refuses the disengaged setting outright and refuses level 0 \
         as a held manual mode. Use it on hardware you are willing to look after.\n\
         \n\
         The software is provided \"as is\", without warranty of any kind, express \
         or implied. The authors or copyright holders shall not be liable for any \
         claim, damages or other liability arising from, out of or in connection \
         with the software or the use or other dealings in the software. LICENSE \
         has the full text.\n\
         \n\
         A hobby project. Issues and pull requests are closed, and there is no \
         support. It was written for one ThinkPad and shared because it may suit \
         yours; if it does not work for you, it does not work for you. That is the \
         honest version rather than an unfriendly one: reaching an embedded \
         controller depends on hardware that varies by model and by firmware \
         revision, and answering \"it doesn't work on my machine\" properly means \
         owning that machine. TPFanControl and its forks have done this job well \
         for twenty years and may support more machines than this does.\n\
         \n\
         Yamato stands on twenty years of prior work:\n\
         \n\
         TPFanControl, the original: https://github.com/ThinkPad-Forum/TPFanControl\n\
         byrnes' dual-fan mod, where second fan support came from: \
         https://github.com/byrnes/TPFanControl\n\
         TPFanCtrl2: https://github.com/Shuzhengz/TPFanCtrl2\n\
         FanDjango's fork, the most current: https://github.com/FanDjango/TPFanCtrl2\n\
         \n\
         No code was copied from any of them: no functions, no identifiers, no \
         comments. What is shared is knowledge of the hardware, facts about a \
         ThinkPad independently documented in the Linux thinkpad_acpi driver and \
         on ThinkWiki.\n\
         \n\
         Yamato reaches the embedded controller through PawnIO, by namazso, \
         GPL-2.0-or-later. PawnIO is not bundled: it is installed separately from \
         https://pawnio.eu, and Yamato speaks to it only over DeviceIoControl.\n\
         \n\
         Two PawnIO modules ship with Yamato: LpcACPIEC.bin and LpcIO.bin, by \
         namazso, LGPL-2.1-or-later, byte for byte unmodified, with their sources \
         LpcACPIEC.p and LpcIO.p beside them.\n\
         \n\
         The Rust crates linked into yamato.exe are permissively licensed, MIT or \
         Apache-2.0 for nearly all of them. Their license texts and copyright \
         notices are reproduced in full in THIRD-PARTY-LICENSES.txt.\n\
         \n\
         The full texts are installed next to yamato.exe: LICENSE, \
         LICENSE.LGPL-2.1.txt, NOTICE.md and THIRD-PARTY-LICENSES.txt.\n\
         \n\
         Project page: {repo}\n",
        repo = env!("CARGO_PKG_REPOSITORY"),
    )
}

/// ShellExecuteW's open verb: a folder opens in Explorer, a URL in the
/// default browser. Failure is ignored, because a machine with no browser
/// has no recovery this box could offer.
fn open(target: &str) {
    unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            wide("open").as_ptr(),
            wide(target).as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Opens the folder the executable was installed to.
///
/// The text says the full license texts sit next to yamato.exe, and this
/// button is what makes that sentence a place rather than a claim.
fn open_install_folder() {
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(folder) = exe.parent() else { return };

    open(&folder.to_string_lossy());
}

/// Shows the box, modally against `owner`, and returns when it is closed.
///
/// Modal for the same reason the name box is: the tray must not be driven
/// into a second copy of itself while a loop of ours is pumping messages.
/// The caller holds the tray's modal pause; this function only owns the
/// window. The owner may be null, which is how `--help` shows it with no
/// tray running, and then there is nothing to disable.
pub fn show(owner: HWND) {
    unsafe {
        let instance = GetModuleHandleW(ptr::null());
        let class = wide("YamatoAbout");

        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: ptr::null_mut(),
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            // The theme's own background rather than the shell's window
            // color. Owned by the class for the life of the process, which
            // is why it is never deleted: the class outlives every box.
            hbrBackground: CreateSolidBrush(BACK),
            lpszMenuName: ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);

        let (x, y) = crate::prompt::centered(owner, WIDTH, HEIGHT);

        let window = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_TOPMOST,
            class.as_ptr(),
            wide("About Yamato").as_ptr(),
            WS_POPUPWINDOW | WS_CAPTION,
            x,
            y,
            WIDTH,
            HEIGHT,
            owner,
            ptr::null_mut(),
            instance,
            ptr::null(),
        );

        if window.is_null() {
            return;
        }

        // The title bar follows the window rather than staying light above a
        // dark box. Ignored before 1809, which is below the supported floor
        // anyway, and the failure is cosmetic either way.
        let dark: u32 = 1;
        let _ = DwmSetWindowAttribute(
            window,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            &dark as *const u32 as *const _,
            std::mem::size_of::<u32>() as u32,
        );

        ask_for_dark_controls(window);

        // Everything below is placed against the client rectangle rather than
        // WIDTH and HEIGHT. Those are the outside of the window, and the
        // caption and frame between them and the inside are not a constant:
        // they change with the DPI the window opened on. Guessing at them is
        // how the button row ended up half under the bottom edge.
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(window, &mut client);

        let (cw, ch) = (client.right, client.bottom);
        let button_y = ch - PAD - BUTTON_H;
        let text_h = button_y - 16 - TEXT_Y;

        // A read-only edit rather than a static, so the URLs can be selected
        // and copied and a scrollbar appears when the text outgrows the box.
        // The edit control ends lines with CRLF, and a bare \n shows as a
        // box character, so the text is converted on the way in.
        //
        // No WS_EX_CLIENTEDGE: the sunken 3D border is the one part of this
        // that cannot be recolored, and it reads as a chiselled gray groove
        // on a dark window. The color messages give the field its own shade
        // instead, which is what separates it from the background here.
        //
        // The identity lines are not in here: the header paints them.
        let edit = CreateWindowExW(
            0,
            wide("EDIT").as_ptr(),
            wide(&body().replace('\n', "\r\n")).as_ptr(),
            WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL
                | (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32,
            PAD,
            TEXT_Y,
            cw - 2 * PAD,
            text_h,
            window,
            ID_TEXT as _,
            instance,
            ptr::null(),
        );
        SendMessageW(edit, WM_SETFONT, body_font() as WPARAM, 1);
        // Without this the scrollbar stays white on a black field.
        use_dark_theme(edit);
        // The edit puts its text hard against its own left edge, which sits
        // hard against the window's. A small inset gives the page a margin,
        // and the right one has to clear the scrollbar or the last few
        // characters of every line go under it.
        let bar = GetSystemMetrics(SM_CXVSCROLL);
        let inset = RECT {
            left: 12,
            top: 10,
            right: cw - 2 * PAD - bar - 12,
            bottom: text_h - 10,
        };
        SendMessageW(edit, EM_SETRECT, 0, &inset as *const RECT as LPARAM);

        // Close is the default button, so Enter and Escape both leave. The
        // other two act without closing: somebody reading licenses may well
        // want the folder and the page both.
        //
        // The widths are generous on purpose. Segoe UI at this size is wider
        // than the stock font these were first measured against, and a button
        // whose label is cut off mid-word is worse than one with air in it.
        //
        // BS_OWNERDRAW because a stock push button is gray plastic that no
        // color message reaches, and three of them along the bottom were the
        // loudest thing on an otherwise dark window.
        for (id, label, x, w, style) in [
            (ID_FOLDER, "Open license folder", PAD, 172, 0u32),
            (ID_PAGE, "Project page", PAD + 182, 130, 0u32),
            (ID_CLOSE, "Close", cw - PAD - 116, 116, BS_DEFPUSHBUTTON as u32),
        ] {
            let button = CreateWindowExW(
                0,
                wide("BUTTON").as_ptr(),
                wide(label).as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_OWNERDRAW as u32 | style,
                x,
                button_y,
                w,
                BUTTON_H,
                window,
                id as _,
                instance,
                ptr::null(),
            );
            SendMessageW(button, WM_SETFONT, small_font() as WPARAM, 1);

            if id == ID_CLOSE {
                SetFocus(button);
            }
        }

        // Modal: disable the owner so it cannot be clicked behind us, and
        // re-enable it before the box goes, or the owner is left dead.
        if !owner.is_null() {
            EnableWindow(owner, 0);
        }

        let _ = ShowWindow(window, SW_SHOW);
        // The --help path has no owner and no foreground rights inherited
        // from a click, so the box is put in front on purpose.
        SetForegroundWindow(window);

        // The same pump as the name box, for the same reasons: the loop ends
        // when the window is gone, checked before waiting, and a quit posted
        // from outside is put back for the real message loop rather than
        // swallowed here.
        let mut msg: MSG = std::mem::zeroed();
        let mut quit_arrived = false;

        while IsWindow(window) != 0 {
            let got = GetMessageW(&mut msg, ptr::null_mut(), 0, 0);

            if got <= 0 {
                quit_arrived = got == 0;
                break;
            }

            // Tab and Escape behave the way they do in a real dialog.
            if IsDialogMessageW(window, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Only reachable by the break above, since the loop's own condition
        // is the window being gone.
        if IsWindow(window) != 0 {
            DestroyWindow(window);
        }

        if !owner.is_null() {
            EnableWindow(owner, 1);
            SetForegroundWindow(owner);
        }

        if quit_arrived {
            PostQuitMessage(0);
        }
    }
}

/// The brushes the color messages hand back.
///
/// Made once and kept, because those messages arrive on every paint and a
/// brush created per call is a leak measured in repaints. Never deleted for
/// the same reason the class brush is not: they live as long as the process
/// that draws with them.
unsafe fn window_brush() -> HBRUSH {
    static mut BRUSH: HBRUSH = ptr::null_mut();

    if BRUSH.is_null() {
        BRUSH = CreateSolidBrush(BACK);
    }

    BRUSH
}

unsafe fn field_brush() -> HBRUSH {
    static mut BRUSH: HBRUSH = ptr::null_mut();

    if BRUSH.is_null() {
        BRUSH = CreateSolidBrush(FIELD);
    }

    BRUSH
}

/// Segoe UI at a given pixel height and weight.
///
/// Negative height, which is how GDI is asked for a cell of that many pixels
/// rather than an em of them. The stock GUI font is still MS Shell Dlg and
/// looks its age beside everything else this program draws, and at 8pt it is
/// simply too small to read a page of text in.
unsafe fn make_font(px: i32, weight: i32) -> HFONT {
    CreateFontW(
        -px,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        OUT_TT_PRECIS as u32,
        0,
        CLEARTYPE_QUALITY as u32,
        (DEFAULT_PITCH | FF_DONTCARE) as u32,
        wide("Segoe UI").as_ptr(),
    )
}

/// The three sizes this box uses, made once and kept for the same reason the
/// brushes are: WM_DRAWITEM and WM_PAINT both want them, and neither is a
/// place to be creating and destroying GDI objects.
unsafe fn body_font() -> HFONT {
    static mut FONT: HFONT = ptr::null_mut();

    if FONT.is_null() {
        FONT = make_font(17, FW_NORMAL as i32);
    }

    FONT
}

unsafe fn title_font() -> HFONT {
    static mut FONT: HFONT = ptr::null_mut();

    if FONT.is_null() {
        FONT = make_font(26, FW_SEMIBOLD as i32);
    }

    FONT
}

unsafe fn small_font() -> HFONT {
    static mut FONT: HFONT = ptr::null_mut();

    if FONT.is_null() {
        FONT = make_font(15, FW_NORMAL as i32);
    }

    FONT
}

/// Puts the shell's own dark controls on a window and everything under it.
///
/// The color messages reach the edit's text and its background, but not its
/// scrollbar: that is drawn by the theme, and a themed scrollbar stays white
/// on a black field no matter what the parent answers. Windows has drawn dark
/// ones since 1809 and Explorer uses them, but the two calls that ask for
/// them are exported from uxtheme by ordinal and by nothing else. Both are
/// looked up rather than linked, so a build of Windows that has moved on just
/// leaves the box with light scrollbars instead of failing to start.
unsafe fn ask_for_dark_controls(window: HWND) {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    let uxtheme = LoadLibraryW(wide("uxtheme.dll").as_ptr());

    if uxtheme.is_null() {
        return;
    }

    // Ordinal 135, SetPreferredAppMode: 2 is ForceDark, which is what makes
    // the scrollbar and the non-client bits follow. Ordinal 133 is
    // AllowDarkModeForWindow, which opts this window in specifically.
    if let Some(set_mode) = GetProcAddress(uxtheme, 135 as *const u8) {
        let set_mode: extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
        set_mode(2);
    }

    if let Some(allow) = GetProcAddress(uxtheme, 133 as *const u8) {
        let allow: extern "system" fn(HWND, i32) -> i32 = std::mem::transmute(allow);
        allow(window, 1);
    }
}

/// Names the dark theme on one control, which is what actually repaints its
/// scrollbar. `ask_for_dark_controls` has to have run first.
unsafe fn use_dark_theme(control: HWND) {
    use windows_sys::Win32::UI::Controls::SetWindowTheme;

    SetWindowTheme(control, wide("DarkMode_Explorer").as_ptr(), ptr::null());
}

/// Draws the header: the program's own icon, its name and version, and the
/// two lines that say what it is and whose it isn't.
///
/// These used to be the first three lines of the text field, where they were
/// indistinguishable from the page of warnings under them. Up here, beside
/// the icon, they are the thing a box called About should lead with.
unsafe fn paint_header(window: HWND, dc: HDC) {
    let instance = GetModuleHandleW(ptr::null());

    let mut client: RECT = std::mem::zeroed();
    GetClientRect(window, &mut client);

    // winresource names the embedded icon 1. A null here costs the icon and
    // nothing else, so the text is laid out the same way either way.
    let icon = LoadImageW(
        instance,
        1 as *const u16,
        IMAGE_ICON,
        ICON,
        ICON,
        LR_DEFAULTCOLOR,
    );

    if !icon.is_null() {
        DrawIconEx(dc, PAD, PAD + 4, icon as _, ICON, ICON, 0, ptr::null_mut(), DI_NORMAL);
        DestroyIcon(icon as _);
    }

    let [name, tagline, lenovo] = heading();

    SetBkMode(dc, TRANSPARENT as i32);

    for (text, font, color, y, height) in [
        (name, title_font(), INK, PAD, 32),
        (tagline, small_font(), DIM, PAD + 36, 20),
        (lenovo, small_font(), FAINT, PAD + 56, 20),
    ] {
        SelectObject(dc, font as _);
        SetTextColor(dc, color);

        let mut at =
            RECT { left: HEAD_X, top: y, right: client.right - PAD, bottom: y + height };
        DrawTextW(dc, wide(&text).as_ptr(), -1, &mut at, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    }

    // A hairline rather than a groove. It separates the header from the page
    // without pretending the window has depth it doesn't have.
    let rule =
        RECT { left: PAD, top: RULE_Y, right: client.right - PAD, bottom: RULE_Y + 1 };
    let brush = CreateSolidBrush(EDGE);
    FillRect(dc, &rule, brush);
    DeleteObject(brush as _);
}

/// Draws one button, because the stock ones are gray plastic on a dark
/// window and no amount of color messaging reaches them.
///
/// Rounded, filled with the same shade as the text field, and edged with the
/// theme's border. The one with focus gets a brighter edge instead of a
/// different color: the accent is red, and a red Close button says something
/// about the button that isn't true.
unsafe fn draw_button(item: &DRAWITEMSTRUCT) {
    let dc = item.hDC;
    let r = item.rcItem;
    let pressed = item.itemState & ODS_SELECTED != 0;
    let focused = item.itemState & (ODS_FOCUS | ODS_DEFAULT) != 0;

    // An owner-drawn button gets no erase of its own, and a rounded shape
    // leaves its four corners untouched. Without this they keep whatever was
    // last under them, which on a first paint is nothing in particular.
    FillRect(dc, &r, window_brush());

    let fill = CreateSolidBrush(if pressed { RAISED } else { FIELD });
    let pen = CreatePen(PS_SOLID, 1, if focused || pressed { EDGE_LIT } else { EDGE });
    let old_brush = SelectObject(dc, fill as _);
    let old_pen = SelectObject(dc, pen as _);

    RoundRect(dc, r.left, r.top, r.right, r.bottom, 8, 8);

    SelectObject(dc, old_brush);
    SelectObject(dc, old_pen);
    DeleteObject(fill as _);
    DeleteObject(pen as _);

    let mut label = [0u16; 64];
    let n = GetWindowTextW(item.hwndItem, label.as_mut_ptr(), label.len() as i32);

    if n > 0 {
        SelectObject(dc, small_font() as _);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, INK);

        let mut at = r;
        DrawTextW(dc, label.as_ptr(), n, &mut at, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
    }
}

unsafe extern "system" fn wnd_proc(
    window: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        // The edit control and the window paint themselves from the shell's
        // colors unless told otherwise, which on a dark window means black
        // text on white in the middle of everything else. Answering these
        // three with the theme's colors is what a dialog gets instead of the
        // owner draw the rest of the program uses.
        WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC | WM_CTLCOLORLISTBOX => {
            let dc = wparam as HDC;
            SetTextColor(dc, INK);
            SetBkColor(dc, FIELD);
            SetBkMode(dc, OPAQUE as i32);

            return field_brush() as LRESULT;
        }
        WM_CTLCOLORDLG => {
            let dc = wparam as HDC;
            SetTextColor(dc, INK);
            SetBkColor(dc, BACK);

            return window_brush() as LRESULT;
        }
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(window, &mut ps);
            paint_header(window, dc);
            EndPaint(window, &ps);

            return 0;
        }
        WM_DRAWITEM => {
            let item = &*(lparam as *const DRAWITEMSTRUCT);
            draw_button(item);

            return 1;
        }
        WM_COMMAND => {
            match (wparam & 0xffff) as isize {
                ID_FOLDER => {
                    open_install_folder();
                    return 0;
                }
                ID_PAGE => {
                    open(env!("CARGO_PKG_REPOSITORY"));
                    return 0;
                }
                ID_CLOSE => {
                    let _ = DestroyWindow(window);
                    return 0;
                }
                _ => {}
            }
        }
        WM_CLOSE => {
            let _ = DestroyWindow(window);
            return 0;
        }
        // Nothing for WM_DESTROY, deliberately: the pump in show() watches
        // for the window going away, and a quit posted here would end more
        // than that pump. The name box learned this the hard way.
        _ => {}
    }

    DefWindowProcW(window, msg, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_about_text_keeps_the_facts_that_matter() {
        // Four things live in this text that a rewording could quietly drop
        // without anything else breaking, and none of them may go: the
        // attributions, the no-warranty disclaimer, the hardware warning,
        // and the no-support note. Asserted on substance rather than exact
        // sentences, so ordinary editing survives and deleting a clause
        // does not.
        let text = text();

        // The version comes from the crate, so this can only fail if
        // somebody hardcodes one.
        assert!(text.contains(env!("CARGO_PKG_VERSION")), "the version has gone");

        // The attributions, URLs exactly as NOTICE.md gives them.
        for owed in [
            "PawnIO",
            "namazso",
            "GPL-2.0-or-later",
            "https://pawnio.eu",
            "DeviceIoControl",
            "LpcACPIEC.bin",
            "LpcIO.bin",
            "LGPL-2.1-or-later",
            "https://github.com/ThinkPad-Forum/TPFanControl",
            "https://github.com/byrnes/TPFanControl",
            "https://github.com/Shuzhengz/TPFanCtrl2",
            "https://github.com/FanDjango/TPFanCtrl2",
            "thinkpad_acpi",
            "ThinkWiki",
            "No code was copied",
            "THIRD-PARTY-LICENSES.txt",
            "LICENSE.LGPL-2.1.txt",
            "NOTICE.md",
        ] {
            assert!(text.contains(owed), "the About text no longer credits: {owed}");
        }

        // The license disclaimer, in MIT's own operative words: as is, no
        // warranty, no liability.
        for said in ["\"as is\"", "without warranty of any kind", "shall not be liable"] {
            assert!(text.contains(said), "the disclaimer lost: {said}");
        }

        // The hardware warning, and what the program actually does about
        // the risk it names.
        for said in [
            "embedded controller",
            "thermal management off",
            "hands the fan back to the firmware",
            "disengaged",
            "level 0",
        ] {
            assert!(text.contains(said), "the hardware warning lost: {said}");
        }

        // The no-support note, and the pointer at the programs that do
        // support more machines.
        for said in ["hobby project", "no support", "TPFanControl and its forks"] {
            assert!(text.contains(said), "the no-support note lost: {said}");
        }

        // Identity: name, license, and the Lenovo line.
        for said in ["Yamato", "MIT licensed", "Not affiliated with, endorsed by, or supported by Lenovo"] {
            assert!(text.contains(said), "the identity block lost: {said}");
        }
    }

    #[test]
    fn the_disclaimer_is_read_before_the_credits() {
        // The warning and the disclaimer are the part somebody should
        // actually read, so they sit under the name where the box opens,
        // not below a scroll of licenses.
        let text = text();

        let warning = text.find("thermal management off").expect("warning missing");
        let disclaimer = text.find("without warranty").expect("disclaimer missing");
        let credits = text.find("https://github.com/ThinkPad-Forum").expect("credits missing");

        assert!(warning < credits, "the hardware warning sits below the credits");
        assert!(disclaimer < credits, "the disclaimer sits below the credits");
    }
}
