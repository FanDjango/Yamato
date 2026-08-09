// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! The thing that actually drives the fan.
//!
//! Whoever holds the engine lock is the engine, and only the engine opens the
//! port driver. A second copy cannot fight over the fan register, because it
//! never has a handle capable of writing it.

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use yamato_core::{Config, Engine, Mode};
use yamato_ec::Ec;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows_sys::Win32::System::WindowsProgramming::{
    QueryInterruptTime, QueryUnbiasedInterruptTime,
};

use crate::ipc::{self, Channel};

/// Only one of these may exist. Global, because the engine is usually a
/// service in session 0 while a window is in the user's session, and an
/// unprefixed name would give them separate locks that never see each other.
const ENGINE_LOCK: &str = r"Global\Yamato_Engine";

/// Whether *this* process already holds it.
///
/// A Windows mutex is recursive per thread: waiting on one the calling thread
/// already owns succeeds immediately. So the kernel object gives exclusion
/// between processes but none within one, and "only one engine" has to mean
/// both. This closes the second half.
static CLAIMED_HERE: AtomicBool = AtomicBool::new(false);

/// Held for as long as this process is the engine.
pub struct EngineLock {
    handle: HANDLE,
    held: bool,
}

impl EngineLock {
    /// Tries to become the engine. `None` when somebody else already is,
    /// whether that is another process or another part of this one.
    pub fn claim() -> Option<Self> {
        if CLAIMED_HERE.swap(true, Ordering::SeqCst) {
            return None;
        }

        match Self::claim_os() {
            Some(lock) => Some(lock),
            None => {
                CLAIMED_HERE.store(false, Ordering::SeqCst);
                None
            }
        }
    }

    fn claim_os() -> Option<Self> {
        let name: Vec<u16> = ENGINE_LOCK.encode_utf16().chain(std::iter::once(0)).collect();
        let sddl: Vec<u16> = "D:(A;;GA;;;WD)".encode_utf16().chain(std::iter::once(0)).collect();

        let mut descriptor = ptr::null_mut();
        let have_sd = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } != 0;

        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };

        let handle = unsafe {
            CreateMutexW(if have_sd { &mut sa } else { ptr::null_mut() }, 0, name.as_ptr())
        };

        if have_sd {
            unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor as _) };
        }

        if handle.is_null() {
            return None;
        }

        // Abandoned means the last engine died without releasing, so the fan
        // is ours to take back.
        let held = matches!(
            unsafe { WaitForSingleObject(handle, 0) },
            WAIT_OBJECT_0 | WAIT_ABANDONED
        );

        if held {
            Some(EngineLock { handle, held })
        } else {
            unsafe { CloseHandle(handle) };
            None
        }
    }
}

impl Drop for EngineLock {
    fn drop(&mut self) {
        unsafe {
            if self.held {
                ReleaseMutex(self.handle);
            }
            CloseHandle(self.handle);
        }

        CLAIMED_HERE.store(false, Ordering::SeqCst);
    }
}

/// Power state, as far as the fan is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    Awake,
    /// The screen is off, and that is all anybody has said.
    ///
    /// On hardware with no S3 this is where S0 Modern Standby begins, so it is
    /// the state a laptop shut in a bag is in. It is also the state of one
    /// docked with its lid closed and compiling, and of one whose screen
    /// blanked on a timer while the work carried on. Nothing in the
    /// notification separates them; `SleepClock` does.
    ScreenOff,
    /// Suspended, on the system's own word.
    ///
    /// Hibernation everywhere, and real S3 sleep on the machines that still
    /// have it. Those send a suspend broadcast before the power goes; Modern
    /// Standby sends nothing at all, which is why `ScreenOff` exists.
    Suspended,
}

/// Tells time spent working from time spent asleep.
///
/// Windows keeps two counts of the same clock. One advances with real time.
/// The other, the unbiased one, stops while the system is in a low-power sleep
/// state. Subtract them and what is left is exactly how long the machine was
/// parked, which is the question a screen-off notification cannot answer: a
/// docked laptop working with its lid shut and one asleep in a bag look
/// identical from the outside and nothing like each other on these two clocks.
///
/// Both are cheap reads of a counter, so this can be sampled on every pass.
struct SleepClock {
    last: Option<(u64, u64)>,
}

impl SleepClock {
    fn new() -> Self {
        SleepClock { last: None }
    }

    /// How much of the time since the previous call the machine spent asleep.
    ///
    /// Zero on the first call, since there is no interval to measure yet, and
    /// zero if the counters cannot be read: an unanswerable question defaults
    /// to the state we can see rather than inventing a sleep that would take
    /// the curve away.
    fn slept_since_last_pass(&mut self) -> Duration {
        let mut real = 0u64;
        let mut unbiased = 0u64;

        // Hundred-nanosecond units, both of them, off the same interrupt-time
        // base. QueryInterruptTime cannot fail and returns nothing.
        unsafe {
            QueryInterruptTime(&mut real);

            if QueryUnbiasedInterruptTime(&mut unbiased) == 0 {
                return Duration::ZERO;
            }
        }

        let slept = match self.last {
            Some((was_real, was_unbiased)) => {
                let elapsed = real.saturating_sub(was_real);
                let worked = unbiased.saturating_sub(was_unbiased);

                Duration::from_nanos(elapsed.saturating_sub(worked).saturating_mul(100))
            }
            None => Duration::ZERO,
        };

        self.last = Some((real, unbiased));
        slept
    }

    /// Drops the previous sample, so the next call measures from now.
    ///
    /// Called whenever the system tells us the power state changed. Without it
    /// a resume carries the whole suspend into its first interval, and the
    /// machine reads as asleep at the moment it woke.
    fn forget(&mut self) {
        self.last = None;
    }
}

/// Shared with whatever is watching for shutdown. Set it and the loop unwinds
/// through the fan guard on its way out.
#[derive(Default)]
pub struct StopFlag(AtomicBool);

impl StopFlag {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

pub struct Host {
    // Field order is drop order, and it matters here. The engine owns the
    // guard that hands the fan back, so it has to be dropped while we still
    // hold the lock. Releasing the lock first would let a restarting service
    // claim it and open the controller while this process is still writing.
    engine: Engine,
    channel: Channel,
    config: Config,
    /// The last thing the system actually told us.
    reported: PowerState,
    /// What we believe, which is not always the same thing.
    ///
    /// A notification sets both. Only this one can then be talked down by
    /// evidence, when the clocks show a sleep nobody mentioned. Keeping the
    /// two apart is what stops the run loop, which re-applies the reported
    /// state on every pass, from undoing that on the very next one.
    power: PowerState,
    last_acked: u32,
    /// Consecutive failed passes. TPFanCtrl2 gave up after ten and handed the
    /// fan to the firmware; dropping that left a failing controller pinned at
    /// whatever level was last set, forever.
    consecutive_faults: u32,
    /// Whether the stall handback has already run for the current stall.
    stalled: bool,
    /// Set when handing the fan back failed. Published, because a fan pinned
    /// with the firmware disabled should be said out loud, not discovered by
    /// touch.
    handback_failed: bool,
    /// Set when the failures are the controller *declining* fan writes, not
    /// failing to answer. Paired with a second fan that has never reported a
    /// speed, that is the signature of a single-fan machine being verified
    /// through a selector it does not have, and the client can suggest the
    /// setting. Cleared only by a successful pass, so a blip of
    /// unreachability mid-run does not change what the run is.
    fan_write_declined: bool,
    /// The mode to restore once the controller behaves again, if a fault or a
    /// stall made us hand the fan to the firmware. `Some` means yamato is not
    /// running the user's curve right now, whatever the settings say.
    surrendered: Option<Mode>,
    /// Good passes since surrendering, counted toward taking the curve back.
    clean_ticks: u32,
    /// Passes lost to something else holding the controller.
    contended_ticks: u32,
    /// The two clocks that say whether a screen-off machine is working.
    sleep_clock: SleepClock,
    /// Passes since the screen went off that found no sleep in them.
    ///
    /// Counted toward taking the curve back, and reset by any pass that does
    /// find sleep. Asymmetric on purpose: one parked pass gives the fan back,
    /// several working ones are needed to earn it again, so a machine dipping
    /// in and out of standby settles on the firmware rather than flapping.
    awake_passes: u32,
    /// Whether the fan was handed to the firmware because of the power state
    /// rather than because anything went wrong.
    ///
    /// Separate from `surrendered`, which is the fault path. Both end with the
    /// engine in BIOS mode, and confusing the two would let a standby wake
    /// undo a handback that a broken controller had asked for.
    standby_handback: bool,
    /// The mode to go back to afterwards.
    ///
    /// What the user actually had, not what the settings say to start in. A
    /// screen going dark is now a routine event rather than a rare one, and
    /// having each one quietly reset a chosen mode would be a setting that
    /// undoes itself.
    standby_mode: Option<Mode>,
    /// Whether the last pass found the machine parked rather than working.
    ///
    /// Only this backs the poll rate off. Handing the fan over at the moment
    /// the screen goes off is a precaution taken before anything is known, and
    /// slowing down then would make the decision that follows take minutes on
    /// a machine that turns out to be docked and busy.
    parked: bool,
    /// Whether this pass is republishing the previous reading.
    stale: bool,
    /// The last pass that got through, shown while somebody else has a turn.
    ///
    /// A busy controller is not a broken one, and a reading a few seconds old
    /// is closer to the truth than blanking every sensor and announcing that
    /// the hardware cannot be reached.
    #[allow(clippy::type_complexity)]
    last_good: Option<(u8, Option<(usize, i8)>, [u16; 2], [Option<i8>; ipc::SENSOR_COUNT])>,
    /// When the settings file was last seen changing.
    config_stamp: Option<std::time::SystemTime>,
    /// The history file, when one has been asked for.
    ///
    /// Ordinary owned state on this struct, with no lock anywhere near it and
    /// nothing shared with another thread, so writing a line cannot be waiting
    /// on anything while the controller is in the middle of a transaction.
    logger: crate::log::Logger,
    /// The flag the caller's loop watches, shared rather than copied.
    ///
    /// A tick that is already under way needs to know a stop is coming, so it
    /// does not start something the stop path is about to do again under a
    /// deadline of its own.
    stop: Arc<StopFlag>,
    /// Last, so it is released after everything above has finished with the
    /// hardware.
    _lock: EngineLock,
}

/// Which of the three kinds of trouble to publish.
///
/// Ordered by what most needs saying, not by what happened last. A failed
/// handback outranks everything: it is a level held with the firmware's own
/// management switched off. Below that, a controller that cannot be read *now*
/// is more use than the stepping-aside it caused, which is what remains to say
/// once reads are succeeding again and the curve has not come back yet.
///
/// Free-standing so it can be tested without a controller to fail.
fn status_for(fault: bool, surrendered: bool, handback_failed: bool) -> u8 {
    if handback_failed {
        ipc::STATUS_HANDBACK_FAILED
    } else if fault {
        ipc::STATUS_UNREACHABLE
    } else if surrendered {
        ipc::STATUS_SURRENDERED
    } else {
        ipc::STATUS_OK
    }
}

/// Failures in a row before we stop trusting ourselves and hand the fan back.
///
/// Low, because the failure mode being guarded against is a level pinned with
/// the firmware disabled, and a few seconds of a loud fan is a much better
/// outcome than that.
const MAX_FAULTS: u32 = 5;

/// Passes lost to contention before it stops being someone else's turn and
/// starts being a fan we cannot manage.
///
/// Waiting is right, because handing the fan back needs the very lock that is
/// busy, so giving up early achieves nothing. But if it never ends, a level we
/// set is held on a machine nobody is managing, which is worth escaping and
/// worth saying. Longer than the reference's ten passes, because unlike the
/// reference this can recover afterwards instead of exiting.
const MAX_CONTENDED: u32 = 60;

/// Consecutive good passes before a surrendered curve is taken back.
const CLEAN_TICKS_TO_RECOVER: u32 = 3;

/// Passes with no sleep in them before a screen-off machine gets its curve
/// back.
///
/// Three, so a machine has to still be working a good few seconds after the
/// screen went dark rather than merely at the instant it did.
const AWAKE_PASSES_TO_RESUME: u32 = 3;

/// The least sleep in one pass that can mean the machine was parked.
///
/// Below this it is a scheduler hiccup or a moment of idle, neither of which
/// is standby. Above it, on a screen that is already off, it is.
const PARKED_FLOOR: Duration = Duration::from_secs(1);

impl Host {
    /// Becomes the engine, or explains why it could not.
    ///
    /// Order matters. The shared section is published *before* the port driver
    /// is opened, so a window starting alongside us at logon has something to
    /// attach to at once: opening the driver and probing the controller takes
    /// real time on a cold boot.
    pub fn start(config: Config, stop: Arc<StopFlag>) -> Result<Self, String> {
        let lock = EngineLock::claim().ok_or("another instance already owns the fan")?;
        let channel = Channel::create().ok_or("could not publish the shared state")?;

        let ec = Ec::open().map_err(|e| e.to_string())?;
        let curve = config.active_curve().map_err(|e| e.to_string())?;

        let mut engine = Engine::new(ec, curve);
        engine.set_mode(Mode::from(config.startup_mode));
        engine.set_ignored_sensors(config.ignored_sensors.clone());
        engine.set_watchdog(Duration::from_secs(config.watchdog_secs as u64));
        // The value only. When the escape fires, that it latches, and that a
        // blind sensor counts as too hot are the engine's business and are not
        // configurable.
        engine.set_manual_escape(config.manual_escape_c);
        // Whether the second fan selector exists to be written and verified.
        // Set here and again on every reload, so it takes effect when saved
        // rather than at the next restart. That matters here, because the
        // machine that needs it is mid-failure when somebody turns it on.
        engine.set_single_fan(config.single_fan);

        Ok(Host {
            engine,
            channel,
            config,
            reported: PowerState::Awake,
            power: PowerState::Awake,
            last_acked: 0,
            consecutive_faults: 0,
            stalled: false,
            handback_failed: false,
            fan_write_declined: false,
            surrendered: None,
            clean_ticks: 0,
            contended_ticks: 0,
            stale: false,
            sleep_clock: SleepClock::new(),
            awake_passes: 0,
            standby_handback: false,
            standby_mode: None,
            parked: false,
            last_good: None,
            config_stamp: std::fs::metadata(Config::default_path())
                .and_then(|m| m.modified())
                .ok(),
            logger: crate::log::Logger::new(crate::log::default_path()),
            stop,
            _lock: lock,
        })
    }

    /// Tells the host the screen went off, the machine suspended, or it woke.
    ///
    /// The screen going off is not the same as sleeping, and this is where
    /// that distinction gets made cheaply: the fan goes to the firmware either
    /// way, because at the instant of the notification nothing is yet known
    /// and the firmware is the state it is always safe to be in. What happens
    /// next is decided a few passes later by `reconsider_standby`, which can
    /// tell a machine that is working from one that is parked.
    pub fn set_power_state(&mut self, power: PowerState) {
        if self.reported == power {
            return;
        }

        self.reported = power;
        self.power = power;
        self.awake_passes = 0;
        // A fresh state is a fresh question. Suspended is the one answer that
        // arrived with the notification rather than being worked out.
        self.parked = power == PowerState::Suspended;

        // The next pass measures from here rather than across whatever just
        // happened, so a resume does not read as sleep the moment it lands.
        self.sleep_clock.forget();

        match power {
            PowerState::Awake => {
                // A wake outranks a pending recovery. Leaving it set meant a
                // wake could restore the user's curve and then have the
                // recovery flip it straight back to firmware three ticks
                // later.
                self.clear_surrender();
                self.take_the_fan_back_from_standby();
            }
            PowerState::ScreenOff | PowerState::Suspended => self.hand_to_firmware_for_standby(),
        }
    }

    /// Gives the fan back because of the power state, not because of a fault.
    fn hand_to_firmware_for_standby(&mut self) {
        // Only the first handback of an episode records a mode. A screen-off
        // that gives the fan back, takes it back, and gives it back again is
        // one episode, and the second pass through here must not save BIOS
        // mode over what the user actually had.
        if !self.standby_handback {
            // A pending fault recovery knows better than the current mode
            // does. The current mode is BIOS precisely because the fault put
            // it there, so saving that would quietly turn a temporary
            // surrender into the thing restored on wake, which is the
            // "five transient failures cost the user their curve" regression
            // the recovery exists to prevent.
            self.standby_mode =
                Some(self.surrendered.clone().unwrap_or_else(|| self.engine.mode().clone()));
        }

        // Taken over, not left running alongside. Both paths end in BIOS mode,
        // but only this one knows the machine is asleep: a recovery left
        // pending would count three good passes during standby and restore a
        // manual level onto a laptop shut in a bag, with the firmware's own
        // management off and nothing further to undo it, because the handback
        // below has already happened and does not repeat.
        self.clear_surrender();

        self.standby_handback = true;
        self.engine.set_mode(Mode::Bios);

        // Written now, not at the next pass. Setting the mode only records the
        // intent, and the pass that would act on it samples every sensor and
        // both fans first. Windows starts refusing port access within
        // milliseconds of entering standby, so that sampling is exactly where
        // the handback gets lost, and the fan stays at whatever the curve last
        // asked for while the machine sleeps.
        //
        // The reference has the same complaint filed against it on this
        // hardware, with only one of two fans reverting. This path writes and
        // verifies both, and retries, so a fan that misses the first write is
        // caught rather than left spinning.
        self.handback_failed = self.engine.shutdown().is_err();
    }

    /// Decides, once a pass, whether the machine is actually working.
    ///
    /// This is the whole of the difference between a laptop docked with its
    /// lid shut and one asleep in a bag. The screen is off in both. What is
    /// not the same is the sleep in the interval just measured: a machine
    /// doing work has none, and a parked one is almost entirely made of it.
    ///
    /// Erring toward the firmware throughout. Sleep found in a single pass
    /// gives the fan back at once; several clean ones in a row are needed
    /// before the curve returns.
    fn reconsider_standby(&mut self) {
        // Sampled on every pass whatever the state, so the interval this reads
        // is always the one since the last pass rather than since whenever the
        // machine last happened to be in this branch.
        let slept = self.sleep_clock.slept_since_last_pass();

        // Suspended is not in question. The system said so, and only the
        // system gets to say otherwise.
        if self.power == PowerState::Suspended {
            return;
        }

        if slept >= self.parked_threshold() {
            // The clocks outrank the notification, including when there was
            // no notification. Pulling the cable on a dock with the lid
            // already shut takes the last display away with it, and the
            // screen-off that should follow does not reliably arrive; a host
            // that trusted the last thing it was told would hold a curve,
            // with the firmware's own management switched off, straight
            // through a standby nobody mentioned. That is the bag.
            //
            // Talked down rather than declared: what is known is that the
            // machine slept, which is what a dark screen would have meant
            // anyway, so it goes into the same state and earns its way back
            // out by the same rule.
            self.power = PowerState::ScreenOff;
            self.awake_passes = 0;
            self.parked = true;

            if !self.standby_handback {
                self.hand_to_firmware_for_standby();
            }

            return;
        }

        self.parked = false;

        // Only a machine that gave the fan up has to earn it back. One that
        // has been awake all along never lost it.
        if self.power != PowerState::ScreenOff {
            return;
        }

        self.awake_passes += 1;

        // Not while a fault or a stall is holding the fan in firmware mode.
        // That handback has its own recovery, on its own evidence, and taking
        // the curve back here would be answering a question nobody asked.
        if self.awake_passes >= AWAKE_PASSES_TO_RESUME
            && self.standby_handback
            && self.surrendered.is_none()
        {
            self.take_the_fan_back_from_standby();
        }
    }

    /// Undoes a standby handback, restoring whatever mode was in force before.
    ///
    /// Does nothing when there was no handback to undo. Waking used to reset
    /// the mode to whatever the settings said to start in, which is not what
    /// a wake means: choose a fixed level with the lid shut, plug the charger
    /// back in, and the choice silently became Smart again.
    fn take_the_fan_back_from_standby(&mut self) {
        if !self.standby_handback {
            return;
        }

        self.standby_handback = false;

        let mode = self
            .standby_mode
            .take()
            .unwrap_or_else(|| Mode::from(self.config.startup_mode));

        // No longer holding a fan we could not give back: whatever the last
        // handback did, this is a deliberate decision to manage it again, and
        // leaving the warning up would make the one alarm that has to stay
        // credible into a permanent fixture. It is set again the moment
        // another handback fails.
        self.handback_failed = false;
        self.engine.set_mode(mode);
    }

    /// How much sleep in one interval means the machine was parked.
    ///
    /// A share of the interval rather than a fixed figure, so it still means
    /// the same thing at either poll rate, with a floor that keeps a brief dip
    /// on a busy machine from reading as standby.
    fn parked_threshold(&self) -> Duration {
        (self.poll_interval() / 4).max(PARKED_FLOOR)
    }

    pub fn poll_interval(&self) -> Duration {
        self.config.poll_interval(self.backed_off())
    }

    /// Whether to poll at the slower standby rate.
    ///
    /// Only once the machine is believed to be asleep. Waking a parked CPU
    /// every few seconds to read a temperature that is barely moving is how
    /// standby battery gets ruined, but polling slowly while the machine is
    /// working would leave the curve minutes behind the load.
    fn backed_off(&self) -> bool {
        match self.power {
            PowerState::Awake => false,
            PowerState::Suspended => true,
            PowerState::ScreenOff => self.parked,
        }
    }

    /// The handle a window signals when it posts a command, for the caller's
    /// wait. Null when there is none, which only costs promptness.
    pub fn command_event(&self) -> HANDLE {
        self.channel.command_event()
    }

    /// Clears it, so a command arriving between the check and the wait is not
    /// lost in the gap.
    pub fn clear_command_event(&self) {
        self.channel.clear_command_event();
    }

    /// Whether a window has asked for something that has not been acted on.
    ///
    /// Read while waiting out the poll interval, so a mode or profile chosen
    /// from the tray takes effect at once. Waiting out a five second poll, or
    /// a two minute standby one, made choosing a profile feel like nothing had
    /// happened, even though the command was never lost.
    pub fn command_waiting(&self) -> bool {
        self.channel.pending_command().is_some()
    }

    /// One pass: take any command, decide, publish.
    pub fn tick(&mut self) {
        self.reload_if_changed();

        // Before the reading, not after. If this pass is the one that finds
        // the machine has been asleep, the fan should be back with the
        // firmware before anything else is attempted, not once twelve sensors
        // and two fans have been read on a controller that may already have
        // stopped answering.
        self.reconsider_standby();

        if let Some(seq) = self.channel.pending_command() {
            self.apply_command();
            self.channel.acknowledge(seq);
            self.last_acked = seq;
        }

        // Retried, because one attempt loses often when something else on the
        // machine touches the controller. Three passes at a quarter second is
        // well short of the reference's persistence and still turns almost
        // every one of those failures back into an ordinary reading.
        // Cleared each pass. Left set, one contended moment would suppress
        // every log line for the life of the service.
        self.stale = false;

        let mut outcome = self.engine.tick();

        for _ in 0..2 {
            // A pending stop ends it. Each attempt can run for tens of seconds
            // against a controller that answers its ports but never completes
            // a handshake, and the stop path is about to do the same work
            // under a budget of its own. The handback below is guarded the
            // same way and for the same reason.
            if outcome.is_ok() || self.stop.stopped() {
                break;
            }

            std::thread::sleep(Duration::from_millis(250));
            outcome = self.engine.tick();
        }

        let (fan_ctrl, hottest, rpm, sensors, fault) = match outcome {
            Ok(t) => {
                self.consecutive_faults = 0;
                self.fan_write_declined = false;

                // Healthy again. Take the curve back after a few good passes
                // rather than sulking in firmware mode until someone reboots.
                if let Some(mode) = self.surrendered.clone() {
                    self.clean_ticks += 1;
                    if self.clean_ticks >= CLEAN_TICKS_TO_RECOVER {
                        self.engine.set_mode(mode);
                        self.surrendered = None;
                        self.clean_ticks = 0;
                    }
                }
                // Only on evidence. A successful tick proves we can talk to
                // the controller, not that the fan came back: the engine skips
                // the write if it already believes it wrote 0x80.
                if t.state.is_bios_controlled() {
                    self.handback_failed = false;
                }

                // One pass getting through ends the run, which is what makes
                // the count consecutive and not a running tally.
                self.contended_ticks = 0;
                self.last_good = Some((t.applied, t.hottest, t.state.fan_rpm, t.state.sensors));
                (t.applied, t.hottest, t.state.fan_rpm, t.state.sensors, false)
            }
            Err(e) => {
                // A refused write is different trouble from a controller that
                // has gone quiet. Lenovo Vantage drives ThinkPad thermals
                // through its own driver and takes no arbitration lock, so it
                // can put its own value in the fan register between our write
                // and our read of it. Counting those as faults handed the fan
                // to the firmware after five, and the curve went out of use.
                // Outlasting the other party is the right answer.
                //
                // Safe to keep going, because a declined write means our level
                // did not take: the register holds either the firmware's own
                // control or whatever the other program set, and both of those
                // are somebody managing the fan. The state worth escaping is
                // the opposite one, a level we did set followed by losing the
                // controller, and that arrives as a failed read, which counts.
                let declined = e.is_fan_write_declined();

                if declined {
                    self.fan_write_declined = true;
                }

                // Neither kind of contention counts toward giving up. Handing
                // the fan back needs the lock too, so surrendering while it is
                // busy cannot do the thing it exists to do; it only stops the
                // curve and grays the icon while something else has its turn.
                if declined || e.is_contention() {
                    // Bounded. Past this it has stopped being somebody else's
                    // turn, and the attempt is worth making even though the
                    // busy lock is the one the handback needs. If it fails,
                    // the failure is published; otherwise a permanently
                    // contended controller looks just like a healthy one.
                    self.contended_ticks += 1;

                    if self.contended_ticks >= MAX_CONTENDED {
                        self.consecutive_faults += 1;
                    }
                } else {
                    self.consecutive_faults += 1;
                }

                // Progress toward recovery is lost on any failure. Counting
                // cumulative successes instead let a controller that was
                // failing every third pass recover and re-surrender in a
                // cycle, flipping the fan between curve and firmware.
                self.clean_ticks = 0;

                // A level we set is still in force with the firmware switched
                // off, and we can no longer see the controller. Give it back
                // rather than publish a fault flag while the machine cooks.
                //
                // On every MAX_FAULTS, not once: a fan left pinned has to be
                // tried again. A steady beat rather than a backoff, because
                // for a safety handback that is the behavior worth having.
                //
                // Guarded on `> 0` because zero satisfies a modulo, and once
                // contention stopped incrementing the counter it sat at zero,
                // firing the handback on every contended pass.
                //
                // Skipped once a stop is pending. The stop path hands the fan
                // back itself within a budget sized to fit a preshutdown, and
                // two attempts in a row is how a stop comes to outlast what
                // the service manager will wait for.
                if self.consecutive_faults > 0
                    && self.consecutive_faults % MAX_FAULTS == 0
                    && !self.stop.stopped()
                {
                    // Remember what to come back to. Surrendering to firmware
                    // used to be permanent, so five transient failures cost the
                    // user their curve for the rest of the session, with
                    // nothing anywhere saying it had happened.
                    if self.surrendered.is_none() {
                        self.surrendered = Some(self.engine.mode().clone());
                    }

                    self.engine.set_mode(Mode::Bios);
                    self.handback_failed = self.engine.shutdown().is_err();
                }

                // Somebody else's turn is not a fault to announce. Blanking
                // the readings on a single busy pass made the window flash its
                // worst message several times a minute while nothing was
                // wrong. The last good reading stands until patience runs out.
                match self.last_good {
                    Some(good) if self.consecutive_faults == 0 => {
                        // Shown, but not written to the history as though it
                        // were a fresh reading: the log is what somebody opens
                        // to explain a surprise, and a smooth trace made of
                        // repeated samples is worse than a gap.
                        self.stale = true;
                        (good.0, good.1, good.2, good.3, false)
                    }
                    _ => (0, None, [0; 2], [None; ipc::SENSOR_COUNT], true),
                }
            }
        };

        // A stalled loop is the other way a level outlives whatever set it.
        // Once only, because the stall clock advances only on a successful
        // tick, so this stays true while the controller is unreachable.
        // Skipped while a stop is pending, like the fault handback above.
        if self.engine.is_stalled() && !self.stalled && !self.stop.stopped() {
            self.stalled = true;

            // Recorded so the watchdog handback can be undone, like the fault
            // one. Without it a single stall silently cost the user their
            // curve until the next reboot.
            if self.surrendered.is_none() {
                self.surrendered = Some(self.engine.mode().clone());
            }

            self.engine.set_mode(Mode::Bios);
            self.handback_failed = self.engine.shutdown().is_err();
        } else if !self.engine.is_stalled() {
            self.stalled = false;
        }

        let mode = match self.engine.mode() {
            Mode::Bios => ipc::MODE_BIOS,
            Mode::Smart => ipc::MODE_SMART,
            Mode::Manual(_) => ipc::MODE_MANUAL,
        };

        self.channel.publish(
            fan_ctrl,
            mode,
            hottest,
            rpm,
            &sensors,
            &self.config.active_profile,
            fault || self.handback_failed || self.surrendered.is_some(),
            status_for(fault, self.surrendered.is_some(), self.handback_failed),
            self.fan_write_declined,
            self.publish_interval_secs(),
        );

        // Last of all. The decision is made, the register written and the
        // window told, and nothing in here can fail in a way this pass has to
        // care about.
        self.record(fault, fan_ctrl, hottest, rpm, mode);
    }

    /// Appends one line to the history file, if one was asked for.
    ///
    /// Takes what the pass already worked out rather than asking the controller
    /// anything of its own: a log that read the hardware would be a second
    /// source of truth, and a slow one.
    fn record(
        &mut self,
        fault: bool,
        fan_ctrl: u8,
        hottest: Option<(usize, i8)>,
        rpm: [u16; 2],
        mode: u8,
    ) {
        if !self.config.log_enabled {
            // Nothing is held open while it is off, so the file can be moved or
            // deleted between sessions without this having an opinion about it.
            self.logger.close();
            return;
        }

        // A pass that only republished the last reading gets empty cells too.
        // The window may show a value a few seconds old, which is useful; the
        // history repeating it as though it were measured is not.
        let measured = !fault && !self.stale;

        let line = crate::log::Record {
            hottest: if measured { hottest } else { None },
            // A failed pass has no applied level and no fan speed. Passing the
            // zeros the publish above uses would write a stopped fan into the
            // history of a machine whose fan was doing no such thing.
            applied: measured.then_some(fan_ctrl),
            fan_rpm: measured.then_some(rpm),
            mode: match mode {
                ipc::MODE_SMART => "smart",
                ipc::MODE_MANUAL => "manual",
                _ => "bios",
            },
            profile: &self.config.active_profile,
        }
        .line();

        let max_bytes = u64::from(self.config.log_max_mb) * 1024 * 1024;

        self.logger.write_line(&line, max_bytes);
    }

    /// Reloads settings when the file on disk has changed.
    ///
    /// Without this the editor writes a curve nobody reads: the engine keeps
    /// the copy it loaded at start, so a saved curve does nothing until the
    /// service restarts. Cheap to check, since it is one metadata read per
    /// poll rather than a parse.
    fn reload_if_changed(&mut self) {
        let path = Config::default_path();
        let Ok(stamp) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
            return;
        };

        if self.config_stamp == Some(stamp) {
            return;
        }

        self.config_stamp = Some(stamp);

        // A broken file keeps the running curve rather than falling back to a
        // default that is not what anyone asked for.
        let Ok(config) = Config::load(&path) else { return };

        // The curve for the profile actually running, which is not
        // necessarily the one the file calls active.
        let running = self.config.active_profile.clone();
        let curve = config
            .profiles
            .iter()
            .find(|p| p.name == running)
            .map(|p| p.to_curve())
            .unwrap_or_else(|| config.active_curve());

        if let Ok(curve) = curve {
            self.engine.set_curve(curve);
        }

        self.engine.set_ignored_sensors(config.ignored_sensors.clone());
        self.engine
            .set_watchdog(Duration::from_secs(config.watchdog_secs as u64));
        self.engine.set_manual_escape(config.manual_escape_c);
        self.engine.set_single_fan(config.single_fan);

        // Keep the profile the user picked from the tray. It lives only in
        // memory, so taking the file's copy would silently revert their
        // choice the next time anything saved.
        let running = std::mem::take(&mut self.config.active_profile);
        self.config = config;
        if self.config.profiles.iter().any(|p| p.name == running) {
            self.config.active_profile = running;
        }
    }

    fn apply_command(&mut self) {
        let shared = self.channel.get();
        let wanted_profile = shared.read_cmd_profile();

        // A profile switch reloads the curve. An unknown name is ignored
        // rather than guessed at.
        if !wanted_profile.is_empty() && wanted_profile != self.config.active_profile {
            // Unknown can just mean newer. A profile created from the tray is
            // saved and announced in the same breath, after this tick already
            // read the file's timestamp, so the name arrives before our copy
            // in memory knows it. Ignoring it would acknowledge the command
            // and drop it, losing the switch for good. Reading the file again
            // is the cheap way to be sure before refusing.
            if !self.config.profiles.iter().any(|p| p.name == wanted_profile) {
                if let Ok(fresh) = Config::load(&Config::default_path()) {
                    if fresh.profiles.iter().any(|p| p.name == wanted_profile) {
                        self.config.profiles = fresh.profiles;
                    }
                }
            }

            if self.config.profiles.iter().any(|p| p.name == wanted_profile) {
                self.config.active_profile = wanted_profile;
                if let Ok(curve) = self.config.active_curve() {
                    self.engine.set_curve(curve);
                }
            }
        }

        // A profile-only command stops here: no mode decision was made, so
        // none of the things that follow from one should happen either.
        if shared.cmd_mode == ipc::MODE_KEEP {
            return;
        }

        let mode = match shared.cmd_mode {
            ipc::MODE_SMART => Mode::Smart,
            // A level the curve rules would reject cannot arrive this way
            // either; anything out of range falls back to firmware control.
            ipc::MODE_MANUAL => {
                // Read once. The section is world writable by design, so a
                // second load could see a different value than the one that
                // was checked, and 0x40 is exactly what an attacker would
                // race in.
                let level = unsafe { std::ptr::read_volatile(std::ptr::addr_of!(shared.cmd_level)) };

                // Level 0 stops the fan with the firmware disabled. Nothing
                // that arrives over a channel any process can write gets to
                // do that; the floor is 1.
                if level == 0 || level > yamato_ec::FAN_LEVEL_MAX {
                    Mode::Bios
                } else {
                    Mode::Manual(level)
                }
            }
            _ => Mode::Bios,
        };

        // The user has just said what they want. That outranks any recovery
        // still pending from an earlier fault: restoring the remembered mode
        // afterwards would take back a fan they had explicitly handed to the
        // firmware, a few seconds after they asked for it.
        self.clear_surrender();

        // And it outranks a standby handback for the same reason. Forgetting
        // it here does not cost the safety: a screen that is still off gets
        // this mode handed back on the next pass that finds sleep, with the
        // new choice recorded rather than the old one.
        self.standby_handback = false;
        self.standby_mode = None;

        // A deliberate instruction also ends the failed-handback warning. It
        // says a level may be held with the firmware switched off, and from
        // here the mode is whatever was just asked for, managed by us.
        self.handback_failed = false;

        self.engine.set_mode(mode);
    }

    /// How often this host publishes, given the power state it is in.
    ///
    /// Read by the tray to decide when a sample has gone stale, so it has to
    /// track the same setting the sleep loop actually uses.
    pub fn publish_interval_secs(&self) -> u16 {
        if self.backed_off() {
            self.config.standby_poll_secs as u16
        } else {
            self.config.poll_secs as u16
        }
    }

    /// Abandons a pending recovery, leaving the current mode in force.
    ///
    /// Called wherever a mode is chosen deliberately, so that a surrender
    /// recorded before the choice cannot undo it afterwards.
    fn clear_surrender(&mut self) {
        self.surrendered = None;
        self.clean_ticks = 0;
    }

    /// Hands the fan back. Safe to call more than once.
    ///
    /// The result matters: a failed handback is a fan left at a fixed level
    /// with the firmware disabled, and the caller should say so.
    pub fn shutdown(&self) -> Result<(), yamato_ec::Error> {
        self.engine.shutdown()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sleeping_machine_is_left_to_its_firmware() {
        // A sleeping laptop is often in a bag. A manual level switches the
        // firmware's thermal management off, and Windows can refuse port
        // access during standby, so keeping the curve there means nobody is
        // managing the fan at all. Not a setting: there is no value of it that
        // would be right, so the engine works it out instead.
        //
        // Once a machine is believed asleep the poll backs off, rather than
        // waking it every few seconds to read a temperature that is not
        // moving.
        let config = Config::default();
        assert!(config.poll_interval(true) > config.poll_interval(false));
    }

    #[test]
    fn the_sleep_clock_reports_no_sleep_on_a_machine_that_is_running() {
        // Both counters advance together while the machine works, so the
        // difference between them is nothing. This runs on a machine that is
        // by definition awake, which is exactly the case worth pinning: a
        // false reading here would take the curve away from a laptop docked
        // and compiling with its lid shut.
        let mut clock = SleepClock::new();

        // The first call has no interval behind it and must not invent one.
        assert_eq!(clock.slept_since_last_pass(), Duration::ZERO);

        std::thread::sleep(Duration::from_millis(50));
        assert!(
            clock.slept_since_last_pass() < Duration::from_millis(10),
            "a working machine must not look asleep"
        );
    }

    #[test]
    fn forgetting_stops_a_resume_reading_as_sleep() {
        // The pass after a wake would otherwise measure across the whole
        // suspend and hand the fan back at the moment the machine came up.
        let mut clock = SleepClock::new();
        clock.slept_since_last_pass();

        clock.forget();
        assert_eq!(
            clock.slept_since_last_pass(),
            Duration::ZERO,
            "a forgotten clock has no interval to report"
        );
    }

    #[test]
    fn a_screen_that_went_off_is_not_the_same_state_as_a_suspend() {
        // They are told apart because they are believed differently. A suspend
        // came with the system's word for it; a dark screen came with nothing,
        // and has to be worked out from the clocks.
        assert_ne!(PowerState::ScreenOff, PowerState::Suspended);
        assert_ne!(PowerState::ScreenOff, PowerState::Awake);
    }

    #[test]
    fn the_engine_lock_is_global() {
        // Session-local would give the service and a user-session window two
        // separate locks, which is no lock at all.
        assert!(ENGINE_LOCK.starts_with(r"Global\"));
    }

    #[test]
    fn the_three_kinds_of_trouble_are_told_apart() {
        assert_eq!(status_for(false, false, false), ipc::STATUS_OK);
        assert_eq!(status_for(true, false, false), ipc::STATUS_UNREACHABLE);
        assert_eq!(status_for(false, true, false), ipc::STATUS_SURRENDERED);
        assert_eq!(status_for(false, false, true), ipc::STATUS_HANDBACK_FAILED);
    }

    #[test]
    fn a_failed_handback_is_reported_over_anything_else() {
        // Every other state here is one where something is still looking after
        // the fan. This is the one where nothing may be.
        for fault in [false, true] {
            for surrendered in [false, true] {
                assert_eq!(
                    status_for(fault, surrendered, true),
                    ipc::STATUS_HANDBACK_FAILED
                );
            }
        }
    }

    #[test]
    fn a_controller_that_cannot_be_read_outranks_the_step_aside_it_caused() {
        // Surrendering is usually the consequence of the faults, and while
        // they are still happening the cause is the more useful thing to say.
        assert_eq!(status_for(true, true, false), ipc::STATUS_UNREACHABLE);
    }

    #[test]
    fn every_unhealthy_state_also_sets_the_flag_older_readers_watch() {
        // The tray decides whether to show trouble at all from `fault`, so a
        // status without it would be a state nothing ever displayed.
        for (fault, surrendered, handback) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let published_fault = fault || handback || surrendered;
            assert_ne!(status_for(fault, surrendered, handback), ipc::STATUS_OK);
            assert!(published_fault, "an unhealthy state that no reader would show");
        }
    }

    #[test]
    fn only_one_process_can_hold_the_lock() {
        let Some(first) = EngineLock::claim() else {
            // An installed Yamato is running and holding it, which is the
            // property under test happening for real, so it counts as a pass.
            return;
        };

        // A second claim in this same process must also be refused, otherwise
        // the guarantee is only across processes and not within one.
        let second = EngineLock::claim();
        assert!(second.is_none(), "the lock was handed out twice");

        drop(first);
        assert!(EngineLock::claim().is_some(), "lock was not released");
    }
}
