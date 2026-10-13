//! Ctrl-C and termination handling.
//!
//! Outside critical sections, SIGINT and SIGTERM end the process at once
//! (exit status 130 or 143). Inside a critical section (an install or
//! upgrade transaction), the signal is only recorded: the operation stops at
//! its next [`check`] and is rolled back before the process exits.

use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use crate::util::Interrupted;

static INSTALL: Once = Once::new();
static DEFERRED: AtomicBool = AtomicBool::new(false);
static PENDING: AtomicI32 = AtomicI32::new(0);

/// Installs the handlers of SIGINT and SIGTERM (once).
pub fn install() {
    INSTALL.call_once(|| {
        for signal in [libc::SIGINT, libc::SIGTERM] {
            // SAFETY: the handler only uses atomics and _exit, which are
            // async-signal-safe
            let _ = unsafe {
                signal_hook_registry::register_unchecked(signal, move |_| {
                    if DEFERRED.load(Ordering::SeqCst) {
                        PENDING.store(signal, Ordering::SeqCst);
                    } else {
                        libc::_exit(128 + signal);
                    }
                })
            };
        }
    });
}

/// Starts or ends a critical section.
pub fn defer(on: bool) {
    DEFERRED.store(on, Ordering::SeqCst);
}

/// Fails with [`Interrupted`] if a signal arrived during the critical
/// section.
pub fn check() -> Result<(), Interrupted> {
    match PENDING.swap(0, Ordering::SeqCst) {
        0 => Ok(()),
        signal => Err(Interrupted(signal)),
    }
}

/// Signal received during the critical section, if any, without clearing it.
pub fn pending() -> Option<i32> {
    match PENDING.load(Ordering::SeqCst) {
        0 => None,
        signal => Some(signal),
    }
}

/// Sends a signal to the current process.
pub fn raise(signal: i32) {
    // SAFETY: plain system calls
    unsafe {
        libc::kill(libc::getpid(), signal);
    }
}
