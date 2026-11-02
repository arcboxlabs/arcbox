//! Agent RPC on vsock 1024, dialled the way the daemon dials it: a fresh
//! host connection per probe, `AgentClient` on the blocking transport.

use std::os::fd::{IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
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
