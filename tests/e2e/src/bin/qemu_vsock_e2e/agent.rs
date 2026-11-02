//! Agent RPC on vsock 1024, dialled the way the daemon dials it: a fresh
//! host connection per probe, `AgentClient` on the blocking transport.

use std::io::Read;
use std::os::fd::{IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Barrier, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use arcbox_constants::ports::AGENT_PORT;
use arcbox_engine::agent_client::AgentClient;
use arcbox_virtio_vsock::vhost_user::VhostUserVsock;

use crate::CID;

/// Pings on fresh connections until one answers.
pub fn wait_ready(vsock: &VhostUserVsock, budget: Duration) -> Result<Instant> {
    let deadline = Instant::now() + budget;
    let mut last = String::new();
    while Instant::now() < deadline {
        match ping_once(vsock) {
            Ok(()) => return Ok(Instant::now()),
            Err(e) => last = format!("{e:#}"),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    bail!("agent did not answer within {budget:?}: {last}")
}

pub fn ping_once(vsock: &VhostUserVsock) -> Result<()> {
    client(vsock.connect_vsock(AGENT_PORT)?, Duration::from_secs(3))?
        .ping_blocking()
        .context("Ping")?;
    Ok(())
}

pub fn system_info(vsock: &VhostUserVsock) -> Result<()> {
    let info = client(vsock.connect_vsock(AGENT_PORT)?, Duration::from_secs(3))?
        .get_system_info_blocking()
        .context("GetSystemInfo")?;
    println!(
        "[ok] GetSystemInfo: kernel {} hostname {} addresses {:?}",
        info.kernel_version, info.hostname, info.ip_addresses
    );
    Ok(())
}

/// An `AgentClient` on the blocking transport, as the HV backend uses, with
/// `timeout` on each read and write.
fn client(fd: OwnedFd, timeout: Duration) -> Result<AgentClient> {
    let stream = UnixStream::from(fd);
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(AgentClient::from_fd_blocking(
        CID as u32,
        OwnedFd::from(stream).into_raw_fd(),
    )?)
}

/// Ping round trips, one fresh connection each (connect + handshake + RPC +
/// close), as the daemon's readiness probes do.
pub fn latency(vsock: &VhostUserVsock, rounds: usize) -> Result<()> {
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t = Instant::now();
        ping_once(vsock)?;
        samples.push(t.elapsed());
    }
    println!(
        "[ok] {rounds} connect+Ping round trips: {}",
        crate::percentiles(&mut samples)
    );
    Ok(())
}

/// The agent's vsock listen backlog (`arcbox-transport` `vsock/linux.rs`).
/// The guest resets a request that finds the accept queue full.
const AGENT_BACKLOG: usize = 128;

/// `n` connections open at once, each with a completed Ping, then one Ping
/// on every one of them simultaneously. Handshakes are kept well inside the
/// agent's backlog; [`burst`] measures what happens past it. The responses
/// need far more RX buffers than the guest posts, so injection runs dry and
/// resumes on the guest's kicks.
pub fn concurrent(vsock: &VhostUserVsock, n: usize) -> Result<()> {
    let barrier = Barrier::new(n);
    let handshakes = (Mutex::new(0usize), Condvar::new());
    let started = Instant::now();
    let results: Vec<Result<Duration>> = std::thread::scope(|s| {
        let threads: Vec<_> = (0..n)
            .map(|_| {
                s.spawn(|| {
                    let mut busy = handshakes.0.lock().expect("gate");
                    while *busy >= AGENT_BACKLOG / 2 {
                        busy = handshakes.1.wait(busy).expect("gate");
                    }
                    *busy += 1;
                    drop(busy);
                    let opened = vsock
                        .connect_vsock(AGENT_PORT)
                        .map_err(anyhow::Error::from)
                        .and_then(|fd| client(fd, Duration::from_secs(60)))
                        .and_then(|mut c| c.ping_blocking().map(|_| c).context("first Ping"));
                    *handshakes.0.lock().expect("gate") -= 1;
                    handshakes.1.notify_one();
                    barrier.wait();
                    let t = Instant::now();
                    opened?.ping_blocking().context("second Ping")?;
                    Ok(t.elapsed())
                })
            })
            .collect();
        threads
            .into_iter()
            .map(|t| t.join().expect("thread"))
            .collect()
    });
    let elapsed = started.elapsed();
    let mut samples = Vec::with_capacity(n);
    for r in results {
        samples.push(r?);
    }
    println!(
        "[ok] {n} connections open at once, {n} simultaneous Pings on them ({:.2}s overall): {}",
        elapsed.as_secs_f64(),
        crate::percentiles(&mut samples)
    );
    drained(vsock, Duration::from_secs(30))
}

/// `n` connect+Ping attempts released at the same instant. Requests past
/// the agent's backlog are reset by the guest and read as EOF; the device
/// must clean every one of them up.
pub fn burst(vsock: &VhostUserVsock, n: usize) -> Result<()> {
    let barrier = Barrier::new(n);
    let (refused, failed) = (AtomicUsize::new(0), Mutex::new(None));
    std::thread::scope(|s| {
        for _ in 0..n {
            s.spawn(|| {
                barrier.wait();
                let ping = vsock
                    .connect_vsock(AGENT_PORT)
                    .map_err(anyhow::Error::from)
                    .and_then(|fd| client(fd, Duration::from_secs(60)))
                    .and_then(|mut c| Ok(c.ping_blocking().map(drop)?));
                match ping {
                    Ok(()) => {}
                    // A reset reads as EOF on the host end.
                    Err(e) if format!("{e:#}").contains("early eof") => {
                        refused.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => *failed.lock().expect("failed") = Some(e),
                }
            });
        }
    });
    if let Some(e) = failed.into_inner().expect("failed") {
        return Err(e).context("burst: a connection failed other than by refusal");
    }
    let refused = refused.into_inner();
    drained(vsock, Duration::from_secs(30))?;
    ping_once(vsock)?;
    println!(
        "[ok] burst of {n} simultaneous connects: {} answered, {refused} refused by the guest (backlog {AGENT_BACKLOG}); none left open",
        n - refused
    );
    Ok(())
}

/// The host closes first: before the guest answered the request, and after
/// a completed RPC. Neither may leave a connection behind or upset the agent.
pub fn host_close(vsock: &VhostUserVsock) -> Result<()> {
    for _ in 0..100 {
        drop(vsock.connect_vsock(AGENT_PORT)?);
    }
    for _ in 0..100 {
        let mut c = client(vsock.connect_vsock(AGENT_PORT)?, Duration::from_secs(3))?;
        c.ping_blocking()?;
    }
    ping_once(vsock)?;
    drained(vsock, Duration::from_secs(30))?;
    println!("[ok] host close: 100 before the handshake, 100 after an RPC; agent still answers");
    Ok(())
}

/// Opens an idle connection the guest has accepted. The returned stream
/// reads EOF once the guest side goes away, within `budget`. The timeout is
/// set now: macOS refuses `setsockopt` once the peer has gone.
pub fn idle_connection(vsock: &VhostUserVsock, budget: Duration) -> Result<UnixStream> {
    let stream = UnixStream::from(vsock.connect_vsock(AGENT_PORT)?);
    stream.set_read_timeout(Some(budget))?;
    // A Ping on a second connection proves the agent is accepting; this one
    // then sits accepted with nothing to read.
    ping_once(vsock)?;
    std::thread::sleep(Duration::from_millis(200));
    Ok(stream)
}

/// Waits until `stream` reads EOF (or a reset) and returns how long after
/// `since` it came.
pub fn wait_eof(mut stream: UnixStream, since: Instant) -> Result<Duration> {
    let mut buf = [0u8; 64];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return Ok(since.elapsed()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                return Ok(since.elapsed());
            }
            Err(e) => return Err(e).context("waiting for the guest to close"),
        }
    }
}

/// Waits for every host connection to be gone from the device.
pub fn drained(vsock: &VhostUserVsock, budget: Duration) -> Result<()> {
    let deadline = Instant::now() + budget;
    while vsock.connection_count() != 0 {
        if Instant::now() > deadline {
            bail!(
                "{} vsock connections still open after {budget:?}",
                vsock.connection_count()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
