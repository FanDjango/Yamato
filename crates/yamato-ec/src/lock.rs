// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein
//
// The cross-process EC locks. Other ThinkPad tools take a mutex of this name
// before touching the controller, and so do we. On the alternate layout a
// second mutex joins it, and the two are always taken and released in one
// fixed order.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenMutexW, ReleaseMutex, WaitForSingleObject,
};

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

/// The right to wait on an object, and the right to let it go again. Together
/// they are the whole of what this file ever does with a mutex, which is why
/// a second-chance open can ask for so little and still arbitrate correctly.
/// Spelled out because windows-sys files SYNCHRONIZE under the file system,
/// where nobody looking for a mutex would think to find it.
const SYNCHRONIZE: u32 = 0x0010_0000;
const MUTEX_MODIFY_STATE: u32 = 0x0001;

pub struct EcLock {
    handle: AtomicPtr<c_void>,
    /// LpcIO's mutex, left null on the standard layout where it does not apply.
    isa: AtomicPtr<c_void>,
    /// Whether the ISA mutex applies at all here, so a null in that slot can
    /// be read as "not wanted" or "not obtained" without guessing.
    needs_isa: bool,
}

impl EcLock {
    /// The lock set the layout needs: `Access_EC` always, the ISA mutex only
    /// where LpcIO is underneath.
    pub fn acquire_handle(layout: Layout) -> Self {
        let needs_isa = layout.uses_isa_lock();

        EcLock {
            handle: AtomicPtr::new(create_shared_mutex(LOCK_NAME)),
            isa: AtomicPtr::new(if needs_isa {
                create_shared_mutex(ISA_LOCK_NAME)
            } else {
                ptr::null_mut()
            }),
            needs_isa,
        }
    }

    /// The handle for one lock, made now if the attempt at startup came up
    /// empty.
    ///
    /// Retried rather than settled for, because a handle that failed once is
    /// otherwise null for the life of the process, and every later decision
    /// inherits that one bad moment.
    fn ensure(slot: &AtomicPtr<c_void>, name: &str) -> HANDLE {
        let existing = slot.load(Ordering::Acquire);
        if !existing.is_null() {
            return existing;
        }

        let made = create_shared_mutex(name);
        if made.is_null() {
            return ptr::null_mut();
        }

        match slot.compare_exchange(ptr::null_mut(), made, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => made,
            // Another thread published one first. Theirs is what every other
            // waiter will see, so keep it and close the duplicate.
            Err(theirs) => {
                unsafe { CloseHandle(made) };

                theirs
            }
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
        // A handle we could not produce is not the same as a competitor who
        // is not there. CreateMutexW returns null here when the name already
        // exists under a descriptor that shuts us out, and create_shared_mutex
        // has already tried the smaller open that usually gets in anyway. What
        // is left is a lock somebody else is arbitrating on that we cannot
        // join: the one case where touching the controller unguarded is least
        // safe, not most. Refuse. release_to_bios is the path allowed to write
        // without a lock, and it already knows how.
        let ec = Self::ensure(&self.handle, LOCK_NAME);
        if ec.is_null() {
            return Err(Error::Busy);
        }

        match unsafe { WaitForSingleObject(ec, LOCK_TIMEOUT_MS) } {
            // Abandoned means the previous holder died mid-transaction. The
            // controller may have a byte in flight, but the lock is ours.
            WAIT_OBJECT_0 | WAIT_ABANDONED => {}
            _ => return Err(Error::Busy),
        }

        let isa = if self.needs_isa {
            let isa = Self::ensure(&self.isa, ISA_LOCK_NAME);
            if isa.is_null() {
                unsafe { ReleaseMutex(ec) };

                return Err(Error::Busy);
            }

            match unsafe { WaitForSingleObject(isa, LOCK_TIMEOUT_MS) } {
                WAIT_OBJECT_0 | WAIT_ABANDONED => isa,
                _ => {
                    // Refused halfway. Let the first one go rather than sit
                    // on it while reporting Busy: a holder that keeps what it
                    // could get is how two waiters starve each other.
                    unsafe { ReleaseMutex(ec) };

                    return Err(Error::Busy);
                }
            }
        } else {
            ptr::null_mut()
        };

        Ok(EcGuard { ec, isa, _lock: PhantomData })
    }
}

impl Drop for EcLock {
    fn drop(&mut self) {
        for slot in [&self.handle, &self.isa] {
            let handle = slot.load(Ordering::Acquire);
            if !handle.is_null() {
                unsafe { CloseHandle(handle) };
            }
        }
    }
}

pub struct EcGuard<'a> {
    /// The handles this guard actually took, rather than the slots they came
    /// from: what was acquired is what has to be released, whatever the slots
    /// say later.
    ec: HANDLE,
    isa: HANDLE,
    _lock: PhantomData<&'a EcLock>,
}

impl Drop for EcGuard<'_> {
    fn drop(&mut self) {
        // Reverse of acquisition: the ISA mutex goes first, Access_EC last.
        if !self.isa.is_null() {
            unsafe { ReleaseMutex(self.isa) };
        }

        unsafe { ReleaseMutex(self.ec) };
    }
}

/// The named mutex, made ours if it is new and joined if it is not.
///
/// Returns null only when the name can be neither created nor opened, which
/// callers have to read as "cannot arbitrate", never as "nobody else is here".
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

    if !handle.is_null() {
        return handle;
    }

    // The name is taken, by a descriptor that will not hand us everything.
    // Waiting and releasing is all we ever do with it, so ask for only that:
    // whoever got here first is still arbitrating on this exact object, and
    // sharing it is the entire point of agreeing on a name.
    unsafe { OpenMutexW(SYNCHRONIZE | MUTEX_MODIFY_STATE, 0, wide.as_ptr()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    use windows_sys::Win32::Foundation::WAIT_TIMEOUT;

    #[test]
    fn the_standard_layout_takes_one_lock_and_the_alternate_takes_two() {
        // The ISA mutex exists only where LpcIO is underneath. Creating it on
        // the standard path would be harmless today and a habit tomorrow, and
        // the ordering rule is easier to keep when only one path ever sees
        // the second handle.
        let standard = EcLock::acquire_handle(Layout::Standard);
        assert!(!standard.needs_isa);
        assert!(standard.isa.load(Ordering::Relaxed).is_null());

        let alternate = EcLock::acquire_handle(Layout::Alternate);
        assert!(alternate.needs_isa);
        assert!(!alternate.handle.load(Ordering::Relaxed).is_null());
        assert!(!alternate.isa.load(Ordering::Relaxed).is_null());

        // Both must actually be takeable, and re-takeable after release,
        // which is as much of the guard's lifecycle as a single process can
        // exercise.
        for lock in [&standard, &alternate] {
            drop(lock.lock().expect("uncontended locks must be grantable"));
            drop(lock.lock().expect("released locks must be grantable again"));
        }
    }

    #[test]
    fn a_name_that_cannot_be_had_reports_null_rather_than_a_usable_handle() {
        // The bug this pins down: a null handle used to mean "nobody can be
        // holding it either", and the EC got driven with no arbitration at
        // all. lock() now refuses on null, so the one thing this helper must
        // never do is invent a handle for a name it could not get. A trailing
        // separator is not a legal object name, so neither call can succeed.
        assert!(create_shared_mutex(r"Global\no\such\object").is_null());
    }

    #[test]
    fn the_ec_lock_is_shared_with_whoever_created_it_first() {
        // Someone else's mutex, made the way a tool that is not us would make
        // it. Ours has to join that object rather than fail and fall through.
        //
        // Deliberately not the real Access_EC: holding that here would stall
        // a running engine on this machine for the whole lock timeout.
        let name = r"Global\Yamato_lock_sharing_test";
        let first = create_shared_mutex(name);
        assert!(!first.is_null());

        let second = create_shared_mutex(name);
        assert!(!second.is_null(), "a name already taken must still be joinable");
        assert_ne!(first, second, "two opens are two handles onto one object");

        // Held here, contended from elsewhere. A mutex is owned by a thread
        // and not by a handle, so this thread could take it twice through
        // either handle and learn nothing about whether they are one object.
        assert_eq!(unsafe { WaitForSingleObject(first, 0) }, WAIT_OBJECT_0);

        let held = second as usize;
        let contended = std::thread::spawn(move || unsafe {
            WaitForSingleObject(held as HANDLE, 0)
        })
        .join()
        .expect("the contending thread must not panic");

        assert_eq!(
            contended, WAIT_TIMEOUT,
            "if the second handle can be taken while the first holds it, they are not the same lock"
        );

        unsafe { ReleaseMutex(first) };
        unsafe { CloseHandle(first) };
        unsafe { CloseHandle(second) };
    }
}
