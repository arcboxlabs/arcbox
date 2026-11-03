//! QMP, QEMU's JSON control protocol, over the monitor socketpair.
//!
//! One reader thread owns the read half: replies go to the command waiting
//! for them, matched by the `id` each command carries, and events update
//! [`GuestEvents`]. With `-no-reboot` a guest reset
//! arrives as `SHUTDOWN` with reason `guest-reset` and QEMU then exits, so
//! the machine looks like HV after a PSCI SYSTEM_RESET: stopped, with the
//! reset flagged for the lifecycle to reboot it. A `STOP` that the host did
//! not ask for (QEMU pauses the guest on a host disk-full write error) is
//! logged, since nothing else would show it.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::{Result, VmmError};

/// How long a QMP command may take to answer.
pub(super) const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// What the guest did, as QEMU reports it.
pub(super) struct GuestEvents {
    /// The `Vmm`'s running flag; cleared when the guest shuts down.
    pub(super) running: Arc<AtomicBool>,
    /// Set by a guest reset.
    pub(super) reset_requested: Arc<AtomicBool>,
    /// Sent once on a guest power-off; dropped when the reader ends.
    pub(super) poweroff: crossbeam_channel::Sender<()>,
}

/// A QMP session past capability negotiation.
pub(super) struct Qmp {
    commands: Mutex<Commands>,
    reader: Option<std::thread::JoinHandle<()>>,
}

/// The write half and the replies, taken together by each command.
struct Commands {
    stream: UnixStream,
    replies: mpsc::Receiver<Reply>,
    /// The `id` of the last command sent.
    last_id: u64,
}

/// QEMU's answer to the command whose `id` it echoes.
#[derive(Debug, PartialEq)]
struct Reply {
    id: Option<u64>,
    result: std::result::Result<Value, String>,
}

impl Qmp {
    /// Starts reading `stream` and negotiates capabilities.
    ///
    /// # Errors
    ///
    /// Returns an error when QEMU closes the socket or does not answer
    /// within [`COMMAND_TIMEOUT`] — typically because it rejected its
    /// command line and exited.
    pub(super) fn connect(stream: UnixStream, events: GuestEvents) -> Result<Self> {
        let read_half = stream.try_clone()?;
        let (replies_tx, replies_rx) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("qemu-qmp".into())
            .spawn(move || read_messages(read_half, &replies_tx, &events))?;
        let qmp = Self {
            commands: Mutex::new(Commands {
                stream,
                replies: replies_rx,
                last_id: 0,
            }),
            reader: Some(reader),
        };
        qmp.execute("qmp_capabilities")?;
        Ok(qmp)
    }

    /// Runs `command` and returns its `return` value.
    ///
    /// # Errors
    ///
    /// Returns QEMU's error, or an error when it does not answer within
    /// [`COMMAND_TIMEOUT`]. A reply that comes after its command timed out
    /// is dropped when it arrives, never taken for a later command's.
    pub(super) fn execute(&self, command: &str) -> Result<Value> {
        let mut guard = self
            .commands
            .lock()
            .map_err(|_| VmmError::Device("QMP lock poisoned".into()))?;
        let commands = &mut *guard;
        commands.last_id += 1;
        let id = commands.last_id;
        let mut line = serde_json::to_vec(&serde_json::json!({ "execute": command, "id": id }))
            .map_err(|e| VmmError::Device(format!("QMP encode: {e}")))?;
        line.push(b'\n');
        commands.stream.write_all(&line)?;
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match commands.replies.recv_timeout(remaining) {
                Ok(reply) if reply.id == Some(id) => {
                    return reply
                        .result
                        .map_err(|message| VmmError::Device(format!("QMP {command}: {message}")));
                }
                Ok(late) => {
                    tracing::debug!(id = ?late.id, "QMP: dropping a reply nobody waits for");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(VmmError::Device(format!(
                        "QMP {command}: no reply within {COMMAND_TIMEOUT:?}"
                    )));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(VmmError::Device(format!(
                        "QMP {command}: QEMU closed the monitor"
                    )));
                }
            }
        }
    }

    /// Waits for the reader, which ends when QEMU closes the socket.
    pub(super) fn join(mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn read_messages(stream: UnixStream, replies: &mpsc::Sender<Reply>, events: &GuestEvents) {
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(e) => {
                tracing::warn!("QMP: unparsable message {line:?}: {e}");
                continue;
            }
        };
        match classify(&message) {
            Message::Greeting => {}
            Message::Reply(reply) => {
                let _ = replies.send(reply);
            }
            Message::Event(event) => apply(&event, events),
        }
    }
}

/// A QMP message, by what the session does with it.
#[derive(Debug, PartialEq)]
enum Message {
    Greeting,
    Reply(Reply),
    Event(Event),
}

/// The events the machine acts on.
#[derive(Debug, PartialEq)]
enum Event {
    /// The guest is going down; QEMU exits next. `reason` is QEMU's
    /// `ShutdownCause`, such as `guest-shutdown` or `guest-reset`.
    Shutdown { reason: String },
    /// The vCPUs stopped.
    Stop,
    /// Anything else, by name.
    Other(String),
}

fn classify(message: &Value) -> Message {
    if message.get("QMP").is_some() {
        return Message::Greeting;
    }
    let id = message.get("id").and_then(Value::as_u64);
    if let Some(value) = message.get("return") {
        return Message::Reply(Reply {
            id,
            result: Ok(value.clone()),
        });
    }
    if let Some(error) = message.get("error") {
        let desc = error
            .get("desc")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Message::Reply(Reply {
            id,
            result: Err(desc.to_owned()),
        });
    }
    let name = message
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Message::Event(match name {
        "SHUTDOWN" => Event::Shutdown {
            reason: message
                .pointer("/data/reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        },
        "STOP" => Event::Stop,
        other => Event::Other(other.to_owned()),
    })
}

fn apply(event: &Event, events: &GuestEvents) {
    match event {
        Event::Shutdown { reason } => {
            tracing::info!(reason, "QEMU guest shut down");
            match reason.as_str() {
                "guest-reset" => events.reset_requested.store(true, Ordering::SeqCst),
                "guest-shutdown" => {
                    let _ = events.poweroff.send(());
                }
                _ => {}
            }
            events.running.store(false, Ordering::SeqCst);
        }
        Event::Stop => tracing::warn!("QEMU stopped the guest's vCPUs"),
        Event::Other(name) => tracing::debug!(event = name, "QMP event"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn guest_events() -> (GuestEvents, crossbeam_channel::Receiver<()>) {
        let (poweroff, rx) = crossbeam_channel::unbounded();
        let events = GuestEvents {
            running: Arc::new(AtomicBool::new(true)),
            reset_requested: Arc::new(AtomicBool::new(false)),
            poweroff,
        };
        (events, rx)
    }

    fn shutdown(reason: &str) -> Event {
        let message = json!({
            "event": "SHUTDOWN",
            "data": { "guest": true, "reason": reason },
            "timestamp": { "seconds": 1, "microseconds": 2 }
        });
        let Message::Event(event) = classify(&message) else {
            panic!("an event classifies as one");
        };
        event
    }

    #[test]
    fn guest_poweroff_completes_the_wait_and_clears_running() {
        let (events, poweroff) = guest_events();
        apply(&shutdown("guest-shutdown"), &events);
        assert!(poweroff.try_recv().is_ok());
        assert!(!events.running.load(Ordering::SeqCst));
        assert!(!events.reset_requested.load(Ordering::SeqCst));
    }

    #[test]
    fn guest_reset_requests_a_reboot_and_is_no_poweroff() {
        let (events, poweroff) = guest_events();
        apply(&shutdown("guest-reset"), &events);
        assert!(events.reset_requested.load(Ordering::SeqCst));
        assert!(poweroff.try_recv().is_err());
        assert!(!events.running.load(Ordering::SeqCst));
    }

    /// `quit` and a signal also end in SHUTDOWN; neither is the guest's
    /// power-off, which a graceful stop must not mistake them for.
    #[test]
    fn host_shutdown_is_neither_poweroff_nor_reset() {
        let (events, poweroff) = guest_events();
        apply(&shutdown("host-qmp-quit"), &events);
        assert!(poweroff.try_recv().is_err());
        assert!(!events.reset_requested.load(Ordering::SeqCst));
        assert!(!events.running.load(Ordering::SeqCst));
    }

    /// A reply that comes after its command timed out carries that command's
    /// `id`; the command waiting now must get its own reply instead.
    #[test]
    fn a_late_reply_is_not_taken_for_a_later_command() {
        let (host, qemu) = UnixStream::pair().unwrap();
        let fake = std::thread::spawn(move || {
            let mut writer = qemu.try_clone().unwrap();
            let mut lines = BufReader::new(qemu).lines();
            writeln!(writer, r#"{{"QMP":{{"version":{{}},"capabilities":[]}}}}"#).unwrap();
            for answer in ["{}", r#""running""#] {
                let command: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
                let id = command["id"].as_u64().unwrap();
                writeln!(writer, r#"{{"return":"late","id":{}}}"#, id - 1).unwrap();
                writeln!(writer, r#"{{"return":{answer},"id":{id}}}"#).unwrap();
            }
        });
        let (events, _poweroff) = guest_events();
        let qmp = Qmp::connect(host, events).unwrap();
        assert_eq!(qmp.execute("query-status").unwrap(), json!("running"));
        fake.join().unwrap();
        qmp.join();
    }

    #[test]
    fn replies_and_greetings_are_told_from_events() {
        assert_eq!(
            classify(&json!({ "QMP": { "version": {}, "capabilities": [] } })),
            Message::Greeting
        );
        assert_eq!(
            classify(&json!({ "return": {}, "id": 7 })),
            Message::Reply(Reply {
                id: Some(7),
                result: Ok(json!({}))
            })
        );
        assert_eq!(
            classify(&json!({ "error": { "class": "GenericError", "desc": "nope" } })),
            Message::Reply(Reply {
                id: None,
                result: Err("nope".into())
            })
        );
        assert_eq!(
            classify(&json!({ "event": "STOP", "timestamp": {} })),
            Message::Event(Event::Stop)
        );
        assert_eq!(
            classify(&json!({ "event": "NIC_RX_FILTER_CHANGED", "data": {} })),
            Message::Event(Event::Other("NIC_RX_FILTER_CHANGED".into()))
        );
    }
}
