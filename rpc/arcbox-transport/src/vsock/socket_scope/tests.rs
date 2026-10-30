use std::future::{Future, poll_fn};
use std::io::{Read, Write};
use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream;
use std::pin::pin;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::*;
use crate::Transport;
use crate::vsock::{BlockingVsockTransport, VsockAddr, VsockStream, VsockTransport};

const WAIT: Duration = Duration::from_secs(2);

fn async_pair(unix: bool) -> (VsockTransport, UnixStream) {
    let (host, peer) = UnixStream::pair().unwrap();
    let stream = if unix {
        host.set_nonblocking(true).unwrap();
        VsockStream::from_unix_stream(tokio::net::UnixStream::from_std(host).unwrap())
    } else {
        VsockStream::from_fd(host.into()).unwrap()
    };
    (
        VsockTransport::from_stream(stream, VsockAddr::host(1024)),
        peer,
    )
}

fn blocking(host: UnixStream) -> BlockingVsockTransport {
    // SAFETY: host transfers ownership of its connected socket.
    unsafe { BlockingVsockTransport::from_raw_fd(host.into_raw_fd()).unwrap() }
}

#[tokio::test]
async fn close_wakes_async_idle_receivers_after_split() {
    for unix in [false, true] {
        let scope = SocketScope::default();
        let (transport, _peer) = async_pair(unix);
        let (writer, mut reader) = transport.with_scope(&scope).unwrap().into_split().unwrap();
        drop(writer);
        let (started, ready) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            let mut started = Some(started);
            let mut receive = pin!(reader.recv());
            poll_fn(|cx| {
                let result = receive.as_mut().poll(cx);
                if result.is_pending() {
                    if let Some(started) = started.take() {
                        started.send(()).unwrap();
                    }
                }
                result
            })
            .await
        });
        tokio::time::timeout(WAIT, ready).await.unwrap().unwrap();
        scope.clone().close().unwrap();
        assert!(
            tokio::time::timeout(WAIT, worker)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        scope.close().unwrap();
    }
}

#[test]
fn close_interrupts_each_remaining_blocking_half() {
    for receive in [false, true] {
        let scope = SocketScope::default();
        let (mut host, _peer) = UnixStream::pair().unwrap();
        host.set_nonblocking(true).unwrap();
        if !receive {
            loop {
                match host.write(&[0; 8192]) {
                    Ok(written) => assert!(written > 0),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("cannot fill socket: {error}"),
                }
            }
        }
        let (mut writer, mut reader) = blocking(host)
            .with_scope(&scope)
            .unwrap()
            .into_split()
            .unwrap();
        let (started, ready) = mpsc::channel();
        let (finished, results) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = if receive {
                drop(writer);
                started.send(()).unwrap();
                reader.recv().map(|_| ())
            } else {
                drop(reader);
                started.send(()).unwrap();
                writer.send(&[1], Instant::now() + Duration::from_secs(5))
            };
            finished.send(result).unwrap();
        });
        ready.recv_timeout(WAIT).unwrap();
        assert!(matches!(results.try_recv(), Err(mpsc::TryRecvError::Empty)));
        scope.close().unwrap();
        let error = results.recv_timeout(WAIT).unwrap().unwrap_err();
        assert!(!error.to_string().contains("deadline"), "{error}");
        worker.join().unwrap();
    }
}

#[tokio::test]
async fn open_scope_does_not_retain_dropped_sockets() {
    let scope = SocketScope::default();
    for unix in [false, true] {
        let (transport, mut peer) = async_pair(unix);
        peer.set_read_timeout(Some(WAIT)).unwrap();
        drop(transport.with_scope(&scope).unwrap());
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
    for close in [false, true] {
        let (host, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(WAIT)).unwrap();
        let mut transport = blocking(host).with_scope(&scope).unwrap();
        if close {
            transport.close();
        } else {
            drop(transport);
        }
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
    scope.close().unwrap();
}

#[tokio::test]
async fn closed_scope_rejects_late_registration_and_connect() {
    let scope = SocketScope::default();
    let mut pending = VsockTransport::to_host().with_scope(&scope).unwrap();
    scope.check_open().unwrap();
    scope.close().unwrap();
    assert_eq!(
        scope.check_open().unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
    assert!(
        pending
            .connect()
            .await
            .unwrap_err()
            .to_string()
            .contains("socket scope is closed")
    );
    let (transport, _peer) = async_pair(false);
    assert_eq!(
        transport.with_scope(&scope).err().unwrap().kind(),
        io::ErrorKind::NotConnected
    );
    let (host, _peer) = UnixStream::pair().unwrap();
    assert_eq!(
        blocking(host).with_scope(&scope).err().unwrap().kind(),
        io::ErrorKind::NotConnected
    );
}
