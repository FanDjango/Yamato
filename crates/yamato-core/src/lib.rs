// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to
// deal in the Software without restriction. See LICENSE for the full text.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED. THE AUTHORS OR COPYRIGHT HOLDERS SHALL NOT BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! Curves, profiles and the control loop. Knows nothing about how the hardware
//! is reached; that is `yamato-ec`'s job.

pub mod config;
pub mod curve;
pub mod engine;
pub mod import;

pub use config::{
    display_temp, is_built_in, unit_suffix, watchdog_floor,
    Config, ConfigError, EcLayout, Profile, StartupMode, StoredPoint,
    BUILT_IN_PROFILES, HYST_DOWN_MAX, HYST_UP_MAX, LOG_MAX_MB_MAX, LOG_MAX_MB_MIN, MANUAL_ESCAPE_MAX,
    MANUAL_ESCAPE_MIN, POLL_SECS_MAX, POLL_SECS_MIN, SCHEMA_VERSION, STANDBY_POLL_SECS_MAX,
    STANDBY_POLL_SECS_MIN,
};
pub use curve::{Curve, CurveError, CurvePoint};
pub use import::{parse_tpfancontrol_ini, Imported, ImportError};
pub use engine::{Engine, FanGuard, Mode, Tick, DEFAULT_WATCHDOG, MANUAL_ESCAPE_C};
