// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! On-disk settings.
//!
//! The file is an implementation detail. Everything here is reachable from the
//! settings window; nobody should need to open it in a text editor.
//!
//! It lives in ProgramData because the engine usually runs as a service under
//! SYSTEM while the window runs as whoever logged in, and both need to read it.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::curve::{Curve, CurveError, CurvePoint};
use crate::engine::Mode;

/// Bumped whenever the shape changes in a way that needs migrating.
pub const SCHEMA_VERSION: u32 = 3;

const DIR_NAME: &str = "Yamato";
const FILE_NAME: &str = "config.json";

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    Parse(serde_json::Error),
    /// A stored curve that no longer satisfies the rules. Reported, not
    /// repaired: quietly changing someone's fan curve is a surprise this
    /// program should not spring.
    Curve { profile: String, source: CurveError },
    /// Written by a newer Yamato than this one.
    FromTheFuture { found: u32, supported: u32 },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "could not read or write the settings file: {e}"),
            ConfigError::Parse(e) => write!(f, "the settings file is not valid JSON: {e}"),
            ConfigError::Curve { profile, source } => {
                write!(f, "profile \"{profile}\" has an invalid curve: {source}")
            }
            ConfigError::FromTheFuture { found, supported } => write!(
                f,
                "the settings file is version {found}, but this build only understands {supported}. \
                 Install a newer Yamato, or move the file aside to start fresh."
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<io::Error> for ConfigError {
    fn from(e: io::Error) -> Self {
        ConfigError::Io(e)
    }
}

/// A curve point as stored. Kept separate from [`CurvePoint`] so the on-disk
/// shape can stay stable while the runtime type changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPoint {
    pub temp: i8,
    pub level: u8,
    #[serde(default)]
    pub hyst_up: i8,
    #[serde(default = "default_hyst_down")]
    pub hyst_down: i8,
}

fn default_hyst_down() -> i8 {
    4
}

impl From<&CurvePoint> for StoredPoint {
    fn from(p: &CurvePoint) -> Self {
        StoredPoint { temp: p.temp, level: p.level, hyst_up: p.hyst_up, hyst_down: p.hyst_down }
    }
}

impl From<&StoredPoint> for CurvePoint {
    fn from(p: &StoredPoint) -> Self {
        CurvePoint { temp: p.temp, level: p.level, hyst_up: p.hyst_up, hyst_down: p.hyst_down }
    }
}

/// A named curve the user can switch between from the tray.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub points: Vec<StoredPoint>,
}

impl Profile {
    pub fn new(name: impl Into<String>, curve: &Curve) -> Self {
        Profile {
            name: name.into(),
            points: curve.points().iter().map(StoredPoint::from).collect(),
        }
    }

    pub fn to_curve(&self) -> Result<Curve, ConfigError> {
        Curve::new(self.points.iter().map(CurvePoint::from).collect()).map_err(|source| {
            ConfigError::Curve { profile: self.name.clone(), source }
        })
    }
}

/// Which mode to be in when the engine starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StartupMode {
    Bios,
    Smart,
}

/// Where this machine keeps its embedded controller, as stored on disk.
///
/// Mirrors `yamato_ec::Layout`, and is a separate type for the same reason
/// [`StoredPoint`] is separate from `CurvePoint`: the on-disk shape has to
/// stay stable whatever the runtime type does. The settings window calls the
/// alternate layout "Compatibility mode", and so does everything else a
/// person reads; `compat` is only its spelling in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EcLayout {
    /// The ACPI-specified ports, 0x62/0x66, via LpcACPIEC.
    Standard,
    /// Compatibility mode: the 0x1600/0x1604 window, via LpcIO, for the
    /// machines that answer nothing at the standard ports.
    Compat,
}

impl From<EcLayout> for yamato_ec::Layout {
    fn from(l: EcLayout) -> Self {
        match l {
            EcLayout::Standard => yamato_ec::Layout::Standard,
            EcLayout::Compat => yamato_ec::Layout::Alternate,
        }
    }
}

impl From<yamato_ec::Layout> for EcLayout {
    fn from(l: yamato_ec::Layout) -> Self {
        match l {
            yamato_ec::Layout::Standard => EcLayout::Standard,
            yamato_ec::Layout::Alternate => EcLayout::Compat,
        }
    }
}

impl From<StartupMode> for Mode {
    fn from(m: StartupMode) -> Self {
        match m {
            StartupMode::Bios => Mode::Bios,
            StartupMode::Smart => Mode::Smart,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    /// Seconds between decisions.
    pub poll_secs: u32,
    /// Hand the fan back if no decision happens for this long.
    pub watchdog_secs: u32,
    pub startup_mode: StartupMode,
    pub active_profile: String,
    pub profiles: Vec<Profile>,
    /// Sensors excluded from the "hottest" decision. Some machines have a
    /// sensor that idles hot by design and would otherwise drive the fan.
    #[serde(default)]
    pub ignored_sensors: Vec<usize>,
    /// Show the settings window on launch instead of going straight to tray.
    #[serde(default)]
    pub show_window_on_start: bool,
    /// Seconds between decisions once the machine is believed asleep.
    ///
    /// Services keep running through Modern Standby, so the passes still
    /// happen. The cost is battery: polling every few seconds wakes a CPU
    /// trying to stay parked, and a sleeping machine's temperature is not
    /// moving fast. Not used while the screen is merely off, because that is
    /// where the working case is decided and it should be decided quickly.
    #[serde(default = "default_standby_poll")]
    pub standby_poll_secs: u32,
    /// Show temperatures in Fahrenheit.
    ///
    /// Display only. Curves are stored and evaluated in Celsius because that is
    /// what the controller reports, and converting on the way in and out would
    /// round twice and drift.
    #[serde(default)]
    pub fahrenheit: bool,
    /// Keep a CSV history of what was read and what was asked for.
    ///
    /// Off unless asked for. A program resident all day should not write to
    /// the disk every few seconds, and nobody needs this until they are trying
    /// to explain a fan that surprised them.
    #[serde(default)]
    pub log_enabled: bool,
    /// How large that history may grow, in megabytes, before it is rotated.
    #[serde(default = "default_log_max_mb")]
    pub log_max_mb: u32,
    /// The temperature at which a held manual level is abandoned.
    ///
    /// The reference implementation calls this ManModeExit and people tune it,
    /// so it is a setting here too. It is not optional: manual mode switches
    /// the firmware's own management off, and this is all that pulls the fan
    /// off a fixed level when the machine gets hot with nobody watching. Held
    /// to a range where it still means something.
    #[serde(default = "default_manual_escape")]
    pub manual_escape_c: i8,
    /// Draw the temperature into the tray icon beside the dot.
    ///
    /// On by default: it is why a lot of people keep one of these in the tray.
    /// The switch is for people who prefer the plain dot, or whose machine
    /// draws it badly.
    #[serde(default = "default_tray_numbers")]
    pub tray_numbers: bool,

    /// Which sensor the tray reports, or the hottest one when unset.
    ///
    /// Only the tray reads this. The fan is still driven by the hottest
    /// sensor, because a curve following one nominated sensor would sit still
    /// while a different part of the machine cooked.
    #[serde(default)]
    pub tray_sensor: Option<u8>,

    /// This machine has one fan, so the second fan selector is never written,
    /// verified or read.
    ///
    /// Some single-fan ThinkPads answer the second selector with a value that
    /// does not track what was written. On those machines every fan write
    /// looks declined and every handback looks failed, so the engine keeps
    /// surrendering to the firmware and reporting a fan that may be held.
    ///
    /// A setting, not a guess. The only passive signal is fan 2 reporting no
    /// speed, and a real second fan reads zero whenever it is stopped, which
    /// at idle is most of the time. Guessing "one fan" from that would switch
    /// off write verification on a machine that has two, and a second fan
    /// holding a manual level with the firmware disabled is the silent failure
    /// verification exists to catch. Wrong at false, it costs a scary message
    /// with a hint attached; wrong the other way, an unmanaged fan.
    #[serde(default)]
    pub single_fan: bool,

    /// Where this machine keeps its embedded controller.
    ///
    /// `None` means not yet determined, which only a fresh install is: the
    /// engine probes both layouts exactly once, at the first start that finds
    /// this unset, and writes the winner here. Every start after that loads
    /// the recorded layout and probes nothing, because probing is a hardware
    /// interaction, on the alternate module a walk of the SuperIO
    /// configuration space, and it has no business happening on every boot of
    /// a machine that answered the question at its first one.
    ///
    /// Also the override. The settings window's Controller mode row writes a
    /// value straight into this field, and the engine drives whatever is
    /// here without validating it away: someone overriding a detection is
    /// owed the override, and a wrong one reports the controller unreachable,
    /// which is the honest outcome. A default is deliberately not supplied,
    /// since defaulting to Standard would mean a fresh install never probes.
    #[serde(default)]
    pub ec_layout: Option<EcLayout>,
}

fn default_standby_poll() -> u32 {
    30
}

fn default_log_max_mb() -> u32 {
    8
}

fn default_manual_escape() -> i8 {
    crate::engine::MANUAL_ESCAPE_C
}

fn default_tray_numbers() -> bool {
    true
}

/// The bounds [`Config::validate`] holds these fields to.
///
/// Public because the tray and the settings window clamp against them before
/// writing, so what is saved is what comes back. A value the loader changed
/// underneath them would be a control that lies about what it did.
pub const POLL_SECS_MIN: u32 = 1;
pub const POLL_SECS_MAX: u32 = 60;
pub const STANDBY_POLL_SECS_MIN: u32 = 5;
pub const STANDBY_POLL_SECS_MAX: u32 = 120;
pub const LOG_MAX_MB_MIN: u32 = 1;
pub const LOG_MAX_MB_MAX: u32 = 128;

/// Bounds on the temperature that ends a held manual level.
///
/// Narrow on purpose, and with no way to switch it off. Below the floor it
/// would fire during ordinary work and manual mode would be unusable; above the
/// ceiling it stops being a safety net and becomes a formality. A file that
/// asks for something outside this gets the nearest end of it.
pub const MANUAL_ESCAPE_MIN: i8 = 60;
pub const MANUAL_ESCAPE_MAX: i8 = 90;

/// Bounds on per-point hysteresis. The two are not the same on purpose.
///
/// `hyst_down` is how far the machine must cool before the curve steps down. A
/// large one holds the fan higher for longer: loud, but never hot, so it has
/// room.
///
/// `hyst_up` is how far *above* a threshold the machine must get before the
/// curve steps up. A large one delays the fan on a machine that is already
/// heating, which is the dangerous direction, so it gets a few degrees of
/// damping and no more.
pub const HYST_DOWN_MAX: i8 = 15;
pub const HYST_UP_MAX: i8 = 5;

/// The shortest watchdog that will not fire during ordinary running.
///
/// It has to outlast the slowest poll, not the fastest, or a single slow tick
/// in standby reads as a stall and hands the fan back for good. Saturates
/// instead of wrapping: this is reachable from a hand-edited file, and a
/// watchdog that wrapped to nothing would fire on the first tick.
pub fn watchdog_floor(poll_secs: u32, standby_poll_secs: u32) -> u32 {
    poll_secs.max(standby_poll_secs).saturating_mul(3)
}

/// A temperature for display, in whichever unit is configured.
///
/// Lives here, not in the window, so everything showing a temperature shares
/// one conversion.
pub fn display_temp(celsius: i8, fahrenheit: bool) -> i32 {
    if fahrenheit {
        (celsius as i32 * 9) / 5 + 32
    } else {
        celsius as i32
    }
}

/// The suffix to draw next to it.
pub fn unit_suffix(fahrenheit: bool) -> &'static str {
    if fahrenheit {
        "\u{00b0}F"
    } else {
        "\u{00b0}C"
    }
}

/// The profiles a fresh install starts with, which it will not let you delete.
///
/// Three, not one. A picker holding a single entry looks the same as a picker
/// holding nothing, and the first person to go looking for profiles here
/// concluded, reasonably, that there were none.
///
/// Balanced is the curve this program has always shipped and is still the
/// active one. The other two are that shape moved: Quiet gives up response for
/// silence under load, Performance does the reverse and hands to the firmware
/// earlier, where firmware reacts faster than any polling loop.
///
/// Quiet and Balanced both idle the fan off, and Quiet holds it off longer.
/// Level 0 leaves no airflow with the firmware's management off, but a curve
/// moves off it as the machine warms, which is why a curve may use it and a
/// manual level may not. Below its first step a curve holds that step's level,
/// so a Quiet profile starting at level 1 would run the fan on an idle machine
/// and be louder at rest than Balanced.
///
/// Renaming or editing them is fine; losing them is not. They are the
/// reference points for judging a curve you made yourself, and getting one
/// back means rebuilding it point by point.
pub const BUILT_IN_PROFILES: [&str; 3] = ["Quiet", "Balanced", "Performance"];

/// Whether a name is one of the built-in curves.
pub fn is_built_in(name: &str) -> bool {
    BUILT_IN_PROFILES.iter().any(|b| *b == name)
}

fn default_profiles() -> Vec<Profile> {
    let quiet = Curve::new(vec![
        CurvePoint::new(50, 0).with_hysteresis(0, 3),
        CurvePoint::new(58, 1).with_hysteresis(0, 5),
        CurvePoint::new(66, 2).with_hysteresis(0, 6),
        CurvePoint::new(74, 3).with_hysteresis(0, 6),
        CurvePoint::new(81, 4).with_hysteresis(0, 6),
        CurvePoint::new(88, 5).with_hysteresis(0, 6),
        CurvePoint::new(93, yamato_ec::FAN_BIOS).with_hysteresis(0, 7),
    ])
    .expect("the built-in quiet curve is valid");

    let performance = Curve::new(vec![
        CurvePoint::new(40, 1).with_hysteresis(0, 4),
        CurvePoint::new(48, 2).with_hysteresis(0, 4),
        CurvePoint::new(55, 3).with_hysteresis(0, 4),
        CurvePoint::new(62, 4).with_hysteresis(0, 5),
        CurvePoint::new(70, 5).with_hysteresis(0, 5),
        CurvePoint::new(78, 6).with_hysteresis(0, 5),
        CurvePoint::new(84, 7).with_hysteresis(0, 5),
        CurvePoint::new(88, yamato_ec::FAN_BIOS).with_hysteresis(0, 6),
    ])
    .expect("the built-in performance curve is valid");

    vec![
        Profile::new("Quiet", &quiet),
        Profile::new("Balanced", &Curve::default()),
        Profile::new("Performance", &performance),
    ]
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: SCHEMA_VERSION,
            poll_secs: 5,
            watchdog_secs: 30,
            startup_mode: StartupMode::Smart,
            active_profile: "Balanced".to_string(),
            profiles: default_profiles(),
            ignored_sensors: Vec::new(),
            show_window_on_start: false,
            standby_poll_secs: default_standby_poll(),
            fahrenheit: false,
            log_enabled: false,
            log_max_mb: default_log_max_mb(),
            manual_escape_c: default_manual_escape(),
            tray_numbers: default_tray_numbers(),
            tray_sensor: None,
            single_fan: false,
            ec_layout: None,
        }
    }
}

/// Turns a stored profile of fewer than two points into one that loads.
///
/// Only ever called from the migration, which is where the reasoning for
/// repairing this and nothing else lives.
fn repair_a_curve_too_short_to_be_one(profile: &mut Profile) {
    match profile.points.len() {
        // Nothing stored at all. There is no level to keep and nothing to
        // extend, so the profile is not saying anything that could be
        // preserved; it gets the baseline curve, which is the same answer
        // validate already gives a file that has no profiles in it. Reachable
        // from a hand-edited file, and from one whose points array was written
        // empty by something other than this program.
        0 => {
            profile.points = Curve::default().points().iter().map(StoredPoint::from).collect();
        }
        1 => {
            // No room for a hotter step above the very top of the range. A lone
            // point's own temperature decides nothing, because below its first
            // step a curve holds that step's level, so its level applied at
            // every temperature wherever the point happened to sit. Moving it
            // down one degree therefore costs nothing and keeps the level,
            // which is the part the user chose.
            if profile.points[0].temp == i8::MAX {
                profile.points[0].temp -= 1;
            }

            // The handoff goes at the ceiling, which is where the engine takes
            // a curve's fan back whatever the curve says. That is the deliberate
            // choice: at and above the ceiling the engine was already handing
            // over, and below it the stored level still applies at every
            // temperature exactly as it did when it was alone, so the repair
            // adds a step that describes what the machine was doing anyway
            // rather than inventing a new opinion about the top of the range.
            // It also lands inside the range the editor draws, so the added
            // step is visible and movable rather than off the end of the graph.
            //
            // The max is for a lone point stored at or above the ceiling:
            // temperatures have to climb strictly, so the added step has to
            // clear the one it follows. FAN_BIOS is exempt from the rule that a
            // hotter step may not run the fan slower, so this is valid whatever
            // level the lone point asks for.
            //
            // No hysteresis, because the engine's own ceiling has none either,
            // and a few degrees of it here would hold the firmware step below
            // the ceiling on the way down, which is the one way this repair
            // could have changed what the fan does.
            let handoff = crate::engine::SMART_CEILING_C.max(profile.points[0].temp + 1);

            profile.points.push(StoredPoint::from(
                &CurvePoint::new(handoff, yamato_ec::FAN_BIOS).with_hysteresis(0, 0),
            ));
        }
        _ => {}
    }
}

impl Config {
    /// Seconds between decisions for the current power state.
    pub fn poll_interval(&self, in_standby: bool) -> std::time::Duration {
        let secs = if in_standby { self.standby_poll_secs } else { self.poll_secs };

        std::time::Duration::from_secs(secs as u64)
    }

    /// `%ProgramData%\Yamato\config.json`
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));

        base.join(DIR_NAME).join(FILE_NAME)
    }

    /// Reads settings, falling back to defaults when the file is not there
    /// yet. A missing file is a first run, not an error.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            // A first run still goes through validation. Returning the raw
            // default here skipped the clamps, and the defaults alone leave
            // the watchdog equal to the standby poll, so a single slow tick
            // while asleep reads as a stall.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut fresh = Config::default();
                fresh.validate()?;

                return Ok(fresh);
            }
            Err(e) => return Err(ConfigError::Io(e)),
        };

        // A byte order mark is not JSON, and serde refuses a document that
        // begins with one. Plenty of editors write one into a UTF-8 file
        // unasked, PowerShell's own Set-Content among them, so anybody who
        // opens this file to change a number by hand stands a fair chance of
        // saving one back.
        //
        // The cost is out of all proportion to three bytes. The parse fails,
        // the service loads with unwrap_or_default, and every profile, every
        // interval and every setting is quietly replaced by the defaults with
        // nothing anywhere saying why. It cost an afternoon of hardware
        // testing here before the file was looked at as bytes.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text.as_str());

        let mut config: Config = serde_json::from_str(text).map_err(ConfigError::Parse)?;
        config.migrate()?;
        config.validate()?;

        Ok(config)
    }

    /// Writes via a temporary file and a rename, so an interrupted save cannot
    /// leave a half-written file that the service then refuses to start on.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }

        let text = serde_json::to_string_pretty(self).map_err(ConfigError::Parse)?;

        let temp = path.with_extension("json.new");
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, path)?;

        Ok(())
    }

    fn migrate(&mut self) -> Result<(), ConfigError> {
        if self.version > SCHEMA_VERSION {
            return Err(ConfigError::FromTheFuture {
                found: self.version,
                supported: SCHEMA_VERSION,
            });
        }

        // Version 1 shipped a single profile, so the picker had one entry and
        // nothing to pick. The other built-ins are added on the way past, and
        // only where that name is not already taken, so a curve someone made
        // and called Quiet is left alone.
        //
        // Runs once, which is what bumping the version buys: delete them
        // afterwards and they stay deleted.
        if self.version < 2 {
            for built_in in default_profiles() {
                if !self.profiles.iter().any(|p| p.name == built_in.name) {
                    self.profiles.push(built_in);
                }
            }
        }

        // Curves of fewer than two points used to be storable, and are now
        // refused. Version 1's editor let you delete your way down to one, and
        // its Curve::new accepted what came back, so files with such a profile
        // exist in the wild.
        //
        // The refusal is right and it stays: one point is one fan level applied
        // at every temperature there is, with the firmware's own management
        // switched off because a level is set. What the refusal did to those
        // files is not right. validate reports the first profile that fails and
        // the whole load fails with it, and the service loads with
        // unwrap_or_default, so one stale profile the user had not even
        // selected threw away every other profile, the poll rate, the startup
        // mode and the ignored sensors, and the engine ran on the shipped
        // defaults with nothing said anywhere. A file that will not load is
        // supposed to be a loud failure, and on the service path it was a
        // completely silent one.
        //
        // So this one case is repaired rather than reported, and it is the only
        // one. Every other kind of invalid curve is still refused, which is
        // this module's stated principle. The exception is here because the
        // alternative is silently discarding everything the user has, which is
        // worse than the surprise of one curve gaining a step.
        //
        // Runs once, like the migration above it, so a curve someone shortens
        // deliberately afterwards is still their problem to hear about.
        if self.version < 3 {
            for profile in &mut self.profiles {
                repair_a_curve_too_short_to_be_one(profile);
            }
        }

        self.version = SCHEMA_VERSION;

        Ok(())
    }

    fn validate(&mut self) -> Result<(), ConfigError> {
        // A repair, not a first run: a file with no profiles needs something
        // runnable, and that is the baseline curve, not the whole set somebody
        // may have pruned on purpose. It also keeps the fallback below landing
        // on Balanced.
        if self.profiles.is_empty() {
            self.profiles.push(Profile::new("Balanced", &Curve::default()));
        }

        // Clamped, not refused: a number that is merely too generous is not a
        // broken curve. The two ends are not the same kind of setting; see the
        // constants.
        for profile in &mut self.profiles {
            for point in &mut profile.points {
                point.hyst_up = point.hyst_up.clamp(0, HYST_UP_MAX);
                point.hyst_down = point.hyst_down.clamp(0, HYST_DOWN_MAX);
            }
        }

        // Every stored curve has to satisfy the same rules a new one would,
        // including the refusal to run the fan disengaged.
        for profile in &self.profiles {
            profile.to_curve()?;
        }

        if !self.profiles.iter().any(|p| p.name == self.active_profile) {
            self.active_profile = self.profiles[0].name.clone();
        }

        // An index past the end of the sensor list falls back to the hottest
        // reading instead of being refused: this only decides what is shown,
        // and a bad value is not worth rejecting a whole settings file over.
        if self.tray_sensor.is_some_and(|i| i as usize >= yamato_ec::SENSOR_COUNT) {
            self.tray_sensor = None;
        }

        // A zero poll interval would spin a core reading the EC.
        self.poll_secs = self.poll_secs.clamp(POLL_SECS_MIN, POLL_SECS_MAX);
        self.standby_poll_secs = self
            .standby_poll_secs
            .clamp(STANDBY_POLL_SECS_MIN, STANDBY_POLL_SECS_MAX);

        // After the polls, so the floor uses the intervals that will actually
        // be used, not the ones that were asked for.
        self.watchdog_secs = self
            .watchdog_secs
            .max(watchdog_floor(self.poll_secs, self.standby_poll_secs));

        // A history that grew until the disk was full would be a worse fault
        // than the one it was kept to explain, and a zero-sized one would
        // rotate on every line.
        self.log_max_mb = self.log_max_mb.clamp(LOG_MAX_MB_MIN, LOG_MAX_MB_MAX);

        // The escape has no off switch. A file asking for 200 C, or for 0, is
        // asking for a manual level that is held whatever happens, and that is
        // the one thing this program will not do.
        self.manual_escape_c = self.manual_escape_c.clamp(MANUAL_ESCAPE_MIN, MANUAL_ESCAPE_MAX);

        Ok(())
    }

    pub fn active_curve(&self) -> Result<Curve, ConfigError> {
        self.profiles
            .iter()
            .find(|p| p.name == self.active_profile)
            .unwrap_or(&self.profiles[0])
            .to_curve()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yamato-test-{tag}-{}.json", std::process::id()));
        p
    }

    #[test]
    fn a_byte_order_mark_does_not_cost_somebody_every_setting_they_have() {
        // Editors add one to UTF-8 files unasked. Without this the parse
        // fails, the service loads with unwrap_or_default, and the machine
        // runs on stock settings while the file on disk still holds the
        // real ones and nothing says a word about it.
        let path = temp_path("bom");
        let mut cfg = Config::default();
        cfg.poll_secs = 9;
        cfg.ec_layout = Some(EcLayout::Compat);
        cfg.save(&path).unwrap();

        let clean = std::fs::read(&path).unwrap();
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice(&clean);
        std::fs::write(&path, &with_bom).unwrap();

        let read = Config::load(&path).expect("a leading BOM must not fail the load");
        assert_eq!(read.poll_secs, 9, "settings were replaced by defaults");
        assert_eq!(read.ec_layout, Some(EcLayout::Compat));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_is_a_first_run_not_a_failure() {
        let cfg = Config::load(Path::new(r"Z:\nowhere\yamato\config.json")).unwrap();
        assert_eq!(cfg.version, SCHEMA_VERSION);
        assert!(!cfg.profiles.is_empty());
    }

    #[test]
    fn round_trips_through_disk() {
        let path = temp_path("roundtrip");
        let mut cfg = Config::default();
        let shipped = cfg.profiles.len();
        cfg.poll_secs = 7;
        cfg.profiles.push(Profile::new("Silent", &Curve::default()));
        cfg.save(&path).unwrap();

        let back = Config::load(&path).unwrap();
        assert_eq!(back.poll_secs, 7);
        assert_eq!(back.profiles.len(), shipped + 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_stored_curve_that_broke_the_rules_is_reported_not_repaired() {
        let path = temp_path("badcurve");
        // 0x40 disengages the fan governor. Even from disk, it is refused.
        let json = r#"{
            "version": 1, "poll_secs": 5, "watchdog_secs": 30,
            "startup_mode": "smart", "active_profile": "X",
            "profiles": [{"name":"X","points":[{"temp":90,"level":64,"hyst_up":0,"hyst_down":4}]}]
        }"#;
        std::fs::write(&path, json).unwrap();

        match Config::load(&path) {
            Err(ConfigError::Curve { profile, .. }) => assert_eq!(profile, "X"),
            other => panic!("expected a curve error, got {other:?}"),
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_guessed_at() {
        let path = temp_path("future");
        let json = r#"{
            "version": 99, "poll_secs": 5, "watchdog_secs": 30,
            "startup_mode": "smart", "active_profile": "X",
            "profiles": [{"name":"X","points":[{"temp":50,"level":1,"hyst_up":0,"hyst_down":4},{"temp":88,"level":128,"hyst_up":0,"hyst_down":5}]}]
        }"#;
        std::fs::write(&path, json).unwrap();

        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::FromTheFuture { found: 99, .. })
        ));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_active_profile_that_vanished_falls_back() {
        let mut cfg = Config::default();
        cfg.active_profile = "Deleted".into();
        cfg.validate().unwrap();
        assert_eq!(cfg.active_profile, cfg.profiles[0].name);
    }

    #[test]
    fn poll_and_watchdog_are_kept_sane() {
        let mut cfg = Config::default();
        cfg.poll_secs = 0; // would spin a core
        cfg.watchdog_secs = 1; // would fire during normal running
        cfg.validate().unwrap();

        assert_eq!(cfg.poll_secs, 1);
        assert!(cfg.watchdog_secs >= cfg.poll_secs * 3);
    }

    #[test]
    fn a_setting_that_no_longer_exists_does_not_stop_a_file_loading() {
        // Standby used to be a choice: give the fan back whenever the screen
        // went off, or never give it back at all. Neither could tell a docked
        // machine working with its lid shut from one asleep in a bag, and now
        // that the difference is measured the choice is gone. A file written
        // while it existed still has to load.
        let path = temp_path("oldstandby");
        let json = r#"{
            "version": 2, "poll_secs": 5, "watchdog_secs": 30,
            "startup_mode": "smart", "active_profile": "X",
            "standby": "keep-control", "standby_poll_secs": 30,
            "profiles": [{"name":"X","points":[{"temp":50,"level":1,"hyst_up":0,"hyst_down":4},{"temp":88,"level":128,"hyst_up":0,"hyst_down":5}]}]
        }"#;
        std::fs::write(&path, json).unwrap();

        let cfg = Config::load(&path).expect("a setting that was removed must be ignored, not fatal");
        assert_eq!(cfg.standby_poll_secs, 30);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn standby_backs_the_poll_rate_off() {
        // Services survive Modern Standby, so we can keep the curve running.
        // Polling at the awake rate would keep waking a parked CPU for no gain.
        let cfg = Config::default();
        assert!(cfg.poll_interval(true) > cfg.poll_interval(false));
    }

    #[test]
    fn standby_settings_survive_an_older_file() {
        // A config written before these fields existed must still load.
        let path = temp_path("nostandby");
        let json = r#"{
            "version": 1, "poll_secs": 5, "watchdog_secs": 30,
            "startup_mode": "smart", "active_profile": "X",
            "profiles": [{"name":"X","points":[{"temp":50,"level":1,"hyst_up":0,"hyst_down":4},{"temp":88,"level":128,"hyst_up":0,"hyst_down":5}]}]
        }"#;
        std::fs::write(&path, json).unwrap();

        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.standby_poll_secs, 30);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn temperatures_convert_for_display_only() {
        // The curve stays Celsius underneath whatever the window shows.
        assert_eq!(display_temp(0, false), 0);
        assert_eq!(display_temp(100, false), 100);
        assert_eq!(display_temp(0, true), 32);
        assert_eq!(display_temp(100, true), 212);
        assert_eq!(display_temp(46, true), 114);
    }

    #[test]
    fn the_unit_suffix_matches_the_setting() {
        assert!(unit_suffix(false).ends_with('C'));
        assert!(unit_suffix(true).ends_with('F'));
    }

    #[test]
    fn celsius_is_the_default() {
        assert!(!Config::default().fahrenheit);
    }

    #[test]
    fn profiles_can_be_added_and_switched_between() {
        let mut cfg = Config::default();
        let shipped = cfg.profiles.len();
        cfg.profiles.push(Profile::new("Silent", &Curve::default()));
        cfg.profiles.push(Profile::new("Aggressive", &Curve::default()));
        cfg.active_profile = "Silent".into();
        cfg.validate().unwrap();

        assert_eq!(cfg.profiles.len(), shipped + 2);
        assert_eq!(cfg.active_profile, "Silent");
        assert!(cfg.active_curve().is_ok());
    }

    #[test]
    fn the_active_curve_resolves() {
        let cfg = Config::default();
        assert!(cfg.active_curve().is_ok());
    }

    #[test]
    fn a_fresh_install_has_more_than_one_profile_to_pick_from() {
        // One entry in a picker looks exactly like none at all.
        let cfg = Config::default();
        assert!(cfg.profiles.len() >= 3);
        assert_eq!(cfg.active_profile, "Balanced");
        assert!(cfg.profiles.iter().any(|p| p.name == "Quiet"));
        assert!(cfg.profiles.iter().any(|p| p.name == "Performance"));
    }

    #[test]
    fn every_shipped_profile_is_a_curve_the_engine_would_accept() {
        // A default that does not validate breaks the program on first run for
        // everyone at once.
        for profile in Config::default().profiles {
            let curve = profile.to_curve().unwrap_or_else(|e| {
                panic!("the shipped \"{}\" curve does not validate: {e}", profile.name)
            });

            let points = curve.points();
            assert!(!points.is_empty(), "{} is empty", profile.name);
            assert!(
                points.last().is_some_and(|p| p.is_bios()),
                "{} does not hand back to the firmware at the top",
                profile.name
            );

            for point in points {
                assert_ne!(
                    point.level,
                    yamato_ec::FAN_DISENGAGED,
                    "{} asks for the disengaged level",
                    profile.name
                );
            }
        }
    }

    #[test]
    fn upgrading_from_the_first_shape_gains_the_other_built_in_profiles() {
        // Version one shipped one profile, so the picker had one entry and
        // nothing to switch to. Anyone upgrading kept that file and concluded
        // profiles did not work.
        let mut old = Config::default();
        old.version = 1;
        old.profiles = vec![Profile::new("Balanced", &Curve::default())];

        old.migrate().expect("an old file should migrate, not be refused");

        let names: Vec<&str> = old.profiles.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"Quiet"), "Quiet was not added");
        assert!(names.contains(&"Performance"), "Performance was not added");
        assert_eq!(old.version, SCHEMA_VERSION);
    }

    #[test]
    fn a_profile_someone_named_themselves_is_not_overwritten_by_the_migration() {
        // Somebody's own curve called Quiet is theirs. Adding a built-in over
        // the top of it would replace tuning with a default and say nothing.
        let mine =
            Curve::new(vec![CurvePoint::new(70, 4), CurvePoint::new(85, yamato_ec::FAN_BIOS)])
                .unwrap();

        let mut old = Config::default();
        old.version = 1;
        old.profiles = vec![Profile::new("Quiet", &mine)];

        old.migrate().unwrap();

        let kept = old.profiles.iter().find(|p| p.name == "Quiet").unwrap();
        assert_eq!(kept.points.len(), 2, "the built-in replaced a user's curve");
        assert_eq!(kept.points[0].temp, 70, "the built-in replaced a user's curve");
    }

    #[test]
    fn the_profiles_that_ship_are_named_and_recognized() {
        // The delete paths ask this to decide whether a profile may go, so a
        // name drifting apart from the curve it belongs to would quietly make
        // one of them deletable again.
        for name in BUILT_IN_PROFILES {
            assert!(is_built_in(name));
            assert!(
                Config::default().profiles.iter().any(|p| p.name == name),
                "{name} is named as built in but is not one of the defaults"
            );
        }

        assert!(!is_built_in("Balanced 2"));
        assert!(!is_built_in(""));
    }

    #[test]
    fn a_profile_left_with_one_point_does_not_cost_the_user_the_whole_file() {
        // An older build let a curve be saved with a single point, and this one
        // refuses to load one. Reported rather than repaired, that took the
        // whole file down, and the service loads with unwrap_or_default, so one
        // stale profile nobody had even selected silently replaced every
        // setting the user had with the shipped defaults.
        let path = temp_path("onepoint");
        let json = r#"{
            "version": 2, "poll_secs": 9, "watchdog_secs": 30,
            "startup_mode": "bios", "active_profile": "Mine",
            "ignored_sensors": [3], "standby_poll_secs": 45, "fahrenheit": true,
            "profiles": [
                {"name":"Mine","points":[{"temp":50,"level":1,"hyst_up":0,"hyst_down":4},{"temp":88,"level":128,"hyst_up":0,"hyst_down":5}]},
                {"name":"Old","points":[{"temp":60,"level":3,"hyst_up":0,"hyst_down":4}]}
            ]
        }"#;
        std::fs::write(&path, json).unwrap();

        let cfg = Config::load(&path).expect("one short curve must not cost the whole file");

        // Everything else is still theirs.
        assert_eq!(cfg.poll_secs, 9);
        assert_eq!(cfg.standby_poll_secs, 45);
        assert_eq!(cfg.startup_mode, StartupMode::Bios);
        assert_eq!(cfg.active_profile, "Mine");
        assert_eq!(cfg.ignored_sensors, vec![3usize]);
        assert!(cfg.fahrenheit);
        assert_eq!(cfg.profiles.len(), 2);

        // And the short one is a curve now: the level they chose, kept, with a
        // hotter step handing the fan back to the firmware above it.
        let repaired = cfg.profiles.iter().find(|p| p.name == "Old").unwrap();
        assert_eq!(repaired.points.len(), 2);
        assert_eq!(repaired.points[0].temp, 60);
        assert_eq!(repaired.points[0].level, 3);
        assert!(repaired.points[1].temp > repaired.points[0].temp);
        assert_eq!(repaired.points[1].level, yamato_ec::FAN_BIOS);
        repaired.to_curve().expect("the repaired profile must be a curve the engine accepts");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_profile_with_no_points_at_all_ends_up_runnable() {
        // Nothing stored is not a level somebody chose, so there is nothing to
        // preserve and the baseline curve is the honest answer.
        let mut cfg = Config::default();
        cfg.version = 2;
        cfg.profiles = vec![Profile { name: "Empty".into(), points: Vec::new() }];

        cfg.migrate().unwrap();
        cfg.validate().expect("an empty profile must not fail the whole file either");

        let curve = cfg.profiles[0].to_curve().unwrap();
        assert_eq!(curve.points().len(), Curve::default().points().len());
    }

    #[test]
    fn a_curve_that_is_wrong_in_some_other_way_is_still_reported_not_repaired() {
        // The short-curve repair is the one exception, not a license to fix
        // curves in general. A curve that eases the fan off as the machine
        // heats is one nobody meant to write, and it is still refused rather
        // than quietly rearranged into something else.
        let path = temp_path("backwards");
        let json = r#"{
            "version": 2, "poll_secs": 5, "watchdog_secs": 30,
            "startup_mode": "smart", "active_profile": "X",
            "profiles": [{"name":"X","points":[{"temp":60,"level":4,"hyst_up":0,"hyst_down":4},{"temp":80,"level":1,"hyst_up":0,"hyst_down":4}]}]
        }"#;
        std::fs::write(&path, json).unwrap();

        match Config::load(&path) {
            Err(ConfigError::Curve { profile, source }) => {
                assert_eq!(profile, "X");
                assert_eq!(source, CurveError::Backwards { index: 1 });
            }
            other => panic!("expected a curve error, got {other:?}"),
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_migration_runs_only_once() {
        // Deleting a built-in profile has to stick. If the migration ran on
        // every load it would come back.
        let mut config = Config::default();
        config.migrate().unwrap();
        config.profiles.retain(|p| p.name != "Quiet");

        config.migrate().unwrap();

        assert!(
            !config.profiles.iter().any(|p| p.name == "Quiet"),
            "a deleted profile came back"
        );
    }

    #[test]
    fn the_profiles_are_ordered_by_how_loud_they_are() {
        // The names are a promise, and idle is where it is easiest to break.
        // Below its first step a curve holds that step's level, so a Quiet
        // profile whose lowest point is level 1 runs the fan on a cold machine
        // while Balanced sits silent. Comparing across the range catches that.
        let profiles = Config::default().profiles;
        let curve_named = |name: &str| {
            profiles
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("{name} should be a default profile"))
                .to_curve()
                .unwrap_or_else(|_| panic!("{name} should be a valid curve"))
        };

        let quiet = curve_named("Quiet");
        let balanced = curve_named("Balanced");
        let performance = curve_named("Performance");

        // A fresh decision at each temperature, so hysteresis cannot mask a
        // difference by holding a level over from the step before.
        let level_at = |c: &Curve, t: i8| {
            let i = c.evaluate(t, None);
            c.points()[i].level
        };

        for temp in 20..=99i8 {
            let (q, b, p) = (
                level_at(&quiet, temp),
                level_at(&balanced, temp),
                level_at(&performance, temp),
            );

            // The firmware step is not a fan level and does not compare.
            if [q, b, p].iter().any(|l| *l == yamato_ec::FAN_BIOS) {
                continue;
            }

            assert!(q <= b, "Quiet is louder than Balanced at {temp}C");
            assert!(b <= p, "Balanced is louder than Performance at {temp}C");
        }
    }

    #[test]
    fn the_baseline_profile_is_the_curve_that_has_always_shipped() {
        let cfg = Config::default();
        let balanced = cfg.profiles.iter().find(|p| p.name == "Balanced").unwrap();
        let shipped = Curve::default();

        assert_eq!(balanced.points.len(), shipped.points().len());
        for (stored, point) in balanced.points.iter().zip(shipped.points()) {
            assert_eq!(stored.temp, point.temp);
            assert_eq!(stored.level, point.level);
        }
    }

    #[test]
    fn quiet_is_never_louder_than_balanced_once_the_fan_is_running() {
        // The point of the profile. Below Balanced's first step it is the
        // other way round by design, because Balanced stops the fan there and
        // Quiet does not.
        let cfg = Config::default();
        let quiet = cfg.profiles.iter().find(|p| p.name == "Quiet").unwrap().to_curve().unwrap();
        let balanced = Curve::default();
        let fast = cfg
            .profiles
            .iter()
            .find(|p| p.name == "Performance")
            .unwrap()
            .to_curve()
            .unwrap();

        // Firmware control is not a speed, so comparisons stop where it starts.
        let level = |c: &Curve, t: i8| c.level_at(c.evaluate(t, None));

        for temp in 52..=84 {
            let (q, b, p) = (level(&quiet, temp), level(&balanced, temp), level(&fast, temp));

            if q != yamato_ec::FAN_BIOS && b != yamato_ec::FAN_BIOS {
                assert!(q <= b, "quiet is louder than balanced at {temp} C");
            }
            if p != yamato_ec::FAN_BIOS && b != yamato_ec::FAN_BIOS {
                assert!(p >= b, "performance is quieter than balanced at {temp} C");
            }
        }
    }

    #[test]
    fn logging_is_off_until_it_is_asked_for() {
        // Writing to the disk every few seconds is not something to start
        // doing because nobody said not to.
        assert!(!Config::default().log_enabled);
    }

    #[test]
    fn the_log_size_is_kept_within_something_sane() {
        let mut cfg = Config::default();

        cfg.log_max_mb = 0; // would rotate on every line
        cfg.validate().unwrap();
        assert_eq!(cfg.log_max_mb, LOG_MAX_MB_MIN);

        cfg.log_max_mb = 100_000; // would fill the disk before rotating
        cfg.validate().unwrap();
        assert_eq!(cfg.log_max_mb, LOG_MAX_MB_MAX);
    }

    #[test]
    fn log_settings_survive_a_file_written_before_they_existed() {
        let path = temp_path("nolog");
        let json = r#"{
            "version": 1, "poll_secs": 5, "watchdog_secs": 30,
            "startup_mode": "smart", "active_profile": "X",
            "profiles": [{"name":"X","points":[{"temp":50,"level":1,"hyst_up":0,"hyst_down":4},{"temp":88,"level":128,"hyst_up":0,"hyst_down":5}]}]
        }"#;
        std::fs::write(&path, json).unwrap();

        let cfg = Config::load(&path).unwrap();
        assert!(!cfg.log_enabled);
        assert_eq!(cfg.log_max_mb, default_log_max_mb());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_published_bounds_are_the_ones_validation_applies() {
        // The tray and the settings window clamp against these before saving,
        // so if they ever stopped matching what the loader does, a control
        // would appear to set a value the engine never used.
        let mut cfg = Config {
            poll_secs: POLL_SECS_MAX + 1,
            standby_poll_secs: STANDBY_POLL_SECS_MIN - 1,
            ..Config::default()
        };
        cfg.validate().unwrap();

        assert_eq!(cfg.poll_secs, POLL_SECS_MAX);
        assert_eq!(cfg.standby_poll_secs, STANDBY_POLL_SECS_MIN);
        assert_eq!(
            cfg.watchdog_secs,
            cfg.watchdog_secs.max(watchdog_floor(cfg.poll_secs, cfg.standby_poll_secs))
        );
    }

    #[test]
    fn the_manual_escape_cannot_be_switched_off_or_made_a_formality() {
        // This is what pulls the fan off a fixed level when the machine gets
        // hot with nobody watching. A hand-edited file does not get to remove
        // it by writing a large number, or make manual mode unusable with a
        // small one.
        let mut cfg = Config { manual_escape_c: 127, ..Config::default() };
        cfg.validate().unwrap();
        assert_eq!(cfg.manual_escape_c, MANUAL_ESCAPE_MAX);

        let mut cfg = Config { manual_escape_c: -40, ..Config::default() };
        cfg.validate().unwrap();
        assert_eq!(cfg.manual_escape_c, MANUAL_ESCAPE_MIN);

        assert_eq!(Config::default().manual_escape_c, crate::engine::MANUAL_ESCAPE_C);
    }

    #[test]
    fn hysteresis_is_clamped_in_both_directions_but_not_equally() {
        // Down is comfort: a generous one holds the fan higher for longer. Up
        // is safety: a generous one delays the fan on a machine that is
        // already climbing, so it gets much less room.
        let mut cfg = Config::default();
        cfg.profiles[0].points[0].hyst_up = 100;
        cfg.profiles[0].points[0].hyst_down = 100;
        cfg.validate().unwrap();

        assert_eq!(cfg.profiles[0].points[0].hyst_up, HYST_UP_MAX);
        assert_eq!(cfg.profiles[0].points[0].hyst_down, HYST_DOWN_MAX);
        assert!(
            cfg.profiles[0].points[0].hyst_up < cfg.profiles[0].points[0].hyst_down,
            "delaying a ramp is not as safe as delaying a drop"
        );

        let mut cfg = Config::default();
        cfg.profiles[0].points[0].hyst_up = -9;
        cfg.profiles[0].points[0].hyst_down = -9;
        cfg.validate().unwrap();

        assert_eq!(cfg.profiles[0].points[0].hyst_up, 0);
        assert_eq!(cfg.profiles[0].points[0].hyst_down, 0);
    }

    #[test]
    fn everything_this_program_ships_is_inside_its_own_hysteresis_bounds() {
        for profile in Config::default().profiles {
            for point in profile.points {
                assert!(point.hyst_up <= HYST_UP_MAX, "{} climbs late", profile.name);
                assert!(point.hyst_down <= HYST_DOWN_MAX, "{} falls late", profile.name);
            }
        }
    }

    #[test]
    fn the_tray_shows_numbers_unless_told_otherwise() {
        assert!(Config::default().tray_numbers);
    }

    #[test]
    fn two_fans_is_the_default_and_what_an_older_file_means() {
        // The dual-fan path is the safe one to presume: verifying a second
        // fan that is not there costs a scary message, while skipping one
        // that is there costs an unmanaged fan.
        assert!(!Config::default().single_fan);

        // A file written before the field existed must mean the same thing.
        let path = temp_path("nosinglefan");
        let json = r#"{
            "version": 1, "poll_secs": 5, "watchdog_secs": 30,
            "startup_mode": "smart", "active_profile": "X",
            "profiles": [{"name":"X","points":[{"temp":50,"level":1,"hyst_up":0,"hyst_down":4},{"temp":88,"level":128,"hyst_up":0,"hyst_down":5}]}]
        }"#;
        std::fs::write(&path, json).unwrap();

        assert!(!Config::load(&path).unwrap().single_fan);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_fresh_install_has_no_layout_on_record() {
        // None is what makes the engine probe, and it must only ever mean a
        // machine nobody has probed: a default of Standard here would mean
        // no install ever probed at all.
        assert_eq!(Config::default().ec_layout, None);

        // A file written before the field existed is a machine that was
        // never probed, and must load as one.
        let path = temp_path("nolayout");
        let json = r#"{
            "version": 1, "poll_secs": 5, "watchdog_secs": 30,
            "startup_mode": "smart", "active_profile": "X",
            "profiles": [{"name":"X","points":[{"temp":50,"level":1,"hyst_up":0,"hyst_down":4},{"temp":88,"level":128,"hyst_up":0,"hyst_down":5}]}]
        }"#;
        std::fs::write(&path, json).unwrap();

        assert_eq!(Config::load(&path).unwrap().ec_layout, None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_recorded_layout_round_trips_and_keeps_its_spelling() {
        // The engine writes this once and every later boot steers by it, so
        // losing it on the way to disk would mean probing forever. The
        // spelling is load bearing too: "compat" is what shipped files will
        // contain, and a rename would silently un-record every machine that
        // needed recording most.
        let path = temp_path("layout");

        for (layout, spelled) in
            [(EcLayout::Standard, "\"standard\""), (EcLayout::Compat, "\"compat\"")]
        {
            let cfg = Config { ec_layout: Some(layout), ..Config::default() };
            cfg.save(&path).unwrap();

            assert_eq!(Config::load(&path).unwrap().ec_layout, Some(layout));
            assert!(
                std::fs::read_to_string(&path).unwrap().contains(spelled),
                "{layout:?} is not stored as {spelled}"
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_stored_layout_and_the_driven_layout_agree_both_ways() {
        // The conversion is the seam between the file and the hardware; a
        // crossed pair here would drive the wrong ports on exactly the
        // machines nobody debugging a config file can test.
        for layout in [EcLayout::Standard, EcLayout::Compat] {
            assert_eq!(EcLayout::from(yamato_ec::Layout::from(layout)), layout);
        }

        assert_eq!(yamato_ec::Layout::from(EcLayout::Compat), yamato_ec::Layout::Alternate);
    }

    #[test]
    fn single_fan_round_trips_through_disk() {
        // The setting is only reachable through save and load, so losing it
        // on the way would mean the row in the window changes nothing.
        let path = temp_path("singlefan");
        let cfg = Config { single_fan: true, ..Config::default() };
        cfg.save(&path).unwrap();

        assert!(Config::load(&path).unwrap().single_fan);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_watchdog_floor_follows_the_slowest_poll() {
        // Standby is usually the slow one, but not always: someone who polls
        // every 60 seconds awake and every 5 asleep needs the same protection.
        assert_eq!(watchdog_floor(5, 30), 90);
        assert_eq!(watchdog_floor(60, 5), 180);
        // Reachable from a hand-edited file, and a wrap here would leave the
        // watchdog firing on the first tick.
        assert_eq!(watchdog_floor(u32::MAX, 1), u32::MAX);
    }
}
