// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! The tray icon and its menu.
//!
//! Everything a person might want to do is here. Nothing is reachable only
//! from a command line: the flags that survive exist so the service manager
//! and the installer have something to call, not so anyone has to type them.
//!
//! The icons are embedded rather than loaded from disk, so the tray cannot end
//! up iconless because someone moved a file.

use std::mem::size_of;
use std::sync::atomic::Ordering;
use std::ptr;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Power::POWERBROADCAST_SETTING;
use windows_sys::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, VK_B, VK_P, VK_S,
};
use windows_sys::Win32::UI::Shell::{
    ShellExecuteExW, ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi};

use crate::curve_editor::Editor;
use crate::ipc::{self, Channel};
use crate::pawnio_status;
use crate::prompt;
use crate::service;
use crate::settings::{Readout, Settings};

/// Icons live in the binary. A tray icon that silently fails to appear was one
/// of the harder faults to chase in the program this replaces.
const ICO_NORMAL: &[u8] = include_bytes!("tray-normal.ico");
const ICO_WARM: &[u8] = include_bytes!("tray-warm.ico");
const ICO_HOT: &[u8] = include_bytes!("tray-hot.ico");
const ICO_IDLE: &[u8] = include_bytes!("tray-idle.ico");

const WM_TRAY: u32 = WM_APP + 1;
/// Refreshes with no new sample before the engine is treated as gone, when
/// the engine has not said how often it publishes.
///
/// Only used before the first sample arrives. After that the threshold comes
/// from the engine's own interval: a fixed six seconds was wrong at anything
/// but the fastest poll and reported a healthy engine as dead.
const STALE_REFRESHES: u32 = 6;

/// Ceiling on the interval the engine claims, matching the largest the
/// settings schema allows for a standby poll.
const MAX_PUBLISH_SECS: u16 = 120;

/// Missed intervals before calling it stale, on top of the interval itself.
///
/// A pass can legitimately overrun, so this is generous: being slow to notice
/// a dead engine costs a stale reading, while being quick to accuse a live one
/// makes the indicator worthless.
const STALE_MARGIN: u32 = 3;

const TIMER_REFRESH: usize = 1;

/// The service manager's code for a service that is running.
///
/// One definition, because the menu and the tooltip both ask what the service
/// is doing, and a magic number written out twice will not stay agreed.
const SERVICE_RUNNING: i32 = 4;

/// How often the tray re-reads the engine's sample while somebody can see it.
const REFRESH_MS: u32 = 1000;

/// And how often once the screen has gone off.
///
/// A timer is nearly free in processor time and not free at all in wakes: once
/// a second is sixty an hour more than a machine trying to reach a low-power
/// state can afford, and it buys a redraw of an icon nobody is looking at. The
/// engine keeps running either way; this is only how often the picture of it
/// is fetched.
const REFRESH_MS_DARK: u32 = 30_000;

/// Which of the two is in force.
///
/// A static rather than a field on `Tray`, because the timer is armed in three
/// places and one of them is a `Drop` that has only the window handle. Reading
/// it there is what stops a modal closing in the dark from quietly putting the
/// fast timer back.
static REFRESH_INTERVAL: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(REFRESH_MS);

fn refresh_interval() -> u32 {
    REFRESH_INTERVAL.load(std::sync::atomic::Ordering::Relaxed)
}

/// Re-arms the refresh timer when the console display goes off or comes back.
///
/// `SetTimer` on an existing id replaces its interval rather than adding a
/// second timer, so this is safe to call whenever the state changes.
unsafe fn on_display_state(wparam: WPARAM, lparam: LPARAM, window: HWND) {
    if wparam as u32 != PBT_POWERSETTINGCHANGE || lparam == 0 {
        return;
    }

    let setting = &*(lparam as *const POWERBROADCAST_SETTING);

    if !crate::service::same_guid(&setting.PowerSetting, &GUID_CONSOLE_DISPLAY_STATE)
        || setting.DataLength == 0
    {
        return;
    }

    // 0 off, 1 on, 2 dimmed. Dimmed is still being looked at.
    let interval = if *setting.Data.as_ptr() == 0 { REFRESH_MS_DARK } else { REFRESH_MS };

    REFRESH_INTERVAL.store(interval, std::sync::atomic::Ordering::Relaxed);
    SetTimer(window, TIMER_REFRESH, interval, None);
}

// Menu command ids.
const ID_MODE_BIOS: usize = 100;
const ID_MODE_SMART: usize = 101;
/// Manual levels get one id each from here upward, level 1 first, so level 7
/// is six past it. Well clear of every other id for the same reason the
/// profile base below is: a level should never be reachable by miscounting.
const ID_MANUAL_BASE: usize = 600;
const ID_SETTINGS: usize = 200;
const ID_SVC_INSTALL: usize = 300;
const ID_SVC_UNINSTALL: usize = 301;
const ID_SVC_START: usize = 302;
const ID_SVC_STOP: usize = 303;

thread_local! {
    /// How many modal things are open on this thread right now.
    ///
    /// A count and not a flag, because these nest: a message box opened from
    /// inside the name box is two deep, and a flag cleared when the inner one
    /// closed would reopen the door while the outer one was still up.
    pub(crate) static MODAL_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Stops the refresh timer for as long as it is held, and marks the thread as
/// being somewhere modal.
///
/// The name box, a popup menu and a message box all run message loops of their
/// own. A WM_TIMER dispatched from inside one re-enters the window procedure
/// and takes a second `&mut Tray` from the same pointer while the outer call
/// still holds one, which Rust does not allow to exist.
///
/// Killing the timer only closes that one way in. The tray icon still posts
/// WM_TRAY when clicked, and disabling the owner window stops input, not
/// posted messages, so a modal loop dispatches it and the menu opens on top of
/// whatever is already running. Rename or Delete then starts a second profile
/// edit inside the first, and the two saves overwrite one another. The depth
/// count is what lets the window procedure turn those away.
pub(crate) struct TimerPause(HWND);

impl TimerPause {
    pub(crate) fn new(window: HWND) -> Self {
        MODAL_DEPTH.with(|d| d.set(d.get() + 1));
        unsafe { KillTimer(window, TIMER_REFRESH) };
        TimerPause(window)
    }
}

impl Drop for TimerPause {
    fn drop(&mut self) {
        MODAL_DEPTH.with(|d| d.set(d.get() - 1));
        unsafe { SetTimer(self.0, TIMER_REFRESH, refresh_interval(), None) };
    }
}

const TOO_LONG: &str = "That name is too long. Please use a shorter one.";
const CANNOT_READ: &str = "Yamato could not read its settings file.";
const CANNOT_SAVE: &str = "Yamato could not save its settings file.";
const NAME_TAKEN: &str = "A profile with that name already exists.";

/// Whether a name survives the trip through the shared section.
///
/// Names are carried in a fixed 64-byte field and truncated to fit, and the
/// engine matches them exactly. A name that does not fit would be accepted by
/// every window, saved to the file, and then silently fail to switch the fan,
/// because the truncated form matches no profile.
fn name_fits(name: &str) -> bool {
    name.len() < ipc::NAME_LEN
}

/// What the profile menu was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProfileAction {
    New,
    Duplicate,
    Rename,
    Delete,
}

/// The half of a profile change that does not involve the user: read the
/// settings file, apply the edit, write it back, and hand back what was saved.
///
/// Shared with the settings window's picker, so creating, duplicating,
/// renaming and deleting a profile mean the same thing wherever they are asked
/// for. Anything needing a person, like asking for a name or saying why
/// something was refused, stays with the caller.
///
/// The file is read here, not passed in. A prompt can sit open for minutes
/// while the rest of the program carries on saving, so a copy loaded before
/// the question was asked can be badly out of date by the time the answer
/// arrives, and writing it back would undo whatever happened meanwhile.
pub(crate) fn apply_profile_action(
    action: ProfileAction,
    active: &str,
    name: &str,
) -> Result<yamato_core::Config, &'static str> {
    // The channel truncates names, and a truncated name matches no profile, so
    // the engine would silently ignore the switch while both windows showed the
    // new profile as active.
    if action != ProfileAction::Delete && !name_fits(name) {
        return Err(TOO_LONG);
    }

    let path = yamato_core::Config::default_path();
    let mut config = yamato_core::Config::load(&path).map_err(|_| CANNOT_READ)?;

    let Some(index) = config.profiles.iter().position(|p| p.name == active) else {
        return Err("Yamato could not find the current profile.");
    };

    let taken = config.profiles.iter().any(|p| p.name == name);

    match action {
        ProfileAction::New | ProfileAction::Duplicate => {
            if taken {
                return Err(NAME_TAKEN);
            }

            // Starts from the live one, so it is immediately runnable and the
            // user is editing a curve rather than authoring one.
            let mut copy = config.profiles[index].clone();
            copy.name = name.to_string();
            config.profiles.push(copy);
            config.active_profile = name.to_string();
        }

        ProfileAction::Rename => {
            if name != active && taken {
                return Err(NAME_TAKEN);
            }

            config.profiles[index].name = name.to_string();

            // active_profile is a name, not an index: without this it points at
            // a profile that no longer exists, and the engine falls back to
            // firmware control.
            if config.active_profile == active {
                config.active_profile = name.to_string();
            }
        }

        ProfileAction::Delete => {
            // The three Yamato ships stay: they are what you judge your own
            // curve against, and getting one back means rebuilding it point by
            // point. Editing or renaming them is fine.
            if yamato_core::is_built_in(&config.profiles[index].name) {
                return Err("The built-in profiles cannot be deleted. You can edit one instead.");
            }

            // Refused rather than allowed to empty the list: a settings file
            // with no profiles has nothing to run.
            if config.profiles.len() <= 1 {
                return Err("The last profile cannot be deleted.");
            }

            config.profiles.remove(index);
            config.active_profile = config.profiles[index.min(config.profiles.len() - 1)]
                .name
                .clone();
        }
    }

    config.save(&path).map_err(|_| CANNOT_SAVE)?;

    Ok(config)
}

/// Starts the service if it is installed and stopped.
///
/// The other half of Exit stopping it. Returns whether an attempt was made, so
/// the caller can wait a moment for the first reading rather than opening on a
/// complaint that there is no engine.
///
/// Silent about failure on purpose: this runs before anything is on screen,
/// and the tray says plainly enough what is wrong once it is up.
pub(crate) fn start_service_if_stopped() -> bool {
    if service::state() == -1 || service::state() == SERVICE_RUNNING {
        return false;
    }

    // Directly, which needs no consent prompt: installing granted people
    // logged in at this machine the right to start and stop this one service.
    // Launching is a bad moment to ask for an administrator token, and it was
    // being asked for on every launch after a quit.
    if service::start().is_ok() {
        return true;
    }

    // A service installed before that permission was being granted. Ask once,
    // rather than leaving a tray icon with nothing behind it.
    run_elevated("--start-service")
}

/// Adds a profile built from a curve that came from somewhere else, and makes
/// it the active one.
///
/// Beside [`apply_profile_action`] and not inside it: the name rules are the
/// same and are applied here too, but folding a curve into that enum would
/// make every other caller carry one it has no use for.
pub(crate) fn add_profile_from_curve(
    name: &str,
    curve: &yamato_core::Curve,
) -> Result<yamato_core::Config, &'static str> {
    if !name_fits(name) {
        return Err(TOO_LONG);
    }

    let path = yamato_core::Config::default_path();
    let mut config = yamato_core::Config::load(&path).map_err(|_| CANNOT_READ)?;

    if config.profiles.iter().any(|p| p.name == name) {
        return Err(NAME_TAKEN);
    }

    config.profiles.push(yamato_core::Profile::new(name, curve));
    config.active_profile = name.to_string();
    config.save(&path).map_err(|_| CANNOT_SAVE)?;

    Ok(config)
}

const ID_PROF_NEW: usize = 500;
const ID_PROF_RENAME: usize = 501;
const ID_PROF_DUPLICATE: usize = 502;
const ID_PROF_DELETE: usize = 503;
const ID_PROF_IMPORT: usize = 504;

const ID_STARTUP: usize = 400;
const ID_UNITS: usize = 401;
const ID_PAWNIO: usize = 304;
const ID_LOGGING: usize = 402;
const ID_LOG_FOLDER: usize = 403;
const ID_TRAY_NUMBERS: usize = 404;
const ID_ABOUT: usize = 405;

/// "Hottest" sits on the base, then one id per sensor above it. Well clear of
/// the manual levels below and the profile list above.
const ID_TRAY_SENSOR_BASE: usize = 700;
const ID_EXIT: usize = 900;
/// Profiles get ids from here upward.
const ID_PROFILE_BASE: usize = 1000;

/// Hotkey ids. Ctrl+Shift+B, S and P, registered on the tray's window.
///
/// Three, and no more: a chord that set a fixed fan level would be a way to
/// take the firmware out of the loop by leaning on a keyboard. These three
/// choose who drives and which curve, and every one is a state the machine can
/// sit in safely.
const HOTKEY_BIOS: i32 = 1;
const HOTKEY_SMART: i32 = 2;
const HOTKEY_PROFILE: i32 = 3;

/// Where the icon changes color. Matches the tray tints: green while things
/// are fine, amber warming, red hot.
pub const WARM_AT: i8 = 70;
pub const HOT_AT: i8 = 85;

pub struct Tray {
    window: HWND,
    icon: NOTIFYICONDATAW,
    channel: Option<Channel>,
    profiles: Vec<String>,
    /// Registered by the shell so applications can put their icon back after
    /// Explorer restarts, or after starting before the taskbar was ready.
    taskbar_created: u32,
    /// What the icon on screen is currently a picture of: the size it was
    /// drawn for, the thermal band, and the number in it if there is one.
    ///
    /// Kept so the icon is rebuilt when it would actually look different and
    /// not once a second for the life of the session. `None` means nothing has
    /// been drawn yet, and is also how a settings change asks for a redraw.
    drawn: Option<(i32, u8, Option<i32>)>,
    /// The last sample sequence seen, and how many refreshes it has sat at.
    ///
    /// A dead engine leaves the shared section mapped and full of its last
    /// reading, because this process is holding it open. Nothing about the
    /// section says the writer is gone, so without this the tray would sit
    /// showing a plausible temperature from an engine that stopped minutes
    /// ago. Only the sequence advancing proves someone is still writing.
    last_seq: u32,
    stale_refreshes: u32,
    /// Cached, so the tooltip does not read the settings file every second.
    fahrenheit: bool,
    /// Cached for the menu's tick boxes, and re-read whenever it is built.
    log_enabled: bool,
    tray_numbers: bool,
    /// Which sensor the icon reports, or the hottest when unset.
    tray_sensor: Option<u8>,
    /// Whether the second fan has reported a speed at any point in this
    /// session.
    ///
    /// Kept for the single-fan hint, and kept for good once true: one
    /// revolution of fan 2 is proof there are two fans, and the hint must
    /// never talk a dual-fan machine out of its second fan's verification.
    /// Session-scoped, because there is nowhere honest to remember it longer
    /// and a fresh session only has to see the fan spin once.
    fan2_ever_spun: bool,
    /// Cached with the other settings, for the hint's benefit: advice to try
    /// a setting that is already on is noise.
    single_fan: bool,
    /// Kept alive for as long as it is open. Closing it hides rather than
    /// destroys, so reopening is instant and the curve keeps its edits.
    settings: Option<Box<Settings>>,
    /// Last known position and size of the settings window (x, y, width, height).
    /// Saved when the window is closed so it can be restored on reopening.
    settings_rect: Option<(i32, i32, i32, i32)>,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Whether to suggest the Single fan setting.
///
/// Three conditions, all of them load bearing. The writes must be failing as
/// declines, because a controller that is not answering needs a different
/// cure. The second fan must never have reported a speed this session, because
/// one revolution is proof of a second fan and this hint must never coax a
/// dual-fan machine into switching its verification off. And the setting must
/// not already be on, because advice to try what has been tried is noise.
///
/// Free-standing so it can be tested without a tray or a controller to fail.
fn suggest_single_fan(write_declined: bool, fan2_ever_spun: bool, already_single: bool) -> bool {
    write_declined && !fan2_ever_spun && !already_single
}

/// The dot's color for a thermal band, matching the icons and the graph.
fn band_color(band: u8) -> u32 {
    match band {
        0 => crate::theme::COOL_HEX,
        1 => crate::theme::WARM_HEX,
        2 => crate::theme::HOT_HEX,
        _ => crate::theme::IDLE_HEX,
    }
}

/// Turns embedded `.ico` bytes into an icon handle at the size the tray wants.
///
/// An `.ico` is a directory of images; this picks the closest and builds an
/// icon from just that one, which is what `LoadImage` would do from a file.
fn icon_from_bytes(bytes: &[u8], size: i32) -> HICON {
    unsafe {
        let offset = LookupIconIdFromDirectoryEx(bytes.as_ptr(), 1, size, size, LR_DEFAULTCOLOR);
        if offset <= 0 {
            return ptr::null_mut();
        }

        CreateIconFromResourceEx(
            bytes.as_ptr().add(offset as usize),
            (bytes.len() - offset as usize) as u32,
            1,
            0x0003_0000,
            size,
            size,
            LR_DEFAULTCOLOR,
        )
    }
}

/// Shows the folder the history file and the settings live in.
///
/// A record nobody can find is not much of a record, and the alternative is
/// telling people to paste a path into an address bar.
fn open_log_folder() {
    let path = crate::log::default_path();
    let Some(folder) = path.parent() else { return };

    // Created if this is the first anyone has looked: with logging off and
    // nothing saved yet, there may be no folder there to open, and Explorer
    // answers that with an error box of its own.
    let _ = std::fs::create_dir_all(folder);

    unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            wide("explore").as_ptr(),
            wide(&folder.to_string_lossy()).as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Relaunches ourselves elevated to do one thing and exit.
///
/// Installing or removing a service needs administrator rights, and the window
/// does not have them. Asking only at the moment they are needed is what keeps
/// the logon launch prompt-free.
///
/// Returns whether it ran and reported success. Throwing that away made a
/// declined consent prompt look exactly like an install that had worked. It
/// blocks the message loop while it waits, with nothing on screen saying so.
fn run_elevated(args: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let file = wide(&exe.to_string_lossy());
    let params = wide(args);
    let verb = wide("runas");

    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE as i32;

    unsafe {
        if ShellExecuteExW(&mut info) == 0 {
            return false;
        }

        // Without a handle there is nothing to wait on and nothing to ask, and
        // inventing a failure from that would put an error box in front of an
        // install that very likely worked. Silence is the honest answer.
        if info.hProcess.is_null() {
            return true;
        }

        windows_sys::Win32::System::Threading::WaitForSingleObject(info.hProcess, 30_000);

        let mut code: u32 = 0;
        let reported =
            windows_sys::Win32::System::Threading::GetExitCodeProcess(info.hProcess, &mut code) != 0;
        windows_sys::Win32::Foundation::CloseHandle(info.hProcess);

        reported && code == 0
    }
}

impl Tray {
    /// Boxed on purpose.
    ///
    /// The window procedure reaches us through a pointer stashed in the
    /// window's user data, so our address has to stay put. Returning `Self` by
    /// value would move it on the way out and leave that pointer dangling.
    pub fn new() -> Option<Box<Self>> {
        unsafe {
            let instance = GetModuleHandleW(ptr::null());
            let class = wide("YamatoTray");

            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: ptr::null_mut(),
                hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
                hbrBackground: ptr::null_mut(),
                lpszMenuName: ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            RegisterClassW(&wc);

            // A real but never-shown window: a message-only window cannot own
            // a popup menu or receive the taskbar-created broadcast.
            let window = CreateWindowExW(
                0,
                class.as_ptr(),
                wide("Yamato").as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                instance,
                ptr::null(),
            );

            if window.is_null() {
                return None;
            }

            // Global hotkeys, and quietly optional. Another program may
            // already own one of these chords, and an error box at logon
            // because something else got there first is worse than a key that
            // does not answer. Nothing else depends on them working.
            //
            // MOD_NOREPEAT, or holding the keys down would send a command every
            // few milliseconds for as long as they were held.
            for (id, key) in [
                (HOTKEY_BIOS, VK_B),
                (HOTKEY_SMART, VK_S),
                (HOTKEY_PROFILE, VK_P),
            ] {
                RegisterHotKey(window, id, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, key as u32);
            }

            let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());

            let mut icon: NOTIFYICONDATAW = std::mem::zeroed();
            icon.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            icon.hWnd = window;
            icon.uID = 1;
            icon.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            icon.uCallbackMessage = WM_TRAY;
            icon.hIcon = icon_from_bytes(ICO_IDLE, 16);

            let mut tray = Box::new(Tray {
                window,
                icon,
                channel: Channel::attach(),
                profiles: Vec::new(),
                taskbar_created,
                drawn: None,
                last_seq: 0,
                stale_refreshes: 0,
                fahrenheit: false,
                log_enabled: false,
                tray_numbers: true,
                tray_sensor: None,
                fan2_ever_spun: false,
                single_fan: false,
                settings: None,
                settings_rect: None,
            });

            // Only now that the box has settled is the address stable enough
            // to hand to the window procedure.
            SetWindowLongPtrW(window, GWLP_USERDATA, tray.as_mut() as *mut Tray as isize);

            // The settings the tooltip and the menu show. Read through the same
            // path that refreshes them later, so there is one description of
            // what this cache holds instead of two that have to agree.
            tray.reload_settings();

            tray.add_icon();
            SetTimer(window, TIMER_REFRESH, refresh_interval(), None);

            // Asked for so the timer can be slowed while nobody is looking.
            // The current value arrives immediately, so a tray started with
            // the screen already off never runs fast at all.
            crate::service::RegisterPowerSettingNotification(
                window as _,
                &GUID_CONSOLE_DISPLAY_STATE,
                DEVICE_NOTIFY_WINDOW_HANDLE,
            );

            Some(tray)
        }
    }

    fn add_icon(&mut self) {
        unsafe { Shell_NotifyIconW(NIM_ADD, &self.icon) };
    }

    /// Reads the engine's latest sample and reflects it in the icon and tip.
    fn refresh(&mut self) {
        // Reattach if the engine appeared, or went away and came back.
        if self.channel.is_none() {
            self.channel = Channel::attach();
        }

        // Ask about liveness before reading the sample, so a stalled writer
        // is never mistaken for a cool machine.
        let stale = match &self.channel {
            Some(channel) => {
                let shared = channel.get();
                let seq = shared.state_seq.load(Ordering::Acquire);

                if seq == self.last_seq {
                    self.stale_refreshes += 1;
                } else {
                    self.last_seq = seq;
                    self.stale_refreshes = 0;
                }

                // The engine publishes its own interval, which changes when
                // the machine goes into standby, so the deadline follows it
                // instead of assuming a poll rate. Clamped on the way in,
                // because this comes from a section any process can write and
                // an absurd value would suppress the staleness check for
                // hours.
                let limit = match shared.publish_secs {
                    0 => STALE_REFRESHES,
                    secs => u32::from(secs.min(MAX_PUBLISH_SECS)) + STALE_MARGIN,
                };

                self.stale_refreshes >= limit
            }
            None => true,
        };

        // Drop a dead channel so the reattach above can pick up a new engine.
        if stale {
            self.channel = None;
        }

        // The session memory the single-fan hint is built on: has the second
        // fan ever once reported a speed. Noted on every sample, healthy or
        // not, because the proof can arrive long before the trouble does.
        if let Some(channel) = &self.channel {
            if channel.get().fan_rpm[1] > 0 {
                self.fan2_ever_spun = true;
            }
        }

        // Whether to suggest the Single fan setting. Decided once, here,
        // because the tooltip and the settings window must tell one story.
        let hint = self.channel.as_ref().is_some_and(|c| {
            suggest_single_fan(
                c.get().fan_write_declined != 0,
                self.fan2_ever_spun,
                self.single_fan,
            )
        });

        // And whether to suggest the other controller mode. The judgment is
        // the engine's, made on the fault run it already counts; the tray
        // only carries the sentence, so a machine that needs Compatibility
        // mode stops looking identical to one that is not supported at all.
        let layout_hint = self.channel.as_ref().map_or(0, |c| c.get().layout_hint);

        // The third of these is the number that goes in the icon, when there is
        // one to show.
        let (band, tip, reading) = match &self.channel {
            Some(channel) => {
                let s = channel.get();

                // Which trouble, not merely that there is some. The three read
                // very differently to whoever is looking at the icon, and one
                // of them wants acting on.
                if let Some((headline, detail)) = ipc::status_words(s.status, s.fault != 0) {
                    // A hint takes the detail's line rather than a fourth
                    // line of its own: the tip is 128 characters hard, and of
                    // the sentences on offer a hint is the one that leads
                    // somewhere. The single-fan hint first: it rests on
                    // declined writes, which means a controller answering,
                    // so the two cannot honestly apply at once.
                    let detail = if hint {
                        ipc::SINGLE_FAN_HINT
                    } else {
                        ipc::layout_hint_words(layout_hint).unwrap_or(detail)
                    };

                    (3, format!("Yamato\n{headline}\n{detail}"), None)
                } else {
                    // The nominated sensor if there is one and it is reading,
                    // otherwise the hottest. A chosen sensor that has gone
                    // quiet, like an unpopulated slot or an absent battery,
                    // should not blank the icon. The tooltip names whichever
                    // one it settled on.
                    let (temp, source) = match self.tray_sensor {
                        Some(i) if s.sensors[i as usize] != ipc::SENSOR_ABSENT => {
                            (s.sensors[i as usize] as i8, Some(i as usize))
                        }
                        _ => (s.hottest_temp, None),
                    };

                    let band = if temp >= HOT_AT {
                        2
                    } else if temp >= WARM_AT {
                        1
                    } else {
                        0
                    };

                    let mode = match s.mode {
                        ipc::MODE_SMART => "Smart",
                        ipc::MODE_MANUAL => "Manual",
                        _ => "BIOS",
                    };

                    // Both fans, because showing one of them hid a fan doing
                    // something else. A single-fan machine reports 0 for the
                    // second, and "1900 / 0 rpm" reads like a broken fan, not
                    // an absent one, so that case shows one number.
                    let rpm = if s.fan_rpm[1] > 0 {
                        format!("{} / {} rpm", s.fan_rpm[0], s.fan_rpm[1])
                    } else {
                        format!("{} rpm", s.fan_rpm[0])
                    };

                    // The unit belongs in the tooltip. There is no room for a
                    // degree symbol in a sixteen pixel icon, and a reading is
                    // not ambiguous to the person who chose the unit.
                    let shown = yamato_core::display_temp(temp, self.fahrenheit);

                    // Named, so a number that looks wrong can be traced to the
                    // sensor it came from rather than doubted.
                    let from = match source {
                        Some(i) => crate::settings::SENSOR_NAMES
                            .get(i)
                            .copied()
                            .unwrap_or("sensor"),
                        None => "hottest",
                    };

                    // Which step of the curve is in force. The mode says how
                    // the level is chosen and the profile says which curve is
                    // choosing it, but neither says what it settled on, so the
                    // number describing what the fan is being told to do was
                    // the one not shown.
                    let step = match s.fan_ctrl {
                        yamato_ec::FAN_BIOS => "firmware".to_string(),
                        level if level <= yamato_ec::FAN_LEVEL_MAX => format!("level {level}"),
                        // Anything else is the controller's own business and
                        // not a step this program ever asks for.
                        other => format!("0x{other:02x}"),
                    };

                    (
                        band,
                        format!(
                            "Yamato  {}{} ({})\n{}  {}  {}\n{}",
                            shown,
                            yamato_core::unit_suffix(self.fahrenheit),
                            from,
                            mode,
                            step,
                            s.read_profile(),
                            rpm
                        ),
                        Some(shown),
                    )
                }
            }
            // "Not controlling the fan" on its own reads the same whether
            // nothing is installed, the service is stopped, or the engine is
            // still finding its feet, which leaves the person who needs to act
            // with no idea what to do. Asking the service manager costs a call
            // only in this arm: with an engine attached it never runs.
            None => {
                // What is missing outranks what the service manager thinks,
                // because a missing driver explains the service state and not
                // the other way about. Without PawnIO the engine starts,
                // cannot open the controller and exits in under a second, so
                // the manager correctly reports it stopped, and telling
                // somebody to start a service that will die again is a loop
                // with no way out. This arm only runs with no engine
                // attached, so the diagnosis is the strict one.
                let why = match pawnio_status::diagnose(false) {
                    pawnio_status::Missing::Nothing => match service::state() {
                        -1 => "No engine installed. Right-click to install the service.",
                        state if state != SERVICE_RUNNING => {
                            "The service is stopped. Right-click to start it."
                        }
                        _ => "Starting up, or the engine cannot reach the controller.",
                    },
                    missing => missing.short(),
                };

                (3, format!("Yamato\nNot controlling the fan\n{why}"), None)
            }
        };

        // Asked for at the display's own scaling. This process is per-monitor
        // aware, and Windows answers a plain GetSystemMetrics for such a
        // process at 96 DPI: sixteen pixels, always, whatever the panel runs
        // at. A ThinkPad at 150% wants twenty-four and gets a sixteen pixel
        // icon in a twenty-four pixel hole.
        //
        // The window asked about is real, but zero by zero at the origin and
        // never shown, and a window with no area does not reliably belong to a
        // monitor, so it can answer with the 96 DPI default anyway. Taking the
        // larger of the two is right on a scaled display without being wrong
        // on an unscaled one.
        let dpi = unsafe { GetDpiForWindow(self.window).max(GetDpiForSystem()) };
        let size = unsafe {
            if dpi == 0 {
                GetSystemMetrics(SM_CXSMICON)
            } else {
                GetSystemMetricsForDpi(SM_CXSMICON, dpi)
            }
        }
        .max(16);
        let number = if self.tray_numbers { reading } else { None };

        // Rebuilt only when it would look different. An icon redrawn every
        // second is a handle created and destroyed every second for the life of
        // the session, to no effect anyone can see.
        let mut retired = None;
        if self.drawn != Some((size, band, number)) {
            let bytes = match band {
                0 => ICO_NORMAL,
                1 => ICO_WARM,
                2 => ICO_HOT,
                _ => ICO_IDLE,
            };

            let fresh = match number {
                Some(value) => crate::icon::compose(size, band_color(band), value),
                None => ptr::null_mut(),
            };

            // Drawing it can fail; having an icon at all cannot. The ones
            // compiled into the program are always there.
            let fresh = if fresh.is_null() { icon_from_bytes(bytes, size) } else { fresh };

            if !fresh.is_null() {
                // The old handle is kept until the shell has been shown the
                // new one, and destroyed straight afterwards. Every path from
                // here to the end of this function frees it.
                retired = Some(std::mem::replace(&mut self.icon.hIcon, fresh));
                self.drawn = Some((size, band, number));
            }
        }

        // A closed window destroys itself, and this is where what it left
        // behind goes. Keeping the box past its window is wrong twice over:
        // it holds a handle that no longer names anything while this function
        // writes a fresh readout into it every second, and it holds the D2D
        // and DirectWrite factories, the type ramp and the render target's
        // back buffer, which is most of what closing was supposed to return.
        // Reopening builds a fresh one; open_settings has always known how.
        if self
            .settings
            .as_ref()
            .is_some_and(|s| unsafe { IsWindow(s.hwnd()) } == 0)
        {
            self.settings = None;
        }

        // The settings window, if open, gets the whole sample rather than
        // just what fits in a tooltip.
        if let Some(settings) = self.settings.as_mut() {
            settings.set_readout(self.channel.as_ref().map_or_else(
                || Readout { fault: true, ..Default::default() },
                |channel| {
                    let s = channel.get();
                    let mut sensors = [None; yamato_ec::SENSOR_COUNT];
                    for (i, slot) in sensors.iter_mut().enumerate() {
                        let raw = s.sensors[i];
                        *slot = (raw != ipc::SENSOR_ABSENT).then_some(raw as i8);
                    }

                    Readout {
                        sensors,
                        hottest: (s.hottest_temp != 0 || s.fault == 0)
                            .then_some((s.hottest_index as usize, s.hottest_temp)),
                        fan_rpm: s.fan_rpm,
                        mode: match s.mode {
                            ipc::MODE_SMART => "Smart",
                            ipc::MODE_MANUAL => "Manual",
                            _ => "BIOS",
                        },
                        mode_raw: s.mode,
                        fan_ctrl: s.fan_ctrl,
                        profile: s.read_profile(),
                        fault: s.fault != 0,
                        status: s.status,
                        single_fan_hint: hint,
                        layout_hint,
                    }
                },
            ));
        }

        // Truncated one short of the field and terminated by hand. The tip is
        // a fixed 128 wide characters and the shell reads it until a NUL, so a
        // long profile name and a long status line together could fill it edge
        // to edge and run off the end.
        let tip = wide(&tip);
        let n = tip.len().min(self.icon.szTip.len() - 1);
        self.icon.szTip[..n].copy_from_slice(&tip[..n]);
        self.icon.szTip[n] = 0;

        // NIM_MODIFY fails when the icon is not there: either the shell was
        // not ready when we started, or a TaskbarCreated broadcast was turned
        // away while something modal was open and will never be sent again.
        // NIM_ADD is idempotent, so the next refresh heals both.
        unsafe {
            if Shell_NotifyIconW(NIM_MODIFY, &self.icon) == 0 {
                Shell_NotifyIconW(NIM_ADD, &self.icon);
            }

            // Now, and not before: the shell has taken its own copy, and the
            // one it replaced is ours to get rid of.
            if let Some(old) = retired {
                if !old.is_null() {
                    DestroyIcon(old);
                }
            }
        }
    }

    /// Shows the menu and carries out whatever was chosen.
    ///
    /// The two halves are separate on purpose. Tracking the menu holds the
    /// modal pause, and the window procedure turns away anything wanting a
    /// second `&mut Tray` while that pause is up, WM_COMMAND included. A menu
    /// that posted its choice had it swallowed whenever TrackPopupMenu's own
    /// pump dispatched the post before returning, which is most of the time.
    /// The choice comes back as a return value instead, acted on here, after
    /// the pause has been dropped.
    fn show_menu(&mut self) {
        // Read afresh, because the settings window creates and deletes profiles
        // too, and owns two of the toggles below. A list cached at startup went
        // stale the moment it did, and the ids the menu hands back are indexes
        // into this one.
        self.reload_settings();

        let chosen = self.track_menu();

        if chosen > 0 {
            self.on_command(chosen as usize);
        }
    }

    /// Re-reads what the menu and the tooltip show from the settings file.
    ///
    /// A file that cannot be read leaves what is here alone rather than
    /// emptying it, since the last thing known to be true is better than
    /// nothing at all.
    fn reload_settings(&mut self) {
        let Ok(config) = yamato_core::Config::load(&yamato_core::Config::default_path()) else {
            return;
        };

        self.fahrenheit = config.fahrenheit;
        self.log_enabled = config.log_enabled;
        self.tray_numbers = config.tray_numbers;
        self.tray_sensor = config.tray_sensor;
        self.single_fan = config.single_fan;
        self.profiles = config.profiles.into_iter().map(|p| p.name).collect();
    }

    /// Builds the menu fresh each time so it always reflects reality: which
    /// mode is active, which profiles exist, and what the service is doing.
    ///
    /// Returns the id chosen, or zero if the menu was dismissed.
    fn track_menu(&self) -> i32 {
        let _pause = TimerPause::new(self.window);

        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return 0;
            }

            let (mode, fan_ctrl, profile) = match &self.channel {
                Some(c) => (c.get().mode, c.get().fan_ctrl, c.get().read_profile()),
                None => (ipc::MODE_BIOS, 0u8, String::new()),
            };

            let checked = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };

            // Modes are asked of the engine, so with nothing to ask they are
            // shown as unavailable rather than accepting a click and dropping
            // it. Profiles below stay live: choosing one is written to the
            // settings file, which is there whether an engine is or not.
            let live = if self.channel.is_some() { MF_ENABLED } else { MF_GRAYED };

            AppendMenuW(menu, MF_STRING | live | checked(mode == ipc::MODE_BIOS), ID_MODE_BIOS, wide("BIOS control").as_ptr());
            AppendMenuW(menu, MF_STRING | live | checked(mode == ipc::MODE_SMART), ID_MODE_SMART, wide("Smart").as_ptr());

            // Manual is a submenu, not one item, because the level is what
            // choosing it means. One item quietly picked 3, so the fan had
            // exactly one manual speed.
            //
            // The tick follows the level the engine says is in force, which is
            // not the last one asked for: above the escape temperature the fan
            // goes back to the firmware while the mode stays manual, and then
            // there is no level to tick.
            let held = (mode == ipc::MODE_MANUAL && fan_ctrl <= yamato_ec::FAN_LEVEL_MAX)
                .then_some(fan_ctrl);

            let manual = CreatePopupMenu();
            for level in 1..=yamato_ec::FAN_LEVEL_MAX {
                AppendMenuW(
                    manual,
                    MF_STRING | checked(held == Some(level)),
                    ID_MANUAL_BASE + level as usize - 1,
                    wide(&format!("Level {level}")).as_ptr(),
                );
            }

            AppendMenuW(
                menu,
                MF_POPUP | live | checked(mode == ipc::MODE_MANUAL),
                manual as usize,
                wide("Manual").as_ptr(),
            );
            AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());

            if !self.profiles.is_empty() {
                // Grayed heading, so a list of bare names is not mystery meat.
                AppendMenuW(menu, MF_STRING | MF_DISABLED, 0, wide("Profile").as_ptr());
            }

            for (i, name) in self.profiles.iter().enumerate() {
                AppendMenuW(
                    menu,
                    MF_STRING | checked(*name == profile),
                    ID_PROFILE_BASE + i,
                    wide(name).as_ptr(),
                );
            }
            let manage = CreatePopupMenu();
            AppendMenuW(manage, MF_STRING, ID_PROF_NEW, wide("New...").as_ptr());
            AppendMenuW(manage, MF_STRING, ID_PROF_DUPLICATE, wide("Duplicate...").as_ptr());
            AppendMenuW(manage, MF_STRING, ID_PROF_RENAME, wide("Rename...").as_ptr());
            AppendMenuW(
                manage,
                MF_STRING,
                ID_PROF_IMPORT,
                wide("Import from TPFanControl...").as_ptr(),
            );

            // Unavailable rather than missing, which would look like a bug.
            // Two things cause it: deleting the last profile would leave
            // nothing to run, and the three Yamato ships stay, because getting
            // one back means building it again from nothing. Editing or
            // renaming them is fine.
            let can_delete = self.profiles.len() > 1 && !yamato_core::is_built_in(&profile);
            AppendMenuW(
                manage,
                MF_STRING | if can_delete { MF_ENABLED } else { MF_GRAYED },
                ID_PROF_DELETE,
                wide("Delete").as_ptr(),
            );

            AppendMenuW(menu, MF_POPUP, manage as usize, wide("Manage profiles").as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());

            AppendMenuW(menu, MF_STRING, ID_SETTINGS, wide("Settings and fan curve...").as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());

            // Both programs take the same lock before touching the controller,
            // so neither corrupts the other's transactions. That is not the
            // same as being safe to run together: two of them with opinions
            // about the fan register take turns writing different levels into
            // it, and a manual level switches the firmware out of the fan
            // loop, so the fan chases two curves with nothing beneath it.
            // Disabled, because the other program is not ours to close.
            if pawnio_status::another_fan_tool_running() {
                AppendMenuW(
                    menu,
                    MF_STRING | MF_DISABLED,
                    0,
                    wide("TPFanControl is running - close it, they will fight").as_ptr(),
                );
                AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
            }

            // Always here, not only when something is wrong. PawnIO is the one
            // part Yamato cannot install for you, so the way to reach it has
            // to be findable at any time: to install it, to reinstall it, or
            // to read what it is. Hiding it while it worked meant the only
            // route to it appeared exactly when somebody was least able to go
            // hunting for a menu item. An attached engine softens the module
            // check: a file the running engine never needed is not a fault to
            // pin the menu with.
            let pawnio_label = match pawnio_status::diagnose(self.channel.is_some()) {
                pawnio_status::Missing::Nothing => "PawnIO driver...",
                pawnio_status::Missing::Outdated(_) => "PawnIO needs an update - click to fix",
                _ => "Yamato needs PawnIO - click to fix",
            };

            AppendMenuW(menu, MF_STRING, ID_PAWNIO, wide(pawnio_label).as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());

            // Service items track the real state, so the menu never offers
            // something that would just fail.
            let state = service::state();

            if state == -1 {
                AppendMenuW(menu, MF_STRING, ID_SVC_INSTALL, wide("Install service").as_ptr());
            } else {
                if state == SERVICE_RUNNING {
                    AppendMenuW(menu, MF_STRING, ID_SVC_STOP, wide("Stop service").as_ptr());
                } else {
                    AppendMenuW(menu, MF_STRING, ID_SVC_START, wide("Start service").as_ptr());
                }
                AppendMenuW(menu, MF_STRING, ID_SVC_UNINSTALL, wide("Uninstall service").as_ptr());
            }

            AppendMenuW(
                menu,
                MF_STRING | checked(self.fahrenheit),
                ID_UNITS,
                wide("Fahrenheit").as_ptr(),
            );

            // The number in the icon. On by default, and switchable for people
            // who want the plain dot or whose machine draws it badly.
            AppendMenuW(
                menu,
                MF_STRING | checked(self.tray_numbers),
                ID_TRAY_NUMBERS,
                wide("Temperature in the tray icon").as_ptr(),
            );

            // Which reading the icon reports. The fan still follows the
            // hottest sensor whatever is chosen here, because a curve watching
            // one nominated sensor would sit still while another part of the
            // machine cooked. This only changes what is displayed.
            let sensors = CreatePopupMenu();
            AppendMenuW(
                sensors,
                MF_STRING | checked(self.tray_sensor.is_none()),
                ID_TRAY_SENSOR_BASE,
                wide("Hottest").as_ptr(),
            );
            AppendMenuW(sensors, MF_SEPARATOR, 0, ptr::null());

            for (i, name) in crate::settings::SENSOR_NAMES.iter().enumerate() {
                let reading = self
                    .channel
                    .as_ref()
                    .map(|c| c.get().sensors[i])
                    .unwrap_or(ipc::SENSOR_ABSENT);

                // A sensor this machine does not have is grayed, not hidden, so
                // the list matches the window's sensor block and nobody
                // wonders which one is missing.
                let absent = reading == ipc::SENSOR_ABSENT;
                let label = if absent {
                    format!("{name}  --")
                } else {
                    format!(
                        "{name}  {}{}",
                        yamato_core::display_temp(reading as i8, self.fahrenheit),
                        yamato_core::unit_suffix(self.fahrenheit)
                    )
                };

                AppendMenuW(
                    sensors,
                    MF_STRING
                        | checked(self.tray_sensor == Some(i as u8))
                        | if absent { MF_GRAYED } else { MF_ENABLED },
                    ID_TRAY_SENSOR_BASE + 1 + i,
                    wide(&label).as_ptr(),
                );
            }

            AppendMenuW(
                menu,
                MF_POPUP,
                sensors as usize,
                wide("Tray shows").as_ptr(),
            );

            AppendMenuW(
                menu,
                MF_STRING | checked(crate::startup::is_enabled()),
                ID_STARTUP,
                wide("Start with Windows").as_ptr(),
            );

            // The history file. Off by default, so it is offered and not
            // announced, and the folder item is here because a file nobody can
            // find is not much of a record.
            AppendMenuW(
                menu,
                MF_STRING | checked(self.log_enabled),
                ID_LOGGING,
                wide("Log temperatures").as_ptr(),
            );
            AppendMenuW(menu, MF_STRING, ID_LOG_FOLDER, wide("Open log folder").as_ptr());

            AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());

            // The customary bottom group: what this program is, then the way
            // out. The About box carries the version, the warnings and the
            // attributions, and this menu is the only surface Yamato has, so
            // this is the one place those can be reachable from.
            AppendMenuW(menu, MF_STRING, ID_ABOUT, wide("About Yamato...").as_ptr());

            // Named for what it does, which is both halves: the tray goes and
            // so does the fan control. Stopping the service asks for
            // administrator rights, and hands the fan back to the firmware on
            // the way out.
            AppendMenuW(
                menu,
                MF_STRING,
                ID_EXIT,
                wide("Exit and stop controlling the fan").as_ptr(),
            );

            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);

            // Required, or the menu will not dismiss when clicked away from.
            SetForegroundWindow(self.window);
            // TPM_RETURNCMD: hand the id back instead of posting WM_COMMAND.
            // The settings window's profile menu has always worked this way,
            // and it is the only form that survives the modal pause above.
            let chosen =
                TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, self.window, ptr::null());
            DestroyMenu(menu);

            chosen
        }
    }

    fn on_command(&mut self, id: usize) {
        match id {
            ID_MODE_BIOS => self.post(ipc::MODE_BIOS, 0),
            ID_MODE_SMART => self.post(ipc::MODE_SMART, 0),
            ID_SETTINGS => self.open_settings(),
            // Reported, because a declined consent prompt or an install that
            // failed inside the elevated copy used to look exactly like one
            // that had worked.
            ID_PAWNIO => self.offer_pawnio(),
            ID_SVC_INSTALL => {
                self.service_command("--install", "Yamato could not install the service.")
            }
            ID_SVC_UNINSTALL => {
                self.service_command(
                    "--uninstall",
                    "Yamato could not remove the service. It may still be controlling the fan.",
                );
                self.channel = None;
            }
            ID_SVC_START => {
                self.service_command("--start-service", "Yamato could not start the service.");
                // Attach on the next refresh once it has published.
                self.channel = None;
            }
            ID_SVC_STOP => {
                // Stopping is not uninstalling. Running --uninstall here
                // removed the service outright when the user only asked it to
                // pause for a moment.
                self.service_command(
                    "--stop-service",
                    "Yamato could not stop the service. The fan may still be under its control.",
                );

                // Let go of the shared section. Holding it kept the mapping
                // alive and the tray went on showing the last sample as though
                // it were live, with a temperature that never changed again.
                self.channel = None;
                self.drawn = None;
            }
            ID_UNITS => self.toggle_units(),
            ID_TRAY_NUMBERS => self.toggle_tray_numbers(),
            id if (ID_TRAY_SENSOR_BASE
                ..=ID_TRAY_SENSOR_BASE + yamato_ec::SENSOR_COUNT)
                .contains(&id) =>
            {
                let choice = match id - ID_TRAY_SENSOR_BASE {
                    0 => None,
                    n => Some((n - 1) as u8),
                };

                self.set_tray_sensor(choice);
            }
            ID_LOGGING => self.toggle_logging(),
            ID_LOG_FOLDER => open_log_folder(),
            ID_ABOUT => self.show_about(),
            ID_STARTUP => {
                crate::startup::toggle();
            }
            ID_EXIT => {
                // Closing the settings window writes out what is in it, so
                // leaving by the tray must not be the one way out that throws
                // the same work away without asking.
                if let Some(settings) = self.settings.as_mut() {
                    settings.save_if_dirty();
                }

                // Exiting stops the fan control, because that is what exiting
                // a program means. A program that keeps working after you quit
                // it is a program you cannot quit. Stopping is also what hands
                // the fan back to the firmware, so the machine is left under
                // its own management instead of holding whatever level was
                // last set.
                //
                // It needs administrator rights, so there is a prompt. Start
                // with Windows will bring it back at the next logon.
                if service::state() == SERVICE_RUNNING {
                    self.service_command(
                        "--stop-service",
                        "Yamato could not stop the service, so it is still controlling the fan.",
                    );
                }

                unsafe { PostQuitMessage(0) }
            }
            ID_PROF_NEW => self.manage_profiles(ProfileAction::New),
            ID_PROF_DUPLICATE => self.manage_profiles(ProfileAction::Duplicate),
            ID_PROF_RENAME => self.manage_profiles(ProfileAction::Rename),
            ID_PROF_DELETE => self.manage_profiles(ProfileAction::Delete),
            ID_PROF_IMPORT => self.import_profile(),
            // The seven manual levels. Ids begin at level 1, so the offset is
            // one behind the level, and the range is closed at both ends: 0
            // stops the fan with the firmware switched off, and the disengage
            // byte is not offered at all.
            _ if (ID_MANUAL_BASE..ID_MANUAL_BASE + yamato_ec::FAN_LEVEL_MAX as usize)
                .contains(&id) =>
            {
                self.post(ipc::MODE_MANUAL, (id - ID_MANUAL_BASE) as u8 + 1);
            }
            _ if id >= ID_PROFILE_BASE => {
                if let Some(name) = self.profiles.get(id - ID_PROFILE_BASE).cloned() {
                    self.switch_profile(&name);
                }
            }
            _ => {}
        }
    }

    /// Runs a service verb, without a consent prompt where one is not needed,
    /// and says so when it did not work.
    ///
    /// Installing grants people logged in at the machine the right to start
    /// and stop this one service, so those two are done here and now.
    /// Installing and removing create or delete a service and do need
    /// administrator rights, so they still relaunch elevated.
    ///
    /// Tried directly first and elevated only if that is refused, which also
    /// covers a service installed by an older version, before the permission
    /// was being granted.
    fn service_command(&self, verb: &str, failure: &str) {
        let direct = match verb {
            "--start-service" => Some(service::start()),
            "--stop-service" => Some(service::stop()),
            _ => None,
        };

        match direct {
            Some(Ok(())) => return,
            Some(Err(_)) | None => {}
        }

        if !run_elevated(verb) {
            self.say(failure);
        }
    }

    /// Moves to the next profile in the list, wrapping round at the end.
    ///
    /// Read from the file rather than from the cached list, because that list
    /// is only refreshed when the menu opens and this can be asked for without
    /// the menu ever being opened at all.
    fn cycle_profile(&mut self) {
        let Ok(config) = yamato_core::Config::load(&yamato_core::Config::default_path()) else {
            return;
        };

        if config.profiles.len() < 2 {
            return;
        }

        // An active profile that matches nothing starts the cycle at the top
        // rather than refusing to move.
        let at = config
            .profiles
            .iter()
            .position(|p| p.name == config.active_profile)
            .unwrap_or(0);

        let next = config.profiles[(at + 1) % config.profiles.len()].name.clone();

        // The menu is built from this list, so it may as well be current.
        self.profiles = config.profiles.into_iter().map(|p| p.name).collect();

        self.switch_profile(&next);
    }

    /// Makes a profile the active one, in the settings file and in the engine.
    ///
    /// Written down as well as announced. The engine keeps its choice in
    /// memory and holds on to it when the file changes underneath, so a switch
    /// that was only posted lasted until the service next restarted. It also
    /// left the settings window editing whatever the file still called active
    /// while showing the name the engine had been given, so saving the curve
    /// wrote it into a different profile.
    ///
    /// It also means picking a profile does something with no engine running.
    fn switch_profile(&mut self, name: &str) {
        let path = yamato_core::Config::default_path();

        match yamato_core::Config::load(&path) {
            Ok(mut config) => {
                if !config.profiles.iter().any(|p| p.name == name) {
                    return;
                }

                if config.active_profile != name {
                    config.active_profile = name.to_string();

                    if config.save(&path).is_err() {
                        // Announcing it now would put the engine on a profile
                        // the file disagrees with, which is the state this
                        // exists to prevent.
                        self.say(CANNOT_SAVE);
                        return;
                    }
                }
            }
            Err(_) => {
                self.say(CANNOT_READ);
                return;
            }
        }

        // Then the engine, because it keeps running the profile it started
        // with when the file changes underneath it.
        //
        // MODE_KEEP, and not the mode the engine last published: echoing MANUAL
        // counts as a fresh manual instruction and clears the 80 C escape
        // latch, and echoing BIOS cancels a pending recovery. Choosing a curve
        // says nothing about who should be driving.
        if let Some(channel) = &self.channel {
            channel.post_command(ipc::MODE_KEEP, 0, name);
        }
    }

    /// Creates, copies, renames or removes a profile.
    ///
    /// Everything goes through the settings file, so this behaves identically
    /// whether the engine is this process, a service, or not running at all.
    ///
    /// The name is asked for *before* the file is read. A prompt can sit open
    /// for minutes, and the settings window stays usable while it does, so a
    /// config loaded beforehand can be badly out of date by the time it is
    /// written back, which would undo whatever was saved meanwhile.
    fn manage_profiles(&mut self, action: ProfileAction) {
        let _pause = TimerPause::new(self.window);

        // Only to seed the prompt and name the target. The copy that gets
        // written is loaded inside apply_profile_action, after the user has
        // answered.
        let path = yamato_core::Config::default_path();
        let Ok(before) = yamato_core::Config::load(&path) else {
            self.say(CANNOT_READ);
            return;
        };

        // Whichever profile is live, so the tray and the engine cannot
        // disagree about what is being renamed or deleted.
        let active = match &self.channel {
            Some(c) => c.get().read_profile(),
            None => before.active_profile.clone(),
        };

        let name = match action {
            ProfileAction::New => prompt::ask(self.window, "New profile", ""),
            ProfileAction::Duplicate => {
                prompt::ask(self.window, "Duplicate profile", &format!("{active} copy"))
            }
            ProfileAction::Rename => prompt::ask(self.window, "Rename profile", &active),
            // Asked about, because it is the one action here that destroys
            // something and the only way back is drawing the curve again point
            // by point. Named in the question too: the menu acts on whichever
            // profile is live, which is not necessarily the one somebody has
            // in mind when they reach for Delete.
            ProfileAction::Delete => {
                let answer = unsafe {
                    MessageBoxW(
                        self.window,
                        wide(&format!(
                            "Delete the profile \"{active}\"?\n\n\
                             Its curve goes with it, and there is no undo."
                        ))
                        .as_ptr(),
                        wide("Yamato").as_ptr(),
                        MB_YESNO | MB_ICONWARNING,
                    )
                };

                if answer == IDYES {
                    Some(active.clone())
                } else {
                    None
                }
            }
        };

        let Some(name) = name else { return };

        let config = match apply_profile_action(action, &active, &name) {
            Ok(config) => config,
            Err(message) => {
                self.say(message);
                return;
            }
        };

        // Tell the engine now instead of waiting for it to notice the file, so
        // the fan follows the new profile immediately.
        if let Some(channel) = &self.channel {
            channel.post_command(ipc::MODE_KEEP, 0, &config.active_profile);
        }

        // The menu is rebuilt from this list each time it opens.
        self.profiles = config.profiles.into_iter().map(|p| p.name).collect();
    }

    /// Brings a curve over from a TPFanControl ini.
    ///
    /// The work is in [`crate::import`], shared with the settings window; what
    /// is here is the tray's way of announcing the result and the bookkeeping
    /// that follows any profile being added.
    fn import_profile(&mut self) {
        // File dialog and name box both run loops of their own.
        let _pause = TimerPause::new(self.window);

        let Some(result) = crate::import::run(self.window) else { return };

        match result {
            Ok(outcome) => {
                if let Some(channel) = &self.channel {
                    channel.post_command(ipc::MODE_KEEP, 0, &outcome.config.active_profile);
                }

                self.profiles = outcome.config.profiles.into_iter().map(|p| p.name).collect();
                self.say(&outcome.summary);
            }
            Err(message) => self.say(&message),
        }
    }

    /// Says where PawnIO stands and offers to go and get it.
    ///
    /// Offered whatever the answer, including when nothing is wrong. PawnIO is
    /// the one piece Yamato cannot install for you, so somebody may want the
    /// page to install it, repair it, or read what they are being asked to
    /// trust with their machine. A missing module file has nowhere useful to
    /// send anyone, since it came from an incomplete copy of Yamato and not a
    /// missing driver, so it says so and stops.
    fn offer_pawnio(&mut self) {
        let missing = pawnio_status::diagnose(self.channel.is_some());
        let _pause = TimerPause::new(self.window);

        if let pawnio_status::Missing::Module(_) = missing {
            drop(_pause);
            self.say(&missing.explain());
            return;
        }

        let answer = unsafe {
            MessageBoxW(
                self.window,
                wide(&missing.explain()).as_ptr(),
                wide("Yamato").as_ptr(),
                MB_YESNO | MB_ICONINFORMATION,
            )
        };

        if answer == IDYES {
            unsafe {
                ShellExecuteW(
                    ptr::null_mut(),
                    wide("open").as_ptr(),
                    wide(pawnio_status::DOWNLOAD_URL).as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    SW_SHOWNORMAL,
                );
            }
        }
    }

    /// Chooses the reading the icon reports, and remembers it.
    ///
    /// Saved, not held in memory, so the next session agrees with this one,
    /// and written through the same load-mutate-save the rest of the menu
    /// uses, so a failure here cannot leave the file half-written.
    fn set_tray_sensor(&mut self, sensor: Option<u8>) {
        let path = yamato_core::Config::default_path();
        let Ok(mut config) = yamato_core::Config::load(&path) else {
            self.say("Yamato could not read its settings file.");
            return;
        };

        config.tray_sensor = sensor;

        if config.save(&path).is_err() {
            self.say("Yamato could not save its settings file.");
            return;
        }

        self.tray_sensor = sensor;

        // Now, not at the next tick, so the icon and the tooltip agree with
        // the tick that was just placed against the menu.
        self.refresh();
    }

    /// A message box that does not block the tray's own message loop badly.
    ///
    /// It runs a message loop of its own, so the refresh timer has to stop
    /// first: a WM_TIMER dispatched from inside it would re-enter the window
    /// procedure and take a second `&mut Tray` while the call that opened this
    /// box still holds one.
    fn say(&self, text: &str) {
        let _pause = TimerPause::new(self.window);

        unsafe {
            MessageBoxW(
                self.window,
                wide(text).as_ptr(),
                wide("Yamato").as_ptr(),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }

    /// Shows the About box: the version, the warnings, and the attributions.
    ///
    /// It runs a message loop of its own, so it takes the pause for the same
    /// reason every modal here does: a WM_TIMER dispatched from inside that
    /// loop would re-enter the window procedure and take a second `&mut Tray`
    /// while this call still holds one.
    fn show_about(&self) {
        let _pause = TimerPause::new(self.window);

        crate::about::show(self.window);
    }

    /// Flips between Celsius and Fahrenheit, everywhere at once.
    ///
    /// Saved, not held in memory, so the window and the next session agree
    /// with the tray. Display only: the curve stays in Celsius.
    fn toggle_units(&mut self) {
        let path = yamato_core::Config::default_path();
        let Ok(mut config) = yamato_core::Config::load(&path) else { return };

        config.fahrenheit = !config.fahrenheit;

        if config.save(&path).is_ok() {
            self.fahrenheit = config.fahrenheit;
            // The tooltip and the number in the icon both change unit, and
            // neither should wait for the next sample to say so.
            self.drawn = None;
            self.refresh();
        }
    }

    /// Puts the temperature in the tray icon, or takes it out again.
    fn toggle_tray_numbers(&mut self) {
        let path = yamato_core::Config::default_path();
        let Ok(mut config) = yamato_core::Config::load(&path) else { return };

        config.tray_numbers = !config.tray_numbers;

        if config.save(&path).is_ok() {
            self.tray_numbers = config.tray_numbers;
            self.drawn = None;
            self.refresh();
        }
    }

    /// Turns the history file on or off.
    ///
    /// Saved, not held in memory, because the engine is usually another
    /// process and the settings file is the only thing both of us read. It
    /// picks the change up on its next pass.
    fn toggle_logging(&mut self) {
        let path = yamato_core::Config::default_path();
        let Ok(mut config) = yamato_core::Config::load(&path) else { return };

        config.log_enabled = !config.log_enabled;

        if config.save(&path).is_ok() {
            self.log_enabled = config.log_enabled;
        }
    }

    /// Opens the curve editor, or brings it back if it was closed.
    ///
    /// Closing that window hides it and keeps it built, so this has to show it
    /// again. Returning early on finding one already made left it hidden with
    /// nothing anywhere that would show it again, so the menu item and the
    /// double click both stopped working after the first close.
    pub fn open_settings(&mut self) {
        if let Some(settings) = self.settings.as_ref() {
            let window = settings.hwnd();

            // A destroyed window leaves a handle that no longer names
            // anything, and showing that fails quietly and for good. Building
            // a fresh one is the honest recovery.
            if unsafe { IsWindow(window) } != 0 {
                unsafe {
                    if IsIconic(window) != 0 {
                        ShowWindow(window, SW_RESTORE);
                    } else {
                        ShowWindow(window, SW_SHOW);
                    }

                    // Or it comes back behind whatever the user was looking at,
                    // which reads exactly like the click having done nothing.
                    SetForegroundWindow(window);
                }

                return;
            }

            self.settings = None;
        }

        let config = yamato_core::Config::load(&yamato_core::Config::default_path())
            .unwrap_or_default();

        if let Ok(curve) = config.active_curve() {
            // The name goes with the curve, so the window knows which profile
            // it is editing rather than looking it up again when it saves.
            match Settings::new(Editor::new(&curve), config.active_profile.clone(), self.window) {
                Ok(window) => {
                    // Restore the saved window position and size if available
                    if let Some((x, y, width, height)) = self.settings_rect {
                        unsafe {
                            SetWindowPos(
                                window.hwnd(),
                                ptr::null_mut(),
                                x,
                                y,
                                width,
                                height,
                                SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                        }
                    }
                    self.settings = Some(window);
                }
                Err(_) => { /* nothing sensible to do; the tray stays up */ }
            }
        }
    }

    /// Toggles the settings window: closes it if visible, opens it if not.
    pub fn toggle_settings(&mut self) {
        if let Some(settings) = self.settings.as_ref() {
            let window = settings.hwnd();

            // Check if the window exists and is visible
            if unsafe { IsWindow(window) } != 0 && unsafe { IsWindowVisible(window) } != 0 {
                // Save the window position and size before closing
                let mut rect: windows_sys::Win32::Foundation::RECT = unsafe { std::mem::zeroed() };
                if unsafe { GetWindowRect(window, &mut rect) } != 0 {
                    self.settings_rect = Some((
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                    ));
                }

                // Window is visible, so close it completely
                unsafe {
                    DestroyWindow(window);
                }
                self.settings = None;
                return;
            }
        }

        // Window doesn't exist or isn't visible, so open/show it
        self.open_settings();
    }

    fn post(&self, mode: u8, level: u8) {
        if let Some(channel) = &self.channel {
            let profile = channel.get().read_profile();
            channel.post_command(mode, level, &profile);
        }
    }

    pub fn set_profiles(&mut self, profiles: Vec<String>) {
        self.profiles = profiles;
    }

    /// Pumps messages until Exit. Returns when the tray is dismissed.
    pub fn run(&mut self) {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            // Clear the back-pointer first. A message arriving after this
            // would otherwise reach memory that is about to be freed.
            SetWindowLongPtrW(self.window, GWLP_USERDATA, 0);
            KillTimer(self.window, TIMER_REFRESH);

            // Handed back whether or not they were ever ours: unregistering one
            // that failed to register is a no-op, and leaving a live
            // registration on a window that is going away is not.
            for id in [HOTKEY_BIOS, HOTKEY_SMART, HOTKEY_PROFILE] {
                UnregisterHotKey(self.window, id);
            }

            Shell_NotifyIconW(NIM_DELETE, &self.icon);
            if !self.icon.hIcon.is_null() {
                DestroyIcon(self.icon.hIcon);
            }
        }
    }
}

unsafe extern "system" fn wnd_proc(
    window: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Answered before anything else, because it needs no `&mut Tray` and so
    // has no business waiting behind whatever modal thing is open. Slowing the
    // timer down is worth doing even while a name box is up.
    if msg == WM_POWERBROADCAST {
        on_display_state(wparam, lparam, window);
        return 1;
    }

    let tray = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Tray;

    if !tray.is_null() {
        // Something modal is open above us and still holds a `&mut Tray`, so
        // anything wanting one of its own has to be turned away. This has to
        // run before the reference below exists, which is why the message it
        // compares against is read through the pointer. Menu choices are not
        // lost: TrackPopupMenu posts its WM_COMMAND, and that arrives once the
        // menu has closed and the pause has been dropped.
        if MODAL_DEPTH.with(|d| d.get()) > 0 {
            match msg {
                // WM_HOTKEY belongs here with the rest: a chord pressed while
                // the name box is open would switch profiles under the
                // question being asked about them, and it needs a `&mut Tray`
                // of its own to do it. Dropped, not queued, because a mode
                // change nobody sees the result of is not worth carrying out
                // three dialogs later.
                WM_TIMER | WM_TRAY | WM_COMMAND | WM_HOTKEY => return 0,
                _ if msg == (*tray).taskbar_created => return 0,
                _ => {
                    return match msg {
                        WM_DESTROY => {
                            PostQuitMessage(0);
                            0
                        }
                        _ => DefWindowProcW(window, msg, wparam, lparam),
                    };
                }
            }
        }

        let tray = &mut *tray;

        // Explorer restarting, or a logon where we started before the taskbar
        // was ready to accept icons. Without this the icon simply never
        // appears and nothing says why.
        if msg == tray.taskbar_created {
            tray.add_icon();
            tray.refresh();
            return 0;
        }

        match msg {
            WM_TIMER if wparam == TIMER_REFRESH => {
                tray.refresh();
                return 0;
            }
            WM_TRAY => {
                match lparam as u32 {
                    WM_RBUTTONUP | WM_CONTEXTMENU => tray.show_menu(),
                    WM_LBUTTONUP => tray.toggle_settings(),
                    WM_LBUTTONDBLCLK => tray.on_command(ID_SETTINGS),
                    _ => {}
                }
                return 0;
            }
            WM_COMMAND => {
                tray.on_command((wparam & 0xffff) as usize);
                return 0;
            }
            WM_HOTKEY => {
                // Through on_command, so a hotkey and the menu item beside it
                // cannot drift into meaning different things. Cycling profiles
                // has no menu item to borrow, but it goes the same way as
                // choosing one from the list, MODE_KEEP and all: none of these
                // three says anything about who drives the fan.
                match wparam as i32 {
                    HOTKEY_BIOS => tray.on_command(ID_MODE_BIOS),
                    HOTKEY_SMART => tray.on_command(ID_MODE_SMART),
                    HOTKEY_PROFILE => tray.cycle_profile(),
                    _ => {}
                }
                return 0;
            }
            _ => {}
        }
    }

    match msg {
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(window, msg, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_embedded_icon_is_a_real_ico() {
        // Reserved word 0, type 1 (icon), at least one image.
        for (name, bytes) in [
            ("normal", ICO_NORMAL),
            ("warm", ICO_WARM),
            ("hot", ICO_HOT),
            ("idle", ICO_IDLE),
        ] {
            assert!(bytes.len() > 22, "{name} is too small to be an icon");
            assert_eq!(u16::from_le_bytes([bytes[0], bytes[1]]), 0, "{name} reserved word");
            assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 1, "{name} is not an icon");
            assert!(u16::from_le_bytes([bytes[4], bytes[5]]) > 0, "{name} has no images");
        }
    }

    #[test]
    fn thermal_bands_are_ordered() {
        assert!(WARM_AT < HOT_AT);
    }

    #[test]
    fn the_single_fan_hint_demands_the_whole_signature() {
        // Declined writes, a second fan that has never spun, and the setting
        // not already on: only all three together.
        assert!(suggest_single_fan(true, false, false));

        // One revolution of fan 2 is proof of a dual-fan machine, and the
        // hint must never appear on one: switching the setting on there would
        // disable the very verification that catches a second fan left
        // holding a level with the firmware off.
        assert!(!suggest_single_fan(true, true, false));

        // A controller that is not declining, whether healthy or simply not
        // answering, is not this problem.
        assert!(!suggest_single_fan(false, false, false));

        // Advice already taken is noise.
        assert!(!suggest_single_fan(true, false, true));
    }

    #[test]
    fn the_menu_hands_its_choice_back_instead_of_posting_it() {
        // Regression. TrackPopupMenu without TPM_RETURNCMD posts WM_COMMAND,
        // and the re-entrancy guard turns WM_COMMAND away while the menu's own
        // modal pause is up. TrackPopupMenu pumps messages itself and usually
        // dispatches that post before returning, so every item in the tray
        // menu silently did nothing.
        let source = include_str!("tray.rs");

        // Split, or the filter below matches its own source line.
        let call = concat!("TrackPopupMenu", "(");

        for line in source.lines().filter(|l| l.contains(call)) {
            assert!(
                line.contains("TPM_RETURNCMD"),
                "the tray menu must not post its choice: {line}"
            );
        }
    }

    #[test]
    fn a_hotkey_arriving_while_something_modal_is_open_is_turned_away() {
        // The guard has to name WM_HOTKEY explicitly. Anything it does not
        // name reaches the arm below it, which takes a `&mut Tray` while the
        // call holding the modal pause still has one.
        let source = include_str!("tray.rs");
        let guard = source
            .split("MODAL_DEPTH.with(|d| d.get()) > 0")
            .nth(1)
            .expect("the re-entrancy guard has gone");

        let swallowed = guard.lines().find(|l| l.contains("=> return 0")).unwrap_or("");

        assert!(swallowed.contains("WM_HOTKEY"), "hotkeys are not turned away: {swallowed}");
    }

    #[test]
    fn no_hotkey_asks_for_a_fixed_fan_level() {
        // Choosing a level takes the firmware out of the loop, and a chord is
        // a thing that gets pressed by accident, or leant on.
        for id in [HOTKEY_BIOS, HOTKEY_SMART, HOTKEY_PROFILE] {
            assert!(!(ID_MANUAL_BASE..ID_MANUAL_BASE + 8).contains(&(id as usize)));
        }
        assert_eq!([HOTKEY_BIOS, HOTKEY_SMART, HOTKEY_PROFILE].len(), 3);
    }

    #[test]
    fn profile_ids_cannot_collide_with_fixed_commands() {
        // Profiles are numbered from a base above every fixed id, so adding a
        // menu item later cannot silently start selecting a profile.
        for id in [
            ID_MODE_BIOS,
            ID_SETTINGS,
            ID_SVC_INSTALL,
            ID_STARTUP,
            ID_LOGGING,
            ID_LOG_FOLDER,
            ID_EXIT,
            ID_PAWNIO,
            ID_TRAY_NUMBERS,
            ID_ABOUT,
            ID_MANUAL_BASE + yamato_ec::FAN_LEVEL_MAX as usize - 1,
            ID_TRAY_SENSOR_BASE + yamato_ec::SENSOR_COUNT,
        ] {
            assert!(id < ID_PROFILE_BASE);
        }
    }

    #[test]
    fn the_sensor_ids_sit_clear_of_the_ranges_on_either_side() {
        // Manual levels below, profiles above. A range that grew into either
        // would quietly turn a sensor choice into a fan level or a profile
        // switch, which shows up only as the wrong thing happening when
        // somebody clicks.
        assert!(ID_MANUAL_BASE + yamato_ec::FAN_LEVEL_MAX as usize <= ID_TRAY_SENSOR_BASE);
        assert!(ID_TRAY_SENSOR_BASE + yamato_ec::SENSOR_COUNT < ID_PROFILE_BASE);
    }
}
