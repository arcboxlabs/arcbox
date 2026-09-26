//! The ArcBox SSH server: `ssh <machine>@arcbox` from the Mac.
//!
//! The daemon runs it on a loopback port. A client authenticates with the
//! one key the daemon generated for it ([`SshKeys`]), and the SSH user name
//! selects the machine ([`Target`]).

mod connection;
mod keys;
mod server;
mod target;

pub use keys::{CLIENT_KEY_FILE, HOST_KEY_FILE, KeyError, SshKeys};
pub use server::SshServer;
pub use target::{InvalidTarget, Target};
