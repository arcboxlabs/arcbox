//! The guest console of the process's VM, out of the machine's log lines.
//!
//! The HV machine logs what the guest writes to its consoles as `tracing`
//! events (`guest_console` for hvc0, `guest_serial` for the PL011). A layer
//! copies those lines into the sink the spec asked for: a file, or a pipe
//! the driver reads through the `Console` capability. Writes never block a
//! vCPU: a pipe nobody drains drops the lines that do not fit.

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// The targets the machine logs guest console output under.
pub const TARGETS: [&str; 2] = ["guest_console", "guest_serial"];

/// Where console lines go, once a VM with a console has booted.
static SINK: OnceLock<Sink> = OnceLock::new();

struct Sink {
    out: Mutex<File>,
    /// The pipe's read end, kept for the driver; `None` for a file.
    reader: Option<OwnedFd>,
}

/// Sends console lines to `path`, appended.
pub fn to_file(path: &Path) -> std::io::Result<()> {
    let file = File::options().create(true).append(true).open(path)?;
    install(file, None)
}

/// Sends console lines into a pipe; [`reader`] hands out its read end.
pub fn to_pipe() -> std::io::Result<()> {
    let (read, write) = nix::unistd::pipe()?;
    for fd in [&read, &write] {
        set_flag(fd.as_fd(), libc::FD_CLOEXEC, libc::F_GETFD, libc::F_SETFD)?;
    }
    set_flag(
        write.as_fd(),
        libc::O_NONBLOCK,
        libc::F_GETFL,
        libc::F_SETFL,
    )?;
    install(File::from(write), Some(read))
}

fn install(out: File, reader: Option<OwnedFd>) -> std::io::Result<()> {
    SINK.set(Sink {
        out: Mutex::new(out),
        reader,
    })
    .map_err(|_| std::io::Error::other("the console is already set up"))
}

/// The pipe's read end, when the console is a pipe.
pub fn reader() -> Option<BorrowedFd<'static>> {
    SINK.get()?.reader.as_ref().map(AsFd::as_fd)
}

fn set_flag(fd: BorrowedFd<'_>, flag: i32, get: i32, set: i32) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: fcntl on a descriptor we hold, with integer arguments only.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), get) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(fd.as_raw_fd(), set, flags | flag) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Copies console events into the sink.
pub struct ConsoleLayer;

impl<S: Subscriber> Layer<S> for ConsoleLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let Some(sink) = SINK.get() else { return };
        let mut line = Line(String::new());
        event.record(&mut line);
        line.0.push('\n');
        let mut out = sink
            .out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A full pipe refuses the line; the console is best-effort.
        let _ = out.write_all(line.0.as_bytes());
    }
}

struct Line(String);

impl Visit for Line {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            use std::fmt::Write as _;
            let _ = write!(self.0, "{value:?}");
        }
    }
}
