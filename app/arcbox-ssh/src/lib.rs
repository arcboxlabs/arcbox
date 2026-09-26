//! The ArcBox SSH server: `ssh <machine>@arcbox` from the Mac.
//!
//! A client authenticates with the one key the daemon generated for it
//! ([`SshKeys`]), and the SSH user name selects the machine ([`Target`]).

mod keys;
mod target;

pub use keys::{CLIENT_KEY_FILE, HOST_KEY_FILE, KeyError, SshKeys};
pub use target::{InvalidTarget, Target};
