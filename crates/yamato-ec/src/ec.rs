// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein
//
// The ACPI embedded controller interface. Register map and the handshake
// are hardware facts, documented by thinkpad_acpi and ThinkWiki, and they are
// the same on every ThinkPad. Which ports carry them is not: see Layout. The
// handshake below is therefore written once and parameterized on the port
// pair, never copied. A second hand-written handshake would be the single
// most likely place for a defect to hide, on exactly the machines nobody can
// test.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::lock::EcLock;
use crate::pawnio::{Error, Layout, PawnIo};

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

/// How many bank 0 sensor bytes a probe reads: the whole bank. Plausibility
/// needs several to compare, since the check that matters is across bytes,
/// not within one.
const PROBE_TEMPS: usize = 8;

/// The band a present sensor on a machine that is up and executing this code
/// can believably be in, in degrees Celsius. Generous at both ends on
/// purpose: a false rejection here strands a real machine, while a false
/// acceptance still has to get past the handshake and the fan register.
const PROBE_TEMP_MIN: i8 = 5;
const PROBE_TEMP_MAX: i8 = 110;

/// What probing one layout found. Both probes are kept on the handle, and on
/// total failure they travel in the error, because the diagnostic that
/// matters from a machine nobody can test is what each layout returned, not
/// just which one was picked.
#[derive(Debug, Clone)]
pub struct Probe {
    pub layout: Layout,
    /// The raw fan control register, when the handshake completed.
    pub fan_ctrl: Option<u8>,
    /// The raw bank 0 sensor bytes, when they could be read.
    pub temps: Option<[u8; PROBE_TEMPS]>,
    /// Why this layout was not accepted. `None` means it passed.
    pub failure: Option<String>,
}

impl Probe {
    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }

    /// A layout that could not be probed at all, module or lock trouble
    /// rather than a verdict on the hardware.
    fn unprobed(layout: Layout, why: &Error) -> Probe {
        Probe { layout, fan_ctrl: None, temps: None, failure: Some(format!("not probed: {why}")) }
    }

    fn rejected(layout: Layout, why: String) -> Probe {
        Probe { layout, fan_ctrl: None, temps: None, failure: Some(why) }
    }
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.layout.describe())?;

        match &self.failure {
            None => write!(f, "passed")?,
            Some(why) => write!(f, "rejected ({why})")?,
        }

        if let Some(v) = self.fan_ctrl {
            write!(f, "; fan register {v:#04x}")?;
        }

        if let Some(temps) = &self.temps {
            write!(f, "; bank 0")?;
            for t in temps {
                write!(f, " {t:02x}")?;
            }
        }

        Ok(())
    }
}

/// Whether a byte read from the fan register could have come from a ThinkPad
/// EC. A completed handshake is not proof by itself: a machine can decode a
/// port window with nothing behind it, and what such a window returns has to
/// be told apart from a fan state by its value.
///
/// Judged on the meaningful bits only, same as every other comparison against
/// this register, because the reserved bits do come back set on some
/// machines. What remains must be one of the states the firmware or a fan
/// tool actually produces: the firmware flag, possibly with the firmware's
/// own level in the low bits; a manual level; or the bare disengage bit.
/// 0xff, the classic floating-bus answer, masks to firmware plus disengage
/// together and fails.
fn plausible_fan_ctrl(value: u8) -> bool {
    let meaningful = value & FAN_BITS;

    if meaningful & FAN_BIOS != 0 {
        return meaningful & FAN_DISENGAGED == 0;
    }

    meaningful == FAN_DISENGAGED || meaningful <= FAN_LEVEL_MAX
}

/// Whether bank 0 reads like temperature sensors rather than like a decoded
/// but empty window.
///
/// This check exists because of a measured machine: a P1 Gen 7 whose EC is
/// at the standard ports also decodes the 0x1600 window, and reads there
/// return constants, 0x00 at some addresses and 0xff at others. A probe that
/// trusted one register would have moved a working machine onto a dead
/// window. So the bank has to disagree with itself, which a floating window
/// never does and a sensor bank always does since absent sensors read 0x00
/// beside live ones, and at least one byte has to be a temperature a running
/// machine could be at. Read as i8, so 0x80 (fitted, not reporting) and 0xff
/// both land far outside the band.
fn plausible_temps(temps: &[u8; PROBE_TEMPS]) -> bool {
    if temps.iter().all(|&t| t == temps[0]) {
        return false;
    }

    temps.iter().any(|&t| (PROBE_TEMP_MIN..=PROBE_TEMP_MAX).contains(&(t as i8)))
}

/// The layout to drive, given what both probes found.
///
/// Only a probe that passed validation is eligible; a layout that merely
/// failed to return an error is not evidence of anything. When both pass,
/// the standard layout wins: it is the ACPI-specified location and the path
/// that has been in production, and the alternate one exists for machines
/// where the standard one demonstrably is not answering.
fn chosen_layout(probes: &[Probe]) -> Option<Layout> {
    [Layout::Standard, Layout::Alternate]
        .into_iter()
        .find(|&wanted| probes.iter().any(|p| p.layout == wanted && p.passed()))
}

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

/// The device behind the handshake. PawnIO in production; in tests, a
/// scripted controller, which is what lets the handback be proven on both
/// layouts without hardware. An enum rather than a generic so `Ec` stays one
/// concrete type everywhere upstream.
enum Io {
    Pawn(PawnIo),
    #[cfg(test)]
    Fake(tests::FakeEc),
}

impl Io {
    fn layout(&self) -> Layout {
        match self {
            Io::Pawn(io) => io.layout(),
            #[cfg(test)]
            Io::Fake(fake) => fake.layout,
        }
    }

    fn prepare(&self) -> Result<(), Error> {
        match self {
            Io::Pawn(io) => io.prepare(),
            #[cfg(test)]
            Io::Fake(fake) => fake.prepare(),
        }
    }

    fn read_port(&self, port: u16) -> Result<u8, Error> {
        match self {
            Io::Pawn(io) => io.read_port(port),
            #[cfg(test)]
            Io::Fake(fake) => fake.read_port(port),
        }
    }

    fn write_port(&self, port: u16, value: u8) -> Result<(), Error> {
        match self {
            Io::Pawn(io) => io.write_port(port, value),
            #[cfg(test)]
            Io::Fake(fake) => fake.write_port(port, value),
        }
    }
}

/// The embedded controller, guarded so only one caller drives it at a time.
pub struct Ec {
    io: Io,
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
    /// What both layouts answered when probed, kept for whoever has to
    /// explain this machine later.
    probes: Vec<Probe>,
}

impl Ec {
    /// Opens the controller at whichever port layout this machine actually
    /// has, decided on evidence from both.
    ///
    /// Both layouts are probed every time, and the probe demands a working
    /// handshake plus values that read like a ThinkPad: see
    /// plausible_fan_ctrl and plausible_temps for why answering at all is
    /// not enough. Choosing on evidence from both, rather than stopping at
    /// the first that fails to error, is what keeps a machine that decodes
    /// both windows on the one that has an EC behind it.
    ///
    /// The probe's job ends at picking the better layout when it can tell.
    /// It is never a reason to refuse to run: when nothing validates, this
    /// returns a handle on the standard layout anyway, with the evidence
    /// attached. Boot is the worst moment to be strict, with Vantage
    /// hammering the controller and the firmware still settling, and the
    /// engine already treats an unreachable controller as an ordinary
    /// running state: it publishes the fact, keeps retrying, and takes the
    /// curve the moment the controller answers. Failing to start would
    /// trade all of that for a service the restart actions abandon in under
    /// a minute. Only the driver being absent or refusing us, or a module
    /// that will not load, stops startup, because no amount of retrying
    /// inside the process fixes either.
    ///
    /// The cost is loading two modules and a handful of register reads, once,
    /// at startup.
    pub fn open() -> Result<Self, Error> {
        let mut probes: Vec<Probe> = Vec::with_capacity(2);
        let mut opened: Vec<Ec> = Vec::new();
        let mut aborted: Option<Error> = None;

        for layout in [Layout::Standard, Layout::Alternate] {
            match Self::try_layout(layout) {
                Ok((ec, probe)) => {
                    opened.push(ec);
                    probes.push(probe);
                }
                // The driver being absent, refusing us, or too old to drive
                // safely stops both layouts equally, so there is nothing to
                // learn from probing on.
                Err(
                    e @ (Error::DriverUnavailable
                    | Error::AccessDenied
                    | Error::DriverTooOld { .. }),
                ) => return Err(e),
                // The module would not load: missing beside the executable,
                // or rejected by the driver. Recorded as evidence, and kept
                // in case it was the fallback layout's module, where it
                // becomes the error.
                Err(e) => {
                    probes.push(Probe::unprobed(layout, &e));
                    if aborted.is_none() {
                        aborted = Some(e);
                    }
                }
            }
        }

        // A validated layout wins. Without one, fall back to the standard
        // layout: it is where almost every machine keeps its EC, and the
        // failures that land here, a contended lock, a handshake timeout, a
        // controller still settling after power-on, are the transient ones
        // the tick loop already retries its way out of.
        let target = chosen_layout(&probes).unwrap_or(Layout::Standard);

        match opened.into_iter().find(|ec| ec.layout() == target) {
            Some(mut ec) => {
                ec.probes = probes;
                Ok(ec)
            }
            // Every layout that opened is in `opened` and the fallback
            // target is the standard layout, so reaching this means the
            // standard module never loaded and `aborted` holds why.
            // NoController stays as the honest answer should that reasoning
            // ever stop holding.
            None => Err(aborted.unwrap_or(Error::NoController(probes))),
        }
    }

    /// Loads one layout's module and probes it. `Err` means the module could
    /// not be loaded at all; a probe that ran and failed, or could not run
    /// because the controller was contended, is an `Ok` carrying the
    /// evidence, along with the handle in case it ends up the fallback.
    fn try_layout(layout: Layout) -> Result<(Ec, Probe), Error> {
        let ec = Ec::with_io(Io::Pawn(PawnIo::open_module(layout)?));

        let probe = match ec.probe() {
            Ok(probe) => probe,
            // The lock was busy, so nothing was learned about the layout and
            // nothing may be concluded from it: another tool holding the
            // controller at logon is routine, not a diagnosis.
            Err(e) => Probe::unprobed(layout, &e),
        };

        Ok((ec, probe))
    }

    fn with_io(io: Io) -> Ec {
        Ec {
            lock: EcLock::acquire_handle(io.layout()),
            io,
            // Two fans until the configuration says otherwise.
            single_fan: AtomicBool::new(false),
            probes: Vec::new(),
        }
    }

    /// Which port layout this handle is driving.
    pub fn layout(&self) -> Layout {
        self.io.layout()
    }

    /// The evidence the choice was made on, one entry per layout.
    pub fn selection(&self) -> &[Probe] {
        &self.probes
    }

    /// The whole selection on one line, for a log entry. Names the winner and
    /// what both layouts answered, which is the difference between a bug
    /// report that can be acted on and one that starts an interrogation.
    ///
    /// A fallback says so in as many words. This string is what a bug report
    /// carries, and "driving the standard layout" would read as a verdict on
    /// hardware that was never actually vouched for.
    pub fn selection_summary(&self) -> String {
        // Derived from the evidence rather than stored, so the two can
        // never disagree: the choice was validated only if the chosen
        // layout's own probe passed.
        let validated =
            self.probes.iter().any(|p| p.layout == self.layout() && p.passed());

        let mut summary = if validated {
            format!("driving the {}", self.layout().describe())
        } else {
            format!(
                "no layout validated at startup; falling back to the {} and \
                 leaving the engine to retry",
                self.layout().describe()
            )
        };

        for probe in &self.probes {
            summary.push_str(". ");
            summary.push_str(&probe.to_string());
        }

        summary
    }

    /// One locked look at whether a ThinkPad EC answers at this layout's
    /// ports, and the evidence either way.
    ///
    /// `Err` is only for a probe that could not run, which today means the
    /// lock: contention says something is alive and holding the controller,
    /// not that the layout is wrong, and concluding anything from it would
    /// misdiagnose a machine where Vantage happened to be mid-transaction.
    ///
    /// The handshake itself is the first check and not a formality. It
    /// cannot complete against a floating port: OBF has to rise in answer to
    /// the read command, which takes an EC, and a port stuck at 0x00 or 0xff
    /// times out in settle or drains forever in begin_transaction. The value
    /// checks on top are for windows that are decoded but empty, which
    /// return constants that a status-bit wait can sail straight through.
    fn probe(&self) -> Result<Probe, Error> {
        let _guard = self.lock.lock()?;
        let layout = self.io.layout();

        // LpcIO refuses every port until a slot is selected and its BARs
        // found; doing it under the same guard makes the whole probe one
        // bracketed transaction. Failure here is a verdict on the layout,
        // a machine with no SuperIO in slot 1 for instance, not on the probe.
        if let Err(e) = self.io.prepare() {
            return Ok(Probe::rejected(layout, format!("could not be set up: {e}")));
        }

        let until = Instant::now() + SAMPLE_BUDGET;

        let fan_ctrl = match self.read_register(REG_FAN_CTRL) {
            Ok(v) => v,
            Err(e) => return Ok(Probe::rejected(layout, format!("no handshake: {e}"))),
        };

        let mut temps = [0u8; PROBE_TEMPS];
        for (i, slot) in temps.iter_mut().enumerate() {
            if Instant::now() >= until {
                return Ok(Probe {
                    layout,
                    fan_ctrl: Some(fan_ctrl),
                    temps: None,
                    failure: Some("probe ran out of budget".to_string()),
                });
            }

            match self.read_register(REG_TEMP_BANK0 + i as u8) {
                Ok(v) => *slot = v,
                Err(e) => {
                    return Ok(Probe {
                        layout,
                        fan_ctrl: Some(fan_ctrl),
                        temps: None,
                        failure: Some(format!("sensors unreadable: {e}")),
                    });
                }
            }
        }

        let failure = if !plausible_fan_ctrl(fan_ctrl) {
            Some(format!("fan register {fan_ctrl:#04x} is not a fan state"))
        } else if !plausible_temps(&temps) {
            Some("bank 0 does not read like temperature sensors".to_string())
        } else {
            None
        };

        Ok(Probe { layout, fan_ctrl: Some(fan_ctrl), temps: Some(temps), failure })
    }

    fn status_port(&self) -> u16 {
        self.io.layout().status_port()
    }

    fn data_port(&self) -> u16 {
        self.io.layout().data_port()
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
    /// which happened. On the alternate layout "the lock" is both mutexes,
    /// acquired and skipped together, so this path needs no second opinion
    /// about which one was refused.
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

        self.io.write_port(self.status_port(), CMD_READ)?;
        self.settle(IBF, false, "read_command")?;

        self.io.write_port(self.data_port(), address)?;
        self.settle(OBF, true, "read_result")?;

        self.io.read_port(self.data_port())
    }

    fn write_register(&self, address: u8, value: u8) -> Result<(), Error> {
        self.begin_transaction()?;

        self.io.write_port(self.status_port(), CMD_WRITE)?;
        self.settle(IBF, false, "write_command")?;

        self.io.write_port(self.data_port(), address)?;
        self.settle(IBF, false, "write_address")?;

        self.io.write_port(self.data_port(), value)?;

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
            let status = self.io.read_port(self.status_port())?;

            if status & OBF != 0 {
                // Something is waiting for us that we did not ask for. Take it
                // and throw it away.
                let _ = self.io.read_port(self.data_port())?;
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
            let status = self.io.read_port(self.status_port())?;
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
            match self.io.read_port(self.status_port()) {
                Ok(status) if status & OBF != 0 => {
                    let _ = self.io.read_port(self.data_port());
                }
                _ => break,
            }

            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Where a fake controller is in the command sequence.
    enum Phase {
        Idle,
        /// A read command was issued; the next data byte is the address.
        WantReadAddress,
        /// A write command was issued; the next data byte is the address.
        WantWriteAddress,
        /// The address arrived; the next data byte is the value for it.
        WantValue(u8),
    }

    /// A controller with the protocol and none of the hardware.
    ///
    /// It stands behind the same `Ec` code that drives a real machine, which
    /// is what lets the handback be proven on both layouts without either
    /// layout's hardware. It follows the ACPI EC command sequence: command on
    /// the status port, address and value on the data port, OBF raised when a
    /// result is waiting. IBF is never raised, since the fake consumes bytes
    /// instantly, so no test spends time in a settle loop.
    pub(super) struct FakeEc {
        pub(super) layout: Layout,
        state: Mutex<FakeState>,
    }

    struct FakeState {
        /// Whether slot select and BAR discovery have happened. Meaningful on
        /// the alternate layout only, mirroring LpcIO's refusal to touch any
        /// port before both.
        prepared: bool,
        /// A window that is decoded but has nothing behind it: every read
        /// answers 0x00 and every write vanishes. What the measured P1 Gen 7
        /// serves at 0x1600/0x1604.
        dead: bool,
        phase: Phase,
        /// The byte waiting in the output buffer, if any.
        obf: Option<u8>,
        selector: usize,
        fan_ctrl: [u8; 2],
        /// Fans that take every byte of a write and then do not hold the
        /// value, which is a failure mode real second fans have.
        declines: [bool; 2],
        temps: [u8; PROBE_TEMPS],
        /// Every register-level write, in order. The handback proof compares
        /// these journals across layouts.
        writes: Vec<(u8, u8)>,
    }

    impl FakeEc {
        /// A healthy ThinkPad at this layout: firmware owns both fans and the
        /// sensor bank reads like a laptop. Already prepared, since open()
        /// prepares before anything else runs.
        fn thinkpad(layout: Layout) -> FakeEc {
            FakeEc {
                layout,
                state: Mutex::new(FakeState {
                    prepared: true,
                    dead: false,
                    phase: Phase::Idle,
                    obf: None,
                    selector: 0,
                    fan_ctrl: [FAN_BIOS; 2],
                    declines: [false; 2],
                    temps: [0x30, 0x2e, 0x00, 0x2d, 0x00, 0x00, 0x00, 0x00],
                    writes: Vec::new(),
                }),
            }
        }

        /// The same machine before slot select and BAR discovery.
        fn unprepared(layout: Layout) -> FakeEc {
            let fake = FakeEc::thinkpad(layout);
            fake.state.lock().unwrap().prepared = false;
            fake
        }

        /// The measured hazard: a window the machine decodes with no EC
        /// behind it. The status port reads 0x00 forever, which a naive
        /// readiness check accepts as "idle and willing".
        fn dead_window(layout: Layout) -> FakeEc {
            let fake = FakeEc::thinkpad(layout);
            fake.state.lock().unwrap().dead = true;
            fake
        }

        fn hold_levels(&self, levels: [u8; 2]) {
            self.state.lock().unwrap().fan_ctrl = levels;
        }

        fn decline_fan(&self, fan: usize) {
            self.state.lock().unwrap().declines[fan] = true;
        }

        fn fan_ctrl(&self) -> [u8; 2] {
            self.state.lock().unwrap().fan_ctrl
        }

        fn writes(&self) -> Vec<(u8, u8)> {
            self.state.lock().unwrap().writes.clone()
        }

        pub(super) fn prepare(&self) -> Result<(), Error> {
            self.state.lock().unwrap().prepared = true;
            Ok(())
        }

        pub(super) fn read_port(&self, port: u16) -> Result<u8, Error> {
            // A port outside the layout's own pair is a test failure, not a
            // condition to model: nothing in this crate may ever ask for one.
            assert!(
                self.layout.permits(port),
                "read of port {port:#06x} outside the {}",
                self.layout.describe()
            );

            let mut state = self.state.lock().unwrap();

            if self.layout == Layout::Alternate && !state.prepared {
                return Err(Error::Call { function: "device_not_ready", code: 0 });
            }

            if state.dead {
                return Ok(0);
            }

            if port == self.layout.status_port() {
                return Ok(if state.obf.is_some() { OBF } else { 0 });
            }

            Ok(state.obf.take().unwrap_or(0xff))
        }

        pub(super) fn write_port(&self, port: u16, value: u8) -> Result<(), Error> {
            assert!(
                self.layout.permits(port),
                "write of port {port:#06x} outside the {}",
                self.layout.describe()
            );

            let mut state = self.state.lock().unwrap();

            if self.layout == Layout::Alternate && !state.prepared {
                return Err(Error::Call { function: "device_not_ready", code: 0 });
            }

            if state.dead {
                return Ok(());
            }

            if port == self.layout.status_port() {
                state.phase = match value {
                    CMD_READ => Phase::WantReadAddress,
                    CMD_WRITE => Phase::WantWriteAddress,
                    _ => Phase::Idle,
                };

                return Ok(());
            }

            match state.phase {
                Phase::WantReadAddress => {
                    state.obf = Some(read_reg(&state, value));
                    state.phase = Phase::Idle;
                }
                Phase::WantWriteAddress => state.phase = Phase::WantValue(value),
                Phase::WantValue(address) => {
                    write_reg(&mut state, address, value);
                    state.phase = Phase::Idle;
                }
                // A stray data byte with no command in flight. A real EC
                // would do something unhelpful; the fake drops it.
                Phase::Idle => {}
            }

            Ok(())
        }
    }

    fn read_reg(state: &FakeState, address: u8) -> u8 {
        match address {
            REG_FAN_CTRL => state.fan_ctrl[state.selector],
            REG_FAN_SELECT => state.selector as u8,
            REG_FAN_SPEED_LO => 0x10,
            REG_FAN_SPEED_HI => 0x0e,
            a if (REG_TEMP_BANK0..REG_TEMP_BANK0 + PROBE_TEMPS as u8).contains(&a) => {
                state.temps[(a - REG_TEMP_BANK0) as usize]
            }
            _ => 0,
        }
    }

    fn write_reg(state: &mut FakeState, address: u8, value: u8) {
        state.writes.push((address, value));

        match address {
            REG_FAN_SELECT => state.selector = (value as usize).min(1),
            REG_FAN_CTRL if !state.declines[state.selector] => {
                state.fan_ctrl[state.selector] = value;
            }
            _ => {}
        }
    }

    fn ec_with(fake: FakeEc) -> Ec {
        Ec {
            lock: EcLock::acquire_handle(fake.layout),
            io: Io::Fake(fake),
            single_fan: AtomicBool::new(false),
            probes: Vec::new(),
        }
    }

    fn fake(ec: &Ec) -> &FakeEc {
        match &ec.io {
            Io::Fake(fake) => fake,
            Io::Pawn(_) => unreachable!("test controllers are always fakes"),
        }
    }

    #[test]
    fn the_handback_is_the_same_bytes_on_both_layouts() {
        // The one guarantee everything else rests on: 0x80 reaches register
        // 0x2f, on every fan, whichever ports carry the transaction. The
        // handback is one piece of code parameterized on the layout, and this
        // proves the parameterization changes nothing that matters: the
        // register-level write sequence, captured on each layout, must be
        // identical byte for byte.
        let mut journals = Vec::new();

        for layout in [Layout::Standard, Layout::Alternate] {
            let controller = FakeEc::thinkpad(layout);
            controller.hold_levels([0x07, 0x03]);

            let ec = ec_with(controller);
            ec.release_to_bios().expect("a healthy controller must accept the handback");

            assert_eq!(
                fake(&ec).fan_ctrl(),
                [FAN_BIOS, FAN_BIOS],
                "both fans must end under firmware control"
            );
            journals.push(fake(&ec).writes());
        }

        assert_eq!(journals[0], journals[1], "the layouts must run the same handback");

        // And the sequence itself: each fan selected, handed 0x80, and the
        // selector parked where the rest of the code expects it.
        assert_eq!(
            journals[0],
            vec![
                (REG_FAN_SELECT, 0),
                (REG_FAN_CTRL, FAN_BIOS),
                (REG_FAN_SELECT, 1),
                (REG_FAN_CTRL, FAN_BIOS),
                (REG_FAN_SELECT, 0),
            ]
        );
    }

    #[test]
    fn set_fan_lands_on_both_fans_whatever_the_layout() {
        for layout in [Layout::Standard, Layout::Alternate] {
            let ec = ec_with(FakeEc::thinkpad(layout));

            ec.set_fan(3).expect("a healthy controller must take a level");
            assert_eq!(fake(&ec).fan_ctrl(), [3, 3]);
        }
    }

    #[test]
    fn a_declining_fan_fails_the_handback_loudly_on_either_layout() {
        // A fan that takes the bytes and drops the value is the silent
        // failure the verified write exists to catch, and the alternate path
        // must catch it with exactly the same behavior: report the failure,
        // and still write the fan that is willing.
        for layout in [Layout::Standard, Layout::Alternate] {
            let controller = FakeEc::thinkpad(layout);
            controller.hold_levels([0x07, 0x07]);
            controller.decline_fan(1);

            let ec = ec_with(controller);
            let error = ec.release_to_bios().expect_err("a dropped 0x80 must be reported");

            assert!(!error.is_contention());
            assert_eq!(
                fake(&ec).fan_ctrl()[0],
                FAN_BIOS,
                "one fan refusing is no reason to abandon the other"
            );
        }
    }

    #[test]
    fn a_probe_accepts_a_thinkpad_on_either_layout() {
        for layout in [Layout::Standard, Layout::Alternate] {
            let ec = ec_with(FakeEc::thinkpad(layout));
            let probe = ec.probe().expect("nothing contends in a test");

            assert!(probe.passed(), "{probe}");
            assert_eq!(probe.fan_ctrl, Some(FAN_BIOS));
        }
    }

    #[test]
    fn a_probe_rejects_a_decoded_but_empty_window() {
        // The measured machine: an EC at the standard ports, and an alternate
        // window that also decodes, reading 0x00 at the status port. "IBF
        // clear, OBF clear, ready" is exactly what a naive check sees there.
        // The handshake cannot complete against it, because OBF never rises
        // in answer to the read command, and the probe must say no.
        let ec = ec_with(FakeEc::dead_window(Layout::Alternate));
        let probe = ec.probe().expect("nothing contends in a test");

        assert!(!probe.passed());
        assert!(probe.fan_ctrl.is_none(), "no handshake means no value to trust");
    }

    #[test]
    fn the_alternate_path_refuses_traffic_until_prepared() {
        // LpcIO answers STATUS_DEVICE_NOT_READY to any port access before
        // slot select and BAR discovery, in that order. The probe is what
        // performs both, so a handle that has not been probed must fail
        // loudly rather than read anything.
        let ec = ec_with(FakeEc::unprepared(Layout::Alternate));
        assert!(ec.sample().is_err());

        let probe = ec.probe().expect("nothing contends in a test");
        assert!(probe.passed(), "{probe}");
        assert!(ec.sample().is_ok(), "a probed handle is a prepared handle");
    }

    #[test]
    fn fan_register_plausibility_is_a_whitelist() {
        // Firmware control, bare or with the firmware's own level showing in
        // the low bits, and with reserved bits set as some machines do.
        assert!(plausible_fan_ctrl(0x80));
        assert!(plausible_fan_ctrl(0x84));
        assert!(plausible_fan_ctrl(0xa0));

        // Manual levels and the bare disengage bit.
        assert!(plausible_fan_ctrl(0x00));
        assert!(plausible_fan_ctrl(0x07));
        assert!(plausible_fan_ctrl(FAN_DISENGAGED));

        // The floating bus answer, and states no ThinkPad produces.
        assert!(!plausible_fan_ctrl(0xff));
        assert!(!plausible_fan_ctrl(0xc0));
        assert!(!plausible_fan_ctrl(0x47));
    }

    #[test]
    fn temperature_plausibility_rejects_constant_windows() {
        // What a decoded-but-empty window returns: one value, forever.
        assert!(!plausible_temps(&[0x00; PROBE_TEMPS]));
        assert!(!plausible_temps(&[0xff; PROBE_TEMPS]));
        // Any constant, not just the classic two.
        assert!(!plausible_temps(&[0x30; PROBE_TEMPS]));

        // Varied, but nothing a running machine could be at.
        assert!(!plausible_temps(&[0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7]));

        // A real bank: live sensors beside absent ones.
        assert!(plausible_temps(&[0x30, 0x2e, 0x00, 0x2d, 0x00, 0x00, 0x00, 0x00]));
        // One live sensor is enough, provided the bank disagrees with itself.
        assert!(plausible_temps(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x37]));
    }

    #[test]
    fn selection_needs_evidence_and_prefers_the_standard_layout() {
        let pass = |layout| Probe { layout, fan_ctrl: Some(FAN_BIOS), temps: None, failure: None };
        let fail = |layout| Probe::rejected(layout, "no handshake".to_string());

        // Both pass: the specified location wins the tie.
        assert_eq!(
            chosen_layout(&[pass(Layout::Standard), pass(Layout::Alternate)]),
            Some(Layout::Standard)
        );
        // The order the evidence arrived in must not matter, only its content.
        assert_eq!(
            chosen_layout(&[pass(Layout::Alternate), pass(Layout::Standard)]),
            Some(Layout::Standard)
        );
        // Only one passes: evidence outranks preference.
        assert_eq!(
            chosen_layout(&[fail(Layout::Standard), pass(Layout::Alternate)]),
            Some(Layout::Alternate)
        );
        // Neither passes: no controller, and no guessing.
        assert_eq!(chosen_layout(&[fail(Layout::Standard), fail(Layout::Alternate)]), None);
    }

    #[test]
    fn the_summary_says_plainly_when_the_choice_was_a_fallback() {
        // What a bug report carries. A fallback must not read like a
        // verdict: "driving the standard layout" on a machine where nothing
        // validated would send whoever reads it hunting in the wrong
        // direction, and the fallback is exactly the case that produces the
        // reports.
        let mut ec = ec_with(FakeEc::thinkpad(Layout::Standard));

        ec.probes = vec![
            Probe::rejected(Layout::Standard, "no handshake: test".to_string()),
            Probe::rejected(Layout::Alternate, "no handshake: test".to_string()),
        ];

        let fallback = ec.selection_summary();
        assert!(fallback.contains("no layout validated"), "{fallback}");
        assert!(fallback.contains("falling back"), "{fallback}");

        // And a validated pass must not cry wolf.
        ec.probes = vec![
            Probe {
                layout: Layout::Standard,
                fan_ctrl: Some(FAN_BIOS),
                temps: None,
                failure: None,
            },
            Probe::rejected(Layout::Alternate, "no handshake: test".to_string()),
        ];

        let validated = ec.selection_summary();
        assert!(validated.starts_with("driving the"), "{validated}");
        assert!(!validated.contains("falling back"), "{validated}");
    }

    #[test]
    fn a_sample_reads_the_same_machine_the_same_way_on_both_layouts() {
        let mut states = Vec::new();

        for layout in [Layout::Standard, Layout::Alternate] {
            let ec = ec_with(FakeEc::thinkpad(layout));
            states.push(ec.sample().expect("a healthy controller must be readable"));
        }

        let (a, b) = (&states[0], &states[1]);
        assert_eq!(a.fan_ctrl_per_fan, b.fan_ctrl_per_fan);
        assert_eq!(a.fan_rpm, b.fan_rpm);
        assert_eq!(a.sensors, b.sensors);

        // And the reading itself decodes as the fake's machine.
        assert!(a.is_bios_controlled());
        assert_eq!(a.fan_rpm[0], 0x0e10);
        assert_eq!(a.sensors[0], Some(0x30));
        assert_eq!(a.sensors[2], None);
    }

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
