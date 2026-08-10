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

//! Embedded controller access for ThinkPads.
//!
//! The path is PawnIO over `DeviceIoControl`. Which module rides on it
//! depends on where this machine keeps its EC: the stock `LpcACPIEC` for the
//! ACPI-specified ports, or `LpcIO` for the P53-class machines that put the
//! controller at 0x1600/0x1604 instead. `Ec::open` probes both layouts and
//! chooses on the evidence; the caller records the verdict and every later
//! start drives it through `Ec::open_configured`, which probes nothing. A
//! verdict that a layout nobody could examine might have outranked stays
//! unrecorded, so the next start probes again: see `Ec::worth_recording`.

mod ec;
mod lock;
mod pawnio;
mod version;

pub use ec::{
    Ec, EcState, Probe, ProbeFailure, FAN_BIOS, FAN_BITS, FAN_DISENGAGED, FAN_LEVEL_MAX,
    SENSOR_COUNT,
};
pub use pawnio::{Error, Layout, PawnIo, MODULE_FILES};
pub use version::{
    driver_version_report, installed_driver_version, DriverVersion, MIN_DRIVER_VERSION,
};
