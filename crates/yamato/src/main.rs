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

//! A windows-subsystem binary, so no console appears behind the tray icon
//! when the run key starts it at logon. The service and the install verbs
//! print nothing anyone sees anyway; failures reach the user through the
//! window or the service manager.
#![windows_subsystem = "windows"]

//! One executable, three jobs, chosen by argument:
//!
//! * no arguments  - the window and tray icon, attaching to whatever owns the fan
//! * `--service`   - the engine, run by the service manager
//! * `--install` / `--uninstall` - service management, then exit
//!
//! Exactly one process ever drives the fan: the one holding the engine lock.
//! Anything else is a client that never opens the port driver, so it cannot
//! write the fan register however it is asked to.

mod about;
mod curve_editor;
mod engine_host;
mod icon;
mod import;
mod ipc;
mod log;
mod pawnio_status;
mod prompt;
mod service;
mod settings;
mod startup;
mod theme;
mod tray;

use std::process::ExitCode;

fn main() -> ExitCode {
    // Before any window exists. Without this Windows renders us at 96 DPI and
    // bitmap-stretches the result, which is why an unaware app looks soft on a
    // high resolution laptop panel. Per-monitor v2 so dragging between a
    // built-in display and an external one rescales instead of blurring until
    // the window is reopened.
    unsafe {
        use windows_sys::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    match Command::parse(std::env::args().skip(1)) {
        Command::Tray => run_tray(),
        Command::Service => run_service(),
        Command::Install => run_install(),
        Command::Uninstall => run_uninstall(),
        Command::EnableStartup => {
            startup::set(true);
            ExitCode::SUCCESS
        }
        Command::StartService => match service::start() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        Command::StopService => match service::stop() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        Command::Help => run_help(),
    }
}

/// Answers `--help`, or any argument this program does not understand.
///
/// A windows-subsystem binary starts with no console, so a plain println!
/// here went to a handle nobody holds and the usage text reached no one.
/// Attaching to the console of whoever launched us puts the answer in the
/// terminal that asked, which is where somebody typing --help is looking.
/// Started from Explorer or a shortcut there is no console to attach to, and
/// then the About box carries the same name, version and identity on screen,
/// which beats both silence and a console flashing into existence.
fn run_help() -> ExitCode {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};

    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0 {
        // The shell printed its prompt before we attached, so start on a
        // fresh line rather than appending to it.
        println!();
        print_usage();
    } else {
        // Same reason run_tray gives: the shell wants an apartment on any
        // thread that calls ShellExecute, which the box's two buttons do.
        unsafe {
            use windows_sys::Win32::System::Ole::OleInitialize;
            OleInitialize(std::ptr::null_mut());
        }

        about::show(std::ptr::null_mut());
    }

    ExitCode::SUCCESS
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Tray,
    Service,
    Install,
    Uninstall,
    /// Installer plumbing. Sets the per-user run entry as the user who is
    /// actually logging in, which an elevated installer cannot do for them.
    EnableStartup,
    StartService,
    StopService,
    Help,
}

impl Command {
    fn parse(args: impl Iterator<Item = String>) -> Self {
        // Only the first argument decides. Anything after it is a mistake, and
        // showing usage beats quietly obeying half of it.
        let Some(arg) = args.into_iter().next() else {
            return Command::Tray;
        };

        match arg.trim_start_matches(['-', '/']).to_ascii_lowercase().as_str() {
            "service" | "s" => Command::Service,
            "install" | "i" => Command::Install,
            "uninstall" | "u" => Command::Uninstall,
            "enable-startup" => Command::EnableStartup,
            "start-service" => Command::StartService,
            "stop-service" => Command::StopService,
            _ => Command::Help,
        }
    }
}

fn print_usage() {
    println!(
        "Yamato {}\n\n\
         Usage:\n  \
           yamato              open the window and tray icon\n  \
           yamato --install    install and start the service\n  \
           yamato --uninstall  stop and remove the service\n  \
           yamato --service    run as the service (the service manager does this)\n",
        env!("CARGO_PKG_VERSION")
    );
}

fn run_tray() -> ExitCode {
    // The shell wants an apartment on any thread that talks to it, and this
    // one does plenty: the import file dialog, ShellExecute for a folder or a
    // download page, and the tray icon. Without it the common file dialog can
    // fail to open, and that failure looks exactly like someone pressing
    // Cancel.
    unsafe {
        use windows_sys::Win32::System::Ole::OleInitialize;
        OleInitialize(std::ptr::null_mut());
    }

    // A stale RUNASADMIN compatibility layer on our own executable overrides
    // the manifest, and Windows then silently refuses to start us from the run
    // key at logon: no window, no error. Clearing it here repairs an in-place
    // upgrade.
    startup::clear_runasadmin_layer();

    // Exiting stops the service, so starting is the other half of the bargain.
    // Without it, quitting once leaves a tray icon with nothing behind it and
    // no obvious way back. This does not fire at logon, where the service has
    // already started automatically; it fires when somebody quit and returned.
    if tray::start_service_if_stopped() {
        // Waited for, not guessed at. A fixed pause is too long on a machine
        // that was ready at once, and too short on one that was not, where the
        // tray opens complaining there is no engine and corrects itself a
        // moment later.
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);

        while std::time::Instant::now() < until && ipc::Channel::attach().is_none() {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    let config =
        yamato_core::Config::load(&yamato_core::Config::default_path()).unwrap_or_default();

    let Some(mut tray) = tray::Tray::new() else {
        return ExitCode::FAILURE;
    };

    tray.set_profiles(config.profiles.iter().map(|p| p.name.clone()).collect());

    if config.show_window_on_start {
        tray.open_settings();
    }

    tray.run();

    ExitCode::SUCCESS
}

fn run_service() -> ExitCode {
    if service::run() {
        ExitCode::SUCCESS
    } else {
        // Almost always means it was launched by hand rather than by the
        // service manager, which is a reasonable mistake to make.
        eprintln!("this is the service entry point; start it with: yamato --install");
        ExitCode::FAILURE
    }
}

fn run_install() -> ExitCode {
    match service::install() {
        Ok(()) => {
            println!("service installed and started");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn run_uninstall() -> ExitCode {
    match service::uninstall() {
        Ok(()) => {
            println!("service stopped and removed");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Command {
        Command::parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_arguments_means_the_window() {
        assert_eq!(parse(&[]), Command::Tray);
    }

    #[test]
    fn accepts_both_dash_and_slash_in_either_case() {
        // The service manager and a command prompt disagree about style, and
        // people type whichever they are used to.
        for form in ["--service", "-service", "/service", "-s", "--SERVICE"] {
            assert_eq!(parse(&[form]), Command::Service, "{form} was not understood");
        }
    }

    #[test]
    fn install_and_uninstall_are_distinct() {
        assert_eq!(parse(&["--install"]), Command::Install);
        assert_eq!(parse(&["--uninstall"]), Command::Uninstall);
    }

    #[test]
    fn anything_unrecognized_shows_usage_rather_than_guessing() {
        // Guessing at an unknown flag on a program that drives cooling
        // hardware is not a good instinct.
        assert_eq!(parse(&["--bogus"]), Command::Help);
    }
}
