//! The ArcBox SSH server: `ssh <machine>@arcbox` from the Mac.
//!
//! The daemon runs it on a loopback port. A client authenticates with the
//! one key the daemon generated for it ([`SshKeys`]), and the SSH user name
//! selects the machine ([`Target`]). Sessions do not run an sshd in the
//! machine: each `shell`/`exec` becomes a login session on the machine exec
//! path — the guest agent runs the account's shell the way sshd would — with
//! `pty-req`, `env`, `window-change`, stdin, signals and the exit status
//! carried over its frames. [`MachineHost`] is the seam to the daemon's
//! runtime, and [`write_client_config`] the OpenSSH config clients use to
//! find the server.

mod client_config;
mod connection;
mod error;
mod host;
mod keys;
mod server;
mod session;
mod signal;
mod target;

pub use client_config::{KNOWN_HOSTS_FILE, remove_client_config, write_client_config};
pub use error::SetupError;
pub use host::{ExecOutput, MachineHost};
pub use keys::{CLIENT_KEY_FILE, HOST_KEY_FILE, SshKeys};
pub use server::SshServer;
pub use target::{InvalidTarget, Target};
