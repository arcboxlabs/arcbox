//! Byte credit for an application-level flow-control window.
//!
//! A vsock connection the host stops reading stalls every new connection
//! to that VM on Virtualization.framework, so a stream's backpressure must
//! never come from leaving bytes in the socket. Each direction carries a
//! window instead: the receiver grants the sender room, the sender never
//! sends past it, and both sides can always drain the connection into
//! memory bounded by that window. [`Credit`] is the counter both sides
//! keep — the sender's view of what it may still send, and the receiver's
//! view of what the peer may still send it.

use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

/// Window bytes still open, bounded by the window's size.
#[derive(Debug)]
pub struct Credit {
    available: AtomicUsize,
    limit: usize,
    /// No more grants can come: the peer is gone.
    closed: AtomicBool,
    /// A sender waiting for the window to reopen.
    waiter: Mutex<Option<Waker>>,
}

impl Credit {
    /// A window of `limit` bytes, fully open.
    pub fn new(limit: usize) -> Self {
        Self {
            available: AtomicUsize::new(limit),
            limit,
            closed: AtomicBool::new(false),
            waiter: Mutex::new(None),
        }
    }

    /// Marks the window as one nothing will reopen — the peer that grants
    /// it is gone — and fails every sender waiting on it, now and later.
    /// Bytes still open stay usable.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.wake();
    }

    /// Bytes still open.
    pub fn available(&self) -> usize {
        self.available.load(Ordering::Acquire)
    }

    /// Takes up to `want` bytes (at least one) once any are open. The
    /// sender's side: the caller sends only what this returns. Fails once
    /// the window is closed with nothing open: no grant can come.
    pub fn poll_take(&self, cx: &mut Context<'_>, want: usize) -> Poll<io::Result<usize>> {
        debug_assert!(want > 0);
        if let Some(taken) = self.try_take(want) {
            return Poll::Ready(Ok(taken));
        }
        *self.waiter.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
        // A grant or close between the first attempt and the waker store
        // must not be lost.
        if let Some(taken) = self.try_take(want) {
            return Poll::Ready(Ok(taken));
        }
        if self.closed.load(Ordering::Acquire) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "peer gone while waiting for flow-control window",
            )));
        }
        Poll::Pending
    }

    fn try_take(&self, want: usize) -> Option<usize> {
        let mut taken = 0;
        self.available
            .try_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                taken = left.min(want);
                (taken > 0).then_some(left - taken)
            })
            .ok()
            .map(|_| taken)
    }

    /// Takes exactly `len` bytes the peer just sent. The receiver's side:
    /// a peer sending past the window it holds is a protocol error.
    pub fn take(&self, len: usize) -> io::Result<()> {
        self.available
            .try_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(len)
            })
            .map(drop)
            .map_err(|_| protocol_error("peer sent past its flow-control window"))
    }

    /// Reopens `len` bytes: on the sender's side the peer's grant, on the
    /// receiver's side what the consumer took, to be granted to the peer.
    /// Growing past the window's size is a protocol error.
    pub fn grant(&self, len: usize) -> io::Result<()> {
        self.available
            .try_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_add(len).filter(|total| *total <= self.limit)
            })
            .map_err(|_| protocol_error("peer granted more flow-control window than exists"))?;
        self.wake();
        Ok(())
    }

    fn wake(&self) {
        let waker = self.waiter.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

fn protocol_error(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::poll_fn;
    use std::time::Duration;

    async fn take(credit: &Credit, want: usize) -> io::Result<usize> {
        poll_fn(|cx| credit.poll_take(cx, want)).await
    }

    #[tokio::test]
    async fn take_waits_for_a_grant_and_takes_at_most_what_is_open() {
        let credit = Credit::new(10);
        assert_eq!(take(&credit, 4).await.unwrap(), 4);
        assert_eq!(take(&credit, 100).await.unwrap(), 6);

        let blocked = tokio::time::timeout(Duration::from_millis(20), take(&credit, 1)).await;
        assert!(blocked.is_err(), "the window is used up");

        credit.grant(3).unwrap();
        assert_eq!(take(&credit, 5).await.unwrap(), 3);
    }

    /// A sender blocked on a window nobody will ever reopen must fail, not
    /// wait forever: this is how a proxy learns its peer is gone when the
    /// only thing it does is write.
    #[tokio::test]
    async fn closing_fails_waiting_and_later_senders_but_keeps_open_bytes() {
        let credit = std::sync::Arc::new(Credit::new(4));
        credit.take(4).unwrap();
        let waiter = tokio::spawn({
            let credit = std::sync::Arc::clone(&credit);
            async move { take(&credit, 1).await }
        });
        tokio::task::yield_now().await;
        credit.close();
        assert_eq!(
            waiter.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            take(&credit, 1).await.unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );

        let credit = Credit::new(4);
        credit.close();
        assert_eq!(take(&credit, 8).await.unwrap(), 4);
    }

    #[tokio::test]
    async fn a_grant_wakes_the_waiting_sender() {
        let credit = std::sync::Arc::new(Credit::new(1));
        credit.take(1).unwrap();
        let waiter = tokio::spawn({
            let credit = std::sync::Arc::clone(&credit);
            async move { take(&credit, 1).await }
        });
        tokio::task::yield_now().await;
        credit.grant(1).unwrap();
        assert_eq!(waiter.await.unwrap().unwrap(), 1);
    }

    #[test]
    fn overrunning_or_overgranting_the_window_is_an_error() {
        let credit = Credit::new(8);
        credit.take(8).unwrap();
        assert_eq!(
            credit.take(1).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        credit.grant(8).unwrap();
        assert_eq!(
            credit.grant(1).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(credit.available(), 8);
    }
}
