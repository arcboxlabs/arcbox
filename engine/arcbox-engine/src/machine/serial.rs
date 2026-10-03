//! Serial drain for a VM's console and agent-log pipes.
//!
//! Every VZ machine gets one for as long as it runs. The console pipes are
//! the only sink for what the guest writes to `hvc0`/`hvc1`, and VZ's serial
//! attachment blocks the guest's virtio-console queue once the host pipe is
//! full: the guest's console write then never completes, the vCPUs spin on
//! the stalled queue at 100% each, and the guest's vsock side stops answering
//! too (measured 2026-09-28, a Debian machine whose `agetty` on `hvc0` filled
//! the pipe within a day). Draining is therefore load-bearing for every VM,
//! not a logging nicety for the System VM.
//!
//! The drain is readiness-driven: each port is an [`AsyncFd`] task that
//! reads whenever the pipe holds bytes and otherwise sleeps in the reactor,
//! so an idle machine costs nothing and a flood drains at pipe speed instead
//! of one pipe per poll interval (the pipe is not always 64 KiB — XNU hands
//! out 512-byte buffers under host pipe-memory pressure). It ends on the
//! cancellation the manager fires when the machine stops, or on EOF once
//! the VZ helper — the last holder of the pipe's write end after the VM
//! has started — exits.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex, PoisonError};

use arcbox_vmm::SerialReaders;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio_util::sync::CancellationToken;

use super::DEFAULT_MACHINE_NAME;

/// An unterminated line longer than this is dropped rather than buffered.
const MAX_LINE_BUF: usize = 64 * 1024;
/// Lines each port keeps for [`DrainHandle::tail`].
const TAIL_LINES: usize = 40;
/// Bytes per `read(2)`.
const READ_CHUNK: usize = 16 * 1024;

/// A running drain. Dropping it stops both port tasks.
pub(super) struct DrainHandle {
    cancel: CancellationToken,
    console: Arc<TailRing>,
    agent_log: Arc<TailRing>,
}

impl DrainHandle {
    /// The last lines seen on the console and agent-log ports, oldest first.
    pub(super) fn tail(&self) -> (Vec<String>, Vec<String>) {
        (self.console.lines(), self.agent_log.lines())
    }
}

impl Drop for DrainHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Starts draining `readers` for `machine` on the current tokio runtime.
pub(super) fn spawn(machine: &str, readers: SerialReaders) -> DrainHandle {
    let cancel = CancellationToken::new();
    let is_default = machine == DEFAULT_MACHINE_NAME;
    // Only the System VM's console is worth INFO: a user machine's getty
    // and journal chatter is diagnostic, not operational.
    let console = Port::new(
        if is_default {
            "Guest".to_owned()
        } else {
            format!("Guest[{machine}]")
        },
        is_default,
    );
    let agent_log = Port::new(
        if is_default {
            "Agent".to_owned()
        } else {
            format!("Agent[{machine}]")
        },
        false,
    );
    let handle = DrainHandle {
        cancel: cancel.clone(),
        console: Arc::clone(&console.tail),
        agent_log: Arc::clone(&agent_log.tail),
    };
    tokio::spawn(drain_port(readers.console, console, cancel.clone()));
    tokio::spawn(drain_port(readers.agent_log, agent_log, cancel));
    handle
}

/// One port's label, log level and retained tail.
struct Port {
    label: String,
    info: bool,
    tail: Arc<TailRing>,
}

impl Port {
    fn new(label: String, info: bool) -> Self {
        Self {
            label,
            info,
            tail: Arc::new(TailRing::default()),
        }
    }

    fn emit(&self, line: &str) {
        if self.info {
            tracing::info!("{}: {line}", self.label);
        } else {
            tracing::debug!("{}: {line}", self.label);
        }
        self.tail.push(line);
    }
}

/// Reads `fd` until cancelled or closed, logging complete lines.
async fn drain_port(fd: OwnedFd, port: Port, cancel: CancellationToken) {
    let fd = match AsyncFd::with_interest(fd, Interest::READABLE) {
        Ok(fd) => fd,
        Err(e) => {
            tracing::warn!("{}: cannot watch the console pipe: {e}", port.label);
            return;
        }
    };
    let mut splitter = LineSplitter::new(MAX_LINE_BUF);
    let mut chunk = vec![0u8; READ_CHUNK];
    loop {
        let mut guard = tokio::select! {
            () = cancel.cancelled() => break,
            ready = fd.readable() => match ready {
                Ok(guard) => guard,
                Err(e) => {
                    tracing::warn!("{}: console pipe readiness failed: {e}", port.label);
                    break;
                }
            },
        };
        let closed = loop {
            match guard.try_io(|inner| read_some(inner.as_raw_fd(), &mut chunk)) {
                Ok(Ok(0)) => break true,
                Ok(Ok(n)) => {
                    let split = splitter.push(&chunk[..n]);
                    for line in &split.lines {
                        port.emit(line);
                    }
                    if split.overflowed {
                        tracing::warn!("{}: line buffer overflow, flushing", port.label);
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!("{}: console pipe read failed: {e}", port.label);
                    break true;
                }
                Err(_would_block) => break false,
            }
        };
        if closed {
            break;
        }
    }
    if let Some(line) = splitter.flush() {
        port.emit(&line);
    }
    tracing::debug!("{}: serial drain stopped", port.label);
}

fn read_some(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `fd` is the live non-blocking pipe fd the `AsyncFd` owns and
    // `buf` is valid for `buf.len()` bytes for the duration of the call.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    usize::try_from(n).map_err(|_| io::Error::last_os_error())
}

/// The most recent [`TAIL_LINES`] lines of one port.
#[derive(Default)]
struct TailRing(Mutex<VecDeque<String>>);

impl TailRing {
    fn push(&self, line: &str) {
        let mut lines = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() == TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line.to_owned());
    }

    fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }
}

/// Splits a byte stream into console lines, keeping an unterminated tail
/// between pushes so a character split across two reads stays whole.
struct LineSplitter {
    buf: Vec<u8>,
    max: usize,
}

struct Split {
    lines: Vec<String>,
    /// The unterminated tail exceeded the bound and was dropped.
    overflowed: bool,
}

impl LineSplitter {
    fn new(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            max,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Split {
        // NUL padding is never text; dropping it here keeps a NUL flood
        // from counting against the unterminated-line bound.
        self.buf.extend(bytes.iter().copied().filter(|&b| b != 0));
        let mut lines = Vec::new();
        let mut start = 0;
        while let Some(pos) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let end = start + pos;
            if let Some(line) = line_text(&self.buf[start..end]) {
                lines.push(line);
            }
            start = end + 1;
        }
        self.buf.drain(..start);
        // Only an unterminated line can grow without bound; complete lines
        // were just consumed.
        let overflowed = self.buf.len() > self.max;
        if overflowed {
            self.buf.clear();
        }
        Split { lines, overflowed }
    }

    /// The unterminated tail, if it says anything.
    fn flush(&mut self) -> Option<String> {
        let line = line_text(&self.buf);
        self.buf.clear();
        line
    }
}

/// One console line as logged: trailing whitespace and `\r` trimmed, lossily
/// decoded; `None` when nothing printable is left.
fn line_text(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let text = text.trim_end();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use std::os::fd::FromRawFd;
    use std::time::Duration;

    use super::*;

    /// A pipe with a non-blocking read end, as `setup_serial_console` makes it.
    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` is a valid two-element array; the fds are owned below.
        unsafe {
            assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
            let flags = libc::fcntl(fds[0], libc::F_GETFL);
            assert_ne!(
                libc::fcntl(fds[0], libc::F_SETFL, flags | libc::O_NONBLOCK),
                -1
            );
            (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
        }
    }

    /// Writes `bytes` whole; the payload always fits a minimum-size pipe.
    fn write(fd: &OwnedFd, bytes: &[u8]) {
        assert!(
            bytes.len() <= 512,
            "keep test payloads inside a 512-byte pipe"
        );
        // SAFETY: `fd` is a live pipe write end and `bytes` is valid.
        let n = unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(usize::try_from(n), Ok(bytes.len()));
    }

    async fn wait_for_lines(tail: &TailRing, n: usize) -> Vec<String> {
        for _ in 0..500 {
            let lines = tail.lines();
            if lines.len() >= n {
                return lines;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("drain logged {:?}, wanted {n} lines", tail.lines());
    }

    fn port() -> (Port, Arc<TailRing>) {
        let port = Port::new("T".to_owned(), false);
        let tail = Arc::clone(&port.tail);
        (port, tail)
    }

    #[tokio::test]
    async fn lines_split_across_writes_arrive_whole_and_in_order() {
        let (r, w) = pipe();
        let (port, tail) = port();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(drain_port(r, port, cancel.clone()));

        write(&w, b"abc");
        write(&w, b"def\n");
        let zhong = "\u{4e2d}".as_bytes();
        write(&w, &zhong[..2]);
        tokio::time::sleep(Duration::from_millis(20)).await;
        write(&w, &zhong[2..]);
        write(&w, b"\r\n");

        assert_eq!(wait_for_lines(&tail, 2).await, ["abcdef", "\u{4e2d}"]);
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_flushes_the_partial_line_and_ends_the_task() {
        let (r, w) = pipe();
        let (port, tail) = port();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(drain_port(r, port, cancel.clone()));

        write(&w, b"no newline yet");
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("cancelled drain ends")
            .unwrap();
        assert_eq!(tail.lines(), ["no newline yet"]);
    }

    #[tokio::test]
    async fn closing_the_last_writer_ends_the_task() {
        let (r, w) = pipe();
        let (port, tail) = port();
        let task = tokio::spawn(drain_port(r, port, CancellationToken::new()));

        write(&w, b"last words\n");
        drop(w);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("drain ends on EOF")
            .unwrap();
        assert_eq!(tail.lines(), ["last words"]);
    }

    #[test]
    fn a_burst_of_complete_lines_is_not_an_overflow() {
        let mut splitter = LineSplitter::new(64);
        let burst: Vec<u8> = b"line\n".repeat(100);
        let split = splitter.push(&burst);
        assert_eq!(split.lines.len(), 100);
        assert!(!split.overflowed);
        assert!(
            splitter.flush().is_none(),
            "every complete line was consumed"
        );
    }

    #[test]
    fn an_unterminated_line_past_the_bound_is_dropped_and_reported() {
        let mut splitter = LineSplitter::new(64);
        let split = splitter.push(&[b'x'; 65]);
        assert_eq!(split.lines, Vec::<String>::new());
        assert!(split.overflowed);
        assert!(splitter.flush().is_none());
    }

    #[test]
    fn nul_padding_is_neither_output_nor_buffered() {
        let mut splitter = LineSplitter::new(64);
        assert_eq!(splitter.push(b"\0\0\n").lines, Vec::<String>::new());
        assert_eq!(splitter.push(b"\0abc\0\n").lines, ["abc"]);
        let split = splitter.push(&[0u8; 200]);
        assert_eq!(split.lines, Vec::<String>::new());
        assert!(!split.overflowed, "a NUL flood is not an unterminated line");
        assert!(splitter.flush().is_none());
    }

    #[test]
    fn the_tail_keeps_only_the_newest_lines() {
        let tail = TailRing::default();
        for i in 0..TAIL_LINES + 5 {
            tail.push(&i.to_string());
        }
        let lines = tail.lines();
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines.first().map(String::as_str), Some("5"));
        assert_eq!(
            lines.last().map(String::as_str),
            Some((TAIL_LINES + 4).to_string().as_str())
        );
    }
}
