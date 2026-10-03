//! Bindings to the `dns_sd` API (`<dns_sd.h>`) that register host records
//! with mDNSResponder, and a connection that owns one `DNSServiceRef`.
//!
//! Only the calls the mirror needs are declared. The symbols live in
//! `libsystem_dnssd`, which `libSystem` re-exports, so no link attribute is
//! needed.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;
use std::net::IpAddr;
use std::os::fd::RawFd;

type DnsServiceRef = *mut c_void;
type DnsRecordRef = *mut c_void;
type DnsServiceRegisterRecordReply = unsafe extern "C" fn(
    sd_ref: DnsServiceRef,
    record_ref: DnsRecordRef,
    flags: u32,
    error: i32,
    context: *mut c_void,
);

/// `kDNSServiceFlagsUnique`: one owner per name; a second registration of
/// the same name is answered `kDNSServiceErr_NameConflict`.
const FLAGS_UNIQUE: u32 = 0x20;
/// The interface the records are bound to. Loopback has no multicast peers,
/// so nothing leaves the host, yet the records are ordinary authoritative
/// records: mDNSResponder answers a query for a type the name lacks (AAAA
/// for an IPv4 name) at once, as it does for records on any real interface.
/// `kDNSServiceInterfaceIndexLocalOnly` records never get that negative, so
/// a Mac without the resolver file waited out the 5 s mDNS timeout on AAAA
/// (measured 2026-10-01, `docs/experiments/2026-10-01-local-domain-negative-answers.md`).
const INTERFACE_NAME: &CStr = c"lo0";
const CLASS_IN: u16 = 1;
const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
/// `kDNSServiceErr_PolicyDenied`: the process has no Local Network access
/// (TN3179), so mDNSResponder refuses every Bonjour operation.
pub const ERR_POLICY_DENIED: i32 = -65570;

unsafe extern "C" {
    fn DNSServiceCreateConnection(sd_ref: *mut DnsServiceRef) -> i32;
    fn DNSServiceRegisterRecord(
        sd_ref: DnsServiceRef,
        record_ref: *mut DnsRecordRef,
        flags: u32,
        interface_index: u32,
        fullname: *const c_char,
        rrtype: u16,
        rrclass: u16,
        rdlen: u16,
        rdata: *const c_void,
        ttl: u32,
        callback: DnsServiceRegisterRecordReply,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceRemoveRecord(sd_ref: DnsServiceRef, record_ref: DnsRecordRef, flags: u32) -> i32;
    fn DNSServiceRefSockFD(sd_ref: DnsServiceRef) -> c_int;
    fn DNSServiceProcessResult(sd_ref: DnsServiceRef) -> i32;
    fn DNSServiceRefDeallocate(sd_ref: DnsServiceRef);
}

/// A record registered on a [`Connection`], identified by the address of
/// its `DNSRecordRef`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordHandle(usize);

/// mDNSResponder's asynchronous answer to a registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    pub handle: RecordHandle,
    /// `kDNSServiceErr_NoError` once the record is live, otherwise the
    /// `kDNSServiceErr_*` code that refused it.
    pub error: i32,
}

/// One shared connection to mDNSResponder and the records registered on it.
///
/// Dropping the connection deallocates the `DNSServiceRef`, which removes
/// every record registered through it.
pub struct Connection {
    sd_ref: DnsServiceRef,
    /// Index of [`INTERFACE_NAME`], resolved once.
    interface_index: u32,
    /// Replies the callback appends while [`Connection::process`] runs.
    /// Boxed so the address handed to mDNSResponder as the callback context
    /// stays valid when the connection moves.
    replies: Box<RefCell<Vec<Reply>>>,
}

// SAFETY: dns_sd does no locking and asks only that a DNSServiceRef is not
// used from two threads at once (`dns_sd.h`, "Thread Safety"); it binds no
// ref to a thread. One task owns the connection and makes every call on it
// sequentially, and the callback runs on that task's thread, inside
// `DNSServiceProcessResult`.
unsafe impl Send for Connection {}

impl Connection {
    /// Connects to mDNSResponder.
    pub fn open() -> io::Result<Self> {
        // SAFETY: a valid NUL-terminated interface name.
        let interface_index = unsafe { libc::if_nametoindex(INTERFACE_NAME.as_ptr()) };
        if interface_index == 0 {
            return Err(io::Error::other(format!(
                "no interface {}",
                INTERFACE_NAME.to_string_lossy()
            )));
        }
        let mut sd_ref: DnsServiceRef = std::ptr::null_mut();
        // SAFETY: `sd_ref` is a valid out-pointer for the duration of the call.
        let error = unsafe { DNSServiceCreateConnection(&raw mut sd_ref) };
        if error != 0 {
            return Err(dns_sd_error("DNSServiceCreateConnection", error));
        }
        Ok(Self {
            sd_ref,
            interface_index,
            replies: Box::new(RefCell::new(Vec::new())),
        })
    }

    /// The socket that becomes readable when a reply is waiting.
    pub fn fd(&self) -> RawFd {
        // SAFETY: `sd_ref` is a live connection until `Drop`.
        unsafe { DNSServiceRefSockFD(self.sd_ref) }
    }

    /// Registers `fqdn` → `ip` as a unique address record on the loopback
    /// interface.
    ///
    /// The outcome arrives later as a [`Reply`] from [`Connection::process`],
    /// after mDNSResponder's probe for the name (about 750 ms).
    pub fn register_record(&mut self, fqdn: &str, ip: IpAddr) -> io::Result<RecordHandle> {
        let name = CString::new(format!("{fqdn}."))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in DNS name"))?;
        let v4;
        let v6;
        let (rrtype, rdata): (u16, &[u8]) = match ip {
            IpAddr::V4(ip) => {
                v4 = ip.octets();
                (TYPE_A, &v4)
            }
            IpAddr::V6(ip) => {
                v6 = ip.octets();
                (TYPE_AAAA, &v6)
            }
        };
        let rdlen = u16::try_from(rdata.len()).expect("an address is 4 or 16 bytes");
        let mut record_ref: DnsRecordRef = std::ptr::null_mut();
        let context = std::ptr::from_ref::<RefCell<Vec<Reply>>>(&*self.replies)
            .cast_mut()
            .cast::<c_void>();
        // SAFETY: every pointer is valid for the call; `context` points at the
        // boxed reply list, which lives as long as the connection and so as
        // long as any record registered on it. A TTL of 0 lets mDNSResponder
        // pick its default.
        let error = unsafe {
            DNSServiceRegisterRecord(
                self.sd_ref,
                &raw mut record_ref,
                FLAGS_UNIQUE,
                self.interface_index,
                name.as_ptr(),
                rrtype,
                CLASS_IN,
                rdlen,
                rdata.as_ptr().cast(),
                0,
                reply,
                context,
            )
        };
        if error != 0 {
            return Err(dns_sd_error("DNSServiceRegisterRecord", error));
        }
        Ok(RecordHandle(record_ref.addr()))
    }

    /// Removes a record; also the required disposal of a record that was
    /// refused.
    pub fn remove_record(&mut self, handle: RecordHandle) {
        // SAFETY: the handle came from `register_record` on this connection
        // and is removed at most once by the mirror that owns it.
        let error = unsafe {
            DNSServiceRemoveRecord(self.sd_ref, std::ptr::without_provenance_mut(handle.0), 0)
        };
        if error != 0 {
            tracing::debug!(error, "DNSServiceRemoveRecord failed");
        }
    }

    /// Whether a reply is waiting on the socket right now.
    pub fn has_pending_reply(&self) -> bool {
        let mut pollfd = libc::pollfd {
            fd: self.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pollfd` is one valid entry; a zero timeout never blocks.
        unsafe { libc::poll(&raw mut pollfd, 1, 0) == 1 }
    }

    /// Reads one reply from mDNSResponder and returns the registrations it
    /// settled. Blocks when none is waiting, so call it after the socket
    /// became readable or [`Connection::has_pending_reply`] returned true.
    pub fn process(&mut self) -> io::Result<Vec<Reply>> {
        // SAFETY: `sd_ref` is live; the callback it runs touches only the
        // boxed reply list.
        let error = unsafe { DNSServiceProcessResult(self.sd_ref) };
        if error != 0 {
            return Err(dns_sd_error("DNSServiceProcessResult", error));
        }
        Ok(self.replies.borrow_mut().drain(..).collect())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: `sd_ref` is live and no record handle outlives the mirror
        // that owns this connection.
        unsafe { DNSServiceRefDeallocate(self.sd_ref) }
    }
}

unsafe extern "C" fn reply(
    _sd_ref: DnsServiceRef,
    record_ref: DnsRecordRef,
    _flags: u32,
    error: i32,
    context: *mut c_void,
) {
    // SAFETY: `context` is the reply list of the connection the record was
    // registered on (see `register_record`), and the list is borrowed only here,
    // from inside `DNSServiceProcessResult`, and in `process` after it
    // returns.
    let replies = unsafe { &*context.cast::<RefCell<Vec<Reply>>>() };
    replies.borrow_mut().push(Reply {
        handle: RecordHandle(record_ref.addr()),
        error,
    });
}

fn dns_sd_error(call: &str, error: i32) -> io::Error {
    io::Error::other(format!("{call} failed: kDNSServiceErr {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::{Duration, Instant};

    /// Exercises the real bindings end to end: the callback signature, the
    /// context pointer and the handle round trip. Whether the record is
    /// accepted depends on this process's Local Network access (TN3179), so
    /// both the success and the policy-denied reply count as a working
    /// binding; anything else is a binding bug.
    #[test]
    fn a_registration_round_trips_through_mdnsresponder() {
        let mut connection = Connection::open().expect("mDNSResponder is running");
        let fqdn = format!("arcbox-dns-sd-test-{}.arcbox.local", std::process::id());
        let handle = connection
            .register_record(&fqdn, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 9)))
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut replies = Vec::new();
        while replies.is_empty() && Instant::now() < deadline {
            if connection.has_pending_reply() {
                replies.extend(connection.process().unwrap());
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        assert_eq!(replies.len(), 1, "one reply for one registration");
        assert_eq!(replies[0].handle, handle);
        eprintln!("mDNSResponder answered kDNSServiceErr {}", replies[0].error);
        assert!(
            matches!(replies[0].error, 0 | ERR_POLICY_DENIED),
            "unexpected kDNSServiceErr {}",
            replies[0].error
        );
        connection.remove_record(handle);
    }
}
