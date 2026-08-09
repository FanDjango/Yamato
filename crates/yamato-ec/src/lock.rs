// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein
//
// The cross-process EC locks. Other ThinkPad tools take a mutex of this name
// before touching the controller, and so do we. On the alternate layout a
// second mutex joins it, and the two are always taken and released in one
// fixed order.

use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

use crate::pawnio::{Error, Layout};

/// The name every ThinkPad fan tool agrees on. Global, because the engine
/// usually runs as a service in session 0 and a window runs in the user's.
const LOCK_NAME: &str = r"Global\Access_EC";

/// The mutex LpcIO documents for its operations, which OpenRGB and
/// LibreHardwareMonitor honor around every SuperIO access. The module names
/// it as `\BaseNamedObjects\Access_ISABUS.HTP.Method`; from user mode the
/// `Global\` prefix resolves to exactly that directory.
///
/// Driving the EC protocol over LpcIO's ports is both an EC transaction and
/// an LpcIO operation, so both mutexes apply there. The order is fixed
/// everywhere in this crate: `Access_EC` first, then this, released in
/// reverse. Any code that took them the other way around could deadlock
/// against Vantage or OpenRGB doing the same two waits in our order, so
/// there is exactly one place that acquires either.
const ISA_LOCK_NAME: &str = r"Global\Access_ISABUS.HTP.Method";

/// Long enough that a slow EC handshake elsewhere finishes, short enough that
/// a wedged holder does not take us with it.
const LOCK_TIMEOUT_MS: u32 = 2_000;

/// Everyone gets access. A mutex created by SYSTEM with the default descriptor
/// cannot be opened from a user session, which silently turns one lock into two
/// that never see each other.
const SDDL_EVERYONE: &str = "D:(A;;GA;;;WD)";

pub struct EcLock {
    handle: HANDLE,
    /// LpcIO's mutex, null on the standard layout where it does not apply.
    isa: HANDLE,
}

unsafe impl Send for EcLock {}
unsafe impl Sync for EcLock {}

impl EcLock {
    /// The lock set the layout needs: `Access_EC` always, the ISA mutex only
    /// where LpcIO is underneath.
    pub fn acquire_handle(layout: Layout) -> Self {
        EcLock {
            handle: create_shared_mutex(LOCK_NAME),
            isa: if layout.uses_isa_lock() {
                create_shared_mutex(ISA_LOCK_NAME)
            } else {
                ptr::null_mut()
            },
        }
    }

    /// Takes the locks, or refuses to touch the controller.
    ///
    /// An EC transaction is a stateful byte sequence on two shared ports.
    /// Interleaving two of them corrupts both: an address byte landing where a
    /// value belongs writes a number nobody chose to a register nobody picked.
    /// Waiting or giving up is the only safe answer.
    ///
    /// `Access_EC` first, the ISA mutex second, never the other way. See
    /// ISA_LOCK_NAME for why the order is load-bearing.
    pub fn lock(&self) -> Result<EcGuard<'_>, Error> {
        // A null handle means the object could not be created, so nothing
        // else can be holding it either. Refusing here would only leave the
        // fan unmanaged.
        let held_ec = if self.handle.is_null() {
            false
        } else {
            match unsafe { WaitForSingleObject(self.handle, LOCK_TIMEOUT_MS) } {
                // Abandoned means the previous holder died mid-transaction.
                // The controller may have a byte in flight, but the lock is
                // ours.
                WAIT_OBJECT_0 | WAIT_ABANDONED => true,
                _ => return Err(Error::Busy),
            }
        };

        let held_isa = if self.isa.is_null() {
            false
        } else {
            match unsafe { WaitForSingleObject(self.isa, LOCK_TIMEOUT_MS) } {
                WAIT_OBJECT_0 | WAIT_ABANDONED => true,
                _ => {
                    // Refused halfway. Let the first one go rather than sit
                    // on it while reporting Busy: a holder that keeps what it
                    // could get is how two waiters starve each other.
                    if held_ec {
                        unsafe { ReleaseMutex(self.handle) };
                    }

                    return Err(Error::Busy);
                }
            }
        };

        Ok(EcGuard { lock: self, held_ec, held_isa })
    }
}

impl Drop for EcLock {
    fn drop(&mut self) {
        for handle in [self.handle, self.isa] {
            if !handle.is_null() {
                unsafe { CloseHandle(handle) };
            }
        }
    }
}

pub struct EcGuard<'a> {
    lock: &'a EcLock,
    held_ec: bool,
    held_isa: bool,
}

impl Drop for EcGuard<'_> {
    fn drop(&mut self) {
        // Reverse of acquisition: the ISA mutex goes first, Access_EC last.
        if self.held_isa {
            unsafe { ReleaseMutex(self.lock.isa) };
        }

        if self.held_ec {
            unsafe { ReleaseMutex(self.lock.handle) };
        }
    }
}

fn create_shared_mutex(name: &str) -> HANDLE {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let sddl: Vec<u16> = SDDL_EVERYONE.encode_utf16().chain(std::iter::once(0)).collect();

    let mut descriptor = ptr::null_mut();
    let have_sd = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } != 0;

    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };

    let handle = unsafe {
        CreateMutexW(
            if have_sd { &mut attributes } else { ptr::null_mut() },
            0,
            wide.as_ptr(),
        )
    };

    if have_sd {
        unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor as _) };
    }

    handle
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_layout_takes_one_lock_and_the_alternate_takes_two() {
        // The ISA mutex exists only where LpcIO is underneath. Creating it on
        // the standard path would be harmless today and a habit tomorrow, and
        // the ordering rule is easier to keep when only one path ever sees
        // the second handle.
        let standard = EcLock::acquire_handle(Layout::Standard);
        assert!(standard.isa.is_null());

        let alternate = EcLock::acquire_handle(Layout::Alternate);
        assert!(!alternate.handle.is_null());
        assert!(!alternate.isa.is_null());

        // Both must actually be takeable, and re-takeable after release,
        // which is as much of the guard's lifecycle as a single process can
        // exercise.
        for lock in [&standard, &alternate] {
            drop(lock.lock().expect("uncontended locks must be grantable"));
            drop(lock.lock().expect("released locks must be grantable again"));
        }
    }
}
