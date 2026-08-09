// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein
//
// Checks the hardware path end to end: PawnIO opens, the module loads, the EC
// answers with numbers that look like a ThinkPad. Read only unless asked.
//
//   cargo run -p yamato-ec --example probe            read and print
//   cargo run -p yamato-ec --example probe -- bios    hand the fan to firmware
//
// Needs administrator rights, same as anything that reaches the controller.

use yamato_ec::{Ec, FAN_BIOS};

fn main() {
    let ec = match Ec::open() {
        Ok(ec) => ec,
        Err(e) => {
            eprintln!("could not reach the embedded controller: {e}");
            std::process::exit(1);
        }
    };

    if std::env::args().nth(1).as_deref() == Some("bios") {
        match ec.release_to_bios() {
            Ok(()) => println!("fan handed back to the firmware"),
            Err(e) => eprintln!("could not hand the fan back: {e}"),
        }
        return;
    }

    let state = match ec.sample() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not read the controller: {e}");
            std::process::exit(1);
        }
    };

    println!("fan register  0x{:02x}", state.fan_ctrl);
    if state.is_bios_controlled() {
        println!("  mode        firmware");
    } else {
        println!("  mode        manual, level {}", state.manual_level().unwrap_or(0));
    }

    println!("fan speed     {} rpm", state.fan_rpm[0]);
    if state.fan_rpm[1] > 0 {
        println!("  second fan  {} rpm", state.fan_rpm[1]);
    }

    println!("sensors");
    for (i, reading) in state.sensors.iter().enumerate() {
        match reading {
            Some(t) => println!("  [{i:2}]        {t} C"),
            None => println!("  [{i:2}]        -"),
        }
    }

    match state.hottest(&[]) {
        Some((i, t)) => println!("hottest       sensor {i} at {t} C"),
        None => println!("hottest       nothing reporting"),
    }

    // Never leaves a level set. A level written with nothing driving it
    // afterwards is the hazard this program exists to prevent.
    debug_assert_ne!(FAN_BIOS, 0);
}
