use crate::error::{EngineError, Result};
use arcbox_constants::wire::{
    ERROR_HEADER_SIZE, FRAME_HEADER_SIZE, MessageType, TRACE_LEN_FIELD_SIZE, TYPE_FIELD_SIZE,
};
use bytes::{Buf, BufMut, Bytes, BytesMut};

/// Builds a V2 wire message with an optional `trace_id`.
///
/// Wire format V2:
/// ```text
/// +----------------+----------------+------------------+----------------+
/// | Length (4B BE) | Type (4B BE)   | TraceLen (2B BE) | TraceID bytes  | Payload
/// +----------------+----------------+------------------+----------------+
/// ```
pub(super) fn build_message(msg_type: MessageType, trace_id: &str, payload: &[u8]) -> Bytes {
    let trace_bytes = trace_id.as_bytes();
    let trace_len = trace_bytes.len().min(u16::MAX as usize);
    // Length = type(4) + trace_len_field(2) + trace_bytes + payload
    let length = TYPE_FIELD_SIZE + TRACE_LEN_FIELD_SIZE + trace_len + payload.len();
    let mut buf = BytesMut::with_capacity(
        FRAME_HEADER_SIZE + TRACE_LEN_FIELD_SIZE + trace_len + payload.len(),
    );
    buf.put_u32(length as u32);
    buf.put_u32(msg_type as u32);
    buf.put_u16(trace_len as u16);
    if trace_len > 0 {
        buf.extend_from_slice(&trace_bytes[..trace_len]);
    }
    buf.extend_from_slice(payload);
    buf.freeze()
}

/// Parses a complete V2 frame from the bounded transport reader.
/// Returns (`resp_type`, `trace_id`, payload); malformed trace fields fail.
pub(super) fn parse_response(response: &[u8]) -> Result<(u32, String, Vec<u8>)> {
    if response.len() < FRAME_HEADER_SIZE + TRACE_LEN_FIELD_SIZE {
        return Err(EngineError::Machine("response too short".to_string()));
    }
    let mut cursor = std::io::Cursor::new(response);
    cursor.advance(std::mem::size_of::<u32>());
    let resp_type = cursor.get_u32();
    let trace_len = cursor.get_u16() as usize;
    let trace_start = FRAME_HEADER_SIZE + TRACE_LEN_FIELD_SIZE;
    let trace_end = trace_start + trace_len;
    let trace = response.get(trace_start..trace_end).ok_or_else(|| {
        EngineError::Machine("response trace length exceeds the frame".to_string())
    })?;
    let trace_id = std::str::from_utf8(trace)
        .map_err(|_| EngineError::Machine("invalid response trace encoding".to_string()))?;
    Ok((
        resp_type,
        trace_id.to_owned(),
        response[trace_end..].to_vec(),
    ))
}

/// Parses an error response from the agent into `(status code, message)`.
///
/// The code is HTTP-style (400/404/409/412/500/503) and is mapped onto a
/// gRPC status by the API layer via `EngineError::Agent`.
pub(super) fn parse_error_response(payload: &[u8]) -> Result<(i32, String)> {
    if payload.len() < ERROR_HEADER_SIZE {
        return Ok((500, "unknown error".to_string()));
    }

    let mut cursor = std::io::Cursor::new(payload);
    let code = cursor.get_i32();
    let msg_len = cursor.get_u32() as usize;

    if payload.len() < ERROR_HEADER_SIZE + msg_len {
        return Ok((code, "truncated error message".to_string()));
    }

    let message =
        String::from_utf8(payload[ERROR_HEADER_SIZE..ERROR_HEADER_SIZE + msg_len].to_vec())
            .map_err(|_| EngineError::Machine("invalid error message encoding".to_string()))?;
    Ok((code, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_requires_complete_v2_trace_fields() {
        for (frame, expected) in [
            (&[][..], "response too short"),
            (&[0, 0, 0, 3, 0, 0, 0][..], "response too short"),
            (&[0, 0, 0, 4, 0, 0, 0, 1][..], "response too short"),
            (&[0, 0, 0, 5, 0, 0, 0, 1, 0][..], "response too short"),
            (
                &[0, 0, 0, 6, 0, 0, 0, 1, 0, 1][..],
                "trace length exceeds the frame",
            ),
            (
                &[0, 0, 0, 7, 0, 0, 0, 1, 0, 1, 0xff][..],
                "invalid response trace encoding",
            ),
        ] {
            assert!(
                parse_response(frame)
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
        }
    }

    #[test]
    fn response_separates_utf8_trace_from_binary_payload() {
        for trace in ["", "trace-λ"] {
            let frame = build_message(MessageType::InstanceAuthorityAck, trace, &[0, 0xff]);
            let (kind, decoded_trace, payload) = parse_response(&frame).unwrap();
            assert_eq!(kind, MessageType::InstanceAuthorityAck as u32);
            assert_eq!(decoded_trace, trace);
            assert_eq!(payload, [0, 0xff]);
        }
    }

    /// Mirror of the agent's `ErrorResponse::encode` framing.
    fn encode_error(code: i32, message: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&code.to_be_bytes());
        buf.extend_from_slice(&(message.len() as u32).to_be_bytes());
        buf.extend_from_slice(message.as_bytes());
        buf
    }

    #[test]
    fn error_response_roundtrips_code_and_message() {
        let payload = encode_error(412, "nested virtualization required");
        let (code, message) = parse_error_response(&payload).unwrap();
        assert_eq!(code, 412);
        assert_eq!(message, "nested virtualization required");
    }

    #[test]
    fn short_error_payload_degrades_to_500() {
        let (code, message) = parse_error_response(&[0x01]).unwrap();
        assert_eq!(code, 500);
        assert_eq!(message, "unknown error");
    }
}
