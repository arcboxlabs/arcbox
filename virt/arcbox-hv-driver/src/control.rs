//! Requests to a VMM process: one connection each, with a deadline.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use arcbox_vm_driver::Result;

use crate::proto::{Request, Response};
use crate::{driver_error, wire};

/// The deadline of a request that only reads state.
pub const QUICK: Duration = Duration::from_secs(10);
/// The deadline of a vsock dial: the machine waits up to 30 s for a vCPU
/// to queue the request.
pub const DIAL: Duration = Duration::from_secs(35);
/// The deadline of a boot, restore or checkpoint, which write or map all
/// of guest memory.
pub const SLOW: Duration = Duration::from_secs(600);

/// The control socket of one VMM process.
#[derive(Debug, Clone)]
pub struct Control {
    socket: PathBuf,
}

impl Control {
    pub const fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// Sends `request` and waits up to `deadline` for the answer. Blocks.
    pub fn call_blocking(
        &self,
        request: &Request,
        deadline: Duration,
    ) -> Result<(Response, Option<OwnedFd>)> {
        let conn = UnixStream::connect(&self.socket)
            .map_err(|e| driver_error(format!("connect {}", self.socket.display()), e))?;
        conn.set_read_timeout(Some(deadline))?;
        conn.set_write_timeout(Some(QUICK))?;
        wire::send(&conn, request, None).map_err(|e| driver_error("send request", e))?;
        match wire::recv::<Response>(&conn) {
            Ok((Response::Error(e), _)) => Err(e.into()),
            Ok(answer) => Ok(answer),
            Err(e) => Err(driver_error("read response", e)),
        }
    }

    /// [`call_blocking`](Self::call_blocking) off the async workers.
    pub async fn call(
        &self,
        request: Request,
        deadline: Duration,
    ) -> Result<(Response, Option<OwnedFd>)> {
        let control = self.clone();
        tokio::task::spawn_blocking(move || control.call_blocking(&request, deadline))
            .await
            .map_err(|e| driver_error("request task", e))?
    }
}

/// A response the request does not call for.
pub fn unexpected(response: &Response) -> arcbox_vm_driver::Error {
    driver_error(
        "unexpected response",
        std::io::Error::other(format!("{response:?}")),
    )
}
