//! The vhost-user wire format, as far as a back-end serving QEMU needs it.
//!
//! Every message is a 12-byte header (`request`, `flags`, `size`) followed by
//! `size` bytes of payload in host byte order, with file descriptors carried
//! as `SCM_RIGHTS` ancillary data on the same `sendmsg`. The authoritative
//! reference is QEMU's `docs/interop/vhost-user.rst`; the payload layouts below
//! mirror `hw/virtio/vhost-user.c`.

use crate::error::{Error, Result};

/// Header size in bytes.
pub const HEADER_SIZE: usize = 12;
/// Largest payload this back-end accepts. The largest message it serves is
/// `SET_MEM_TABLE` with eight regions (264 bytes) or a 256-byte config read.
pub const MAX_PAYLOAD: usize = 4096;
/// Most file descriptors one message carries (`SET_MEM_TABLE`, one per region).
pub const MAX_FDS: usize = 8;

/// Protocol version carried in the low two bits of `flags`.
const VERSION: u32 = 0x1;
const VERSION_MASK: u32 = 0x3;
/// The message is a reply.
const FLAG_REPLY: u32 = 1 << 2;
/// The front-end asks for a `REPLY_ACK` status reply.
const FLAG_NEED_REPLY: u32 = 1 << 3;

/// Virtio feature bit the back-end sets in `GET_FEATURES` to say it speaks the
/// protocol-feature extension (`GET/SET_PROTOCOL_FEATURES`, `SET_VRING_ENABLE`).
pub const VHOST_USER_F_PROTOCOL_FEATURES: u64 = 1 << 30;

/// Front-end → back-end request codes this back-end recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Request {
    GetFeatures = 1,
    SetFeatures = 2,
    SetOwner = 3,
    ResetOwner = 4,
    SetMemTable = 5,
    SetLogBase = 6,
    SetLogFd = 7,
    SetVringNum = 8,
    SetVringAddr = 9,
    SetVringBase = 10,
    GetVringBase = 11,
    SetVringKick = 12,
    SetVringCall = 13,
    SetVringErr = 14,
    GetProtocolFeatures = 15,
    SetProtocolFeatures = 16,
    GetQueueNum = 17,
    SetVringEnable = 18,
    SetBackendReqFd = 21,
    GetConfig = 24,
    SetConfig = 25,
    GetInflightFd = 31,
    SetInflightFd = 32,
    ResetDevice = 34,
    GetMaxMemSlots = 36,
    AddMemReg = 37,
    RemMemReg = 38,
    SetStatus = 39,
    GetStatus = 40,
}

impl Request {
    /// Decodes a request code; `None` for codes this back-end does not know.
    #[must_use]
    pub const fn from_u32(code: u32) -> Option<Self> {
        Some(match code {
            1 => Self::GetFeatures,
            2 => Self::SetFeatures,
            3 => Self::SetOwner,
            4 => Self::ResetOwner,
            5 => Self::SetMemTable,
            6 => Self::SetLogBase,
            7 => Self::SetLogFd,
            8 => Self::SetVringNum,
            9 => Self::SetVringAddr,
            10 => Self::SetVringBase,
            11 => Self::GetVringBase,
            12 => Self::SetVringKick,
            13 => Self::SetVringCall,
            14 => Self::SetVringErr,
            15 => Self::GetProtocolFeatures,
            16 => Self::SetProtocolFeatures,
            17 => Self::GetQueueNum,
            18 => Self::SetVringEnable,
            21 => Self::SetBackendReqFd,
            24 => Self::GetConfig,
            25 => Self::SetConfig,
            31 => Self::GetInflightFd,
            32 => Self::SetInflightFd,
            34 => Self::ResetDevice,
            36 => Self::GetMaxMemSlots,
            37 => Self::AddMemReg,
            38 => Self::RemMemReg,
            39 => Self::SetStatus,
            40 => Self::GetStatus,
            _ => return None,
        })
    }

    /// Whether the request always solicits a reply, so `REPLY_ACK` adds none.
    #[must_use]
    pub const fn has_reply(self) -> bool {
        matches!(
            self,
            Self::GetFeatures
                | Self::GetProtocolFeatures
                | Self::GetVringBase
                | Self::GetQueueNum
                | Self::GetConfig
                | Self::GetInflightFd
                | Self::GetMaxMemSlots
                | Self::GetStatus
        )
    }
}

/// `VHOST_USER_PROTOCOL_F_*` bits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProtocolFeatures(u64);

impl ProtocolFeatures {
    /// `GET_QUEUE_NUM` is supported.
    pub const MQ: Self = Self(1 << 0);
    /// The back-end acknowledges requests that set `NEED_REPLY`.
    pub const REPLY_ACK: Self = Self(1 << 3);
    /// `GET_CONFIG`/`SET_CONFIG` are supported (vhost-user-vsock requires it).
    pub const CONFIG: Self = Self(1 << 9);
    /// `RESET_DEVICE` is supported.
    pub const RESET_DEVICE: Self = Self(1 << 13);

    /// No protocol features.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Builds the set from raw bits.
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// Raw bits.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Whether every bit of `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for ProtocolFeatures {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// A message header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Raw request code (see [`Request::from_u32`]).
    pub request: u32,
    /// Version and reply flags.
    pub flags: u32,
    /// Payload length in bytes.
    pub size: u32,
}

impl Header {
    /// Decodes a header and checks it is a version-1 request, not a reply.
    pub fn decode(bytes: &[u8; HEADER_SIZE]) -> Result<Self> {
        let header = Self {
            request: read_u32(bytes, 0),
            flags: read_u32(bytes, 4),
            size: read_u32(bytes, 8),
        };
        if header.flags & VERSION_MASK != VERSION {
            return Err(Error::Protocol(format!(
                "unsupported vhost-user version in flags {:#x}",
                header.flags
            )));
        }
        if header.flags & FLAG_REPLY != 0 {
            return Err(Error::Protocol(format!(
                "front-end sent a reply (request {})",
                header.request
            )));
        }
        if header.size as usize > MAX_PAYLOAD {
            return Err(Error::Protocol(format!(
                "request {} payload of {} bytes exceeds {MAX_PAYLOAD}",
                header.request, header.size
            )));
        }
        Ok(header)
    }

    /// Whether the front-end asked for a `REPLY_ACK` status.
    #[must_use]
    pub const fn need_reply(&self) -> bool {
        self.flags & FLAG_NEED_REPLY != 0
    }
}

/// Encodes a reply to `request` carrying `payload`.
#[must_use]
pub fn encode_reply(request: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_SIZE + payload.len());
    out.extend_from_slice(&request.to_ne_bytes());
    out.extend_from_slice(&(VERSION | FLAG_REPLY).to_ne_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    out.extend_from_slice(payload);
    out
}

/// Decodes a `u64` payload (features, protocol features, vring fd index).
pub fn decode_u64(payload: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = payload.try_into().map_err(|_| {
        Error::Protocol(format!("expected an 8-byte payload, got {}", payload.len()))
    })?;
    Ok(u64::from_ne_bytes(bytes))
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4-byte slice"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_bytes(request: u32, flags: u32, size: u32) -> [u8; HEADER_SIZE] {
        let mut out = [0u8; HEADER_SIZE];
        out[..4].copy_from_slice(&request.to_ne_bytes());
        out[4..8].copy_from_slice(&flags.to_ne_bytes());
        out[8..].copy_from_slice(&size.to_ne_bytes());
        out
    }

    #[test]
    fn header_rejects_replies_wrong_versions_and_oversized_payloads() {
        let ok = Header::decode(&header_bytes(5, VERSION | FLAG_NEED_REPLY, 264)).unwrap();
        assert!(ok.need_reply());
        assert!(Header::decode(&header_bytes(5, VERSION | FLAG_REPLY, 0)).is_err());
        assert!(Header::decode(&header_bytes(5, 0x2, 0)).is_err());
        assert!(Header::decode(&header_bytes(5, VERSION, MAX_PAYLOAD as u32 + 1)).is_err());
    }

    #[test]
    fn reply_round_trips_through_the_header_codec() {
        let reply = encode_reply(Request::GetVringBase as u32, &[1, 2, 3]);
        let header: [u8; HEADER_SIZE] = reply[..HEADER_SIZE].try_into().unwrap();
        assert_eq!(read_u32(&header, 0), 11);
        assert_eq!(read_u32(&header, 4), VERSION | FLAG_REPLY);
        assert_eq!(read_u32(&header, 8), 3);
        assert_eq!(&reply[HEADER_SIZE..], &[1, 2, 3]);
    }
}
