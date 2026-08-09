// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! A small modal box for typing a name into.
//!
//! Built by hand rather than from a resource script, so there is no .rc file
//! and no resource compiler in the build. It is one edit field and two
//! buttons; a dialog template would be more machinery than the thing it makes.

use std::cell::RefCell;
use std::ptr;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, GetStockObject, MonitorFromPoint, COLOR_WINDOW, DEFAULT_GUI_FONT, HFONT,
    MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// EM_SETSEL. Not re-exported by windows-sys for plain edit controls.
const EM_SETSEL: u32 = 0x00B1;

const ID_EDIT: isize = 100;
const ID_OK: isize = 1; // IDOK
const ID_CANCEL: isize = 2; // IDCANCEL

thread_local! {
    /// Where the accepted text lands. Thread local because the box is modal
    /// and only ever driven from the thread that opened it.
    static RESULT: RefCell<Option<String>> = const { RefCell::new(None) };
}

const WIDTH: i32 = 360;
const HEIGHT: i32 = 150;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Where to put the box so it lands where the eye already is.
///
/// CW_USEDEFAULT is only honored for overlapped windows. A popup given it goes
/// to the top-left corner of the primary display, so the box appeared in one
/// corner of the screen while the person was looking at another, and since
/// everything else is modal meanwhile, that reads as a hang.
///
/// Centered on the owner when there is one on screen. The tray's window is
/// real but never shown, so the fallback is the display the pointer is on.
fn centered(owner: HWND) -> (i32, i32) {
    let mut area = RECT { left: 0, top: 0, right: 0, bottom: 0 };

    let on_owner = !owner.is_null()
        && unsafe { IsWindowVisible(owner) != 0 }
        && unsafe { GetWindowRect(owner, &mut area) != 0 };

    if !on_owner {
        let mut point = POINT { x: 0, y: 0 };
        unsafe { GetCursorPos(&mut point) };

        let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
        let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;

        if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
            return (CW_USEDEFAULT, CW_USEDEFAULT);
        }

        // The work area, not the whole display, so the box does not sit under
        // the taskbar.
        area = info.rcWork;
    }

    (
        area.left + (area.right - area.left - WIDTH) / 2,
        area.top + (area.bottom - area.top - HEIGHT) / 2,
    )
}

/// Asks for a name. `None` if canceled or left empty.
///
/// Modal against `owner`, so the tray cannot be driven into a second copy of
/// itself while this is open.
pub fn ask(owner: HWND, title: &str, initial: &str) -> Option<String> {
    unsafe {
        let instance = GetModuleHandleW(ptr::null());
        let class = wide("YamatoPrompt");

        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: ptr::null_mut(),
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as _,
            lpszMenuName: ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);

        RESULT.with(|r| *r.borrow_mut() = None);

        let (x, y) = centered(owner);

        let window = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_TOPMOST,
            class.as_ptr(),
            wide(title).as_ptr(),
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
            return None;
        }

        let font = GetStockObject(DEFAULT_GUI_FONT) as HFONT;

        let edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            wide("EDIT").as_ptr(),
            wide(initial).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | (ES_AUTOHSCROLL as u32),
            16,
            16,
            310,
            26,
            window,
            ID_EDIT as _,
            instance,
            ptr::null(),
        );
        SendMessageW(edit, WM_SETFONT, font as WPARAM, 1);
        // Whole name selected, so typing replaces rather than appends.
        SendMessageW(edit, EM_SETSEL, 0, -1);

        for (id, label, x) in [(ID_OK, "OK", 150), (ID_CANCEL, "Cancel", 240)] {
            let button = CreateWindowExW(
                0,
                wide("BUTTON").as_ptr(),
                wide(label).as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                x,
                60,
                86,
                28,
                window,
                id as _,
                instance,
                ptr::null(),
            );
            SendMessageW(button, WM_SETFONT, font as WPARAM, 1);
        }

        // Modal: disable the owner so it cannot be clicked behind us, and
        // re-enable it before the box goes, or the owner is left dead.
        if !owner.is_null() {
            EnableWindow(owner, 0);
        }

        let _ = ShowWindow(window, SW_SHOW);
        SetFocus(edit);

        // The loop ends when the box is gone, checked *before* waiting for the
        // next message, so the message that destroyed it is also the last one
        // this pump handles.
        //
        // A nested loop must not end by posting a quit: a quit is thread-wide,
        // and GetMessage hands over every posted message before it reports
        // one. Hovering the tray icon posts a WM_TRAY, so a box closed with
        // one already queued left the quit flag set for the real message loop,
        // which then ended the program.
        let mut msg: MSG = std::mem::zeroed();
        let mut quit_arrived = false;

        while IsWindow(window) != 0 {
            let got = GetMessageW(&mut msg, ptr::null_mut(), 0, 0);

            if got <= 0 {
                // A quit from somewhere outside this box, or an error. Either
                // way this loop is over; the quit is put back below so the
                // program still ends the way it was asked to.
                quit_arrived = got == 0;
                break;
            }

            // Tab and Enter behave the way they do in a real dialog.
            if IsDialogMessageW(window, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Only reachable by the break above, since the loop's own condition is
        // the window being gone.
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

        RESULT
            .with(|r| r.borrow_mut().take())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

unsafe extern "system" fn wnd_proc(
    window: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam & 0xffff) as isize;

            if id == ID_OK {
                let edit = GetDlgItem(window, ID_EDIT as i32);
                let mut buf = [0u16; 128];
                let n = GetWindowTextW(edit, buf.as_mut_ptr(), buf.len() as i32);
                let text = String::from_utf16_lossy(&buf[..n.max(0) as usize]);

                RESULT.with(|r| *r.borrow_mut() = Some(text));
                let _ = DestroyWindow(window);
                return 0;
            }

            if id == ID_CANCEL {
                RESULT.with(|r| *r.borrow_mut() = None);
                let _ = DestroyWindow(window);
                return 0;
            }
        }
        WM_CLOSE => {
            RESULT.with(|r| *r.borrow_mut() = None);
            let _ = DestroyWindow(window);
            return 0;
        }
        // Nothing for WM_DESTROY. Posting a quit here ended more than the pump
        // in ask(); the pump now watches for the window going away instead.
        _ => {}
    }

    DefWindowProcW(window, msg, wparam, lparam)
}
