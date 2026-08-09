// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein
//
// The ACPI embedded controller interface. Register layout and the handshake
// are hardware facts, documented by thinkpad_acpi and ThinkWiki.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::lock::EcLock;
use crate::pawnio::{Error, PawnIo};

const EC_STATUS: u16 = 0x66;
const EC_DATA: u16 = 0x62;

/// Status register bits.
const OBF: u8 = 0x01; // output buffer full, a byte is waiting for us
const IBF: u8 = 0x02; // input buffer full, the EC has not taken ours yet

/// Commands written to the status port.
const CMD_READ: u8 = 0x80;
const CMD_WRITE: u8 = 0x81;

/// Fan control. 0x00-0x07 are manual levels, 0x80 hands control to the
/// firmware, 0x40 disengages the governor entirely.
pub const REG_FAN_CTRL: u8 = 0x2f;
/// Which fan the speed registers report on. 0 is the first, 1 the second.
pub const REG_FAN_SELECT: u8 = 0x31;
/// 16 bit fan speed, low byte first.
pub const REG_FAN_SPEED_LO: u8 = 0x84;
pub const REG_FAN_SPEED_HI: u8 = 0x85;

/// Temperature sensors. Two banks, both one degree Celsius per count.
pub const REG_TEMP_BANK0: u8 = 0x78; // 8 sensors
pub const REG_TEMP_BANK1: u8 = 0xc0; // 4 more
pub const SENSOR_COUNT: usize = 12;

/// The bits of the fan register that carry meaning: the firmware flag, the
/// disengage bit, and the level. Everything else belongs to the controller and
/// is not ours to compare against.
pub const FAN_BITS: u8 = 0xc7;

/// Written to REG_FAN_CTRL to give the fan back to the firmware.
pub const FAN_BIOS: u8 = 0x80;
/// Highest ordinary level. The fan can go faster; nothing should ask it to.
pub const FAN_LEVEL_MAX: u8 = 7;
/// Disengages the EC's fan governor. Runs the blower unregulated, above level
/// 7, and is documented as potentially unsupported and damaging. Never
/// produced by a curve, and only reachable if something asks for it outright.
pub const FAN_DISENGAGED: u8 = 0x40;

/// How long to wait for a status bit before deciding the EC is not answering.
///
/// Per wait, not per pass; SAMPLE_BUDGET bounds the pass. Kept generous
/// because a five millisecond sleep is really about fifteen on Windows, so a
/// short timeout is far fewer polls than it looks, and a slow but healthy
/// controller still has to be able to meet it.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(750);

/// Gap between status reads while waiting.
///
/// A sleep, not a spin. Every poll is a round trip through a driver, and
/// pegging a core to find out why the fan is not responding is a poor way to
/// cool a laptop.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// How many stale bytes to clear before deciding the controller is wedged and
/// not just behind.
const DRAIN_READS: usize = 8;

/// Ceiling on one whole read pass, regardless of how the individual waits go.
const SAMPLE_BUDGET: Duration = Duration::from_secs(3);

/// The same for a fan write, which retries across both fans and so has more
/// room to overrun. Sized so a slow controller still gets its retries without
/// a tick swallowing a shutdown.
const SET_FAN_BUDGET: Duration = Duration::from_secs(5);

/// Puts the fan selector back however the caller leaves.
struct SelectorGuard<'a> {
    ec: &'a Ec,
}

impl Drop for SelectorGuard<'_> {
    fn drop(&mut self) {
        let _ = self.ec.write_register(REG_FAN_SELECT, 0);
    }
}

/// One sample of everything worth knowing.
#[derive(Debug, Clone, Default)]
pub struct EcState {
    /// Raw contents of the fan control register.
    pub fan_ctrl: u8,
    /// The fan register as seen through each fan's own selector.
    ///
    /// Reading it once describes fan 1 only, which let fan 2 sit at a manual
    /// level with the firmware disabled while everything upstream looked fine.
    pub fan_ctrl_per_fan: [u8; 2],
    /// RPM per fan. A machine with one fan reports one.
    pub fan_rpm: [u16; 2],
    /// Degrees Celsius. `None` where the sensor is absent or disconnected.
    pub sensors: [Option<i8>; SENSOR_COUNT],
}

impl EcState {
    /// The reading a curve should act on: the hottest sensor that is present
    /// and not being ignored.
    pub fn hottest(&self, ignored: &[usize]) -> Option<(usize, i8)> {
        self.sensors
            .iter()
            .enumerate()
            .filter(|(i, _)| !ignored.contains(i))
            .filter_map(|(i, t)| t.map(|t| (i, t)))
            .max_by_key(|(_, t)| *t)
    }

    /// True only when the firmware owns *every* fan.
    ///
    /// One fan back on firmware while another holds a level is half a laptop
    /// with no fan management and no sign of it.
    pub fn is_bios_controlled(&self) -> bool {
        self.fan_ctrl_per_fan.iter().all(|c| c & FAN_BIOS != 0)
    }

    /// The manual level currently set, if we are not in firmware mode.
    pub fn manual_level(&self) -> Option<u8> {
        if self.is_bios_controlled() {
            None
        } else {
            Some(self.fan_ctrl & 0x3f)
        }
    }
}

/// Fills the second fan's slot from the first, for a machine that has no
/// second fan to read.
///
/// [`EcState::is_bios_controlled`] requires *every* slot to carry the firmware
/// flag, and that rule stays in one place. Instead of teaching the check about
/// fan counts, a single-fan sample produces a state where the rule holds
/// whenever the one real fan is on firmware. Free-standing so it can be tested
/// without a controller.
fn mirror_lone_fan(state: &mut EcState) {
    state.fan_ctrl_per_fan[1] = state.fan_ctrl_per_fan[0];
}

/// The name set_fan gives the failure where the controller took every byte and
/// then did not hold the value. Defined once because
/// [`Error::is_fan_write_declined`] matches on it.
const SET_FAN_DECLINED: &str = "set_fan_declined";

impl Error {
    /// Whether this failure was the controller declining a fan value or not
    /// answering at all.
    ///
    /// Upstream both look like a failed tick, but they need different answers.
    /// Unreachable means nothing but time helps. Declined, on a machine whose
    /// second fan has never once reported a speed, usually means a single-fan
    /// machine being verified through a selector it does not have, and there
    /// is a setting for that. The host publishes the distinction so a client
    /// can offer the hint.
    pub fn is_fan_write_declined(&self) -> bool {
        matches!(self, Error::Call { function: SET_FAN_DECLINED, .. })
    }

    /// Whether somebody else was holding the controller, as against the
    /// controller failing.
    ///
    /// The answer to a fan that cannot be managed is to hand it back to the
    /// firmware, and the handback needs this same lock. So giving up on
    /// contention gains nothing: the handback would be refused for the same
    /// reason the read was. All it does is stop the curve and gray the icon
    /// while another program has a turn. Waiting is the only thing that works.
    pub fn is_contention(&self) -> bool {
        matches!(self, Error::Busy)
    }
}

/// The embedded controller, guarded so only one caller drives it at a time.
pub struct Ec {
    io: PawnIo,
    lock: EcLock,
    /// Whether this machine has one fan, so the second selector is never
    /// touched. See the `single_fan` configuration field for why it is an
    /// explicit setting and not detected.
    ///
    /// An atomic on the handle, not a constructor argument: the engine holds
    /// this behind an `Arc` and the setting has to follow a configuration
    /// reload, so a flag fixed at construction would only take effect on the
    /// next restart.
    single_fan: AtomicBool,
}

impl Ec {
    pub fn open() -> Result<Self, Error> {
        Ok(Ec {
            io: PawnIo::open()?,
            lock: EcLock::acquire_handle(),
            // Two fans until the configuration says otherwise.
            single_fan: AtomicBool::new(false),
        })
    }

    /// Tells the handle how many fans to talk to.
    ///
    /// Callable any time. The next locked pass acts on it; a pass already
    /// under way finishes under the rule it started with, which is harmless
    /// either way.
    pub fn set_single_fan(&self, single: bool) {
        self.single_fan.store(single, Ordering::SeqCst);
    }

    /// One past the last fan selector to drive: 1 on a single-fan machine,
    /// 2 everywhere else.
    fn fan_count(&self) -> u8 {
        if self.single_fan.load(Ordering::SeqCst) { 1 } else { 2 }
    }

    /// Reads everything in one locked pass, so a sample is internally
    /// consistent and not smeared across other tools' accesses.
    pub fn sample(&self) -> Result<EcState, Error> {
        let _guard = self.lock.lock()?;
        // A whole-pass deadline on top of the per-wait one, started before the
        // first transaction. The seven fan and selector reads below were once
        // outside it, which left the pass unbounded.
        let until = Instant::now() + SAMPLE_BUDGET;
        // Restores the selector however this returns. An error partway through
        // would leave it on fan 2, and the next sample reads the fan register
        // before touching the selector.
        let _selector = SelectorGuard { ec: self };

        let mut state = EcState::default();

        // On a single-fan machine the loop stops after the first fan: nothing
        // that comes back through the second selector describes anything. The
        // second fan's speed stays at the zero it was initialized to, which is
        // what an absent fan should read.
        let fans = self.fan_count();

        for fan in 0..fans {
            self.write_register(REG_FAN_SELECT, fan)?;

            // Through this fan's own selector, so a second fan holding a level
            // is visible instead of hidden behind the first.
            state.fan_ctrl_per_fan[fan as usize] = self.read_register(REG_FAN_CTRL)?;

            let lo = self.read_register(REG_FAN_SPEED_LO)? as u16;
            let hi = self.read_register(REG_FAN_SPEED_HI)? as u16;
            let rpm = (hi << 8) | lo;

            // The EC briefly reports nonsense while it switches fans.
            state.fan_rpm[fan as usize] = if rpm > 0x1fff { 0 } else { rpm };

            if Instant::now() >= until {
                return Err(Error::Call { function: "sample_budget", code: fan as u32 });
            }
        }

        // With one fan, the second slot was never read, so fill it from the
        // first instead of leaving it at zero.
        if fans == 1 {
            mirror_lone_fan(&mut state);
        }

        // Fan 1 stays the headline figure for display.
        state.fan_ctrl = state.fan_ctrl_per_fan[0];

        // Put the selector back. Leaving it on the second fan means the next
        // write to the fan register lands on that fan alone, which on a two
        // fan machine is how one of them gets left running unmanaged.
        self.write_register(REG_FAN_SELECT, 0)?;

        for i in 0..SENSOR_COUNT {
            if Instant::now() >= until {
                return Err(Error::Call { function: "sample_budget", code: i as u32 });
            }

            let reg = if i < 8 {
                REG_TEMP_BANK0 + i as u8
            } else {
                REG_TEMP_BANK1 + (i as u8 - 8)
            };

            let raw = self.read_register(reg)? as i8;

            // 0x00 means no sensor fitted, 0x80 means fitted but not reporting.
            state.sensors[i] = if raw == 0 || raw == -128 { None } else { Some(raw) };
        }

        Ok(state)
    }

    /// Sets the fan, on every fan the machine has, and proves it took.
    ///
    /// The fan register is read through whichever fan the selector points at,
    /// so a dual fan machine needs the value written twice. Writing it once
    /// leaves the other fan wherever it was, including on the way out.
    ///
    /// The write is read back and compared, because a quietly declined write
    /// gets recorded as applied and never tried again.
    ///
    /// It retries, because the EC does sometimes need a moment.
    pub fn set_fan(&self, value: u8) -> Result<u8, Error> {
        let _guard = self.lock.lock()?;
        let _selector = SelectorGuard { ec: self };

        // The same whole-call deadline sample() has. Five attempts across two
        // fans is thirty handshakes; against a controller that answers its
        // ports but never completes one, that was roughly a hundred seconds in
        // a single tick, long enough for a stop to overrun the shutdown limit
        // and leave a level set.
        let until = Instant::now() + SET_FAN_BUDGET;

        // How many fans to write and prove. On a single-fan machine the second
        // selector reads back something that never matches, so including it
        // would make every write a decline: the caller counts the faults and
        // surrenders to the firmware over a fan that does not exist.
        let fans = self.fan_count();

        for attempt in 0..5 {
            let mut all_took = true;

            // Each fan verified through its own selector. Reading back only
            // through fan 1 would let fan 2 decline silently and, on the way
            // out, leave it spinning at a fixed level with nothing managing it.
            for fan in 0..fans {
                self.write_register(REG_FAN_SELECT, fan)?;
                self.write_register(REG_FAN_CTRL, value)?;

                // Compare only the bits that mean something. The rest belongs
                // to the controller and does come back set on some machines,
                // so a whole-byte comparison would call a good handoff a
                // failure.
                if self.read_register(REG_FAN_CTRL)? & FAN_BITS != value & FAN_BITS {
                    all_took = false;
                }
            }

            // Leave the selector where the rest of the code expects it.
            self.write_register(REG_FAN_SELECT, 0)?;

            if all_took {
                return Ok(value);
            }

            if Instant::now() >= until {
                break;
            }

            // Settling time. The controller can take a moment to accept a
            // change, particularly when coming out of firmware control.
            if attempt < 4 {
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        Err(Error::Call { function: SET_FAN_DECLINED, code: value as u32 })
    }

    /// Hands the fan back to the firmware. The one operation that must work,
    /// and it never returns without having written the register.
    ///
    /// Writing without the lock can interleave with another tool and land a
    /// foreign byte in the fan register. Refusing to write without it is
    /// worse: that trades a small chance of a corrupt write for a certainty of
    /// a fan pinned at a manual level with the firmware disabled. So chase the
    /// lock hard, and if it cannot be had, write anyway. The caller is told
    /// which happened.
    pub fn release_to_bios(&self) -> Result<(), Error> {
        for attempt in 0..6 {
            if let Ok(_guard) = self.lock.lock() {
                return self.write_bios_verified();
            }

            if attempt < 5 {
                std::thread::sleep(Duration::from_millis(250));
            }
        }

        // Last resort, and unguarded. Calling begin_transaction here would not
        // help: write_register already does, and the race that matters is
        // between our own address and value bytes, which nothing outside the
        // lock can bracket. Thirteen seconds of contention means the holder is
        // wedged, not transacting, so the risk is small and taken knowingly.
        self.write_bios_verified()
    }

    /// Writes the firmware flag to every fan and proves it on each one.
    ///
    /// Verifying through fan 1 alone would let fan 2 decline silently, leaving
    /// it spinning at a fixed level with nothing managing it.
    fn write_bios_verified(&self) -> Result<(), Error> {
        let mut failed = None;

        // Written to both selectors whatever the fan count, since writing
        // 0x80 to a selector that is not there costs nothing and covers the
        // case where the setting is wrong the other way. Only the fans this
        // machine has are allowed to report failure: verifying a phantom
        // second fan always fails, which latched the handback-failed warning
        // permanently on a single-fan machine and made every service stop
        // spend its whole budget before writing a false event log entry.
        for fan in 0..2u8 {
            if let Err(e) = self.select_and_verify(fan, FAN_BIOS) {
                if fan < self.fan_count() {
                    // Carry on to the other fan regardless. One of them
                    // refusing is no reason to abandon the other.
                    failed = Some(e);
                }
            }
        }

        let _ = self.write_register(REG_FAN_SELECT, 0);

        match failed {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn select_and_verify(&self, fan: u8, value: u8) -> Result<(), Error> {
        self.write_register(REG_FAN_SELECT, fan)?;
        self.write_register(REG_FAN_CTRL, value)?;

        if self.read_register(REG_FAN_CTRL)? & FAN_BITS == value & FAN_BITS {
            Ok(())
        } else {
            Err(Error::Call { function: "fan_write_declined", code: fan as u32 })
        }
    }

    fn read_register(&self, address: u8) -> Result<u8, Error> {
        self.begin_transaction()?;

        self.io.write_port(EC_STATUS, CMD_READ)?;
        self.settle(IBF, false, "read_command")?;

        self.io.write_port(EC_DATA, address)?;
        self.settle(OBF, true, "read_result")?;

        self.io.read_port(EC_DATA)
    }

    fn write_register(&self, address: u8, value: u8) -> Result<(), Error> {
        self.begin_transaction()?;

        self.io.write_port(EC_STATUS, CMD_WRITE)?;
        self.settle(IBF, false, "write_command")?;

        self.io.write_port(EC_DATA, address)?;
        self.settle(IBF, false, "write_address")?;

        self.io.write_port(EC_DATA, value)?;

        // Wait for the controller to actually take the value before letting go
        // of the lock. Returning with a byte still in flight means the next
        // caller starts talking over the tail of this transaction.
        self.settle(IBF, false, "write_value")
    }

    /// Waits until the controller is idle with nothing left over.
    ///
    /// If a byte is already sitting in the output buffer, from an aborted
    /// transaction elsewhere or an event the controller raised on its own, the
    /// next read returns *that* byte instead of the register it asked for.
    /// Every read after it in the pass is shifted by one, and a shifted read
    /// looks like a cold laptop.
    fn begin_transaction(&self) -> Result<(), Error> {
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;

        loop {
            let status = self.io.read_port(EC_STATUS)?;

            if status & OBF != 0 {
                // Something is waiting for us that we did not ask for. Take it
                // and throw it away.
                let _ = self.io.read_port(EC_DATA)?;
            } else if status & IBF == 0 {
                return Ok(());
            }

            if Instant::now() >= deadline {
                return Err(Error::Call { function: "ec_not_idle", code: status as u32 });
            }

            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Waits for one status bit to reach a state, part way through a
    /// transaction.
    ///
    /// On failure the controller is left mid-sequence, expecting bytes we are
    /// never going to send, so this clears it out before giving up. Without
    /// that, the next transaction's command byte gets consumed as this one's
    /// missing operand and lands in a register nobody chose.
    fn settle(&self, bit: u8, set: bool, step: &'static str) -> Result<(), Error> {
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;

        loop {
            let status = self.io.read_port(EC_STATUS)?;
            if (status & bit != 0) == set {
                return Ok(());
            }

            if Instant::now() >= deadline {
                self.abandon_transaction();

                return Err(Error::Call { function: step, code: status as u32 });
            }

            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Best effort tidy-up after a transaction we could not finish.
    ///
    /// Nothing here can fail usefully. We are already in the error path.
    fn abandon_transaction(&self) {
        for _ in 0..DRAIN_READS {
            match self.io.read_port(EC_STATUS) {
                Ok(status) if status & OBF != 0 => {
                    let _ = self.io.read_port(EC_DATA);
                }
                _ => break,
            }

            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(sensors: [Option<i8>; SENSOR_COUNT], fan_ctrl: u8) -> EcState {
        EcState {
            fan_ctrl,
            fan_ctrl_per_fan: [fan_ctrl; 2],
            fan_rpm: [0; 2],
            sensors,
        }
    }

    #[test]
    fn one_fan_still_held_is_not_bios_controlled() {
        // The dual fan case. Reading the register once described fan 1 only,
        // so a machine with fan 2 pinned at a manual level, firmware disabled,
        // reported itself healthy and handed back.
        let mut state = state_with([None; SENSOR_COUNT], FAN_BIOS);
        state.fan_ctrl_per_fan = [FAN_BIOS, 0x07];

        assert!(!state.is_bios_controlled());

        state.fan_ctrl_per_fan = [FAN_BIOS, FAN_BIOS];
        assert!(state.is_bios_controlled());
    }

    #[test]
    fn a_single_fan_sample_is_judged_on_the_fan_it_has() {
        // What sample() produces when single_fan is set: the second slot is
        // never read, so it mirrors the first and the handback is judged on
        // the real fan alone. Without this a single-fan machine could never
        // satisfy is_bios_controlled, and the "fan may be held" warning stayed
        // latched forever over nothing.
        let mut state = state_with([None; SENSOR_COUNT], FAN_BIOS);
        state.fan_ctrl_per_fan = [FAN_BIOS, 0x07];
        mirror_lone_fan(&mut state);

        assert!(state.is_bios_controlled());

        // And the mirror must not manufacture a handback either: the one real
        // fan holding a level is still a fan holding a level.
        state.fan_ctrl_per_fan = [0x07, FAN_BIOS];
        mirror_lone_fan(&mut state);

        assert!(!state.is_bios_controlled());
    }

    #[test]
    fn a_declined_fan_write_is_told_apart_from_an_unanswered_one() {
        // The single-fan hint hangs off this distinction: declined means the
        // controller is answering and refusing the value, which with a second
        // fan that has never spun points at the setting. Unreachable means no
        // such thing.
        assert!(Error::Call { function: SET_FAN_DECLINED, code: 7 }.is_fan_write_declined());
        assert!(!Error::Call { function: "ec_not_idle", code: 0 }.is_fan_write_declined());
        assert!(!Error::Busy.is_fan_write_declined());
    }

    #[test]
    fn hottest_skips_absent_and_ignored_sensors() {
        let mut s = [None; SENSOR_COUNT];
        s[0] = Some(50);
        s[1] = Some(91); // the one we will ignore
        s[2] = Some(62);

        let st = state_with(s, FAN_BIOS);
        assert_eq!(st.hottest(&[]), Some((1, 91)));
        assert_eq!(st.hottest(&[1]), Some((2, 62)));
    }

    #[test]
    fn hottest_is_none_when_nothing_reports() {
        let st = state_with([None; SENSOR_COUNT], FAN_BIOS);
        assert_eq!(st.hottest(&[]), None);
    }

    #[test]
    fn bios_bit_and_manual_level_decode() {
        assert!(state_with([None; SENSOR_COUNT], FAN_BIOS).is_bios_controlled());
        assert_eq!(state_with([None; SENSOR_COUNT], FAN_BIOS).manual_level(), None);

        let manual = state_with([None; SENSOR_COUNT], 0x03);
        assert!(!manual.is_bios_controlled());
        assert_eq!(manual.manual_level(), Some(3));
    }
}
