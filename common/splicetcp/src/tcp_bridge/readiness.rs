//! Level-triggered TCP read readiness without Darwin's zero-timeout wait path.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

// sys/event.h defines this flag; libc does not expose the constant.
const KEVENT_FLAG_IMMEDIATE: u32 = 0x000001;

pub(super) struct Readiness {
    queue: OwnedFd,
    changes: Vec<libc::kevent64_s>,
    events: Vec<libc::kevent64_s>,
    ready: Vec<RawFd>,
}

impl Readiness {
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: kqueue takes no pointers and returns a new descriptor or an error.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            // SAFETY: the successful kqueue call transfers this descriptor to us.
            queue: unsafe { OwnedFd::from_raw_fd(fd) },
            changes: Vec::new(),
            events: Vec::new(),
            ready: Vec::new(),
        })
    }

    pub(super) fn poll<'a>(
        &mut self,
        streams: impl Iterator<Item = (BorrowedFd<'a>, bool)>,
    ) -> io::Result<()> {
        self.ready.clear();
        self.changes.clear();
        let mut count = 0;
        for (fd, registered) in streams {
            count += 1;
            if !registered {
                self.changes.push(libc::kevent64_s {
                    ident: fd.as_raw_fd() as u64,
                    filter: libc::EVFILT_READ,
                    // Keep unread data and EOF ready across bounded reads.
                    flags: libc::EV_ADD,
                    fflags: 0,
                    data: 0,
                    udata: 0,
                    ext: [0; 2],
                });
            }
        }
        if count == 0 {
            return Ok(());
        }
        let capacity = i32::try_from(count)
            .map_err(|_| io::Error::other("TCP read readiness exceeds kevent capacity"))?;
        let changes = i32::try_from(self.changes.len())
            .expect("changes cannot exceed the checked stream count");
        self.events.resize(
            count,
            libc::kevent64_s {
                ident: 0,
                filter: 0,
                flags: 0,
                fflags: 0,
                data: 0,
                udata: 0,
                ext: [0; 2],
            },
        );
        // SAFETY: both buffers cover their counts; every borrowed descriptor stays open for this call.
        let received = unsafe {
            libc::kevent64(
                self.queue.as_raw_fd(),
                self.changes.as_ptr(),
                changes,
                self.events.as_mut_ptr(),
                capacity,
                KEVENT_FLAG_IMMEDIATE,
                std::ptr::null(),
            )
        };
        if received < 0 {
            return Err(io::Error::last_os_error());
        }
        for event in &self.events[..received as usize] {
            if event.flags & libc::EV_ERROR != 0 {
                return Err(io::Error::from_raw_os_error(event.data as i32));
            }
            self.ready.push(event.ident as RawFd);
        }
        self.ready.sort_unstable();
        Ok(())
    }

    pub(super) fn is_ready(&self, fd: BorrowedFd<'_>) -> bool {
        self.ready.binary_search(&fd.as_raw_fd()).is_ok()
    }
}

impl super::TcpBridge {
    pub(super) fn poll_readiness(&mut self) -> io::Result<()> {
        // Include retained EOF sockets so their events cannot crowd out live reads.
        self.readiness.poll(
            self.fast_path_conns
                .values()
                .filter(|c| !c.inline_owned)
                .map(|c| (c.stream.as_fd(), c.read_registered)),
        )?;
        for conn in self
            .fast_path_conns
            .values_mut()
            .filter(|c| !c.inline_owned)
        {
            conn.read_registered = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(true).unwrap();
        (stream, peer)
    }

    fn wait_ready(readiness: &mut Readiness, stream: &TcpStream) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            readiness
                .poll(std::iter::once((stream.as_fd(), true)))
                .unwrap();
            if readiness.is_ready(stream.as_fd()) {
                break;
            }
            assert!(Instant::now() < deadline, "read readiness did not arrive");
            std::thread::yield_now();
        }
    }

    #[test]
    fn bounded_reads_keep_data_ready_and_deliver_eof() {
        let mut readiness = Readiness::new().unwrap();
        let (mut stream, mut peer) = pair();
        readiness
            .poll(std::iter::once((stream.as_fd(), false)))
            .unwrap();
        assert!(!readiness.is_ready(stream.as_fd()));
        peer.write_all(b"abcdef").unwrap();
        wait_ready(&mut readiness, &stream);
        let mut prefix = [0; 2];
        stream.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"ab");
        readiness
            .poll(std::iter::once((stream.as_fd(), true)))
            .unwrap();
        assert!(readiness.is_ready(stream.as_fd()));
        let mut tail = [0; 4];
        stream.read_exact(&mut tail).unwrap();
        assert_eq!(&tail, b"cdef");
        readiness
            .poll(std::iter::once((stream.as_fd(), true)))
            .unwrap();
        assert!(!readiness.is_ready(stream.as_fd()));
        peer.shutdown(Shutdown::Write).unwrap();
        wait_ready(&mut readiness, &stream);
        assert_eq!(stream.read(&mut prefix).unwrap(), 0);
        readiness
            .poll(std::iter::once((stream.as_fd(), true)))
            .unwrap();
        assert!(readiness.is_ready(stream.as_fd()));
    }

    #[test]
    fn replaced_descriptor_does_not_keep_old_readiness() {
        let mut readiness = Readiness::new().unwrap();
        let (stream, mut old_peer) = pair();
        readiness
            .poll(std::iter::once((stream.as_fd(), false)))
            .unwrap();
        old_peer.write_all(b"old").unwrap();
        wait_ready(&mut readiness, &stream);
        let (replacement, mut new_peer) = pair();
        // SAFETY: both descriptors are owned by this test; replacing stream cannot close another test's descriptor.
        let replaced = unsafe { libc::dup2(replacement.as_raw_fd(), stream.as_raw_fd()) };
        assert_eq!(replaced, stream.as_raw_fd());
        readiness
            .poll(std::iter::once((stream.as_fd(), false)))
            .unwrap();
        assert!(!readiness.is_ready(stream.as_fd()));
        new_peer.write_all(b"new").unwrap();
        wait_ready(&mut readiness, &stream);
        let mut data = [0; 3];
        (&stream).read_exact(&mut data).unwrap();
        assert_eq!(&data, b"new");
        drop(stream);
        readiness.poll(std::iter::empty()).unwrap();
        assert!(readiness.ready.is_empty());
    }

    #[test]
    fn polling_errors_are_not_reported_as_idle_sockets() {
        let mut readiness = Readiness::new().unwrap();
        let (stream, _peer) = pair();
        readiness.queue = std::fs::File::open("/dev/null").unwrap().into();
        assert!(
            readiness
                .poll(std::iter::once((stream.as_fd(), false)))
                .is_err()
        );
    }
}
