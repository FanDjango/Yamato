// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! Embeds the icon and version info into the executable.
//!
//! The tray icons come from `include_bytes!`, but Explorer, the taskbar, Task
//! Manager and the shield on an elevation prompt all read a Win32 resource.
//! Without one they show a blank sheet of paper.

fn main() {
    // Guarded so a cargo check on any other host does not fail on a missing
    // resource compiler.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    println!("cargo:rerun-if-changed=../../assets/yamato.ico");

    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../assets/yamato.ico");
    res.set("ProductName", "Yamato");
    res.set("FileDescription", "Fan control software for ThinkPads");
    res.set("LegalCopyright", "Copyright (c) 2026 David Brustein. MIT. No warranty.");
    res.set("Comments", "Not affiliated with, endorsed by, or supported by Lenovo.");

    // A failure here costs the icon, not the build. Building without the
    // Windows SDK's resource compiler should still produce a working program.
    if let Err(e) = res.compile() {
        println!("cargo:warning=could not embed the icon or version info: {e}");
    }
}
