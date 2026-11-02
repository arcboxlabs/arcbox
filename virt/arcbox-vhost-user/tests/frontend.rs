//! Drives a back-end the way QEMU does — shared-memory guest RAM, pipe
//! notifiers, rings addressed by front-end virtual address — and checks what
//! the guest would observe in its rings.
#![cfg(target_os = "macos")]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use arcbox_vhost_user::{Backend, VhostUserDevice, Vrings};

const GPA_BASE: u64 = 0x4000_0000;
const RAM: usize = 1 << 20;
const QUEUE: u32 = 1;
const QUEUE_SIZE: u16 = 8;
const DESC: u64 = 0x1000;
const AVAIL: u64 = 0x2000;
const USED: u64 = 0x3000;
const DATA: u64 = 0x4000;

const F_VERSION_1: u64 = 1 << 32;
const F_EVENT_IDX: u64 = 1 << 29;
const F_INDIRECT_DESC: u64 = 1 << 28;
const F_PROTOCOL_FEATURES: u64 = 1 << 30;
const P_MQ: u64 = 1 << 0;
const P_REPLY_ACK: u64 = 1 << 3;
const NEED_REPLY: u32 = 1 << 3;

/// Upper-cases the readable part of each chain into its writable part.
struct Upper {
    acked: Arc<AtomicU64>,
    resets: Arc<AtomicUsize>,
}

impl VhostUserDevice for Upper {
    fn num_queues(&self) -> usize {
        2
    }

    fn max_queue_size(&self) -> u16 {
        QUEUE_SIZE
    }

    fn features(&self) -> u64 {
        // INDIRECT_DESC must never reach the front-end: SplitQueue cannot walk it.
        F_VERSION_1 | F_EVENT_IDX | F_INDIRECT_DESC
    }

    fn set_features(&mut self, features: u64) {
        self.acked.store(features, Ordering::SeqCst);
    }

    fn process_queue(&mut self, queue: u16, vrings: &mut Vrings) -> arcbox_vhost_user::Result<()> {
        vrings.get_mut(queue).unwrap().process_avail(|mem, chain| {
            let mut input = Vec::new();
            for d in chain.descriptors.iter().filter(|d| !d.is_write()) {
                input.extend_from_slice(mem.slice(d.addr as usize, d.len as usize).unwrap());
            }
            let out = input.to_ascii_uppercase();
            let w = chain.descriptors.iter().find(|d| d.is_write()).unwrap();
            // SAFETY: the writable descriptor belongs to the device until the
            // used ring publishes it.
            unsafe { mem.slice_mut(w.addr as usize, out.len()) }
                .unwrap()
                .copy_from_slice(&out);
            out.len() as u32
        });
        Ok(())
    }

    fn reset(&mut self) {
        self.resets.fetch_add(1, Ordering::SeqCst);
    }
}

/// The guest's view of RAM, mapped from the same file the back-end gets.
struct Guest {
    file: std::fs::File,
    ptr: *mut u8,
}

impl Guest {
    fn new() -> Self {
        let file = tempfile::tempfile().unwrap();
        file.set_len(RAM as u64).unwrap();
        // SAFETY: maps the whole file shared; unmapped in Drop.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                RAM,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        Self {
            file,
            ptr: ptr.cast(),
        }
    }

    /// The front-end virtual address of a guest physical address — what QEMU
    /// puts in SET_VRING_ADDR. Deliberately unrelated to the GPA.
    fn uva(&self, gpa: u64) -> u64 {
        self.ptr as u64 + (gpa - GPA_BASE)
    }

    fn write(&self, gpa: u64, bytes: &[u8]) {
        let off = (gpa - GPA_BASE) as usize;
        // SAFETY: in bounds of the mapping.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(off), bytes.len()) };
    }

    fn read(&self, gpa: u64, len: usize) -> Vec<u8> {
        let off = (gpa - GPA_BASE) as usize;
        // SAFETY: in bounds of the mapping.
        unsafe { std::slice::from_raw_parts(self.ptr.add(off), len) }.to_vec()
    }

    fn u16_at(&self, gpa: u64) -> u16 {
        u16::from_le_bytes(self.read(gpa, 2).try_into().unwrap())
    }

    /// Posts a two-descriptor chain (readable `input`, writable 64 bytes) at
    /// avail slot `slot`, using descriptors `2*slot` and `2*slot+1`.
    fn post(&self, slot: u16, input: &[u8]) -> u64 {
        let head = 2 * slot;
        let inbuf = GPA_BASE + DATA + u64::from(slot) * 0x100;
        let outbuf = inbuf + 0x80;
        self.write(inbuf, input);
        self.desc(head, inbuf, input.len() as u32, 1, head + 1); // NEXT
        self.desc(head + 1, outbuf, 64, 2, 0); // WRITE
        let ring = GPA_BASE + AVAIL + 4 + u64::from(slot % QUEUE_SIZE) * 2;
        self.write(ring, &head.to_le_bytes());
        std::sync::atomic::fence(Ordering::Release);
        self.write(GPA_BASE + AVAIL + 2, &(slot + 1).to_le_bytes());
        outbuf
    }

    fn desc(&self, idx: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let mut d = Vec::new();
        d.extend_from_slice(&addr.to_le_bytes());
        d.extend_from_slice(&len.to_le_bytes());
        d.extend_from_slice(&flags.to_le_bytes());
        d.extend_from_slice(&next.to_le_bytes());
        self.write(GPA_BASE + DESC + u64::from(idx) * 16, &d);
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        // SAFETY: the mapping created in `new`.
        unsafe { libc::munmap(self.ptr.cast(), RAM) };
    }
}

struct Frontend {
    sock: UnixStream,
}

impl Frontend {
    fn send(&self, request: u32, flags: u32, payload: &[u8], fds: &[RawFd]) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&request.to_ne_bytes());
        bytes.extend_from_slice(&(1 | flags).to_ne_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        bytes.extend_from_slice(payload);
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut control = [0u64; 16];
        // SAFETY: zeroed msghdr is valid; fields set below.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            // SAFETY: the control buffer fits one cmsg with these descriptors.
            unsafe {
                msg.msg_control = control.as_mut_ptr().cast();
                msg.msg_controllen = libc::CMSG_SPACE(size_of_val(fds) as u32) as _;
                let cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(size_of_val(fds) as u32) as _;
                std::ptr::copy_nonoverlapping(
                    fds.as_ptr().cast::<u8>(),
                    libc::CMSG_DATA(cmsg),
                    size_of_val(fds),
                );
            }
        }
        // SAFETY: msg references live buffers.
        let n = unsafe { libc::sendmsg(self.sock.as_raw_fd(), &raw const msg, 0) };
        assert_eq!(n as usize, bytes.len());
    }

    fn reply(&self, request: u32) -> Vec<u8> {
        use std::io::Read;
        let mut header = [0u8; 12];
        (&self.sock).read_exact(&mut header).unwrap();
        assert_eq!(u32::from_ne_bytes(header[..4].try_into().unwrap()), request);
        assert_eq!(
            u32::from_ne_bytes(header[4..8].try_into().unwrap()),
            1 | (1 << 2)
        );
        let mut payload = vec![0u8; u32::from_ne_bytes(header[8..].try_into().unwrap()) as usize];
        (&self.sock).read_exact(&mut payload).unwrap();
        payload
    }

    fn get_u64(&self, request: u32) -> u64 {
        self.send(request, 0, &[], &[]);
        u64::from_ne_bytes(self.reply(request).try_into().unwrap())
    }

    fn vring_state(&self, request: u32, index: u32, num: u32, flags: u32) {
        let mut p = index.to_ne_bytes().to_vec();
        p.extend_from_slice(&num.to_ne_bytes());
        self.send(request, flags, &p, &[]);
    }

    /// Sends a REPLY_ACK request and returns the back-end's status.
    fn acked(&self, request: u32, payload: &[u8], fds: &[RawFd]) -> u64 {
        self.send(request, NEED_REPLY, payload, fds);
        u64::from_ne_bytes(self.reply(request).try_into().unwrap())
    }
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: pipe writes two descriptors into `fds`.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: fresh descriptors, sole owner.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Waits until `fd` is readable (the back-end raised the call notifier).
fn wait_readable(fd: &OwnedFd) -> bool {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&raw mut pfd, 1, 5_000) == 1 }
}

fn drain(fd: &OwnedFd) {
    let mut buf = [0u8; 64];
    // SAFETY: reads into a live buffer.
    unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
}

fn kick(fd: &OwnedFd) {
    let one = 1u64.to_ne_bytes();
    // SAFETY: writes 8 bytes from a live buffer.
    assert_eq!(
        unsafe { libc::write(fd.as_raw_fd(), one.as_ptr().cast(), 8) },
        8
    );
}

/// Sets queue 1 up the way `vhost_virtqueue_start` does and returns the
/// guest's ends of the kick and call pipes.
fn start_queue(fe: &Frontend, guest: &Guest, base: u32) -> (OwnedFd, OwnedFd) {
    fe.vring_state(8, QUEUE, u32::from(QUEUE_SIZE), 0); // SET_VRING_NUM
    fe.vring_state(10, QUEUE, base, 0); // SET_VRING_BASE
    let mut addr = QUEUE.to_ne_bytes().to_vec();
    addr.extend_from_slice(&0u32.to_ne_bytes());
    addr.extend_from_slice(&guest.uva(GPA_BASE + DESC).to_ne_bytes());
    addr.extend_from_slice(&guest.uva(GPA_BASE + USED).to_ne_bytes());
    addr.extend_from_slice(&guest.uva(GPA_BASE + AVAIL).to_ne_bytes());
    addr.extend_from_slice(&0u64.to_ne_bytes());
    fe.send(9, 0, &addr, &[]); // SET_VRING_ADDR
    let (kick_rd, kick_wr) = pipe();
    let (call_rd, call_wr) = pipe();
    fe.send(
        12,
        0,
        &u64::from(QUEUE).to_ne_bytes(),
        &[kick_rd.as_raw_fd()],
    ); // SET_VRING_KICK
    fe.send(
        13,
        0,
        &u64::from(QUEUE).to_ne_bytes(),
        &[call_wr.as_raw_fd()],
    ); // SET_VRING_CALL
    fe.vring_state(18, QUEUE, 1, 0); // SET_VRING_ENABLE
    (kick_wr, call_rd)
}

#[test]
fn qemu_style_session_processes_rings_and_survives_a_restart() {
    let (front, back) = UnixStream::pair().unwrap();
    let acked = Arc::new(AtomicU64::new(0));
    let resets = Arc::new(AtomicUsize::new(0));
    let device = Upper {
        acked: Arc::clone(&acked),
        resets: Arc::clone(&resets),
    };
    let handle = Backend::new("test", device).spawn(back).unwrap();
    // A reply the back-end never sends fails the test instead of hanging it.
    front
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let fe = Frontend { sock: front };
    let guest = Guest::new();

    // Negotiation.
    let offered = fe.get_u64(1);
    assert_eq!(offered, F_VERSION_1 | F_EVENT_IDX | F_PROTOCOL_FEATURES);
    let protocol = fe.get_u64(15);
    assert_eq!(protocol & (P_MQ | P_REPLY_ACK), P_MQ | P_REPLY_ACK);
    fe.send(16, 0, &(P_MQ | P_REPLY_ACK).to_ne_bytes(), &[]);
    assert_eq!(fe.get_u64(17), 2);
    fe.send(3, 0, &[], &[]); // SET_OWNER
    // The guest driver resets the device before it starts it; QEMU keeps
    // relying on the protocol features negotiated before.
    fe.send(34, 0, &[], &[]); // RESET_DEVICE
    let (_, call_wr) = pipe();
    assert_eq!(fe.acked(13, &0u64.to_ne_bytes(), &[call_wr.as_raw_fd()]), 0); // SET_VRING_CALL
    fe.send(
        2,
        0,
        &(F_VERSION_1 | F_EVENT_IDX | F_PROTOCOL_FEATURES).to_ne_bytes(),
        &[],
    );

    // Guest RAM: one region at GPA_BASE, mapped by the front-end elsewhere.
    let mut table = 1u32.to_ne_bytes().to_vec();
    table.extend_from_slice(&0u32.to_ne_bytes());
    table.extend_from_slice(&GPA_BASE.to_ne_bytes());
    table.extend_from_slice(&(RAM as u64).to_ne_bytes());
    table.extend_from_slice(&guest.uva(GPA_BASE).to_ne_bytes());
    table.extend_from_slice(&0u64.to_ne_bytes());
    assert_eq!(fe.acked(5, &table, &[guest.file.as_raw_fd()]), 0);

    // A bad request with NEED_REPLY fails without dropping the connection.
    let mut bad = QUEUE.to_ne_bytes().to_vec();
    bad.extend_from_slice(&3u32.to_ne_bytes());
    assert_eq!(fe.acked(8, &bad, &[]), 1);

    // EVENT_IDX: the guest wants an interrupt once used.idx passes 0.
    guest.write(
        GPA_BASE + AVAIL + 4 + u64::from(QUEUE_SIZE) * 2,
        &0u16.to_le_bytes(),
    );
    let (kick_wr, call_rd) = start_queue(&fe, &guest, 0);
    let out = guest.post(0, b"hello");
    kick(&kick_wr);
    assert!(
        wait_readable(&call_rd),
        "no interrupt for the first completion"
    );
    drain(&call_rd);
    assert_eq!(guest.u16_at(GPA_BASE + USED + 2), 1);
    assert_eq!(guest.read(GPA_BASE + USED + 4, 8), [0, 0, 0, 0, 5, 0, 0, 0]);
    assert_eq!(guest.read(out, 5), b"HELLO");
    // The device re-armed kicks: avail_event = consumed cursor.
    assert_eq!(
        guest.u16_at(GPA_BASE + USED + 4 + u64::from(QUEUE_SIZE) * 8),
        1
    );
    assert_eq!(acked.load(Ordering::SeqCst), F_VERSION_1 | F_EVENT_IDX);

    // Stop: the cursor comes back.
    fe.vring_state(11, QUEUE, 0, 0);
    let state = fe.reply(11);
    assert_eq!(state, [1, 0, 0, 0, 1, 0, 0, 0]);

    // Restart from that cursor; only the new chain is consumed.
    guest.write(
        GPA_BASE + AVAIL + 4 + u64::from(QUEUE_SIZE) * 2,
        &1u16.to_le_bytes(),
    );
    let (kick_wr, call_rd) = start_queue(&fe, &guest, 1);
    let out = guest.post(1, b"again");
    kick(&kick_wr);
    assert!(wait_readable(&call_rd), "no interrupt after restart");
    assert_eq!(guest.u16_at(GPA_BASE + USED + 2), 2);
    assert_eq!(
        guest.read(GPA_BASE + USED + 12, 8),
        [2, 0, 0, 0, 5, 0, 0, 0]
    );
    assert_eq!(guest.read(out, 5), b"AGAIN");

    // EVENT_IDX suppression: used_event stays at 1, so publishing used.idx
    // 2 -> 3 must not interrupt.
    drain(&call_rd);
    let out = guest.post(2, b"quiet");
    kick(&kick_wr);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while guest.u16_at(GPA_BASE + USED + 2) != 3 {
        assert!(
            std::time::Instant::now() < deadline,
            "third chain not consumed"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(guest.read(out, 5), b"QUIET");
    let mut pfd = libc::pollfd {
        fd: call_rd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    assert_eq!(
        unsafe { libc::poll(&raw mut pfd, 1, 100) },
        0,
        "suppressed interrupt was raised"
    );

    // The front-end goes away: the device is reset and handed back.
    drop(fe);
    let device = handle.join().unwrap();
    assert_eq!(
        resets.load(Ordering::SeqCst),
        2,
        "RESET_DEVICE and disconnect"
    );
    drop(device);
}

/// A device with an 8-byte config space, like vhost-user-vsock's guest CID.
struct WithConfig;

impl VhostUserDevice for WithConfig {
    fn num_queues(&self) -> usize {
        2
    }

    fn features(&self) -> u64 {
        F_VERSION_1
    }

    fn protocol_features(&self) -> arcbox_vhost_user::ProtocolFeatures {
        arcbox_vhost_user::ProtocolFeatures::CONFIG
    }

    fn read_config(&self, offset: u32, data: &mut [u8]) -> arcbox_vhost_user::Result<()> {
        let config = 0x1122_3344_5566_7788u64.to_le_bytes();
        data.copy_from_slice(&config[offset as usize..offset as usize + data.len()]);
        Ok(())
    }

    fn process_queue(&mut self, _: u16, _: &mut Vrings) -> arcbox_vhost_user::Result<()> {
        Ok(())
    }
}

fn get_config(fe: &Frontend, offset: u32, size: u32) -> Vec<u8> {
    let mut p = offset.to_ne_bytes().to_vec();
    p.extend_from_slice(&size.to_ne_bytes());
    p.extend_from_slice(&0u32.to_ne_bytes());
    p.extend(std::iter::repeat_n(0u8, size as usize));
    fe.send(24, 0, &p, &[]);
    fe.reply(24)
}

#[test]
fn config_space_is_served_only_once_negotiated() {
    let (front, back) = UnixStream::pair().unwrap();
    front
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let handle = Backend::new("test", WithConfig).spawn(back).unwrap();
    let fe = Frontend { sock: front };
    const P_CONFIG: u64 = 1 << 9;
    assert_eq!(fe.get_u64(15) & P_CONFIG, P_CONFIG);
    fe.send(16, 0, &(P_CONFIG | P_REPLY_ACK).to_ne_bytes(), &[]);

    // Upper half of the 8-byte config: the reply echoes offset and size and
    // carries exactly the bytes asked for.
    let reply = get_config(&fe, 4, 4);
    assert_eq!(&reply[..4], &4u32.to_ne_bytes());
    assert_eq!(&reply[4..8], &4u32.to_ne_bytes());
    assert_eq!(&reply[12..], &[0x44, 0x33, 0x22, 0x11]);
    drop(fe);
    handle.join().unwrap();

    // Without CONFIG negotiated the request ends the connection.
    let (front, back) = UnixStream::pair().unwrap();
    let handle = Backend::new("test", WithConfig).spawn(back).unwrap();
    let fe = Frontend { sock: front };
    let mut p = 0u32.to_ne_bytes().to_vec();
    p.extend_from_slice(&8u32.to_ne_bytes());
    p.extend_from_slice(&[0u8; 12]);
    fe.send(24, 0, &p, &[]);
    assert!(handle.join().is_err());
}

#[test]
fn unknown_request_ends_the_connection() {
    let (front, back) = UnixStream::pair().unwrap();
    let device = Upper {
        acked: Arc::default(),
        resets: Arc::default(),
    };
    let handle = Backend::new("test", device).spawn(back).unwrap();
    let fe = Frontend { sock: front };
    fe.send(99, 0, &[], &[]);
    // The front-end reads EOF while the handle, which holds a clone of the
    // socket, is still alive.
    use std::io::Read;
    fe.sock
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    assert_eq!((&fe.sock).read(&mut [0u8; 1]).unwrap(), 0);
    assert!(handle.join().is_err());
}
