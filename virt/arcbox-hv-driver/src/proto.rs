//! The control protocol between the driver and an `arcbox-hv-vmm` process.
//!
//! One request per connection, answered by one response, except
//! [`Request::Watch`], whose connection then carries [`Event`]s until the
//! process exits. [`Response::Fd`] arrives with a descriptor attached
//! ([`crate::wire`]).

use std::net::Ipv4Addr;
use std::path::PathBuf;

use arcbox_vm_driver::{
    CheckpointImage, Error, ExitStatus, ForwardProtocol, ForwardRule, RestoreSpec, VmId, VmSpec,
    VmState,
};
use serde::{Deserialize, Serialize};

/// What the driver asks of the process.
#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    /// Who are you and where is your VM: answered by [`Response::Hello`].
    Hello,
    /// Boot `spec`; the process has no VM yet.
    Boot(Box<VmSpec>),
    /// Boot the checkpoint `image` with `spec` applied.
    Restore {
        image: CheckpointImage,
        spec: RestoreSpec,
    },
    /// Subscribe to [`Event`]s on this connection.
    Watch,
    /// Dial guest vsock `port`: answered by [`Response::Fd`].
    Dial(u32),
    /// Checkpoint into `dir`; `hold` leaves the guest quiesced.
    Checkpoint { dir: PathBuf, hold: bool },
    /// Set the balloon target, in bytes.
    BalloonTarget(u64),
    /// The balloon's `(target, current)`.
    BalloonStats,
    /// The VM's debug snapshot.
    Debug,
    /// A reader of the console pipe: answered by [`Response::Fd`].
    Console,
    /// Add a port forward: answered by [`Response::Port`].
    ForwardAdd(Forward),
    /// Remove a port forward.
    ForwardRemove(Forward),
    /// Remove every port forward.
    ForwardClear,
}

/// The process's answer.
#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    /// The process and its VM.
    Hello {
        pid: u32,
        id: VmId,
        /// `None` while no VM has booted.
        vm: Option<Booted>,
    },
    /// A VM is running: what it offers.
    Booted(Booted),
    /// Done.
    Ok,
    /// A descriptor came with this frame.
    Fd,
    /// The checkpoint written.
    Image(CheckpointImage),
    /// The balloon's target and current size, in bytes.
    Balloon { target: u64, current: Option<u64> },
    /// A debug snapshot.
    Debug(serde_json::Value),
    /// The host port a forward bound.
    Port(u16),
    /// The request failed.
    Error(WireError),
}

/// A running VM, as the process reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Booted {
    pub state: VmState,
    pub vsock: bool,
    pub balloon: bool,
    pub console: bool,
    pub port_forward: bool,
}

/// What the VM did on its own, on a [`Request::Watch`] connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// The guest asked for a reset.
    ResetRequested,
    /// The guest is gone and the process exits next, with this status.
    Exited(ExitStatus),
}

/// [`ForwardRule`] on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    pub udp: bool,
    pub host_ip: Ipv4Addr,
    pub host_port: u16,
    pub guest_port: u16,
}

impl From<ForwardRule> for Forward {
    fn from(rule: ForwardRule) -> Self {
        Self {
            udp: rule.protocol == ForwardProtocol::Udp,
            host_ip: rule.host_ip,
            host_port: rule.host_port,
            guest_port: rule.guest_port,
        }
    }
}

impl From<Forward> for ForwardRule {
    fn from(f: Forward) -> Self {
        Self {
            protocol: if f.udp {
                ForwardProtocol::Udp
            } else {
                ForwardProtocol::Tcp
            },
            host_ip: f.host_ip,
            host_port: f.host_port,
            guest_port: f.guest_port,
        }
    }
}

/// The port's [`Error`], as far as it crosses a process boundary: the
/// variants a caller branches on keep their shape, the rest keep their
/// message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireError {
    InvalidSpec(String),
    NotFound(VmId),
    WrongState {
        id: VmId,
        state: VmState,
        expected: String,
    },
    ForeignCheckpoint(String),
    Io {
        refused: bool,
        message: String,
    },
    Other(String),
}

impl From<&Error> for WireError {
    fn from(e: &Error) -> Self {
        match e {
            Error::InvalidSpec(s) => Self::InvalidSpec(s.clone()),
            Error::NotFound(id) => Self::NotFound(id.clone()),
            Error::WrongState {
                id,
                state,
                expected,
            } => Self::WrongState {
                id: id.clone(),
                state: *state,
                expected: (*expected).to_owned(),
            },
            Error::ForeignCheckpoint(f) => Self::ForeignCheckpoint(f.as_str().to_owned()),
            Error::Io(io) => Self::Io {
                refused: io.kind() == std::io::ErrorKind::ConnectionRefused,
                message: io.to_string(),
            },
            // The chain carries the cause; the receiver only gets text.
            other => Self::Other(chain(other)),
        }
    }
}

impl From<WireError> for Error {
    fn from(e: WireError) -> Self {
        match e {
            WireError::InvalidSpec(s) => Self::InvalidSpec(s),
            WireError::NotFound(id) => Self::NotFound(id),
            WireError::WrongState {
                id,
                state,
                expected,
            } => Self::WrongState {
                id,
                state,
                expected: expected_name(&expected),
            },
            WireError::ForeignCheckpoint(f) => {
                Self::ForeignCheckpoint(arcbox_vm_driver::CheckpointFormat::new(f))
            }
            WireError::Io { refused, message } => Self::Io(std::io::Error::new(
                if refused {
                    std::io::ErrorKind::ConnectionRefused
                } else {
                    std::io::ErrorKind::Other
                },
                message,
            )),
            WireError::Other(message) => Self::Driver {
                driver: crate::NAME,
                message,
                source: None,
            },
        }
    }
}

/// `WrongState::expected` is a `&'static str`; the names the engine uses
/// map back to themselves, anything else to a generic one.
fn expected_name(name: &str) -> &'static str {
    match name {
        "running" => "running",
        "quiesced" => "quiesced",
        _ => "another state",
    }
}

/// `e` and its sources, joined.
fn chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(e: Error) -> Error {
        let wire = WireError::from(&e);
        let json = serde_json::to_string(&wire).unwrap();
        serde_json::from_str::<WireError>(&json).unwrap().into()
    }

    #[test]
    fn errors_a_caller_branches_on_keep_their_shape() {
        let id = VmId::new("vm-1").unwrap();
        match round_trip(Error::WrongState {
            id: id.clone(),
            state: VmState::Quiesced,
            expected: "running",
        }) {
            Error::WrongState {
                id: got,
                state,
                expected,
            } => assert_eq!((got, state, expected), (id, VmState::Quiesced, "running")),
            other => panic!("{other}"),
        }
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        match round_trip(Error::Io(refused)) {
            Error::Io(io) => assert_eq!(io.kind(), std::io::ErrorKind::ConnectionRefused),
            other => panic!("{other}"),
        }
        assert!(matches!(
            round_trip(Error::ForeignCheckpoint(
                arcbox_vm_driver::CheckpointFormat::new("fc/1")
            )),
            Error::ForeignCheckpoint(f) if f.as_str() == "fc/1"
        ));
    }

    #[test]
    fn a_driver_error_keeps_its_cause_in_the_message() {
        let e = Error::Driver {
            driver: "hv",
            message: "restore".into(),
            source: Some(Box::new(std::io::Error::other("state.json: bad build"))),
        };
        match round_trip(e) {
            Error::Driver { message, .. } => {
                assert!(message.contains("state.json: bad build"), "{message}");
            }
            other => panic!("{other}"),
        }
    }
}
