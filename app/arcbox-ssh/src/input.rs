//! Client input on its way to a session's process, and when the connection
//! may wait for it.
//!
//! russh returns window to a client as soon as data arrives, so the only
//! backpressure a client ever sees is its connection's handler taking time.
//! The same connection carries every channel's output, and output waiting
//! on a client window or on russh's queue needs that connection to move: a
//! handler that waits for a process which is itself waiting to write its
//! output never returns. So input queues without bound for the handler, and
//! the handler waits only while the queue is past [`HIGH_WATER`] and no
//! output is blocked on the connection. Past [`LIMIT`] — a process not
//! reading while its client reads slower than it writes — the session is
//! ended rather than the daemon's memory.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::Poll;

use arcbox_engine::agent_client::ExecSessionInput;
use tokio::sync::{Notify, mpsc};

/// Queued stdin past which the connection waits for the process.
const HIGH_WATER: usize = 1024 * 1024;

/// Queued stdin past which a session is ended.
pub const LIMIT: usize = 64 * 1024 * 1024;

/// A session's queued input passed [`LIMIT`].
#[derive(Debug)]
pub struct Overflow;

/// Output sends of one connection that are blocked on it.
#[derive(Default)]
pub struct Outbound {
    blocked: AtomicUsize,
    /// Raised when a send blocks.
    blocked_one: Notify,
}

impl Outbound {
    /// Runs an output send. A send that cannot complete at once is waiting
    /// for the connection — for client window, or for room in russh's queue
    /// — and counts as blocked until it does.
    pub async fn send<F: Future>(&self, send: F) -> F::Output {
        let mut send = pin!(send);
        if let Poll::Ready(done) =
            std::future::poll_fn(|cx| Poll::Ready(send.as_mut().poll(cx))).await
        {
            return done;
        }
        let _blocked = Blocked::new(self);
        send.await
    }

    fn any_blocked(&self) -> bool {
        self.blocked.load(Ordering::Acquire) > 0
    }
}

/// Counts one blocked send for as long as it lives.
struct Blocked<'a>(&'a Outbound);

impl<'a> Blocked<'a> {
    fn new(outbound: &'a Outbound) -> Self {
        outbound.blocked.fetch_add(1, Ordering::AcqRel);
        outbound.blocked_one.notify_waiters();
        Self(outbound)
    }
}

impl Drop for Blocked<'_> {
    fn drop(&mut self) {
        self.0.blocked.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The input side of one session.
pub struct SessionInput {
    queue: mpsc::UnboundedSender<ExecSessionInput>,
    backlog: Arc<Backlog>,
}

#[derive(Default)]
struct Backlog {
    /// Stdin bytes queued and not yet handed to the process.
    bytes: AtomicUsize,
    /// Raised whenever input is handed on, and when the process is gone.
    moved: Notify,
    /// The process no longer takes input.
    closed: AtomicBool,
}

impl SessionInput {
    /// Starts handing input to `process` as fast as it takes it.
    pub fn new(process: mpsc::Sender<ExecSessionInput>) -> Self {
        let (queue, pending) = mpsc::unbounded_channel();
        let backlog = Arc::new(Backlog::default());
        tokio::spawn(hand_on(pending, process, Arc::clone(&backlog)));
        Self { queue, backlog }
    }

    /// Queues `input`. Stdin past [`HIGH_WATER`] then waits for the process
    /// to catch up — unless output is blocked on `outbound`, which needs the
    /// caller back.
    ///
    /// # Errors
    ///
    /// Returns [`Overflow`] once the queue passes [`LIMIT`].
    pub async fn send(&self, input: ExecSessionInput, outbound: &Outbound) -> Result<(), Overflow> {
        let len = stdin_len(&input);
        let queued = self.backlog.bytes.fetch_add(len, Ordering::AcqRel) + len;
        // A closed queue means the process is gone; its input no longer
        // matters.
        let _ = self.queue.send(input);
        if queued > LIMIT {
            return Err(Overflow);
        }
        if len > 0 {
            self.backlog.wait_for_room(outbound).await;
        }
        Ok(())
    }
}

impl Backlog {
    async fn wait_for_room(&self, outbound: &Outbound) {
        loop {
            // Registered before the checks, so a change between a check and
            // the wait still wakes it.
            let mut moved = pin!(self.moved.notified());
            let mut blocked = pin!(outbound.blocked_one.notified());
            moved.as_mut().enable();
            blocked.as_mut().enable();
            if self.bytes.load(Ordering::Acquire) <= HIGH_WATER
                || self.closed.load(Ordering::Acquire)
                || outbound.any_blocked()
            {
                return;
            }
            tokio::select! {
                () = moved => {}
                () = blocked => {}
            }
        }
    }
}

/// Hands queued input to the process in order until it stops taking any.
async fn hand_on(
    mut pending: mpsc::UnboundedReceiver<ExecSessionInput>,
    process: mpsc::Sender<ExecSessionInput>,
    backlog: Arc<Backlog>,
) {
    while let Some(input) = pending.recv().await {
        let len = stdin_len(&input);
        let taken = process.send(input).await;
        backlog.bytes.fetch_sub(len, Ordering::AcqRel);
        backlog.moved.notify_waiters();
        if taken.is_err() {
            break;
        }
    }
    backlog.closed.store(true, Ordering::Release);
    backlog.moved.notify_waiters();
}

fn stdin_len(input: &ExecSessionInput) -> usize {
    match input {
        ExecSessionInput::Stdin(data) => data.len(),
        ExecSessionInput::Resize { .. } | ExecSessionInput::Signal(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const CHUNK: usize = 256 * 1024;

    fn stdin() -> ExecSessionInput {
        ExecSessionInput::Stdin(vec![0; CHUNK])
    }

    /// Queues stdin until the queue is past [`HIGH_WATER`], into a process
    /// that takes none.
    async fn backlogged(input: &SessionInput, outbound: &Outbound) {
        while input.backlog.bytes.load(Ordering::Acquire) <= HIGH_WATER {
            let queued =
                tokio::time::timeout(Duration::from_millis(100), input.send(stdin(), outbound))
                    .await;
            if queued.is_err() {
                return;
            }
        }
    }

    async fn completes<F: Future>(future: F) -> bool {
        tokio::time::timeout(Duration::from_millis(100), future)
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn stdin_past_the_high_water_mark_waits_for_the_process() {
        let (process, mut taken) = mpsc::channel(1);
        let input = SessionInput::new(process);
        let outbound = Outbound::default();
        backlogged(&input, &outbound).await;

        let send = input.send(stdin(), &outbound);
        let mut send = pin!(send);
        assert!(
            !completes(send.as_mut()).await,
            "the queue is past the mark"
        );
        while input.backlog.bytes.load(Ordering::Acquire) > HIGH_WATER {
            taken.recv().await.unwrap();
        }
        assert!(completes(send).await, "the process caught up");
    }

    #[tokio::test]
    async fn control_input_never_waits() {
        let (process, _taken) = mpsc::channel(1);
        let input = SessionInput::new(process);
        let outbound = Outbound::default();
        backlogged(&input, &outbound).await;

        let resize = ExecSessionInput::Resize {
            width: 80,
            height: 24,
        };
        assert!(completes(input.send(resize, &outbound)).await);
    }

    #[tokio::test]
    async fn output_blocked_on_the_connection_releases_waiting_input() {
        let (process, _taken) = mpsc::channel(1);
        let input = SessionInput::new(process);
        let outbound = Outbound::default();
        backlogged(&input, &outbound).await;

        let send = input.send(stdin(), &outbound);
        let mut send = pin!(send);
        assert!(!completes(send.as_mut()).await);
        // An output send that cannot complete: the handler must go back to
        // the connection for it.
        let never = outbound.send(std::future::pending::<()>());
        let mut never = pin!(never);
        assert!(!completes(never.as_mut()).await);
        assert!(completes(send).await);
        // And input does not wait while it stays blocked.
        assert!(completes(input.send(stdin(), &outbound)).await);
    }

    #[tokio::test]
    async fn output_that_completes_at_once_does_not_count_as_blocked() {
        let outbound = Outbound::default();
        outbound.send(std::future::ready(())).await;
        assert!(!outbound.any_blocked());
    }

    #[tokio::test]
    async fn a_session_past_the_limit_overflows() {
        let (process, _taken) = mpsc::channel(1);
        let input = SessionInput::new(process);
        let outbound = Outbound::default();
        let never = outbound.send(std::future::pending::<()>());
        let mut never = pin!(never);
        assert!(!completes(never.as_mut()).await);

        let chunk = vec![0; 4 * 1024 * 1024];
        let mut queued = 0;
        loop {
            queued += chunk.len();
            let send = input.send(ExecSessionInput::Stdin(chunk.clone()), &outbound);
            let sent = tokio::time::timeout(Duration::from_secs(1), send)
                .await
                .expect("input does not wait while output is blocked");
            if sent.is_err() {
                break;
            }
        }
        assert!(queued > LIMIT);
    }

    #[tokio::test]
    async fn a_process_that_is_gone_releases_waiting_input() {
        let (process, taken) = mpsc::channel(1);
        let input = SessionInput::new(process);
        let outbound = Outbound::default();
        backlogged(&input, &outbound).await;

        let send = input.send(stdin(), &outbound);
        let mut send = pin!(send);
        assert!(!completes(send.as_mut()).await);
        drop(taken);
        assert!(completes(send).await);
    }
}
