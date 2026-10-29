//! Cancellation and lease release through the real Tokio connection sink.

use super::*;

use std::sync::atomic::{AtomicBool, AtomicU32};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

const TIMEOUT: Duration = Duration::from_secs(2);

struct LeaseDrop(Option<oneshot::Sender<()>>);

impl Drop for LeaseDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

struct Fixture {
    conn: PromotedConn,
    peer: tokio::net::TcpStream,
    generation: CancellationToken,
    dropped: oneshot::Receiver<()>,
}

async fn fixture(window: u32) -> Fixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (peer, accepted) = tokio::join!(tokio::net::TcpStream::connect(addr), listener.accept());
    let peer = peer.unwrap();
    let stream = accepted.unwrap().0.into_std().unwrap();
    let generation = CancellationToken::new();
    let (tx, dropped) = oneshot::channel();
    let stream =
        NativeTcp::with_lifecycle(stream, Arc::new(LeaseDrop(Some(tx))), generation.clone());
    Fixture {
        conn: PromotedConn {
            stream,
            reader_cancel: generation.child_token(),
            remote_ip: std::net::Ipv4Addr::new(203, 0, 113, 10),
            guest_ip: std::net::Ipv4Addr::new(192, 168, 64, 2),
            remote_port: 443,
            guest_port: 50000,
            peer_mss: 1460,
            our_seq: Arc::new(AtomicU32::new(1000)),
            last_ack: Arc::new(AtomicU32::new(2000)),
            guest_acked: Arc::new(AtomicU32::new(1000)),
            guest_window: Arc::new(AtomicU32::new(window)),
            down_bytes: None,
            gw_mac: [0x02, 0, 0, 0, 0, 1],
            guest_mac: [0x02, 0, 0, 0, 0, 2],
            dead: Arc::new(AtomicBool::new(false)),
            retx: Arc::new(std::sync::Mutex::new((
                1000,
                std::collections::VecDeque::new(),
                None,
            ))),
        },
        peer,
        generation,
        dropped,
    }
}

async fn released(dropped: oneshot::Receiver<()>) {
    tokio::time::timeout(TIMEOUT, dropped)
        .await
        .expect("the reader must release its lease")
        .expect("the lease must report its destruction");
}

async fn wait_until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(TIMEOUT, async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the reader must reach the blocked frame send");
}

async fn cancel_pending_reader(window: u32) {
    let Fixture {
        conn,
        mut peer,
        generation,
        dropped,
    } = fixture(window).await;
    let cancel = conn.reader_cancel.clone();
    let (sink, mut frames) = TokioFrameConnSink::channel(1);
    assert!(sink.send_conn(conn));
    drop(sink);
    tokio::task::yield_now().await;
    cancel.cancel();
    released(dropped).await;
    assert!(!generation.is_cancelled());
    assert_eq!(
        tokio::time::timeout(TIMEOUT, frames.recv()).await.unwrap(),
        None
    );
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(TIMEOUT, peer.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn cancellation_releases_an_idle_reader() {
    cancel_pending_reader(65535).await;
}

#[tokio::test]
async fn cancellation_releases_a_zero_window_reader() {
    cancel_pending_reader(0).await;
}

#[derive(Clone, Copy)]
enum BlockedFrame {
    Data,
    Fin,
    Rst,
}

async fn cancel_blocked_frame(kind: BlockedFrame) {
    let Fixture {
        conn,
        mut peer,
        generation,
        dropped,
    } = fixture(65535).await;
    let cancel = conn.reader_cancel.clone();
    let retx = conn.retx.clone();
    let dead = conn.dead.clone();
    let (tx, mut frames) = mpsc::channel(1);
    tx.try_send(Vec::new()).unwrap();
    let sink = TokioFrameConnSink::new(tx);
    assert!(sink.send_conn(conn));
    drop(sink);
    match kind {
        BlockedFrame::Data => peer.write_all(b"x").await.unwrap(),
        BlockedFrame::Fin => peer.shutdown().await.unwrap(),
        BlockedFrame::Rst => {
            let peer = peer.into_std().unwrap();
            socket2::SockRef::from(&peer)
                .set_linger(Some(Duration::ZERO))
                .unwrap();
            drop(peer);
        }
    }
    wait_until(|| match kind {
        BlockedFrame::Data => retx.lock().unwrap().1.len() == 1,
        BlockedFrame::Fin => retx.lock().unwrap().2.is_some(),
        BlockedFrame::Rst => dead.load(Ordering::Relaxed),
    })
    .await;
    cancel.cancel();
    released(dropped).await;
    assert!(!generation.is_cancelled());
    assert_eq!(frames.try_recv().unwrap(), Vec::<u8>::new());
    assert_eq!(
        tokio::time::timeout(TIMEOUT, frames.recv()).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn cancellation_releases_a_reader_blocked_sending_data() {
    cancel_blocked_frame(BlockedFrame::Data).await;
}

#[tokio::test]
async fn cancellation_releases_a_reader_blocked_sending_fin() {
    cancel_blocked_frame(BlockedFrame::Fin).await;
}

#[tokio::test]
async fn cancellation_releases_a_reader_blocked_sending_rst() {
    cancel_blocked_frame(BlockedFrame::Rst).await;
}

#[tokio::test]
async fn clean_eof_releases_the_reader_without_cancelling_its_primary() {
    let Fixture {
        conn,
        mut peer,
        generation,
        dropped,
    } = fixture(65535).await;
    let cancel = conn.reader_cancel.clone();
    let dead = conn.dead.clone();
    let (sink, mut frames) = TokioFrameConnSink::channel(1);
    assert!(sink.send_conn(conn));
    drop(sink);
    peer.shutdown().await.unwrap();
    let fin = tokio::time::timeout(TIMEOUT, frames.recv())
        .await
        .unwrap()
        .unwrap();
    let flags = fin[ETH_HEADER_LEN + 20 + 13];
    assert_ne!(flags & 0x01, 0);
    assert_eq!(flags & 0x04, 0);
    released(dropped).await;
    assert!(!cancel.is_cancelled());
    assert!(!generation.is_cancelled());
    assert!(!dead.load(Ordering::Relaxed));
}

#[tokio::test]
async fn cancellation_takes_priority_over_readable_data() {
    let Fixture {
        conn,
        mut peer,
        generation,
        dropped,
    } = fixture(65535).await;
    peer.write_all(b"unpublished").await.unwrap();
    conn.reader_cancel.cancel();
    let (sink, mut frames) = TokioFrameConnSink::channel(1);
    assert!(sink.send_conn(conn));
    drop(sink);
    released(dropped).await;
    assert!(!generation.is_cancelled());
    assert_eq!(
        tokio::time::timeout(TIMEOUT, frames.recv()).await.unwrap(),
        None
    );
}
