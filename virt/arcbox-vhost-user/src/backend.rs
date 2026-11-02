//! Serving one vhost-user connection: the calling thread answers front-end
//! messages.

use std::os::unix::net::UnixStream;
use std::thread::JoinHandle;

use arcbox_virtio_core::virtio_bindings::virtio_config::VIRTIO_F_RING_PACKED;
use arcbox_virtio_core::virtio_bindings::virtio_ring::VIRTIO_RING_F_INDIRECT_DESC;

use crate::connection::{Connection, Message};
use crate::device::VhostUserDevice;
use crate::error::{Error, Result};
use crate::protocol::{
    ConfigHeader, ProtocolFeatures, Request, VHOST_USER_F_PROTOCOL_FEATURES, decode_u64,
};

/// Ring layouts `SplitQueue` does not walk; never offered to the front-end.
const UNSUPPORTED_RING_FEATURES: u64 =
    (1u64 << VIRTIO_RING_F_INDIRECT_DESC) | (1u64 << VIRTIO_F_RING_PACKED);

/// Protocol features every back-end offers.
const BASE_PROTOCOL_FEATURES: ProtocolFeatures = ProtocolFeatures::from_bits(
    ProtocolFeatures::MQ.bits()
        | ProtocolFeatures::REPLY_ACK.bits()
        | ProtocolFeatures::RESET_DEVICE.bits(),
);

/// Most queues a front-end can address: vring descriptor messages carry the
/// index in eight bits.
const MAX_QUEUES: usize = 256;

/// A device ready to be served to one front-end connection.
pub struct Backend<D: VhostUserDevice> {
    name: String,
    device: D,
}

impl<D: VhostUserDevice> Backend<D> {
    /// `name` labels threads and logs (e.g. `vhost-fs-arcbox`).
    pub fn new(name: impl Into<String>, device: D) -> Self {
        Self {
            name: name.into(),
            device,
        }
    }

    /// Serves `stream` on the calling thread until the front-end disconnects
    /// (or [`BackendHandle::stop`] shuts the socket down), then resets the
    /// device and hands it back.
    pub fn serve(self, stream: UnixStream) -> Result<D> {
        let Self { name, device } = self;
        let num_queues = device.num_queues();
        if num_queues == 0 || num_queues > MAX_QUEUES {
            return Err(Error::Device(format!(
                "{name}: {num_queues} queues (supported: 1..={MAX_QUEUES})"
            )));
        }

        let offered_features =
            (device.features() & !UNSUPPORTED_RING_FEATURES) | VHOST_USER_F_PROTOCOL_FEATURES;
        let offered_protocol = BASE_PROTOCOL_FEATURES | device.protocol_features();
        let mut state = State {
            device,
            offered_features,
            offered_protocol,
            acked_features: 0,
            acked_protocol: ProtocolFeatures::empty(),
        };

        let conn = Connection::new(stream);
        let result = control_loop(&name, &conn, &mut state);
        if let Err(e) = &result {
            tracing::warn!("{name}: vhost-user connection failed: {e}");
        }
        state.reset();
        tracing::info!("{name}: vhost-user front-end disconnected");
        result.map(|()| state.device)
    }

    /// Serves `stream` on a new thread named after the back-end.
    pub fn spawn(self, stream: UnixStream) -> std::io::Result<BackendHandle<D>> {
        let socket = stream.try_clone()?;
        let thread = std::thread::Builder::new()
            .name(self.name.clone())
            .spawn(move || self.serve(stream))?;
        Ok(BackendHandle { socket, thread })
    }
}

/// A back-end serving on its own thread.
pub struct BackendHandle<D: VhostUserDevice> {
    socket: UnixStream,
    thread: JoinHandle<Result<D>>,
}

impl<D: VhostUserDevice> BackendHandle<D> {
    /// Disconnects from the front-end and waits for the back-end to finish.
    pub fn stop(self) -> Result<D> {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        self.join()
    }

    /// Waits for the front-end to disconnect.
    pub fn join(self) -> Result<D> {
        self.thread
            .join()
            .unwrap_or_else(|_| Err(Error::Device("vhost-user control thread panicked".into())))
    }

    /// Whether the connection has ended.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }
}

struct State<D> {
    device: D,
    offered_features: u64,
    offered_protocol: ProtocolFeatures,
    acked_features: u64,
    acked_protocol: ProtocolFeatures,
}

impl<D: VhostUserDevice> State<D> {
    fn handle(&mut self, request: Request, msg: Message) -> Result<Option<Vec<u8>>> {
        let payload = msg.payload.as_slice();
        match request {
            Request::GetFeatures => Ok(Some(self.offered_features.to_ne_bytes().to_vec())),
            Request::SetFeatures => {
                let features = decode_u64(payload)?;
                if features & !self.offered_features != 0 {
                    return Err(Error::Protocol(format!(
                        "acked features {features:#x} exceed offered {:#x}",
                        self.offered_features
                    )));
                }
                self.acked_features = features;
                self.device
                    .set_features(features & !VHOST_USER_F_PROTOCOL_FEATURES);
                Ok(None)
            }
            Request::SetOwner => Ok(None),
            Request::ResetOwner | Request::ResetDevice => {
                self.reset();
                Ok(None)
            }
            Request::GetProtocolFeatures => {
                Ok(Some(self.offered_protocol.bits().to_ne_bytes().to_vec()))
            }
            Request::SetProtocolFeatures => {
                let acked = ProtocolFeatures::from_bits(decode_u64(payload)?);
                if !self.offered_protocol.contains(acked) {
                    return Err(Error::Protocol(format!(
                        "acked protocol features {:#x} exceed offered {:#x}",
                        acked.bits(),
                        self.offered_protocol.bits()
                    )));
                }
                self.acked_protocol = acked;
                Ok(None)
            }
            Request::GetQueueNum => Ok(Some(
                (self.device.num_queues() as u64).to_ne_bytes().to_vec(),
            )),
            Request::GetConfig => {
                self.require_protocol(ProtocolFeatures::CONFIG, "GET_CONFIG")?;
                let (header, _) = ConfigHeader::decode(payload)?;
                let mut data = vec![0u8; header.size as usize];
                self.device.read_config(header.offset, &mut data)?;
                Ok(Some(header.encode_with(&data)))
            }
            Request::SetConfig => {
                self.require_protocol(ProtocolFeatures::CONFIG, "SET_CONFIG")?;
                let (header, data) = ConfigHeader::decode(payload)?;
                self.device.write_config(header.offset, data)?;
                Ok(None)
            }
            Request::SetMemTable
            | Request::SetVringNum
            | Request::SetVringAddr
            | Request::SetVringBase
            | Request::GetVringBase
            | Request::SetVringKick
            | Request::SetVringCall
            | Request::SetVringErr
            | Request::SetVringEnable => Err(Error::Unsupported(format!("{request:?}"))),
            // Each of these belongs to a feature this back-end never offers.
            Request::SetLogBase
            | Request::SetLogFd
            | Request::SetBackendReqFd
            | Request::GetInflightFd
            | Request::SetInflightFd
            | Request::GetMaxMemSlots
            | Request::AddMemReg
            | Request::RemMemReg
            | Request::SetStatus
            | Request::GetStatus => Err(Error::Unsupported(format!("{request:?}"))),
        }
    }

    fn require_protocol(&self, feature: ProtocolFeatures, what: &str) -> Result<()> {
        if self.acked_protocol.contains(feature) {
            Ok(())
        } else {
            Err(Error::Protocol(format!(
                "{what} without its protocol feature"
            )))
        }
    }

    /// Forgets the virtio features. Protocol features survive: QEMU
    /// negotiates them once per connection and keeps relying on them
    /// (`REPLY_ACK` above all — it blocks for the ack of the
    /// `SET_VRING_CALL` it sends right after a guest-driven `RESET_DEVICE`).
    fn reset(&mut self) {
        self.acked_features = 0;
        self.device.reset();
    }
}

fn control_loop<D: VhostUserDevice>(
    name: &str,
    conn: &Connection,
    state: &mut State<D>,
) -> Result<()> {
    while let Some(msg) = conn.recv()? {
        let code = msg.header.request;
        let request = Request::from_u32(code)
            .ok_or_else(|| Error::Unsupported(format!("unknown request {code}")))?;
        tracing::debug!(
            "{name}: {request:?} ({} bytes, {} fds)",
            msg.payload.len(),
            msg.fds.len()
        );
        let ack = msg.header.need_reply()
            && !request.has_reply()
            && state.acked_protocol.contains(ProtocolFeatures::REPLY_ACK);
        match state.handle(request, msg) {
            Ok(Some(reply)) => conn.send_reply(code, &reply)?,
            Ok(None) if ack => conn.send_reply(code, &0u64.to_ne_bytes())?,
            Ok(None) => {}
            Err(e) if ack => {
                tracing::warn!("{name}: {request:?} failed: {e}");
                conn.send_reply(code, &1u64.to_ne_bytes())?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
