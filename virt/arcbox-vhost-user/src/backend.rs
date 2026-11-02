//! Serving one vhost-user connection: the control thread answers front-end
//! messages, a worker thread runs the data plane on a kqueue.
//!
//! Both threads share one lock over the device and its vrings. The control
//! thread takes it per message, the worker per event, so a ring is never
//! reconfigured while the device is processing it and `GET_VRING_BASE`
//! reports a cursor no handler is still moving.

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use arcbox_virtio_core::virtio_bindings::virtio_config::VIRTIO_F_RING_PACKED;
use arcbox_virtio_core::virtio_bindings::virtio_ring::{
    VIRTIO_RING_F_EVENT_IDX, VIRTIO_RING_F_INDIRECT_DESC,
};

use crate::connection::{Connection, Message};
use crate::device::{VhostUserDevice, Vrings};
use crate::error::{Error, Result};
use crate::event_loop::{Event, Kqueue, Registrar, Trigger};
use crate::memory::GuestMemory;
use crate::protocol::{
    ConfigHeader, ProtocolFeatures, Request, VHOST_USER_F_PROTOCOL_FEATURES, VringAddr, VringState,
    decode_mem_table, decode_u64, decode_vring_fd,
};
use crate::vring::Vring;

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

/// Events the worker takes from the kqueue per wait.
const EVENT_BATCH: usize = 64;

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
        let Self { name, mut device } = self;
        let num_queues = device.num_queues();
        if num_queues == 0 || num_queues > MAX_QUEUES {
            return Err(Error::Device(format!(
                "{name}: {num_queues} queues (supported: 1..={MAX_QUEUES})"
            )));
        }

        let kq = Arc::new(Kqueue::new()?);
        kq.add_stop()?;
        for queue in 0..num_queues {
            kq.add_user(queue, Event::Trigger(queue as u16))?;
        }
        let registrar = Registrar::new(Arc::clone(&kq));
        device.activate(&registrar)?;

        let offered_features =
            (device.features() & !UNSUPPORTED_RING_FEATURES) | VHOST_USER_F_PROTOCOL_FEATURES;
        let offered_protocol = BASE_PROTOCOL_FEATURES | device.protocol_features();
        let vrings = Vrings::new(num_queues, device.max_queue_size(), registrar);
        let shared = Arc::new(Mutex::new(State {
            device,
            vrings,
            offered_features,
            offered_protocol,
            acked_features: 0,
            acked_protocol: ProtocolFeatures::empty(),
            memory: None,
        }));

        let worker = {
            let state = Arc::clone(&shared);
            let kq = Arc::clone(&kq);
            let socket = stream.try_clone()?;
            let name = name.clone();
            std::thread::Builder::new()
                .name(format!("{name}-vq"))
                .spawn(move || run_worker(&name, &state, &kq, &socket))?
        };

        let conn = Connection::new(stream);
        let result = control_loop(&name, &conn, &shared, &kq);
        if let Err(e) = &result {
            tracing::warn!("{name}: vhost-user connection failed: {e}");
        }
        let _ = kq.stop();
        let worker_result = worker.join();

        let mut guard = lock(&shared);
        guard.reset(&kq);
        guard.memory = None;
        drop(guard);
        let device = Arc::into_inner(shared)
            .expect("worker joined; no other state owner")
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
            .device;

        if worker_result.is_err() {
            return Err(Error::Device(format!("{name}: data-plane worker panicked")));
        }
        tracing::info!("{name}: vhost-user front-end disconnected");
        result.map(|()| device)
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
    vrings: Vrings,
    offered_features: u64,
    offered_protocol: ProtocolFeatures,
    acked_features: u64,
    acked_protocol: ProtocolFeatures,
    /// The current guest memory table. An identical table (every device
    /// restart re-sends one) reuses its mapping; a replaced one is unmapped
    /// once no ring or device holds it.
    memory: Option<Arc<GuestMemory>>,
}

impl<D: VhostUserDevice> State<D> {
    fn handle(&mut self, request: Request, msg: Message, kq: &Kqueue) -> Result<Option<Vec<u8>>> {
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
                let event_idx = features & (1u64 << VIRTIO_RING_F_EVENT_IDX) != 0;
                // Without the protocol-feature extension there is no
                // SET_VRING_ENABLE, and rings start enabled.
                let enable_all = features & VHOST_USER_F_PROTOCOL_FEATURES == 0;
                for vring in self.vrings.iter_mut() {
                    vring.set_event_idx(event_idx);
                    if enable_all {
                        vring.set_enabled(true);
                        trigger(kq, vring);
                    }
                }
                self.device
                    .set_features(features & !VHOST_USER_F_PROTOCOL_FEATURES);
                Ok(None)
            }
            Request::SetOwner => Ok(None),
            Request::ResetOwner | Request::ResetDevice => {
                self.reset(kq);
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
            Request::GetQueueNum => Ok(Some((self.vrings.len() as u64).to_ne_bytes().to_vec())),
            Request::SetMemTable => {
                let regions = decode_mem_table(payload)?;
                let memory = match &self.memory {
                    Some(current) if current.same_layout(&regions) => {
                        tracing::debug!("vhost-user memory table unchanged; reusing its mapping");
                        Arc::clone(current)
                    }
                    _ => {
                        let mapped = Arc::new(GuestMemory::map(&regions, &msg.fds)?);
                        tracing::info!("vhost-user guest memory mapped: {mapped:?}");
                        self.memory = Some(Arc::clone(&mapped));
                        mapped
                    }
                };
                for vring in self.vrings.iter_mut() {
                    vring.set_memory(Arc::clone(&memory))?;
                    trigger(kq, vring);
                }
                Ok(None)
            }
            Request::SetVringNum => {
                let state = VringState::decode(payload)?;
                self.vring(state.index)?.set_size(state.num)?;
                Ok(None)
            }
            Request::SetVringAddr => {
                let addr = VringAddr::decode(payload)?;
                self.vring(addr.index)?.set_addr(addr)?;
                Ok(None)
            }
            Request::SetVringBase => {
                let state = VringState::decode(payload)?;
                self.vring(state.index)?.set_base(state.num)?;
                Ok(None)
            }
            Request::GetVringBase => {
                let state = VringState::decode(payload)?;
                let (base, kick) = self.vring(state.index)?.stop();
                unwatch(kq, kick);
                let reply = VringState {
                    index: state.index,
                    num: u32::from(base),
                };
                Ok(Some(reply.encode().to_vec()))
            }
            Request::SetVringKick => {
                let (index, fd) = vring_fd(payload, msg.fds)?;
                let vring = self.vring(index)?;
                unwatch(kq, vring.start(fd)?);
                if let Some(kick) = vring.kick_fd() {
                    kq.add_read(kick.as_raw_fd(), Event::Kick(index as u16), Trigger::Level)?;
                }
                trigger(kq, vring);
                Ok(None)
            }
            Request::SetVringCall => {
                let (index, fd) = vring_fd(payload, msg.fds)?;
                self.vring(index)?.set_call(fd)?;
                Ok(None)
            }
            Request::SetVringErr => {
                // QEMU builds without eventfd hand over the *read* end of the
                // error pipe (`event_notifier_get_fd`), so it cannot be
                // written; the back-end reports failures by closing instead.
                let (index, _fd) = vring_fd(payload, msg.fds)?;
                self.vring(index)?;
                Ok(None)
            }
            Request::SetVringEnable => {
                if self.acked_features & VHOST_USER_F_PROTOCOL_FEATURES == 0 {
                    return Err(Error::Protocol(
                        "SET_VRING_ENABLE without VHOST_USER_F_PROTOCOL_FEATURES".into(),
                    ));
                }
                let state = VringState::decode(payload)?;
                let enable = match state.num {
                    0 => false,
                    1 => true,
                    n => return Err(Error::Protocol(format!("SET_VRING_ENABLE value {n}"))),
                };
                let vring = self.vring(state.index)?;
                vring.set_enabled(enable);
                trigger(kq, vring);
                Ok(None)
            }
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

    fn vring(&mut self, index: u32) -> Result<&mut Vring> {
        u16::try_from(index)
            .ok()
            .and_then(|i| self.vrings.get_mut(i))
            .ok_or_else(|| Error::Protocol(format!("queue index {index} out of range")))
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

    /// Stops every ring and forgets the virtio features; the memory table
    /// stays until the front-end replaces it. Protocol features survive: QEMU
    /// negotiates them once per connection and keeps relying on them
    /// (`REPLY_ACK` above all — it blocks for the ack of the `SET_VRING_CALL`
    /// it sends right after a guest-driven `RESET_DEVICE`).
    fn reset(&mut self, kq: &Kqueue) {
        for vring in self.vrings.iter_mut() {
            unwatch(kq, vring.reset());
        }
        self.acked_features = 0;
        self.device.reset();
    }
}

fn control_loop<D: VhostUserDevice>(
    name: &str,
    conn: &Connection,
    state: &Mutex<State<D>>,
    kq: &Kqueue,
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
        let mut guard = lock(state);
        let ack = msg.header.need_reply()
            && !request.has_reply()
            && guard.acked_protocol.contains(ProtocolFeatures::REPLY_ACK);
        let outcome = guard.handle(request, msg, kq);
        drop(guard);
        match outcome {
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

fn run_worker<D: VhostUserDevice>(
    name: &str,
    state: &Mutex<State<D>>,
    kq: &Kqueue,
    socket: &UnixStream,
) {
    // Ends the connection if the data plane dies, so the front-end sees the
    // back-end go away instead of a device that silently stops.
    struct HangUp<'a>(&'a UnixStream);
    impl Drop for HangUp<'_> {
        fn drop(&mut self) {
            if std::thread::panicking() {
                let _ = self.0.shutdown(std::net::Shutdown::Both);
            }
        }
    }
    let _hang_up = HangUp(socket);

    // SAFETY: an all-zero kevent is a valid value.
    let mut buf: [libc::kevent; EVENT_BATCH] = unsafe { std::mem::zeroed() };
    let mut events = Vec::with_capacity(EVENT_BATCH);
    loop {
        events.clear();
        if let Err(e) = kq.wait(&mut buf, &mut events) {
            tracing::error!("{name}: kevent failed: {e}");
            let _ = socket.shutdown(std::net::Shutdown::Both);
            return;
        }
        for &event in &events {
            if event == Event::Stop {
                return;
            }
            let mut guard = lock(state);
            let State { device, vrings, .. } = &mut *guard;
            let result = match event {
                Event::Kick(queue) | Event::Trigger(queue) => {
                    let Some(vring) = vrings.get_mut(queue) else {
                        continue;
                    };
                    if matches!(event, Event::Kick(_)) && !vring.drain_kick() {
                        if let Some(fd) = vring.kick_fd() {
                            let _ = kq.delete(fd.as_raw_fd());
                        }
                    }
                    if !vring.is_running() {
                        continue;
                    }
                    device.process_queue(queue, vrings)
                }
                Event::Device(token) => device.handle_event(token, vrings),
                Event::Stop => unreachable!("handled above"),
            };
            if let Err(e) = result {
                tracing::warn!("{name}: {event:?}: {e}");
            }
        }
    }
}

fn vring_fd(payload: &[u8], mut fds: Vec<OwnedFd>) -> Result<(u32, Option<OwnedFd>)> {
    let (index, has_fd) = decode_vring_fd(payload)?;
    let expected = usize::from(has_fd);
    if fds.len() != expected {
        return Err(Error::Protocol(format!(
            "queue {index} notifier message carries {} descriptors, expected {expected}",
            fds.len()
        )));
    }
    Ok((index, fds.pop()))
}

fn trigger(kq: &Kqueue, vring: &Vring) {
    if vring.is_running() {
        let _ = kq.trigger_queue(vring.index());
    }
}

fn unwatch(kq: &Kqueue, kick: Option<OwnedFd>) {
    if let Some(fd) = kick {
        let _ = kq.delete(fd.as_raw_fd());
    }
}

fn lock<D>(state: &Mutex<State<D>>) -> MutexGuard<'_, State<D>> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}
