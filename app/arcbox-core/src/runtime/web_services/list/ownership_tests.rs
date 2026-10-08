//! Real socket cancellation must finish before an owned query releases its hold.

use super::*;
use std::{
    io::Read as _,
    os::{fd::IntoRawFd as _, unix::net::UnixStream},
};

struct ReleaseOnDrop(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = self.0.take().unwrap().send(());
    }
}

#[tokio::test]
async fn caller_abort_and_deadline_cancel_rpc_before_owned_cleanup_releases_hold() {
    for deadline in [false, true] {
        let (host, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let socket = std::thread::spawn(move || {
            let mut length = [0; 4];
            peer.read_exact(&mut length).unwrap();
            let mut handshake = vec![0; u32::from_be_bytes(length) as usize];
            peer.read_exact(&mut handshake).unwrap();
            started.send(()).unwrap();
            assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
        });
        let agent =
            arcbox_engine::agent_client::AgentClient::from_fd_blocking(3, host.into_raw_fd())
                .unwrap();
        let (released, mut release_seen) = tokio::sync::oneshot::channel();
        let (drained, drain_seen) = tokio::sync::oneshot::channel();
        let (finish, cleanup) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn(owned_query(move |cancelled| async move {
            let query = async {
                let _hold = ReleaseOnDrop(Some(released));
                let result = agent
                    .list_guest_web_services(ListGuestWebServicesRequest::default(), &cancelled)
                    .await;
                assert!(result.unwrap_err().to_string().contains("cancelled"));
                drained.send(()).unwrap();
                cleanup.await.unwrap();
                Err::<(), _>(CoreError::invalid_state("cancelled query drained"))
            };
            query_deadline(query, &cancelled).await
        }));
        ready.await.unwrap();
        if deadline {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(5)).await;
            tokio::time::resume();
        } else {
            caller.abort();
        }
        drain_seen.await.unwrap();
        socket.join().unwrap();
        assert!(matches!(
            release_seen.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        if deadline {
            assert!(!caller.is_finished());
        }
        finish.send(()).unwrap();
        release_seen.await.unwrap();
        if deadline {
            assert!(
                caller
                    .await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("timed out")
            );
        } else {
            assert!(caller.await.unwrap_err().is_cancelled());
        }
    }
}
