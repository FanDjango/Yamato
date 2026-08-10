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
///
/// Also the whole envelope for probing one layout, retries included, so the
/// probe's persistence never buys a later start than one slow pass already
/// could. See probe_persistently.
const SAMPLE_BUDGET: Duration = Duration::from_secs(3);

/// Pause between probe attempts on a layout that could not be examined.
///
/// The same quarter second the caller's tick loop waits between its retries
/// of a failed pass. When the failures are quick ones, an IOCTL erroring
/// outright, SAMPLE_BUDGET holds roughly the ten tries TPFanControl gives
/// the controller before believing a read failed.
const PROBE_RETRY_PAUSE: Duration = Duration::from_millis(250);

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
    pub failure: Option<ProbeFailure>,
}

/// Why a layout failed its probe, split by what the failure is evidence of.
///
/// The split carries the one distinction the caller must not lose: whether
/// the verdict may be written into the settings. A rejection describes the
/// machine and will still be true at the next boot. An unexamined layout
/// describes one moment of one boot: four back-to-back service restarts on a
/// measured machine produced three of them against a standard layout that
/// works, and a decision it could have outranked, recorded anyway, would
/// freeze an accident of timing into every boot that follows. See
/// worth_recording for when the distinction bites.
#[derive(Debug, Clone)]
pub enum ProbeFailure {
    /// The layout answered, and what came back is not a ThinkPad EC: the
    /// values failed plausibility, or the fan register would not hold a
    /// write it was given a fair chance to take. Evidence about the machine.
    Rejected(String),
    /// The layout could not be examined: the controller was locked, an
    /// IOCTL failed, a handshake timed out, the budget ran out, or the
    /// module never loaded. Evidence about this moment only.
    Unexamined(String),
}

impl Probe {
    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }

    /// Whether this probe reached a verdict about the machine rather than
    /// about the moment. A pass and a rejection both did; a layout that
    /// could not be examined got no verdict at all, and nothing may be
    /// recorded on the strength of it.
    pub fn definitive(&self) -> bool {
        !matches!(self.failure, Some(ProbeFailure::Unexamined(_)))
    }

    fn rejected(layout: Layout, why: String) -> Probe {
        Probe { layout, fan_ctrl: None, temps: None, failure: Some(ProbeFailure::Rejected(why)) }
    }

    /// A layout that could not be examined, lock or transport trouble
    /// rather than a verdict on the hardware.
    fn unexamined(layout: Layout, why: String) -> Probe {
        Probe { layout, fan_ctrl: None, temps: None, failure: Some(ProbeFailure::Unexamined(why)) }
    }
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.layout.describe())?;

        // "Rejected" and "could not be examined" are different claims, and
        // the event log is where the difference reaches a person: one says
        // this window is not an EC, the other says nothing about the window
        // was learned at all.
        match &self.failure {
            None => write!(f, "passed")?,
            Some(ProbeFailure::Rejected(why)) => write!(f, "rejected ({why})")?,
            Some(ProbeFailure::Unexamined(why)) => write!(f, "could not be examined ({why})")?,
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
    /// explain this machine later. Empty on a configured handle, which
    /// probes nothing.
    probes: Vec<Probe>,
    /// Whether the layout was dictated by the settings rather than probed.
    /// A configured layout is trusted, not vouched for, and the summary has
    /// to say which.
    configured: bool,
    /// Whether the module's own setup has run on this handle. See
    /// [`Ec::ensure_prepared`].
    prepared: AtomicBool,
}

impl Ec {
    /// Opens the controller at whichever port layout this machine actually
    /// has, decided on evidence from both.
    ///
    /// This runs until a verdict sticks: the caller records a definitive
    /// verdict in the settings, and every start after that goes through
    /// [`Ec::open_configured`] instead. Probing is a hardware interaction,
    /// on the alternate module a walk of the SuperIO configuration space
    /// through 0x4e/0x4f, and repeating it at every boot on machines that
    /// settled the question at their first one bought nothing.
    ///
    /// Both layouts are probed, and the probe demands a working handshake,
    /// values that read like a ThinkPad, and a fan register that holds a
    /// write: see plausible_fan_ctrl, plausible_temps and prove_fan_control
    /// for why answering at all is not enough. Each layout gets the tick
    /// path's persistence before any failure is believed, because one look
    /// at a bad moment once rejected a working layout on three restarts out
    /// of four: see probe_persistently. Choosing on evidence from both,
    /// rather than stopping at the first that fails to error, is what keeps
    /// a machine that decodes both windows on the one that has an EC behind
    /// it. A choice that an unexamined loser could have outranked is driven
    /// for this session but never recorded, so the next start looks again:
    /// see worth_recording.
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
                // or rejected by the driver. That is trouble with this
                // install, not evidence about the machine's ports, so it
                // counts as unexamined. Kept in case it was the fallback
                // layout's module, where it becomes the error.
                Err(e) => {
                    probes.push(Probe::unexamined(layout, format!("not probed: {e}")));
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

    /// Loads one layout's module and probes it, with retries. `Err` means
    /// the module could not be loaded at all; a probe that ran and failed,
    /// or could not run because the controller was contended, is an `Ok`
    /// carrying the evidence, along with the handle in case it ends up the
    /// fallback.
    fn try_layout(layout: Layout) -> Result<(Ec, Probe), Error> {
        let ec = Ec::with_io(Io::Pawn(PawnIo::open_module(layout)?));
        let probe = ec.probe_persistently();

        Ok((ec, probe))
    }

    /// Opens the controller on the given layout because the settings say so,
    /// probing nothing and validating nothing away.
    ///
    /// This is every start after the first. The layout is driven as given,
    /// whether it came from the probe that ran once at first start or from
    /// somebody overriding it, and no SuperIO discovery happens here: on the
    /// alternate layout the module's own setup runs lazily, under the lock,
    /// at the first transaction. If the configuration is wrong, the engine
    /// reports the controller unreachable exactly as it would for any dead
    /// controller, which is the honest outcome of overriding a detection,
    /// and the client suggests trying the other mode.
    pub fn open_configured(layout: Layout) -> Result<Self, Error> {
        let mut ec = Ec::with_io(Io::Pawn(PawnIo::open_module(layout)?));
        ec.configured = true;

        Ok(ec)
    }

    fn with_io(io: Io) -> Ec {
        Ec {
            lock: EcLock::acquire_handle(io.layout()),
            io,
            // Two fans until the configuration says otherwise.
            single_fan: AtomicBool::new(false),
            probes: Vec::new(),
            configured: false,
            prepared: AtomicBool::new(false),
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

    /// Whether the layout being driven passed its own probe. Derived from
    /// the evidence rather than stored, so the two can never disagree.
    ///
    /// Always false on a configured handle, which probes nothing: a
    /// configured layout is trusted, not vouched for, and nothing that never
    /// ran gets to be called a validation. worth_recording starts from this
    /// answer, so a fallback nothing vouched for is never frozen in as an
    /// answer.
    pub fn validated(&self) -> bool {
        self.probes.iter().any(|p| p.layout == self.layout() && p.passed())
    }

    /// Whether the selection is one the caller may write into the settings,
    /// where it becomes every later boot's answer.
    ///
    /// The record has to survive one question: could the layout that was
    /// not examined have beaten the winner? Ties go to the standard layout,
    /// so a passing standard layout is recorded whatever became of the
    /// alternate: even a pass there would have changed nothing, and holding
    /// the record back would re-probe machines with nothing left to learn
    /// on every boot, through the SuperIO walk the record exists to avoid.
    /// The alternate winning is different: it wins only over a standard
    /// layout that did not pass, so its record may stand on a standard
    /// rejection but never on a standard layout that merely could not be
    /// examined. On a machine having a bad moment, that unexamined layout
    /// may be the right one, and freezing the rival in would strand it
    /// permanently. That one case is driven for the session and left
    /// unrecorded, which costs one more probe at the next start.
    pub fn worth_recording(&self) -> bool {
        self.validated()
            && (self.layout() == Layout::Standard
                || self.probes.iter().all(|p| p.definitive()))
    }

    /// The whole selection on one line, for a log entry. Names the winner and
    /// what both layouts answered, which is the difference between a bug
    /// report that can be acted on and one that starts an interrogation.
    ///
    /// A fallback says so in as many words. This string is what a bug report
    /// carries, and "driving the standard mode" would read as a verdict on
    /// hardware that was never actually vouched for. A configured layout says
    /// so too, for the same reason: nothing probed anything this boot.
    pub fn selection_summary(&self) -> String {
        if self.configured {
            return format!(
                "driving the {} because the settings say so, without probing",
                self.layout().describe()
            );
        }

        let mut summary = if !self.validated() {
            format!(
                "no layout validated at startup; falling back to the {} and \
                 leaving the engine to retry",
                self.layout().describe()
            )
        } else if self.worth_recording() {
            format!("driving the {}", self.layout().describe())
        } else {
            // Only one case lands here: the alternate layout won while the
            // standard one could not be examined, and the standard layout
            // would have taken precedence had it passed. Driven, and
            // deliberately not written down so the next start stays free to
            // look again. Said in as many words, because this line is what
            // a bug report from that next start will carry.
            format!(
                "driving the {} for this session without recording it, \
                 because the {} could not be examined and would have taken \
                 precedence had it passed; the next start probes again",
                self.layout().describe(),
                Layout::Standard.describe()
            )
        };

        for probe in &self.probes {
            summary.push_str(". ");
            summary.push_str(&probe.to_string());
        }

        summary
    }

    /// Probes this layout with the persistence the running engine has, and
    /// returns the last word: the first definitive answer, or the final
    /// transient failure once the budget is spent.
    ///
    /// One look was not a fair chance. Back-to-back service restarts on a
    /// measured machine rejected a working standard layout three starts out
    /// of four, on transient failures from the previous instance still
    /// letting go of the controller. The tick path retries a failed pass
    /// for exactly this reason, and TPFanControl gives the controller ten
    /// tries before believing a read failed. Only transient failures are
    /// retried here: a rejection is evidence about the machine, and asking
    /// again would only wear the same answer out of it.
    ///
    /// The attempts share one SAMPLE_BUDGET rather than getting one each,
    /// and the envelope opens before the lock is first tried, so the
    /// persistence never buys a meaningfully later start than one slow
    /// probe already could.
    fn probe_persistently(&self) -> Probe {
        let until = Instant::now() + SAMPLE_BUDGET;

        loop {
            let probe = match self.probe(until) {
                Ok(probe) => probe,
                // The lock was busy: something is alive and holding the
                // controller, which at logon is routine, not a diagnosis.
                // Worth waiting out, never worth concluding from.
                Err(why) => Probe::unexamined(self.io.layout(), why.to_string()),
            };

            if probe.definitive() || Instant::now() + PROBE_RETRY_PAUSE >= until {
                return probe;
            }

            std::thread::sleep(PROBE_RETRY_PAUSE);
        }
    }

    /// One locked look at whether a ThinkPad EC answers at this layout's
    /// ports, and the evidence either way. `until` bounds the pass; it is
    /// shared with the caller's retries so the whole examination of a
    /// layout stays inside one budget.
    ///
    /// `Err` is only for a probe that could not run, which today means the
    /// lock: contention says something is alive and holding the controller,
    /// not that the layout is wrong, and concluding anything from it would
    /// misdiagnose a machine where Vantage happened to be mid-transaction.
    ///
    /// The handshake itself is the first check and not a formality. It
    /// cannot complete against a floating port: OBF has to rise in answer to
    /// the read command, which takes an EC, and a port stuck at 0x00 or 0xff
    /// times out in settle or drains forever in begin_transaction. A timeout
    /// there keeps the layout from passing, but it is never a rejection: the
    /// same silence comes from a real EC with another tool mid-transaction
    /// on it, so all it proves is that nothing was learned. The value checks
    /// on top are for windows that are decoded but empty, which return
    /// constants that a status-bit wait can sail straight through.
    fn probe(&self, until: Instant) -> Result<Probe, Error> {
        let _guard = self.lock.lock()?;
        let layout = self.io.layout();

        // LpcIO refuses every port until a slot is selected and its BARs
        // found; doing it under the same guard makes the whole probe one
        // bracketed transaction. Failure here is a rejection, not a
        // transient: it is the module's own discovery reporting what it
        // found, a machine with no SuperIO in slot 1 for instance. Calling
        // it definitive can never record a wrong layout, because only a
        // layout that passed its own probe is ever recorded, and the
        // standard layout has nothing to prepare and so nothing to fail.
        if let Err(e) = self.ensure_prepared() {
            return Ok(Probe::rejected(layout, format!("could not be set up: {e}")));
        }

        let fan_ctrl = match self.read_register(REG_FAN_CTRL) {
            Ok(v) => v,
            Err(e) => return Ok(Probe::unexamined(layout, format!("no handshake: {e}"))),
        };

        let mut temps = [0u8; PROBE_TEMPS];
        for (i, slot) in temps.iter_mut().enumerate() {
            if Instant::now() >= until {
                return Ok(Probe {
                    layout,
                    fan_ctrl: Some(fan_ctrl),
                    temps: None,
                    failure: Some(ProbeFailure::Unexamined(
                        "probe ran out of budget".to_string(),
                    )),
                });
            }

            match self.read_register(REG_TEMP_BANK0 + i as u8) {
                Ok(v) => *slot = v,
                Err(e) => {
                    return Ok(Probe {
                        layout,
                        fan_ctrl: Some(fan_ctrl),
                        temps: None,
                        failure: Some(ProbeFailure::Unexamined(format!(
                            "sensors unreadable: {e}"
                        ))),
                    });
                }
            }
        }

        let mut failure = if !plausible_fan_ctrl(fan_ctrl) {
            Some(ProbeFailure::Rejected(format!(
                "fan register {fan_ctrl:#04x} is not a fan state"
            )))
        } else if !plausible_temps(&temps) {
            Some(ProbeFailure::Rejected(
                "bank 0 does not read like temperature sensors".to_string(),
            ))
        } else {
            None
        };

        // Reading proves the transport. It does not prove the fan register is
        // writable, and a layout that can be read but not driven is worse
        // than useless: chosen, it would leave the engine believing it has
        // control it does not have. So the last check is a write that has to
        // hold. Tried only once everything above has passed, so no byte is
        // ever written through a window that has not already read like a
        // ThinkPad.
        if failure.is_none() {
            failure = if Instant::now() >= until {
                Some(ProbeFailure::Unexamined("probe ran out of budget".to_string()))
            } else {
                self.prove_fan_control(until)
            };
        }

        Ok(Probe { layout, fan_ctrl: Some(fan_ctrl), temps: Some(temps), failure })
    }

    /// Writes the firmware handoff to the fan register and requires it to
    /// hold. `None` means it did; `Some` carries why it did not, and whether
    /// that is a verdict on the machine or only on the moment.
    ///
    /// The test value is 0x80 and nothing else. It is the safe resting
    /// state, it is where the engine wants the machine at startup anyway,
    /// and unlike any manual level it cannot leave a fan pinned if the probe
    /// is interrupted between the write and the read. The original value is
    /// deliberately not restored afterwards: writing a manual level back
    /// would recreate exactly the hazard the choice of 0x80 avoids.
    ///
    /// A write that lands but does not hold is retried with a settling
    /// pause, the same discipline set_fan applies before it calls a write
    /// declined, because the controller can take a moment to accept a
    /// change. Only after that fair chance is the refusal a rejection. A
    /// write or read that fails outright is the transport failing, which
    /// says nothing about the machine, so it comes back transient for the
    /// caller to retry.
    ///
    /// Every way out leaves the controller in a state something is managing.
    /// A write that fails mid-handshake never lands, so the register keeps
    /// whatever it held; a write that lands puts the firmware in charge; and
    /// a write that lands but does not hold means whatever held the register
    /// before still does.
    fn prove_fan_control(&self, until: Instant) -> Option<ProbeFailure> {
        let mut read_back = 0u8;

        for attempt in 0..5 {
            // Settling time between attempts, matching set_fan: the
            // controller can take a moment to accept a change, particularly
            // when coming out of firmware control.
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(100));
            }

            if let Err(e) = self.write_register(REG_FAN_CTRL, FAN_BIOS) {
                return Some(ProbeFailure::Unexamined(format!(
                    "fan register not writable: {e}"
                )));
            }

            match self.read_register(REG_FAN_CTRL) {
                // Judged like every other look at this register: the firmware
                // flag on and the disengage bit off, reserved bits ignored.
                Ok(v) if v & FAN_BIOS != 0 && v & FAN_DISENGAGED == 0 => return None,
                Ok(v) => read_back = v,
                Err(e) => {
                    return Some(ProbeFailure::Unexamined(format!(
                        "fan register unreadable after the handoff: {e}"
                    )));
                }
            }

            if Instant::now() >= until {
                break;
            }
        }

        Some(ProbeFailure::Rejected(format!(
            "fan register did not hold a write: wrote {FAN_BIOS:#04x}, read back {read_back:#04x}"
        )))
    }

    /// Runs the module's own setup once, before the first transaction.
    ///
    /// LpcIO refuses every port until a slot is selected and its BARs found;
    /// on the standard layout there is nothing to do and the first call is a
    /// cheap no-op never repeated. Lazy rather than done at open, because the
    /// configured path opens without probing and the EC lock can be busy at
    /// boot: a handle that could only prepare at open would stay dead until
    /// the next service restart, while this one heals on the first pass that
    /// gets the lock.
    ///
    /// The caller must hold the EC lock, which on the alternate layout
    /// includes the ISA mutex the module documents for these calls.
    fn ensure_prepared(&self) -> Result<(), Error> {
        if self.prepared.load(Ordering::SeqCst) {
            return Ok(());
        }

        self.io.prepare()?;
        self.prepared.store(true, Ordering::SeqCst);

        Ok(())
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
        self.ensure_prepared()?;
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
        self.ensure_prepared()?;
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
        // Best effort, unlike everywhere else: this is the handback, and a
        // handle that never got to prepare should still try the writes and
        // report their failure honestly rather than refuse to attempt them.
        let _ = self.ensure_prepared();

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
        /// Port reads that fail at the IOCTL before the controller starts
        /// answering, counting down. What the first moments after a service
        /// restart look like while the previous instance is still letting
        /// go of the controller.
        failing_reads: usize,
        /// Fan register writes the controller drops before it starts
        /// holding them, counting down. Real controllers do take a moment
        /// sometimes, which is why set_fan retries, and the probe's write
        /// test owes the same patience.
        dropped_ctrl_writes: usize,
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
                    failing_reads: 0,
                    dropped_ctrl_writes: 0,
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

        /// The next `count` port reads fail at the IOCTL, then the
        /// controller answers normally.
        fn fail_next_reads(&self, count: usize) {
            self.state.lock().unwrap().failing_reads = count;
        }

        /// The next `count` fan register writes are taken and dropped, then
        /// writes hold normally.
        fn drop_ctrl_writes(&self, count: usize) {
            self.state.lock().unwrap().dropped_ctrl_writes = count;
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

            // A staged transport failure comes first: the IOCTL itself
            // errors, before any question of what the controller would say.
            if state.failing_reads > 0 {
                state.failing_reads -= 1;
                return Err(Error::Call { function: "read_result", code: 0 });
            }

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
            // A transient drop: the bytes were all taken, the value did not
            // stick, and the next attempt will do better.
            REG_FAN_CTRL if state.dropped_ctrl_writes > 0 => state.dropped_ctrl_writes -= 1,
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
            configured: false,
            prepared: AtomicBool::new(false),
        }
    }

    fn fake(ec: &Ec) -> &FakeEc {
        match &ec.io {
            Io::Fake(fake) => fake,
            Io::Pawn(_) => unreachable!("test controllers are always fakes"),
        }
    }

    /// One probe attempt with a fresh budget, which is what most tests
    /// want: the retry loop is exercised where retrying is the point, and
    /// nowhere else, so a failure a test stages is seen on the first look.
    fn probe_once(ec: &Ec) -> Result<Probe, Error> {
        ec.probe(Instant::now() + SAMPLE_BUDGET)
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
            let probe = probe_once(&ec).expect("nothing contends in a test");

            assert!(probe.passed(), "{probe}");
            assert_eq!(probe.fan_ctrl, Some(FAN_BIOS));
        }
    }

    #[test]
    fn a_probe_outlasts_the_transient_failures_a_restart_leaves_behind() {
        // The measured defect: four back-to-back service restarts, and three
        // of them rejected the standard layout on IOCTL failures from the
        // previous instance still letting go of the controller. One look is
        // not a fair chance. The tick path already retries a failed pass;
        // the probe extends the same patience before it believes anything.
        let flaky = FakeEc::thinkpad(Layout::Standard);
        flaky.fail_next_reads(1);

        let ec = ec_with(flaky);
        let single = probe_once(&ec).expect("nothing contends in a test");
        assert!(!single.passed(), "the staged failure must reach a single attempt");
        assert!(!single.definitive(), "an IOCTL failure is not evidence about the machine");

        let flaky = FakeEc::thinkpad(Layout::Standard);
        flaky.fail_next_reads(2);

        let ec = ec_with(flaky);
        let probe = ec.probe_persistently();
        assert!(probe.passed(), "retries must turn a transient failure into a pass: {probe}");
    }

    #[test]
    fn a_controller_slow_to_take_the_handoff_is_not_called_unwritable() {
        // set_fan retries a write that did not hold, because the controller
        // can take a moment to accept a change. The probe's write test
        // judges the machine on the same action and owes it the same
        // patience: a fan register that holds the value on a later attempt
        // is a working EC, not a refusal to record against.
        let controller = FakeEc::thinkpad(Layout::Standard);
        controller.hold_levels([0x03, 0x03]);
        controller.drop_ctrl_writes(2);

        let ec = ec_with(controller);
        let probe = probe_once(&ec).expect("nothing contends in a test");

        assert!(probe.passed(), "{probe}");
        assert_eq!(fake(&ec).fan_ctrl()[0], FAN_BIOS, "the handoff must have landed");
    }

    /// A probe that passed, for staging selection evidence by hand.
    fn passing(layout: Layout) -> Probe {
        Probe { layout, fan_ctrl: Some(FAN_BIOS), temps: None, failure: None }
    }

    // The recording table, one test per row. The question the record has to
    // survive is whether the layout that was not examined could have beaten
    // the winner, and the four rows are the four answers.

    #[test]
    fn a_standard_win_is_recorded_whatever_became_of_the_alternate() {
        // Row one, the common machine. Ties go to the standard layout, so
        // nothing the alternate could have said would have changed this
        // outcome: even a pass would have lost. Holding the record back
        // here would re-probe the most ordinary machines on every boot,
        // through the SuperIO walk the record exists to avoid.
        let mut ec = ec_with(FakeEc::thinkpad(Layout::Standard));
        ec.probes = vec![
            passing(Layout::Standard),
            Probe::unexamined(Layout::Alternate, "no handshake: test".to_string()),
        ];

        assert!(ec.worth_recording(), "an unexamined alternate cannot outrank a standard pass");

        let summary = ec.selection_summary();
        assert!(summary.starts_with("driving the"), "{summary}");
        assert!(!summary.contains("without recording"), "{summary}");

        // A definitive alternate changes nothing about the verdict, whether
        // it was rejected or even passed and lost the tie.
        ec.probes = vec![passing(Layout::Standard), passing(Layout::Alternate)];
        assert!(ec.worth_recording(), "a tie the standard layout won is a full verdict");
    }

    #[test]
    fn a_compat_win_over_a_rejected_standard_is_recorded() {
        // Row two, the P53 class. The standard layout answered and is not
        // this machine's controller; that is evidence about the machine,
        // and the record may stand on it so no later boot asks again.
        let mut ec = ec_with(FakeEc::thinkpad(Layout::Alternate));
        ec.probes = vec![
            Probe::rejected(Layout::Standard, "fan register 0xff is not a fan state".to_string()),
            passing(Layout::Alternate),
        ];

        assert!(ec.worth_recording(), "a standard rejection is evidence to record on");
        assert!(!ec.selection_summary().contains("without recording"));
    }

    #[test]
    fn a_compat_win_over_an_unexamined_standard_is_driven_but_not_recorded() {
        // Row three, the only unrecorded win, and the case the whole rule
        // guards: the standard layout would have taken precedence had it
        // passed, and nobody got to ask it. Recording compatibility mode
        // here is exactly how one unlucky boot used to strand a standard
        // machine permanently. The winner still drives this session; the
        // record stays unset so the next start looks again.
        let mut ec = ec_with(FakeEc::thinkpad(Layout::Alternate));
        ec.probes = vec![
            Probe::unexamined(Layout::Standard, "lock busy".to_string()),
            passing(Layout::Alternate),
        ];

        assert!(ec.validated(), "the winner still drives this session");
        assert!(!ec.worth_recording(), "the loser might have won; nothing may be recorded");

        // And the summary says which happened, in as many words, because
        // this line is what the next bug report carries.
        let summary = ec.selection_summary();
        assert!(summary.contains("without recording"), "{summary}");
        assert!(summary.contains("could not be examined"), "{summary}");
        assert!(!summary.contains("falling back"), "{summary}");
    }

    #[test]
    fn a_boot_where_nothing_passed_records_nothing() {
        // Row four, unchanged from before the rule: the fallback is nothing
        // vouched for, whatever mix of rejection and silence produced it.
        let mut ec = ec_with(FakeEc::thinkpad(Layout::Standard));
        ec.probes = vec![
            Probe::unexamined(Layout::Standard, "lock busy".to_string()),
            Probe::rejected(
                Layout::Alternate,
                "bank 0 does not read like temperature sensors".to_string(),
            ),
        ];

        assert!(!ec.validated());
        assert!(!ec.worth_recording(), "a fallback nothing vouched for is never recorded");
        assert!(ec.selection_summary().contains("falling back"));
    }

    #[test]
    fn a_probe_rejects_a_controller_it_can_read_but_not_drive() {
        // The gap the write test closes: a window can complete handshakes
        // and serve plausible values while quietly dropping writes, and a
        // layout picked on reads alone would leave the engine believing it
        // has control it does not have. Held at a manual level first, so a
        // dropped 0x80 is distinguishable from a register that already read
        // as firmware controlled.
        for layout in [Layout::Standard, Layout::Alternate] {
            let controller = FakeEc::thinkpad(layout);
            controller.hold_levels([0x03, 0x03]);
            controller.decline_fan(0);

            let ec = ec_with(controller);
            let probe = probe_once(&ec).expect("nothing contends in a test");

            assert!(!probe.passed(), "a read-only fan register was accepted: {probe}");
            // A refusal that outlasted the write test's own retries is a
            // verdict on the machine, the kind a layout record may rest on.
            assert!(probe.definitive(), "a persistent refusal must be a rejection: {probe}");
        }
    }

    #[test]
    fn a_passing_probe_leaves_the_firmware_in_charge() {
        // The write test's value is the firmware handoff, chosen because it
        // is the state that is safe to leave behind: interrupted after the
        // write, rejected after the read, or passed outright, the register
        // must never end at a manual level the probe set. The original level
        // still travels in the evidence.
        let controller = FakeEc::thinkpad(Layout::Standard);
        controller.hold_levels([0x07, 0x07]);

        let ec = ec_with(controller);
        let probe = probe_once(&ec).expect("nothing contends in a test");

        assert!(probe.passed(), "{probe}");
        assert_eq!(probe.fan_ctrl, Some(0x07), "the evidence must keep the original value");
        assert_eq!(fake(&ec).fan_ctrl()[0], FAN_BIOS, "the probe must not leave a level set");
    }

    #[test]
    fn a_probe_writes_nothing_through_a_window_that_failed_the_read_checks() {
        // The write test runs last on purpose: a window that already failed
        // plausibility gets no byte written into it, so probing the layout a
        // machine does not use stays read only there.
        let controller = FakeEc::thinkpad(Layout::Alternate);
        controller.state.lock().unwrap().temps = [0x30; PROBE_TEMPS];

        let ec = ec_with(controller);
        let probe = probe_once(&ec).expect("nothing contends in a test");

        assert!(!probe.passed(), "constant sensors must still be rejected");
        assert!(
            fake(&ec).writes().is_empty(),
            "a rejected window took a write: {:?}",
            fake(&ec).writes()
        );
    }

    #[test]
    fn a_configured_handle_claims_no_verdict_it_never_reached() {
        // The configured path probes nothing, so its summary must say the
        // layout came from the settings rather than reading like a verdict
        // on hardware, and validated() must not vouch for it: that answer
        // decides whether a probe result is written into the settings, and a
        // configured boot has none to write.
        let mut ec = ec_with(FakeEc::thinkpad(Layout::Alternate));
        ec.configured = true;

        assert!(!ec.validated());

        let summary = ec.selection_summary();
        assert!(summary.contains("settings"), "{summary}");
        assert!(summary.contains("without probing"), "{summary}");
        assert!(!summary.contains("falling back"), "{summary}");
    }

    #[test]
    fn a_decoded_but_empty_window_never_passes_and_is_never_evidence() {
        // The measured machine: an EC at the standard ports, and an alternate
        // window that also decodes, reading 0x00 at the status port. "IBF
        // clear, OBF clear, ready" is exactly what a naive check sees there.
        // The handshake cannot complete against it, because OBF never rises
        // in answer to the read command, so the window can never pass. It is
        // also never a rejection: the same silence comes from a real EC with
        // another tool mid-transaction on it, so the honest verdict is that
        // the window could not be examined, and the layout record stays
        // unset for the next start to try again.
        let ec = ec_with(FakeEc::dead_window(Layout::Alternate));
        let probe = probe_once(&ec).expect("nothing contends in a test");

        assert!(!probe.passed());
        assert!(!probe.definitive(), "a timeout must not read as a verdict on the machine");
        assert!(probe.fan_ctrl.is_none(), "no handshake means no value to trust");
    }

    #[test]
    fn the_alternate_path_prepares_itself_before_any_traffic() {
        // LpcIO answers STATUS_DEVICE_NOT_READY to any port access before
        // slot select and BAR discovery, in that order. The configured path
        // opens without probing, so nothing has prepared the handle by the
        // time the first pass runs; the pass has to do it itself, under the
        // lock, or a remembered alternate layout would be dead on every
        // boot.
        let ec = ec_with(FakeEc::unprepared(Layout::Alternate));
        assert!(ec.sample().is_ok(), "the first pass must prepare the module");

        let probe = probe_once(&ec).expect("nothing contends in a test");
        assert!(probe.passed(), "{probe}");
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

        // A layout that could not be examined is not evidence either way:
        // it neither wins nor blocks the layout that answered.
        let missing = |layout| Probe::unexamined(layout, "lock busy".to_string());
        assert_eq!(
            chosen_layout(&[missing(Layout::Standard), pass(Layout::Alternate)]),
            Some(Layout::Alternate)
        );
        assert_eq!(
            chosen_layout(&[missing(Layout::Standard), missing(Layout::Alternate)]),
            None
        );
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
