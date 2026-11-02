//! Guest-side agent RPC client for `qemu_system_vm_probe`.
//!
//! Sends one request to the agent over vsock loopback (CID 1, port 1024),
//! the way the daemon would over the host transport, and prints the reply's
//! type and top-level protobuf fields on one line. Not part of any crate: the
//! probe builds it with `rustc --target aarch64-unknown-linux-musl`.
//!
//! ```text
//! agent-rpc <message type> [hex payload]    # agent-rpc 3 0801 = EnsureRuntime
//! ```

use std::io::{Read, Write};
use std::os::fd::FromRawFd;

const AF_VSOCK: i32 = 40;
const SOCK_STREAM: i32 = 1;
const VMADDR_CID_LOCAL: u32 = 1;
const AGENT_PORT: u32 = 1024;

#[repr(C)]
struct SockaddrVm {
    family: u16,
    reserved: u16,
    port: u32,
    cid: u32,
    flags: u8,
    zero: [u8; 3],
}

unsafe extern "C" {
    fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
    fn connect(fd: i32, addr: *const SockaddrVm, len: u32) -> i32;
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let ty: u32 = args
        .get(1)
        .and_then(|a| a.parse().ok())
        .expect("message type");
    let payload: Vec<u8> = args.get(2).map_or_else(Vec::new, |hex| {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex payload"))
            .collect()
    });

    // SAFETY: plain socket syscalls on a sockaddr that lives across the call;
    // the descriptor is owned by the File below.
    let mut stream = unsafe {
        let fd = socket(AF_VSOCK, SOCK_STREAM, 0);
        let addr = SockaddrVm {
            family: AF_VSOCK as u16,
            reserved: 0,
            port: AGENT_PORT,
            cid: VMADDR_CID_LOCAL,
            flags: 0,
            zero: [0; 3],
        };
        if fd < 0 || connect(fd, &raw const addr, size_of::<SockaddrVm>() as u32) != 0 {
            eprintln!("connect: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }
        std::fs::File::from_raw_fd(fd)
    };

    // Wire format V2: length, type, trace length (no trace), payload.
    let mut frame = (6 + payload.len() as u32).to_be_bytes().to_vec();
    frame.extend(ty.to_be_bytes());
    frame.extend(0u16.to_be_bytes());
    frame.extend(&payload);
    stream.write_all(&frame).expect("write request");

    let mut len = [0u8; 4];
    stream.read_exact(&mut len).expect("read reply length");
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut body).expect("read reply");
    let reply_type = u32::from_be_bytes(body[0..4].try_into().unwrap());
    let trace_len = usize::from(u16::from_be_bytes(body[4..6].try_into().unwrap()));
    let message = &body[6 + trace_len..];
    println!("type={reply_type:#06x} {}", fields(message));
}

/// Top-level protobuf fields as `n=value`; the raw bytes if it is not one.
fn fields(mut buf: &[u8]) -> String {
    let mut out = Vec::new();
    while !buf.is_empty() {
        let Some(key) = varint(&mut buf) else { break };
        let value = match key & 7 {
            0 => varint(&mut buf).map(|v| v.to_string()),
            2 => varint(&mut buf).and_then(|n| {
                let (bytes, rest) = buf.split_at_checked(n as usize)?;
                buf = rest;
                Some(format!("{:?}", String::from_utf8_lossy(bytes)))
            }),
            _ => None,
        };
        match value {
            Some(v) => out.push(format!("{}={v}", key >> 3)),
            None => return format!("raw={:?}", String::from_utf8_lossy(buf)),
        }
    }
    out.join(" ")
}

fn varint(buf: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = buf.split_first()?;
        *buf = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}
