//! Whether the console user's session can take a VZ checkpoint right now.
//!
//! [`VirtualMachine::save_state`](crate::VirtualMachine::save_state)
//! protects the state file with a key it creates in the Secure Enclave on
//! every save, and the Secure Enclave refuses to create it
//! (`kIOReturnNotPermitted`) while the console session is locked
//! (`docs/experiments/2026-10-31-vz-checkpoint-spike.md` and its
//! 2026-11-01 follow-up). Ask [`SessionState::current`] before scheduling
//! a save instead of learning it from a failed one.

use std::fmt;

/// The console session as the IOKit registry reports it
/// (`IOConsoleUsers`, `IOConsoleLocked`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionState {
    /// A user is logged in at the console and the screen is unlocked.
    Unlocked,
    /// The console user's screen is locked.
    Locked,
    /// Nobody is logged in at the console (the login window, a headless
    /// host).
    NoConsoleUser,
}

impl SessionState {
    /// The session right now. Cheap: one registry read, no window-server
    /// connection, so it can be asked before every save.
    #[must_use]
    pub fn current() -> Self {
        // SAFETY: the shim reads two properties of the registry root; no
        // preconditions.
        match unsafe { crate::shim_ffi::abx_console_session_state() } {
            0 => Self::Unlocked,
            1 => Self::Locked,
            _ => Self::NoConsoleUser,
        }
    }

    /// Whether a save or restore can create or use its Secure Enclave key.
    #[must_use]
    pub fn allows_checkpoint(self) -> bool {
        self == Self::Unlocked
    }
}

impl fmt::Display for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unlocked => "the console session is unlocked",
            Self::Locked => "the console session is locked",
            Self::NoConsoleUser => "nobody is logged in at the console",
        })
    }
}
