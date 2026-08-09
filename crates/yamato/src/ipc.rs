// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! What the engine and the window say to each other.
//!
//! One shared section. The engine publishes what it read, a window posts what
//! the user asked for, and a sequence/ack pair tells the window when its
//! request landed. No pipes, no sockets, no serialization.
//!
//! Two details are load bearing. The section carries an explicit descriptor: a
//! named object created by a service running as SYSTEM with the default
//! descriptor cannot be opened from a user session, which silently turns one
//! channel into two that never meet.
//!
//! And a section exists only while somebody holds it open, so its presence
//! *is* the answer to "is there an engine". An engine that was killed leaves
//! nothing behind to be wrong about, unlike an event or a flag on disk.

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, OpenEventW, ResetEvent, SetEvent, EVENT_MODIFY_STATE,
};

const SECTION_NAME: &str = r"Global\Yamato_State";
const SDDL_EVERYONE: &str = "D:(A;;GA;;;WD)";

/// Signalled by a window when it posts a command, waited on by the engine.
///
/// The section alone could only be read by asking it repeatedly, and the
/// engine was doing that five times a second so a click would not sit there.
/// A service waking at that rate keeps a modern standby machine out of its
/// low-power state entirely: the laptop never really sleeps, and the very
/// clocks the engine uses to notice sleep then agree that it never did.
///
/// So the engine waits on this instead. A command wakes it at once, which is
/// faster than the polling was, and between commands nothing runs at all.
const COMMAND_EVENT: &str = r"Global\Yamato_Command";

/// Standard access right, not bound by windows-sys.
const SYNCHRONIZE: u32 = 0x0010_0000;

pub const NAME_LEN: usize = 64;
pub const SENSOR_COUNT: usize = 12;

/// Encodes "this sensor is not reporting" without a separate mask.
pub const SENSOR_ABSENT: i16 = i16::MIN;

/// Mode as it crosses the boundary. Kept as a plain byte so the shared block
/// stays a POD struct with a stable layout.
pub const MODE_BIOS: u8 = 0;
pub const MODE_SMART: u8 = 1;
pub const MODE_MANUAL: u8 = 2;

/// What kind of trouble the engine is in, when it is in any.
///
/// `fault` said something was wrong and nothing said what, so all three showed
/// the same sentence, including the one where a fan may be held at a fixed
/// level with the firmware's own management switched off.
///
/// Zero is healthy on purpose: a freshly zeroed section reads as "nothing to
/// report" rather than as a state nobody has published yet.
pub const STATUS_OK: u8 = 0;
/// Running, but the controller cannot be reached.
pub const STATUS_UNREACHABLE: u8 = 1;
/// Faults made the engine step aside. The firmware has the fan, and the curve
/// comes back once the controller behaves.
pub const STATUS_SURRENDERED: u8 = 2;
/// Handing the fan back failed.
pub const STATUS_HANDBACK_FAILED: u8 = 3;

/// A headline and a line of detail for each state, or nothing when healthy.
///
/// `fault` is still consulted: a window from this build talking to an older
/// engine gets a section where `status` is a byte nobody ever wrote. Reading
/// that as healthy while the flag beside it says otherwise would be wrong.
pub fn status_words(status: u8, fault: bool) -> Option<(&'static str, &'static str)> {
    const UNREACHABLE: (&str, &str) = (
        "Cannot reach the embedded controller",
        "Readings return when it answers again.",
    );

    match status {
        STATUS_HANDBACK_FAILED => Some((
            "The fan may be held at a fixed level",
            "Firmware fan control is off. Restart Yamato, or reboot.",
        )),
        STATUS_UNREACHABLE => Some(UNREACHABLE),
        STATUS_SURRENDERED => Some((
            "The firmware has the fan",
            "Yamato stepped aside after a fault; the curve returns shortly.",
        )),
        _ if fault => Some(UNREACHABLE),
        _ => None,
    }
}

/// Offered when a run of declined fan writes meets a second fan that has never
/// once reported a speed: the signature of a single-fan machine being verified
/// through a selector it does not have.
///
/// Defined once, because the tray tooltip and the settings window both show
/// it. Worded to suggest, not conclude: the same evidence could just barely be
/// a dual-fan machine in real trouble, which is why the setting is a setting.
/// It replaces the detail line in the tooltip, whose 128 characters have no
/// room for both, so it has to fit beside every headline; a test holds it
/// to that.
pub const SINGLE_FAN_HINT: &str =
    "This may be a single-fan machine. Set Fans to Single in Settings.";

/// "Change the profile, leave the mode alone."
///
/// Echoing back the mode the engine just published is not harmless. Echoing
/// MANUAL counts as a fresh manual instruction, clearing the 80 C escape latch
/// and re-applying the level on a machine that is still hot. Echoing BIOS
/// while the engine had stepped aside after a fault cancels the pending
/// recovery for good. Neither is what renaming a profile means.
pub const MODE_KEEP: u8 = 0xff;

/// The shared block. `repr(C)` because both sides map the same bytes and the
/// layout has to be something we chose rather than something rustc chose.
#[repr(C)]
pub struct Shared {
    /// Bumped by the engine after each publish. A window watches this to know
    /// there is something new rather than redrawing on a timer.
    pub state_seq: AtomicU32,
    /// Bumped by a window after posting a command.
    pub cmd_seq: AtomicU32,
    /// The `cmd_seq` the engine has acted on. Until this catches up, a window
    /// keeps showing what the user asked for rather than snapping back to the
    /// engine's older answer, which would look like the click did nothing.
    pub ack_seq: AtomicU32,

    // Published by the engine.
    pub fan_ctrl: u8,
    pub mode: u8,
    pub hottest_index: u8,
    pub hottest_temp: i8,
    pub fan_rpm: [u16; 2],
    pub sensors: [i16; SENSOR_COUNT],
    pub profile: [u8; NAME_LEN],
    /// Set when the engine is running but could not reach the hardware, so a
    /// window can say why instead of showing a frozen reading.
    pub fault: u8,
    /// Which kind of trouble, as one of the `STATUS_` values.
    ///
    /// Placed in the padding byte after `fault`, so the block keeps the size
    /// and the offsets it already had. An older engine that never writes here
    /// leaves the zero that means healthy, and `fault` is still published.
    pub status: u8,
    /// How often the engine intends to publish, in seconds.
    ///
    /// A reader cannot judge staleness without knowing what "fresh" means, and
    /// the interval is a user setting that also changes entering standby.
    /// Guessing it had the tray calling a healthy engine dead at any poll
    /// above about six seconds, while a real manual level was still held.
    pub publish_secs: u16,

    // Posted by a window.
    pub cmd_mode: u8,
    pub cmd_level: u8,
    pub cmd_profile: [u8; NAME_LEN],

    /// Set while the engine's fan writes are being declined: the controller
    /// answers every transaction but does not hold the value, as against not
    /// answering at all.
    ///
    /// Published by the engine despite sitting below the window's fields. Like
    /// `status`, it lives in padding, here the tail bytes the compiler already
    /// left for alignment, so nothing moves and nothing grows.
    ///
    /// Paired with a second fan that has never once reported a speed, a run of
    /// declines is the signature of a single-fan machine being verified
    /// through a selector it does not have, and the tray can suggest the
    /// setting that ends it. The pairing and the remembering are the client's
    /// job; the engine only says what kind of failure it is having.
    pub fan_write_declined: u8,
}

impl Shared {
    pub fn read_profile(&self) -> String {
        decode_name(&self.profile)
    }

    pub fn read_cmd_profile(&self) -> String {
        decode_name(&self.cmd_profile)
    }
}

fn decode_name(raw: &[u8; NAME_LEN]) -> String {
    let end = raw.iter().position(|b| *b == 0).unwrap_or(NAME_LEN);

    String::from_utf8_lossy(&raw[..end]).into_owned()
}

fn encode_name(into: &mut [u8; NAME_LEN], name: &str) {
    into.fill(0);
    let bytes = name.as_bytes();
    let n = bytes.len().min(NAME_LEN - 1);
    into[..n].copy_from_slice(&bytes[..n]);
}

/// A mapped view of the shared block.
pub struct Channel {
    mapping: HANDLE,
    view: *mut Shared,
    /// True for the side that created it. Only the engine publishes.
    owner: bool,
    /// Set by a window with a command to post, waited on by the engine.
    ///
    /// Null when it could not be created or opened, which is not fatal to
    /// either side: the engine's wait then simply runs to its timeout, and a
    /// command waits out the poll interval as it used to.
    command: HANDLE,
}

unsafe impl Send for Channel {}
unsafe impl Sync for Channel {}

impl Channel {
    /// Engine side. Creates the section.
    ///
    /// Called *before* the port driver is opened, so a window starting
    /// alongside us at logon has something to attach to instead of giving up
    /// while we are still probing the controller.
    pub fn create() -> Option<Self> {
        let name = wide(SECTION_NAME);

        let mut descriptor = ptr::null_mut();
        let sddl = wide(SDDL_EVERYONE);
        let have_sd = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } != 0;

        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };

        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                if have_sd { &mut sa } else { ptr::null_mut() },
                PAGE_READWRITE,
                0,
                std::mem::size_of::<Shared>() as u32,
                name.as_ptr(),
            )
        };

        if have_sd {
            unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor as _) };
        }

        if mapping.is_null() {
            return None;
        }

        let view = unsafe {
            MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, std::mem::size_of::<Shared>())
        };

        if view.Value.is_null() {
            unsafe { CloseHandle(mapping) };
            return None;
        }

        let view = view.Value as *mut Shared;
        // Fresh section pages are already zeroed, which is a valid resting
        // state: BIOS mode, no fault, no command.
        unsafe { ptr::write_bytes(view as *mut u8, 0, std::mem::size_of::<Shared>()) };

        Some(Channel { mapping, view, owner: true, command: create_command_event() })
    }

    /// Window side. Fails when there is no engine.
    pub fn attach() -> Option<Self> {
        let name = wide(SECTION_NAME);

        let mapping = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, name.as_ptr()) };
        if mapping.is_null() {
            return None;
        }

        let view = unsafe {
            MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, std::mem::size_of::<Shared>())
        };

        if view.Value.is_null() {
            unsafe { CloseHandle(mapping) };
            return None;
        }

        let command = unsafe {
            OpenEventW(EVENT_MODIFY_STATE | SYNCHRONIZE, 0, wide(COMMAND_EVENT).as_ptr())
        };

        Some(Channel { mapping, view: view.Value as *mut Shared, owner: false, command })
    }

    pub fn get(&self) -> &Shared {
        unsafe { &*self.view }
    }

    /// A raw pointer, not a `&mut` conjured from `&self`.
    ///
    /// This memory is shared and mutable from other processes, so a Rust
    /// reference would assert an exclusivity that does not exist. Writes go
    /// through the pointer.
    fn as_ptr(&self) -> *mut Shared {
        self.view
    }

    /// Engine side: publish a sample.
    pub fn publish(
        &self,
        fan_ctrl: u8,
        mode: u8,
        hottest: Option<(usize, i8)>,
        fan_rpm: [u16; 2],
        sensors: &[Option<i8>; SENSOR_COUNT],
        profile: &str,
        fault: bool,
        status: u8,
        fan_write_declined: bool,
        publish_secs: u16,
    ) {
        debug_assert!(self.owner, "only the engine publishes");

        let s = unsafe { &mut *self.as_ptr() };
        s.fan_ctrl = fan_ctrl;
        s.mode = mode;
        s.fan_rpm = fan_rpm;
        s.fault = u8::from(fault);
        s.status = status;
        s.fan_write_declined = u8::from(fan_write_declined);
        s.publish_secs = publish_secs.max(1);

        let (idx, temp) = hottest.unwrap_or((0, 0));
        s.hottest_index = idx as u8;
        s.hottest_temp = temp;

        for (i, reading) in sensors.iter().enumerate() {
            s.sensors[i] = reading.map_or(SENSOR_ABSENT, i16::from);
        }

        encode_name(&mut s.profile, profile);

        // Last, and with release ordering, so a reader that sees the new
        // sequence number is guaranteed to see the fields behind it.
        s.state_seq.fetch_add(1, Ordering::Release);
    }

    /// Engine side: record that a command has been acted on.
    pub fn acknowledge(&self, seq: u32) {
        unsafe { (*self.as_ptr()).ack_seq.store(seq, Ordering::Release) };
    }

    /// Window side: ask for a mode. Returns the sequence to watch for.
    pub fn post_command(&self, mode: u8, level: u8, profile: &str) -> u32 {
        let s = unsafe { &mut *self.as_ptr() };
        s.cmd_mode = mode;
        s.cmd_level = level;
        encode_name(&mut s.cmd_profile, profile);

        // Fields first, then the sequence, for the same reason as publish.
        let seq = s.cmd_seq.fetch_add(1, Ordering::Release) + 1;

        // Then wake the engine. After the sequence, always: a wait that comes
        // back to find nothing posted goes straight back to sleeping, and the
        // click waits out the whole poll interval.
        if !self.command.is_null() {
            unsafe { SetEvent(self.command) };
        }

        seq
    }

    /// Engine side: the handle to wait on, or null when there is none.
    pub fn command_event(&self) -> HANDLE {
        self.command
    }

    /// Engine side: clear it before checking, so a signal that arrives during
    /// the check is not lost between looking and waiting.
    pub fn clear_command_event(&self) {
        if !self.command.is_null() {
            unsafe { ResetEvent(self.command) };
        }
    }

    /// Engine side: the command sequence waiting to be acted on, if any.
    pub fn pending_command(&self) -> Option<u32> {
        let s = self.get();
        let posted = s.cmd_seq.load(Ordering::Acquire);

        (posted != s.ack_seq.load(Ordering::Acquire)).then_some(posted)
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        unsafe {
            if !self.view.is_null() {
                UnmapViewOfFile(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.view as *mut c_void,
                });
            }
            if !self.mapping.is_null() {
                CloseHandle(self.mapping);
            }
            if !self.command.is_null() {
                CloseHandle(self.command);
            }
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Manual reset, so a signal is not lost to whichever wait happens to be
/// running, and world writable for the same reason the section is: the engine
/// creates it as SYSTEM and the window that has to set it is not.
fn create_command_event() -> HANDLE {
    let mut descriptor = ptr::null_mut();
    let sddl = wide(SDDL_EVERYONE);
    let have_sd = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } != 0;

    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };

    let event = unsafe {
        CreateEventW(
            if have_sd { &mut sa } else { ptr::null_mut() },
            1,
            0,
            wide(COMMAND_EVENT).as_ptr(),
        )
    };

    if have_sd {
        unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor as _) };
    }

    event
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_truncate_safely() {
        let mut buf = [0u8; NAME_LEN];
        encode_name(&mut buf, "Balanced");
        assert_eq!(decode_name(&buf), "Balanced");

        // Longer than the field, and the result must still be NUL terminated.
        let long = "x".repeat(NAME_LEN * 2);
        encode_name(&mut buf, &long);
        assert_eq!(decode_name(&buf).len(), NAME_LEN - 1);
        assert_eq!(buf[NAME_LEN - 1], 0);
    }

    #[test]
    fn a_zeroed_block_is_a_safe_resting_state() {
        // Fresh section pages are zeroed, and that has to mean BIOS mode with
        // no fault, not "manual level 0" with the firmware switched off.
        assert_eq!(MODE_BIOS, 0);
    }

    #[test]
    fn absent_sensors_are_distinguishable_from_cold_ones() {
        // 0 C is a legitimate reading, so absence needs its own value.
        assert_ne!(SENSOR_ABSENT, 0);
        assert!(SENSOR_ABSENT < -128);
    }

    #[test]
    fn the_shared_block_layout_is_ours_not_the_compilers() {
        // Both sides map the same bytes; the layout must be stable.
        assert!(std::mem::size_of::<Shared>() >= 2 * NAME_LEN);

        // Written out, because this is the one structure whose shape two
        // processes have to agree on without being able to check. Both status
        // and fan_write_declined went into padding the compiler was already
        // leaving, so nothing moved and nothing grew.
        assert_eq!(
            std::mem::size_of::<Shared>(),
            180,
            "the shared block changed size, so an older reader is now wrong about every field after the change"
        );
    }

    #[test]
    fn a_healthy_engine_is_the_zero_state() {
        // The section starts zeroed and is zeroed again on creation, so the
        // resting value has to be the harmless one.
        assert_eq!(STATUS_OK, 0);
        assert!(status_words(STATUS_OK, false).is_none());
    }

    #[test]
    fn each_kind_of_trouble_says_its_own_thing() {
        let all = [STATUS_UNREACHABLE, STATUS_SURRENDERED, STATUS_HANDBACK_FAILED];

        for (i, status) in all.iter().enumerate() {
            let (headline, detail) = status_words(*status, true).expect("a state with no words");
            assert!(!headline.is_empty() && !detail.is_empty());

            for other in &all[..i] {
                assert_ne!(
                    status_words(*other, true).unwrap().0,
                    headline,
                    "two states share a headline"
                );
            }
        }
    }

    #[test]
    fn a_failed_handback_reads_as_the_serious_one() {
        // This is the state where a fan may be pinned with the firmware's own
        // management switched off. It must not read like a passing glitch.
        let (headline, detail) = status_words(STATUS_HANDBACK_FAILED, true).unwrap();

        assert!(headline.to_lowercase().contains("held"), "{headline}");
        assert!(detail.to_lowercase().contains("firmware"), "{detail}");
        assert!(detail.to_lowercase().contains("restart"), "{detail}");
    }

    #[test]
    fn an_older_engine_that_only_sets_the_flag_still_gets_a_sentence() {
        // Its section has never had a status written into it, so the byte is
        // the zero that otherwise means healthy.
        assert!(status_words(STATUS_OK, true).is_some());
    }

    #[test]
    fn every_state_fits_in_a_tooltip() {
        // szTip is 128 wide characters including the terminator, and the tray
        // builds "Yamato", a headline and a detail out of these.
        for status in [STATUS_UNREACHABLE, STATUS_SURRENDERED, STATUS_HANDBACK_FAILED] {
            let (headline, detail) = status_words(status, true).unwrap();
            let tip = format!("Yamato\n{headline}\n{detail}");

            assert!(tip.encode_utf16().count() < 128, "{tip}");
        }
    }

    #[test]
    fn the_single_fan_hint_fits_beside_every_headline() {
        // The hint takes the detail line's place in the tooltip, so it has to
        // fit under whichever headline the trouble is wearing.
        for status in [STATUS_UNREACHABLE, STATUS_SURRENDERED, STATUS_HANDBACK_FAILED] {
            let (headline, _) = status_words(status, true).unwrap();
            let tip = format!("Yamato\n{headline}\n{SINGLE_FAN_HINT}");

            assert!(tip.encode_utf16().count() < 128, "{tip}");
        }
    }
}
