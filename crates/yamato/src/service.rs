// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! Running as a Windows service, and installing ourselves as one.
//!
//! The service is how the fan gets controlled before anyone logs in. It is
//! also the only context that can reach the port driver without a consent
//! prompt, which is what lets the window run without administrator rights.

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use yamato_core::Config;
use windows_sys::core::{GUID, PWSTR};
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS, ERROR_SERVICE_NOT_ACTIVE,
    HANDLE,
};
use windows_sys::Win32::System::EventLog::{
    DeregisterEventSource, EvtRender, EvtSubscribe, EvtRenderEventXml, EvtSubscribeActionDeliver,
    EvtSubscribeToFutureEvents, RegisterEventSourceW, ReportEventW, EVENTLOG_ERROR_TYPE,
    EVENTLOG_INFORMATION_TYPE, EVENTLOG_WARNING_TYPE, EVT_HANDLE, EVT_SUBSCRIBE_NOTIFY_ACTION,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::System::Power::POWERBROADCAST_SETTING;
use windows_sys::Win32::System::Services::*;
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects,
};
use windows_sys::Win32::System::SystemServices::{
    GUID_ACDC_POWER_SOURCE, GUID_CONSOLE_DISPLAY_STATE, GUID_LIDSWITCH_STATE_CHANGE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_SERVICE_HANDLE, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND,
    PBT_POWERSETTINGCHANGE,
};

/// Standard access right. windows-sys only exposes it as a file right, which
/// it also is, but here it is the one needed to remove a service.
const DELETE: u32 = 0x0001_0000;

/// Says "the code that matters is in dwServiceSpecificExitCode". Any non-zero
/// exit is what makes the manager treat a stop as a failure worth retrying.
const ERROR_SERVICE_SPECIFIC_ERROR: u32 = 1066;

// Not bound by windows-sys 0.59, so declared here. This is what tells us the
// machine is heading into standby on hardware that has no S3 to suspend to.
#[link(name = "user32")]
extern "system" {
    pub(crate) fn RegisterPowerSettingNotification(
        hrecipient: HANDLE,
        powersettingguid: *const GUID,
        flags: u32,
    ) -> HANDLE;
}

use crate::engine_host::{Host, PowerState, StopFlag};

pub const SERVICE_NAME: &str = "Yamato";
pub const DISPLAY_NAME: &str = "Yamato Fan Control";

/// Set by the control handler, read by the run loop. A plain atomic rather
/// than a lock, because the handler runs on a thread the service manager owns
/// and must return promptly.
static POWER: AtomicU8 = AtomicU8::new(0);
static CHECKPOINT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const POWER_AWAKE: u8 = 0;
const POWER_SCREEN_OFF: u8 = 1;
const POWER_SUSPENDED: u8 = 2;

/// The two things the system tells us, kept apart because they change
/// independently and either one can arrive first.
///
/// A machine can resume from suspend with its screen still off, which is how
/// standby maintenance work happens, and folding the two into one value made
/// that look like a full wake.
static DISPLAY_ON: AtomicBool = AtomicBool::new(true);
static SUSPENDING: AtomicBool = AtomicBool::new(false);

/// The lid switch, and whether it is plugged in.
///
/// Both default to the state that changes nothing, so a machine that never
/// reports either behaves exactly as it did before they were asked for.
///
/// The lid is asked for because the display state does not always answer.
/// Undock a laptop whose lid has been shut the whole time and Windows lights
/// the internal panel behind it: one display, console display on, no standby,
/// and nothing said. The panel is under a closed lid, so nobody sees it, but
/// as far as every notification goes the machine is sitting there awake with
/// its screen on. The lid switch is hardware and says what is actually true.
static LID_OPEN: AtomicBool = AtomicBool::new(true);
static ON_BATTERY: AtomicBool = AtomicBool::new(false);

/// Whether a Modern Standby session is open, on the system's own say-so.
///
/// There is no broadcast for this. There is, however, an event log entry, and
/// subscribing to it is a push notification like any other: Kernel-Power logs
/// 506 when a session begins and 507 when it ends. TPFanControl does the same
/// thing, and it is a better answer than the clocks, which can only report a
/// sleep once it is over. The clocks stay as a backstop for machines where the
/// subscription cannot be made.
static IN_MODERN_STANDBY: AtomicBool = AtomicBool::new(false);

/// Set by the run loop once it has acted on a suspend, watched by the handler.
///
/// A suspend broadcast is the last chance to write the fan register before the
/// power goes, so the handler waits briefly for the handback rather than
/// returning and letting the machine suspend with a level still held.
static SUSPEND_HANDLED: AtomicBool = AtomicBool::new(false);

/// How long that wait may last.
///
/// Windows gives an application a couple of seconds to deal with a suspend
/// before going ahead without it, so this stays inside that. It is best
/// effort: a pass already talking to a busy controller can outlast it, and
/// blocking a power event indefinitely would be the worse fault.
const SUSPEND_ACK_WAIT: Duration = Duration::from_millis(1_500);

/// How many times to try handing the fan back before giving up and letting the
/// service manager decide. Generous, because every pass reports progress, so a
/// slow but healthy stop is not mistaken for a hang, and the alternative is
/// exiting with the fan still set.
const HANDBACK_ATTEMPTS: u32 = 6;

/// Total time the stop path will spend trying to hand the fan back.
///
/// Sized to fit inside a preshutdown (180 s) with room to spare, and to be a
/// number uninstall can actually wait out.
const HANDBACK_BUDGET: Duration = Duration::from_secs(45);

/// Longest a single handback attempt can credibly take.
///
/// Three release passes, each pursuing the lock for about thirteen seconds and
/// then waiting out EC handshake timeouts. Advertised to the service manager
/// so it waits for a slow handback instead of terminating one.
const ATTEMPT_WORST_CASE_MS: u32 = 110_000;

static STOP: OnceLock<Arc<StopFlag>> = OnceLock::new();
static STATUS_HANDLE: OnceLock<usize> = OnceLock::new();

/// What the control handler signals to cut the run loop's wait short.
///
/// Manual reset, so a stop or a power change that arrives while the loop is
/// between waits is still there when it looks. Unnamed and with the default
/// descriptor: nothing outside this process ever touches it.
static CONTROL_EVENT: OnceLock<usize> = OnceLock::new();

fn control_event() -> HANDLE {
    CONTROL_EVENT.get().map_or(ptr::null_mut(), |h| *h as HANDLE)
}

/// Wakes the run loop now rather than at the end of its interval.
fn signal_control_event() {
    let handle = control_event();

    if !handle.is_null() {
        unsafe { SetEvent(handle) };
    }
}

/// Sleeps until something happens or the interval runs out.
///
/// False when there is nothing to wait on at all, which leaves the caller to
/// fall back rather than spin.
fn wait_for_wake(command: HANDLE, left: Duration) -> bool {
    let mut handles = [ptr::null_mut(); 2];
    let mut count = 0;

    for handle in [control_event(), command] {
        if !handle.is_null() {
            handles[count] = handle;
            count += 1;
        }
    }

    if count == 0 {
        return false;
    }

    // Saturating, because a hand-edited standby poll is still bounded well
    // below the point where this would matter, and wrapping it would turn a
    // long wait into no wait at all.
    let ms = left.as_millis().min(u32::MAX as u128 - 1) as u32;

    unsafe { WaitForMultipleObjects(count as u32, handles.as_ptr(), 0, ms) };
    true
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn stop_flag() -> Arc<StopFlag> {
    Arc::clone(STOP.get_or_init(|| Arc::new(StopFlag::default())))
}

/// Entry point the service manager calls. Blocks until the service stops.
pub fn run() -> bool {
    let name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_ptr() as *mut u16,
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW { lpServiceName: ptr::null_mut(), lpServiceProc: None },
    ];

    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) != 0 }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let name = wide(SERVICE_NAME);
    let handle = RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control_handler), ptr::null_mut());
    if handle.is_null() {
        return;
    }

    let _ = STATUS_HANDLE.set(handle as usize);

    // Created before anything can signal it. Failure is survivable: the run
    // loop falls back to short sleeps, which is what it used to do.
    let event = CreateEventW(ptr::null(), 1, 0, ptr::null());
    if !event.is_null() {
        let _ = CONTROL_EVENT.set(event as usize);
    }

    // Ask for power events so we notice Modern Standby. On hardware with no S3
    // the legacy suspend broadcast never arrives, so the display-state
    // notification is the signal that actually fires.
    //
    // The lid and the power source are asked for because the display state is
    // not always telling the truth about what the machine is doing. Each one
    // is delivered once at registration with its current value, so all three
    // are known before the first pass rather than at the first change.
    for setting in [
        &GUID_CONSOLE_DISPLAY_STATE,
        &GUID_LIDSWITCH_STATE_CHANGE,
        &GUID_ACDC_POWER_SOURCE,
    ] {
        RegisterPowerSettingNotification(handle as HANDLE, setting, DEVICE_NOTIFY_SERVICE_HANDLE);
    }

    subscribe_to_standby_events();

    report(SERVICE_START_PENDING, 15_000);

    let config = Config::load(&Config::default_path()).unwrap_or_default();

    match Host::start(config, stop_flag()) {
        Ok(host) => {
            // Which EC port layout the probe chose and what both answered,
            // into the application log. This is the only place the choice is
            // visible from outside the process, and the first question about
            // any misbehaving machine nobody can test is which path it was on.
            log_event(
                EVENTLOG_INFORMATION_TYPE,
                &format!("Yamato started, {}.", host.ec_report()),
            );

            report(SERVICE_RUNNING, 0);
            run_loop(host);
            report(SERVICE_STOPPED, 0);
        }
        Err(reason) => {
            // Nothing can be shown from session 0, and a message box here
            // would hold the service in start-pending until something killed
            // it. Failing outright is the honest outcome, and the reason
            // still goes into the application log. A probed-and-unanswering
            // controller is deliberately not among the reasons: the engine
            // starts on a fallback layout and retries instead, so what
            // remains here is the driver missing or refusing us, a module
            // file that would not load, or another engine holding the lock.
            //
            // A warning, not an error, since most arrivals here pass on
            // their own: the port driver not yet openable during a cold
            // boot, and a previous process still holding the engine lock
            // through a fast restart.
            log_event(EVENTLOG_WARNING_TYPE, &format!("Yamato could not start: {reason}"));

            // Reported as a failure rather than a stop, which is what makes
            // the restart actions installed at setup mean anything: an exit
            // code of zero reads as "asked to stop and did", and the service
            // manager does not retry those. Reported as a stop, the
            // transient failures above never got the second attempt that
            // would have worked, and the machine ran unmanaged until
            // somebody noticed.
            report_failure();
        }
    }
}

/// Reports a stop the service manager should act on.
///
/// A specific code, because a generic one is indistinguishable from the
/// service having been asked to stop.
fn report_failure() {
    let Some(handle) = STATUS_HANDLE.get() else { return };

    let mut status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: SERVICE_STOPPED,
        dwControlsAccepted: 0,
        dwWin32ExitCode: ERROR_SERVICE_SPECIFIC_ERROR,
        dwServiceSpecificExitCode: 1,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    };

    unsafe { SetServiceStatus(*handle as SERVICE_STATUS_HANDLE, &mut status) };
}

/// Drives the host, applying power state changes the handler recorded.
fn run_loop(mut host: Host) {
    let stop = stop_flag();

    while !stop.stopped() {
        let power = POWER.load(Ordering::SeqCst);
        host.set_power_state(match power {
            POWER_SCREEN_OFF => PowerState::ScreenOff,
            POWER_SUSPENDED => PowerState::Suspended,
            _ => PowerState::Awake,
        });

        // After the state has been applied, which for a suspend means after
        // the fan is back with the firmware. The handler is waiting on this.
        if power == POWER_SUSPENDED {
            SUSPEND_HANDLED.store(true, Ordering::SeqCst);
        }

        host.tick();

        // Checked after the tick as well as before, so a stop that arrived
        // while we were talking to the controller is acted on now.
        if stop.stopped() {
            break;
        }

        // Wait out the poll interval asleep on two handles rather than awake
        // watching a clock.
        //
        // What breaks the wait is unchanged, and all three now break it at
        // once rather than within a fifth of a second. A stop, because it must
        // be noticed promptly even when the standby interval is long. A power
        // change, because waiting out a standby interval after a wake delayed
        // the first decision by up to two minutes, and that decision is where
        // the manual-mode thermal escape is evaluated. A command, so choosing
        // a mode or a profile takes effect now instead of at the end of the
        // interval.
        //
        // What has changed is the cost of waiting. This ran a 200 ms sleep in
        // a loop, so the service woke five times a second forever. A machine
        // with something running at that rate never reaches the low-power
        // state modern standby exists to reach, so the laptop did not really
        // sleep, and the clocks the engine reads to notice sleep then agreed
        // that it had not. Nothing else in the loop moves: the same tick, the
        // same retries against a controller somebody else is using, the same
        // watchdog.
        let deadline = Instant::now() + host.poll_interval();

        loop {
            // Cleared before the conditions are read, not after. A command
            // that lands between the check and the wait leaves the event set,
            // so the wait returns at once instead of losing it.
            host.clear_command_event();
            if !control_event().is_null() {
                unsafe { ResetEvent(control_event()) };
            }

            if stop.stopped()
                || POWER.load(Ordering::SeqCst) != power
                || host.command_waiting()
            {
                break;
            }

            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }

            if !wait_for_wake(host.command_event(), left) {
                // No handle to wait on, so fall back to what this always did
                // rather than spinning. Only reachable if creating the event
                // failed, which also means nothing will ever signal it.
                std::thread::sleep(Duration::from_millis(200).min(left));
            }
        }
    }

    // Do not leave while the fan is still ours. Reporting STOPPED after a
    // failed handback tells the manager the job is done and ends the process
    // with a manual level set and the firmware disabled.
    //
    // This loop cannot promise never to report it, since falling out of the
    // bottom does exactly that. What it can do is bound the attempt and say so
    // on the way out.
    let deadline = Instant::now() + HANDBACK_BUDGET;

    for attempt in 0..HANDBACK_ATTEMPTS {
        // Sized to a single attempt's worst case, not to the time left in the
        // loop. One shutdown() can spend around a hundred seconds pursuing the
        // lock and waiting out handshake timeouts against a wedged controller,
        // so a smaller hint invites the manager to kill us part way through a
        // handback, with the fan still ours.
        let left = deadline.saturating_duration_since(Instant::now());
        let hint = (left.as_millis() as u32).max(ATTEMPT_WORST_CASE_MS) + 5_000;
        report(SERVICE_STOP_PENDING, hint);

        if host.shutdown().is_ok() {
            return;
        }

        if Instant::now() >= deadline {
            break;
        }

        // Something else is likely holding the controller. Give it room.
        if attempt + 1 < HANDBACK_ATTEMPTS {
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    // Out of budget with the fan still set. Refusing to report STOPPED only
    // gets the process terminated a moment later in the same state, so leave a
    // record instead.
    log_handback_failure();
}

/// Records a handback that never completed, into the application event log.
fn log_handback_failure() {
    log_event(
        EVENTLOG_ERROR_TYPE,
        "Yamato could not return the fan to firmware control before stopping. The fan may be held at a fixed level with firmware management disabled. Restart Yamato, or reboot, to restore automatic fan control.",
    );
}

/// Writes one line into the application event log.
///
/// Best effort by design. Every caller is somewhere a failure to log must not
/// matter: on the way out of a stop that has already gone wrong, or on the
/// way into a start whose outcome is decided regardless.
fn log_event(kind: u16, text: &str) {
    unsafe {
        let source = wide(SERVICE_NAME);
        let handle = RegisterEventSourceW(std::ptr::null(), source.as_ptr());
        if handle.is_null() {
            return;
        }

        let text = wide(text);
        let mut strings = [text.as_ptr()];

        ReportEventW(
            handle,
            kind,
            0,
            1,
            std::ptr::null_mut(),
            1,
            0,
            strings.as_mut_ptr(),
            std::ptr::null_mut(),
        );

        DeregisterEventSource(handle);
    }
}

unsafe extern "system" fn control_handler(
    control: u32,
    event_type: u32,
    event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN | SERVICE_CONTROL_PRESHUTDOWN => {
            // Generous hint: handing the fan back involves an EC handshake,
            // and being declared hung halfway through that is how a level gets
            // left set with the firmware switched off.
            report(SERVICE_STOP_PENDING, 20_000);
            stop_flag().stop();
            signal_control_event();
        }
        SERVICE_CONTROL_POWEREVENT => match event_type {
            PBT_POWERSETTINGCHANGE if !event_data.is_null() => {
                let setting = &*(event_data as *const POWERBROADCAST_SETTING);

                if setting.DataLength == 0 {
                    return NO_ERROR;
                }

                // All three of these carry a DWORD whose values are 0, 1 or 2,
                // so the first byte is the whole answer on this architecture.
                let value = *setting.Data.as_ptr();
                let guid = &setting.PowerSetting;

                if same_guid(guid, &GUID_CONSOLE_DISPLAY_STATE) {
                    // A screen coming on is the other end of a standby
                    // session, and a more reliable one than the log entry,
                    // since it is broadcast rather than rendered and searched.
                    if value != 0 {
                        IN_MODERN_STANDBY.store(false, Ordering::SeqCst);
                    }

                    // 0 off, 1 on, 2 dimmed. A screen that has gone off is
                    // where Modern Standby begins, and it is also a docked
                    // laptop working with its lid shut. Recorded as what it
                    // is; the engine works out which.
                    DISPLAY_ON.store(value != 0, Ordering::SeqCst);
                } else if same_guid(guid, &GUID_LIDSWITCH_STATE_CHANGE) {
                    // 0 closed, 1 open.
                    LID_OPEN.store(value != 0, Ordering::SeqCst);
                } else if same_guid(guid, &GUID_ACDC_POWER_SOURCE) {
                    // 0 wall power, 1 battery, 2 a short-term supply.
                    ON_BATTERY.store(value != 0, Ordering::SeqCst);
                } else {
                    return NO_ERROR;
                }

                publish_power();
                signal_control_event();
            }
            // The authoritative signal, and the only one on hardware that
            // still has S3: the machine is about to lose power to everything
            // but the controller. Modern Standby never sends it, which is what
            // the screen-off path is for, but hibernation does, on every
            // machine.
            PBT_APMSUSPEND => {
                SUSPEND_HANDLED.store(false, Ordering::SeqCst);
                SUSPENDING.store(true, Ordering::SeqCst);
                publish_power();
                signal_control_event();

                // The run loop is checking between its short sleeps, so this
                // is normally over in a fraction of a second. Bounded either
                // way, because holding up a suspend is not this handler's to
                // do.
                let deadline = Instant::now() + SUSPEND_ACK_WAIT;

                while !SUSPEND_HANDLED.load(Ordering::SeqCst) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            // Resuming says the machine is running again, and nothing about
            // the screen. A wake for standby maintenance leaves it off, so the
            // display state stands until the display itself says otherwise.
            PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => {
                SUSPENDING.store(false, Ordering::SeqCst);
                // A resume ends a standby session whether or not its own end
                // was logged. Event 507 is the ordinary way out, but it is one
                // rendered string away from being missed, and a missed one
                // would leave the engine in firmware mode at the standby poll
                // rate for the rest of the session with nothing saying why.
                IN_MODERN_STANDBY.store(false, Ordering::SeqCst);
                publish_power();
                signal_control_event();
            }
            _ => {}
        },
        SERVICE_CONTROL_INTERROGATE => {}
        _ => return ERROR_CALL_NOT_IMPLEMENTED,
    }

    NO_ERROR
}

/// Kept alive for the life of the service. Dropping it ends the subscription.
static STANDBY_SUBSCRIPTION: OnceLock<isize> = OnceLock::new();

/// Kernel-Power logs 506 when a Modern Standby session begins and 507 when it
/// ends. Nothing broadcasts either, but the log can be subscribed to.
const STANDBY_QUERY: &str =
    "*[System[Provider[@Name='Microsoft-Windows-Kernel-Power'] and (EventID=506 or EventID=507)]]";

/// Asks to be told when a Modern Standby session opens or closes.
///
/// Failure is survivable and silent. The clocks the engine reads still notice
/// a sleep, just after it has happened rather than as it begins, which is the
/// behavior this replaces rather than the behavior it depends on.
unsafe fn subscribe_to_standby_events() {
    let channel = wide("System");
    let query = wide(STANDBY_QUERY);

    let handle = EvtSubscribe(
        0,
        ptr::null_mut(),
        channel.as_ptr(),
        query.as_ptr(),
        0,
        ptr::null(),
        Some(on_standby_event),
        EvtSubscribeToFutureEvents,
    );

    if handle != 0 {
        let _ = STANDBY_SUBSCRIPTION.set(handle);
    }
}

/// Runs on a thread the event log owns, so it does what the service control
/// handler does: sets an atomic, wakes the run loop, and returns.
unsafe extern "system" fn on_standby_event(
    action: EVT_SUBSCRIBE_NOTIFY_ACTION,
    _context: *const c_void,
    event: EVT_HANDLE,
) -> u32 {
    if action != EvtSubscribeActionDeliver {
        return 0;
    }

    // Sized by asking, which is what the first call is for.
    let mut used = 0u32;
    let mut properties = 0u32;
    EvtRender(0, event, EvtRenderEventXml, 0, ptr::null_mut(), &mut used, &mut properties);

    if used == 0 {
        return 0;
    }

    let mut buffer = vec![0u16; used as usize / 2 + 1];
    let rendered = EvtRender(
        0,
        event,
        EvtRenderEventXml,
        used,
        buffer.as_mut_ptr() as *mut c_void,
        &mut used,
        &mut properties,
    );

    if rendered == 0 {
        return 0;
    }

    // A string search rather than an XML parse, for one number in a document
    // whose shape is fixed. The reference does the same.
    let xml = String::from_utf16_lossy(&buffer);

    if xml.contains("<EventID>506</EventID>") {
        IN_MODERN_STANDBY.store(true, Ordering::SeqCst);
    } else if xml.contains("<EventID>507</EventID>") {
        IN_MODERN_STANDBY.store(false, Ordering::SeqCst);
    } else {
        return 0;
    }

    publish_power();
    signal_control_event();
    0
}

/// Works the run loop's power state out from the two things we were told.
///
/// Suspending outranks everything, because it is the one that came with a
/// guarantee. Below it the screen decides, and what a dark screen means is the
/// engine's question, not this one's.
fn publish_power() {
    let power = if SUSPENDING.load(Ordering::SeqCst) || IN_MODERN_STANDBY.load(Ordering::SeqCst) {
        // Both came with the system's word for it, so neither is worked out
        // again from a measurement, and neither is left until the system says
        // the other thing.
        POWER_SUSPENDED
    } else if !LID_OPEN.load(Ordering::SeqCst) && ON_BATTERY.load(Ordering::SeqCst) {
        // Shut and unplugged, whatever the display claims.
        //
        // This is the case the display state gets wrong. Undock a laptop whose
        // lid has been shut the whole time and Windows lights the internal
        // panel behind it: console display on, no standby session, nothing
        // broadcast, and a machine that by every available measurement is
        // sitting there working. It is, but nobody is looking at it.
        //
        // Reported as a dark screen, because that is what it is, and treated
        // like one: the fan goes back to the firmware while nothing is known,
        // and comes back to the curve if the machine turns out to still be
        // running. That matters here. A closed laptop on battery stays awake
        // until Windows' own idle timers expire, which can be half an hour,
        // and the firmware's own curve is louder than the one the user chose.
        // Holding it there would be trading real noise for no safety: while
        // the machine is awake the controller answers, so the curve is
        // managing the fan. What is not safe is a level held once it stops
        // answering, and the clocks catch that within a pass.
        //
        // Not the power cord on its own, which was refused for standby and
        // still is: unplugging at a desk cannot see whether the machine is
        // about to be picked up. Paired with a closed lid it stops being a
        // prediction. Both are watched, and either changing puts the curve
        // straight back.
        POWER_SCREEN_OFF
    } else if DISPLAY_ON.load(Ordering::SeqCst) {
        POWER_AWAKE
    } else {
        POWER_SCREEN_OFF
    };

    POWER.store(power, Ordering::SeqCst);
}

/// windows-sys does not derive PartialEq on GUID, and we only ever compare
/// against constants, so a field-wise check is all that is needed.
pub(crate) fn same_guid(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;
const NO_ERROR: u32 = 0;

fn report(state: u32, wait_hint: u32) {
    let Some(handle) = STATUS_HANDLE.get() else { return };

    // A pending state with a checkpoint that never moves reads as a hung
    // service, and the manager stops waiting. It has to advance every time we
    // say we are still working.
    let checkpoint = if state == SERVICE_RUNNING || state == SERVICE_STOPPED {
        CHECKPOINT.store(0, Ordering::SeqCst);
        0
    } else {
        CHECKPOINT.fetch_add(1, Ordering::SeqCst) + 1
    };

    let mut status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            // PRESHUTDOWN matters: on a plain SHUTDOWN the wait hint is
            // ignored and the kill timeout defaults to five seconds, which a
            // handback with retries can exceed. Preshutdown gets minutes.
            SERVICE_ACCEPT_STOP
                | SERVICE_ACCEPT_SHUTDOWN
                | SERVICE_ACCEPT_PRESHUTDOWN
                | SERVICE_ACCEPT_POWEREVENT
        } else {
            0
        },
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: checkpoint,
        dwWaitHint: wait_hint,
    };

    unsafe { SetServiceStatus(*handle as SERVICE_STATUS_HANDLE, &mut status) };
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

pub fn install() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // Quoted, because an unquoted path with a space in it is read as a command
    // followed by arguments.
    let command = format!("\"{}\" --service", exe.display());

    unsafe {
        let manager = OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_ALL_ACCESS);
        if manager.is_null() {
            return Err("could not open the service manager; try running as administrator".into());
        }

        let name = wide(SERVICE_NAME);
        let display = wide(DISPLAY_NAME);
        let path = wide(&command);

        let service = CreateServiceW(
            manager,
            name.as_ptr(),
            display.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            path.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
        );

        // An upgrade finds the service already registered, and creating it
        // again correctly fails. Treating that as an error left a freshly
        // installed program with a stopped service and nothing managing the
        // fan. Adopt the existing registration instead, repointed at wherever
        // we were just run from in case the program moved, and start it.
        let service = if service.is_null() && GetLastError() == ERROR_SERVICE_EXISTS {
            let existing = OpenServiceW(manager, name.as_ptr(), SERVICE_ALL_ACCESS);

            if !existing.is_null() {
                ChangeServiceConfigW(
                    existing,
                    SERVICE_NO_CHANGE,
                    SERVICE_AUTO_START,
                    SERVICE_NO_CHANGE,
                    path.as_ptr(),
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    display.as_ptr(),
                );
            }

            existing
        } else {
            service
        };

        if service.is_null() {
            let code = GetLastError();
            CloseServiceHandle(manager);
            return Err(format!("could not create the service (error {code})"));
        }

        set_restart_on_failure(service);
        allow_users_to_start_and_stop(service);

        // Start it now so the fan is controlled without waiting for a reboot.
        StartServiceW(service, 0, ptr::null());

        CloseServiceHandle(service);
        CloseServiceHandle(manager);
    }

    Ok(())
}

/// Lets an ordinary logged-in user start and stop this one service.
///
/// Without it the tray had to relaunch itself elevated just to quit: a consent
/// prompt each time, and a whole administrator token borrowed for one small
/// operation. Query, start and stop, on this service alone, to people already
/// logged in at the machine, is the smaller grant.
///
/// Little is given away. Stopping hands the fan back to the firmware, which is
/// the safe state and the one the machine runs in when Yamato is not installed
/// at all. Starting is what Windows does by itself at every boot. Neither
/// reaches the controller; only the service does.
///
/// Installing and removing still need administrator rights, because those
/// create and delete a service rather than operate one.
unsafe fn allow_users_to_start_and_stop(service: SC_HANDLE) {
    // System and administrators keep full control; one entry is added for
    // interactive users (IU), who get query, start (RP) and stop (WP) only.
    // Change-config (DC), delete (SD) and write-DAC (WD) are withheld on
    // purpose, so a user cannot repoint the service at another binary or widen
    // their own access. SDDL because hand-assembling an access control list is
    // where a mistake becomes a security hole rather than a wrong pixel.
    let sddl: Vec<u16> = "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)\
                          (A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)\
                          (A;;CCLCSWLOCRRPWP;;;IU)"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let mut descriptor = ptr::null_mut();

    if ConvertStringSecurityDescriptorToSecurityDescriptorW(
        sddl.as_ptr(),
        SDDL_REVISION_1,
        &mut descriptor,
        ptr::null_mut(),
    ) == 0
    {
        return;
    }

    // A failure here costs the convenience, not the installation: the tray
    // falls back to asking for consent, which is where it started.
    SetServiceObjectSecurity(service, DACL_SECURITY_INFORMATION, descriptor);

    windows_sys::Win32::Foundation::LocalFree(descriptor as _);
}

/// Asks the service manager to restart us if we ever die unexpectedly.
///
/// An engine that stops leaves whatever level it last set, with the firmware
/// still out of the loop.
unsafe fn set_restart_on_failure(service: SC_HANDLE) {
    let mut actions = [
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 5_000 },
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 10_000 },
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 30_000 },
    ];

    let mut failure = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 86_400,
        lpRebootMsg: ptr::null_mut(),
        lpCommand: ptr::null_mut(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };

    ChangeServiceConfig2W(
        service,
        SERVICE_CONFIG_FAILURE_ACTIONS,
        &mut failure as *mut _ as *mut c_void,
    );

    // The actions above are queued for a service that reports SERVICE_STOPPED
    // with a non-zero exit code only while this flag is on. It is off by
    // default, and off it means the actions fire only when the process dies
    // without reporting a stop at all. This service always reports one: the
    // start that cannot reach the controller ends in report_failure, which sets
    // SERVICE_STOPPED with error 1066 precisely so the manager would treat it
    // as a failure. Without the flag the manager recorded the failure and did
    // nothing about it, so the three restarts sat in the service's recovery
    // page looking configured while no failure this program can actually have
    // was ever eligible for them.
    //
    // What that cost is a cold boot where the port driver is not openable yet.
    // The engine fails to start, the service stops, nothing is queued, and the
    // machine runs with the fan on the firmware's own curve until somebody
    // notices, when the retry five seconds later would have worked. Same for a
    // fast restart where the previous process still holds the engine lock.
    //
    // report_failure is left as it was. It was already right; it was just not
    // enough on its own.
    let mut flag = SERVICE_FAILURE_ACTIONS_FLAG { fFailureActionsOnNonCrashFailures: 1 };

    ChangeServiceConfig2W(
        service,
        SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
        &mut flag as *mut _ as *mut c_void,
    );
}

/// Starts an installed service. Separate from install, which creates one.
pub fn start() -> Result<(), String> {
    unsafe {
        let manager = OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT);
        if manager.is_null() {
            return Err("could not open the service manager".into());
        }

        let name = wide(SERVICE_NAME);
        let service = OpenServiceW(manager, name.as_ptr(), SERVICE_START);
        let ok = !service.is_null() && StartServiceW(service, 0, ptr::null()) != 0;

        if !service.is_null() {
            CloseServiceHandle(service);
        }
        CloseServiceHandle(manager);

        if ok { Ok(()) } else { Err("could not start the service".into()) }
    }
}

/// Stops a running service without removing it.
///
/// Stopping is how the fan gets handed back, so this reports honestly.
/// Returning Ok on a timeout or a failed status query would tell the caller
/// the fan was released when it might still be held.
pub fn stop() -> Result<(), String> {
    unsafe {
        let manager = OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT);
        if manager.is_null() {
            return Err("could not open the service manager".into());
        }

        let name = wide(SERVICE_NAME);
        let service = OpenServiceW(manager, name.as_ptr(), SERVICE_STOP | SERVICE_QUERY_STATUS);
        if service.is_null() {
            let code = GetLastError();
            CloseServiceHandle(manager);

            return if code == ERROR_SERVICE_DOES_NOT_EXIST {
                Ok(())
            } else {
                Err(format!("could not open the service (error {code})"))
            };
        }

        let mut status: SERVICE_STATUS = std::mem::zeroed();
        let requested = ControlService(service, SERVICE_CONTROL_STOP, &mut status) != 0;
        let control_error = if requested { 0 } else { GetLastError() };

        // Already stopped is the outcome we wanted, not a failure.
        let already_stopped = control_error == ERROR_SERVICE_NOT_ACTIVE;

        if !requested && !already_stopped {
            CloseServiceHandle(service);
            CloseServiceHandle(manager);

            return Err(format!("could not stop the service (error {control_error})"));
        }

        // Long enough to outlast a handback that is waiting on the shared EC
        // lock, which is the slow case worth waiting for.
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut stopped = already_stopped;

        while !stopped && Instant::now() < deadline {
            if QueryServiceStatus(service, &mut status) == 0 {
                // Cannot see it any more. Say so rather than assuming the best.
                CloseServiceHandle(service);
                CloseServiceHandle(manager);

                return Err("lost track of the service while it was stopping".into());
            }

            if status.dwCurrentState == SERVICE_STOPPED {
                stopped = true;
                break;
            }

            std::thread::sleep(Duration::from_millis(250));
        }

        CloseServiceHandle(service);
        CloseServiceHandle(manager);

        if stopped {
            Ok(())
        } else {
            Err("the service did not stop in time; the fan may still be held".into())
        }
    }
}

pub fn uninstall() -> Result<(), String> {
    unsafe {
        let manager = OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_ALL_ACCESS);
        if manager.is_null() {
            return Err("could not open the service manager; try running as administrator".into());
        }

        let name = wide(SERVICE_NAME);
        let service = OpenServiceW(manager, name.as_ptr(), SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE);

        if service.is_null() {
            let code = GetLastError();
            CloseServiceHandle(manager);

            return if code == ERROR_SERVICE_DOES_NOT_EXIST {
                Ok(())
            } else {
                Err(format!("could not open the service (error {code})"))
            };
        }

        // Stop before deleting. DeleteService on a running service only marks
        // it for deletion, so it keeps running, and keeps driving the fan,
        // while appearing to be gone.
        let mut status: SERVICE_STATUS = std::mem::zeroed();
        ControlService(service, SERVICE_CONTROL_STOP, &mut status);

        // Long enough to cover a stop that spends its whole handback budget.
        // Anything shorter could give up while the service was still working,
        // then delete it anyway.
        let deadline = Instant::now() + HANDBACK_BUDGET + Duration::from_secs(30);
        let mut stopped = false;

        while Instant::now() < deadline {
            if QueryServiceStatus(service, &mut status) == 0 {
                break;
            }
            if status.dwCurrentState == SERVICE_STOPPED {
                stopped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }

        // Refuse rather than delete a service that is still running. A pending
        // delete leaves it driving the fan while every UI reports it gone,
        // which is worse than an uninstall that says it could not finish.
        if !stopped {
            CloseServiceHandle(service);
            CloseServiceHandle(manager);
            return Err(
                "the service did not stop, so it was not removed. It may still be controlling the fan. Try again in a moment."
                    .to_string(),
            );
        }

        let ok = DeleteService(service) != 0;
        let code = if ok { 0 } else { GetLastError() };

        CloseServiceHandle(service);
        CloseServiceHandle(manager);

        if ok {
            Ok(())
        } else {
            Err(format!("could not remove the service (error {code})"))
        }
    }
}

/// -1 when not installed, otherwise the current SERVICE_ state.
pub fn state() -> i32 {
    unsafe {
        let manager = OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT);
        if manager.is_null() {
            return -1;
        }

        let name = wide(SERVICE_NAME);
        let service = OpenServiceW(manager, name.as_ptr(), SERVICE_QUERY_STATUS);
        if service.is_null() {
            CloseServiceHandle(manager);
            return -1;
        }

        let mut status: SERVICE_STATUS = std::mem::zeroed();
        let got = QueryServiceStatus(service, &mut status) != 0;

        CloseServiceHandle(service);
        CloseServiceHandle(manager);

        if got {
            status.dwCurrentState as i32
        } else {
            -1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_command_line_is_quoted() {
        // An unquoted path containing a space is parsed as a command plus
        // arguments, which is a classic way to break on "Program Files".
        let command = format!("\"{}\" --service", r"C:\Program Files\Yamato\yamato.exe");
        assert!(command.starts_with('"'));
        assert!(command.contains("\" --service"));
    }

    #[test]
    fn querying_a_service_that_is_not_installed_is_not_an_error() {
        // -1 rather than a panic, so callers can just ask.
        let s = state();
        assert!(s == -1 || s > 0);
    }

    #[test]
    fn stop_flag_is_shared_between_handler_and_loop() {
        let a = stop_flag();
        let b = stop_flag();
        assert!(!a.stopped());

        a.stop();
        assert!(b.stopped(), "the handler and the run loop must see one flag");
    }
}
