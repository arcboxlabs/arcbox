use super::*;

use std::time::Duration;
use tokio::{io::AsyncReadExt, net::TcpListener, sync::oneshot};

struct LeaseDrop {
    peer: std::net::TcpStream,
    released: Option<oneshot::Sender<io::Result<usize>>>,
}

impl Drop for LeaseDrop {
    fn drop(&mut self) {
        let result = self.peer.read(&mut [0; 1]);
        self.released.take().unwrap().send(result).unwrap();
    }
}

#[tokio::test]
async fn native_conversion_and_clones_retain_the_owner_until_last_socket_drop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (peer, _) = listener.accept().await.unwrap();
    let peer = peer.into_std().unwrap();
    peer.set_nonblocking(false).unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let (dropped, mut released) = oneshot::channel();
    let cancelled = CancellationToken::new();
    let stream = NativeTcp::with_lifecycle(
        stream,
        Arc::new(LeaseDrop {
            peer,
            released: Some(dropped),
        }),
        cancelled.clone(),
    );
    let stream = stream.into_std().unwrap();
    let clone = stream.try_clone().unwrap();
    let observed = clone.cancellation().unwrap();
    drop(stream);
    assert!(released.try_recv().is_err());
    assert!(!cancelled.is_cancelled());
    let clone = clone.into_tokio().unwrap().into_std().unwrap();
    assert_eq!(
        clone.into_unmanaged().unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        released
            .await
            .unwrap()
            .expect("native socket must close before its lease is released"),
        0
    );
    observed.cancel();
}

#[tokio::test]
async fn unmanaged_extraction_preserves_the_live_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (mut peer, _) = listener.accept().await.unwrap();
    let mut stream = NativeTcp::new(stream)
        .into_std()
        .unwrap()
        .into_unmanaged()
        .unwrap();
    stream.write_all(b"native").unwrap();
    let mut bytes = [0; 6];
    peer.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"native");
}
