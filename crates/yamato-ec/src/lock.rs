// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein
//
// The cross-process EC lock. Other ThinkPad tools take a mutex of this name
// before touching the controller, and so do we.

use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

use crate::pawnio::Error;

/// The name every ThinkPad fan tool agrees on. Global, because the engine
/// usually runs as a service in session 0 and a window runs in the user's.
const LOCK_NAME: &str = r"Global\Access_EC";

/// Long enough that a slow EC handshake elsewhere finishes, short enough that
/// a wedged holder does not take us with it.
const LOCK_TIMEOUT_MS: u32 = 2_000;

/// Everyone gets access. A mutex created by SYSTEM with the default descriptor
/// cannot be opened from a user session, which silently turns one lock into two
/// that never see each other.
const SDDL_EVERYONE: &str = "D:(A;;GA;;;WD)";

pub struct EcLock {
    handle: HANDLE,
}

unsafe impl Send for EcLock {}
unsafe impl Sync for EcLock {}

impl EcLock {
    pub fn acquire_handle() -> Self {
        EcLock { handle: create_shared_mutex(LOCK_NAME) }
    }

    /// Takes the lock, or refuses to touch the controller.
    ///
    /// An EC transaction is a stateful byte sequence on two shared ports.
    /// Interleaving two of them corrupts both: an address byte landing where a
    /// value belongs writes a number nobody chose to a register nobody picked.
    /// Waiting or giving up is the only safe answer.
    pub fn lock(&self) -> Result<EcGuard<'_>, Error> {
        if self.handle.is_null() {
            // No lock object, so nothing else can be holding it either.
            // Refusing here would only leave the fan unmanaged.
            return Ok(EcGuard { lock: self, held: false });
        }

        match unsafe { WaitForSingleObject(self.handle, LOCK_TIMEOUT_MS) } {
            // Abandoned means the previous holder died mid-transaction. The
            // controller may have a byte in flight, but the lock is ours.
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(EcGuard { lock: self, held: true }),
            _ => Err(Error::Busy),
        }
    }
}

impl Drop for EcLock {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

pub struct EcGuard<'a> {
    lock: &'a EcLock,
    held: bool,
}

impl Drop for EcGuard<'_> {
    fn drop(&mut self) {
        if self.held {
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
