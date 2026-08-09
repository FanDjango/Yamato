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
//! The path is PawnIO over `DeviceIoControl`, using the stock `LpcACPIEC`
//! module to reach the two ACPI EC ports.

mod ec;
mod lock;
mod pawnio;

pub use ec::{
    Ec, EcState, FAN_BIOS, FAN_BITS, FAN_DISENGAGED, FAN_LEVEL_MAX, SENSOR_COUNT,
};
pub use pawnio::{Error, PawnIo, MODULE_FILE};
