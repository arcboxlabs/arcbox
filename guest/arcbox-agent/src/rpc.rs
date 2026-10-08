//! RPC protocol implementation for Guest Agent.
//!
//! This module implements a length-prefixed RPC protocol over vsock.
//! Payloads are protobuf-encoded messages.

#![allow(dead_code)]

use anyhow::{Context, Result};
use buffa::Message;
use bytes::{Buf, BufMut, BytesMut};
use std::io::Cursor;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use arcbox_connect::v1::{
    AgentPingRequest as PingRequest, AgentPingResponse as PingResponse, ContainerFsPathsRequest,
    ContainerFsPathsResponse, DiskTrimRequest, DiskTrimResponse, Empty, EnsureMachineExportRequest,
    EnsureMachineExportResponse, EnsureNfsExportRequest, EnsureNfsExportResponse,
    ImageFsPathsRequest, ImageFsPathsResponse, InstanceAuthorityAck, KubernetesDeleteRequest,
    KubernetesDeleteResponse, KubernetesKubeconfigRequest, KubernetesKubeconfigResponse,
    KubernetesLoadBalancersRequest, KubernetesLoadBalancersResponse, KubernetesStartRequest,
    KubernetesStartResponse, KubernetesStatusRequest, KubernetesStatusResponse,
    KubernetesStopRequest, KubernetesStopResponse, ListGuestWebServicesRequest,
    ListGuestWebServicesResponse, MemoryPressureEvent, MmapReadFileRequest, MmapReadFileResponse,
    OpenInstanceAuthorityRequest, OpenInstanceAuthorityResponse, PortBindingsChanged,
    PortBindingsRemoved, ReadinessEvent, ReconcileHttpsAccessRequest, ReconcileHttpsAccessResponse,
    ReplaceInstanceAuthorityRequest, RuntimeEnsureRequest, RuntimeEnsureResponse,
    RuntimeStatusRequest, RuntimeStatusResponse, ShutdownRequest, ShutdownResponse,
    StorageCheckRequest, StorageCheckResponse, SystemInfo, WatchMemoryPressureRequest,
    WatchReadinessRequest, WatchStatsRequest, WatchStorageHealthRequest,
};
pub use arcbox_constants::wire::MessageType;

/// Agent version string.
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Error response message.
#[derive(Debug, Clone)]
pub struct ErrorResponse {
    pub code: i32,
    pub message: String,
}

impl ErrorResponse {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.put_i32(self.code);
        let msg_bytes = self.message.as_bytes();
        buf.put_u32(msg_bytes.len() as u32);
        buf.extend_from_slice(msg_bytes);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(data);
        if data.len() < 8 {
            anyhow::bail!("error response too short");
        }
        let code = cursor.get_i32();
        let msg_len = cursor.get_u32() as usize;
        if data.len() < 8 + msg_len {
            anyhow::bail!("error response message truncated");
        }
        let message = String::from_utf8(data[8..8 + msg_len].to_vec())?;
        Ok(Self { code, message })
    }
}

/// RPC request envelope.
#[derive(Debug)]
pub enum RpcRequest {
    Ping(PingRequest),
    GetSystemInfo,
    ListGuestWebServices(ListGuestWebServicesRequest),
    ReconcileHttpsAccess(ReconcileHttpsAccessRequest),
    OpenInstanceAuthority(OpenInstanceAuthorityRequest),
    ReplaceInstanceAuthority(ReplaceInstanceAuthorityRequest),
    EnsureRuntime(RuntimeEnsureRequest),
    RuntimeStatus(RuntimeStatusRequest),
    StartKubernetes(KubernetesStartRequest),
    StopKubernetes(KubernetesStopRequest),
    DeleteKubernetes(KubernetesDeleteRequest),
    KubernetesStatus(KubernetesStatusRequest),
    KubernetesKubeconfig(KubernetesKubeconfigRequest),
    KubernetesLoadBalancers(KubernetesLoadBalancersRequest),
    Shutdown(ShutdownRequest),
    MmapReadFile(MmapReadFileRequest),
    DiskTrim(DiskTrimRequest),
    ContainerFsPaths(ContainerFsPathsRequest),
    ImageFsPaths(ImageFsPathsRequest),
    EnsureNfsExport(EnsureNfsExportRequest),
    EnsureMachineExport(EnsureMachineExportRequest),
    WatchReadiness(WatchReadinessRequest),
    WatchMemoryPressure(WatchMemoryPressureRequest),
    WatchStats(WatchStatsRequest),
    WatchStorageHealth(WatchStorageHealthRequest),
    StorageCheck(StorageCheckRequest),
    /// Test-only: exit the agent so PID 1 (busybox init) respawns it.
    KillAgent,
}

/// RPC response envelope.
#[derive(Debug)]
pub enum RpcResponse {
    Ping(PingResponse),
    SystemInfo(SystemInfo),
    ListGuestWebServices(ListGuestWebServicesResponse),
    ReconcileHttpsAccess(ReconcileHttpsAccessResponse),
    OpenInstanceAuthority(OpenInstanceAuthorityResponse),
    InstanceAuthorityAck(InstanceAuthorityAck),
    RuntimeEnsure(RuntimeEnsureResponse),
    RuntimeStatus(RuntimeStatusResponse),
    KubernetesStart(KubernetesStartResponse),
    KubernetesStop(KubernetesStopResponse),
    KubernetesDelete(KubernetesDeleteResponse),
    KubernetesStatus(KubernetesStatusResponse),
    KubernetesKubeconfig(KubernetesKubeconfigResponse),
    KubernetesLoadBalancers(KubernetesLoadBalancersResponse),
    Shutdown(ShutdownResponse),
    DiskTrim(DiskTrimResponse),
    StorageCheck(StorageCheckResponse),
    Empty,
    PortBindingsChanged(PortBindingsChanged),
    PortBindingsRemoved(PortBindingsRemoved),
    ReadinessEvent(ReadinessEvent),
    MemoryPressureEvent(MemoryPressureEvent),
    Error(ErrorResponse),
    MmapReadFile(MmapReadFileResponse),
    ContainerFsPaths(ContainerFsPathsResponse),
    ImageFsPaths(ImageFsPathsResponse),
    EnsureNfsExport(EnsureNfsExportResponse),
    EnsureMachineExport(EnsureMachineExportResponse),
    /// Test-only: acknowledgement for [`RpcRequest::KillAgent`].
    KillAgent,
}

impl RpcResponse {
    /// Returns the message type for this response.
    pub fn message_type(&self) -> MessageType {
        match self {
            Self::Ping(_) => MessageType::PingResponse,
            Self::SystemInfo(_) => MessageType::GetSystemInfoResponse,
            Self::ListGuestWebServices(_) => MessageType::ListGuestWebServicesResponse,
            Self::ReconcileHttpsAccess(_) => MessageType::ReconcileHttpsAccessResponse,
            Self::OpenInstanceAuthority(_) => MessageType::OpenInstanceAuthorityResponse,
            Self::InstanceAuthorityAck(_) => MessageType::InstanceAuthorityAck,
            Self::RuntimeEnsure(_) => MessageType::EnsureRuntimeResponse,
            Self::RuntimeStatus(_) => MessageType::RuntimeStatusResponse,
            Self::KubernetesStart(_) => MessageType::KubernetesStartResponse,
            Self::KubernetesStop(_) => MessageType::KubernetesStopResponse,
            Self::KubernetesDelete(_) => MessageType::KubernetesDeleteResponse,
            Self::KubernetesStatus(_) => MessageType::KubernetesStatusResponse,
            Self::KubernetesKubeconfig(_) => MessageType::KubernetesKubeconfigResponse,
            Self::KubernetesLoadBalancers(_) => MessageType::KubernetesLoadBalancersResponse,
            Self::Shutdown(_) => MessageType::ShutdownResponse,
            Self::DiskTrim(_) => MessageType::DiskTrimResponse,
            Self::StorageCheck(_) => MessageType::StorageCheckResponse,
            Self::Empty => MessageType::Empty,
            Self::PortBindingsChanged(_) => MessageType::PortBindingsChanged,
            Self::PortBindingsRemoved(_) => MessageType::PortBindingsRemoved,
            Self::ReadinessEvent(_) => MessageType::ReadinessEvent,
            Self::MemoryPressureEvent(_) => MessageType::MemoryPressureEvent,
            Self::Error(_) => MessageType::Error,
            Self::MmapReadFile(_) => MessageType::MmapReadFileResponse,
            Self::ContainerFsPaths(_) => MessageType::ContainerFsPathsResponse,
            Self::ImageFsPaths(_) => MessageType::ImageFsPathsResponse,
            Self::EnsureNfsExport(_) => MessageType::EnsureNfsExportResponse,
            Self::EnsureMachineExport(_) => MessageType::EnsureMachineExportResponse,
            Self::KillAgent => MessageType::KillAgentResponse,
        }
    }

    /// Encodes the response payload.
    pub fn encode_payload(&self) -> Vec<u8> {
        match self {
            Self::Ping(msg) => msg.encode_to_vec(),
            Self::SystemInfo(msg) => msg.encode_to_vec(),
            Self::ListGuestWebServices(msg) => msg.encode_to_vec(),
            Self::ReconcileHttpsAccess(msg) => msg.encode_to_vec(),
            Self::OpenInstanceAuthority(msg) => msg.encode_to_vec(),
            Self::InstanceAuthorityAck(msg) => msg.encode_to_vec(),
            Self::RuntimeEnsure(msg) => msg.encode_to_vec(),
            Self::RuntimeStatus(msg) => msg.encode_to_vec(),
            Self::KubernetesStart(msg) => msg.encode_to_vec(),
            Self::KubernetesStop(msg) => msg.encode_to_vec(),
            Self::KubernetesDelete(msg) => msg.encode_to_vec(),
            Self::KubernetesStatus(msg) => msg.encode_to_vec(),
            Self::KubernetesKubeconfig(msg) => msg.encode_to_vec(),
            Self::KubernetesLoadBalancers(msg) => msg.encode_to_vec(),
            Self::Shutdown(msg) => msg.encode_to_vec(),
            Self::DiskTrim(msg) => msg.encode_to_vec(),
            Self::StorageCheck(msg) => msg.encode_to_vec(),
            Self::Empty => Empty::default().encode_to_vec(),
            Self::PortBindingsChanged(msg) => msg.encode_to_vec(),
            Self::PortBindingsRemoved(msg) => msg.encode_to_vec(),
            Self::ReadinessEvent(msg) => msg.encode_to_vec(),
            Self::MemoryPressureEvent(msg) => msg.encode_to_vec(),
            Self::Error(err) => err.encode(),
            Self::MmapReadFile(msg) => msg.encode_to_vec(),
            Self::ContainerFsPaths(msg) => msg.encode_to_vec(),
            Self::ImageFsPaths(msg) => msg.encode_to_vec(),
            Self::EnsureNfsExport(msg) => msg.encode_to_vec(),
            Self::EnsureMachineExport(msg) => msg.encode_to_vec(),
            Self::KillAgent => Empty::default().encode_to_vec(),
        }
    }
}

/// Reads a single RPC message from the stream.
///
/// Wire format V2:
/// ```text
/// +----------------+----------------+------------------+----------------+
/// | Length (4B BE) | Type (4B BE)   | TraceLen (2B BE) | TraceID bytes  | Payload
/// +----------------+----------------+------------------+----------------+
/// ```
/// Length = sizeof(Type) + sizeof(TraceLen) + TraceLen + PayloadLen
///        = 4 + 2 + TraceLen + PayloadLen
///
/// Returns (message_type, trace_id, payload).
pub async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<(MessageType, String, Vec<u8>)> {
    let mut frame = arcbox_transport::vsock::read_framed(reader)
        .await
        .context("failed to read RPC frame")?;
    frame.advance(4);
    anyhow::ensure!(
        frame.remaining() >= 6,
        "RPC frame is shorter than V2 header"
    );
    let msg_type_raw = frame.get_u32();
    let msg_type = MessageType::from_u32(msg_type_raw)
        .with_context(|| format!("unknown message type: {msg_type_raw}"))?;
    let trace_len = usize::from(frame.get_u16());
    anyhow::ensure!(trace_len <= frame.remaining(), "trace ID exceeds RPC frame");
    let trace_id = std::str::from_utf8(&frame.split_to(trace_len))
        .context("trace ID is not UTF-8")?
        .to_owned();
    Ok((msg_type, trace_id, frame.to_vec()))
}

/// Writes a single RPC message to the stream.
///
/// Wire format V2 (see `read_message` for layout).
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg_type: MessageType,
    trace_id: &str,
    payload: &[u8],
) -> Result<()> {
    let trace_bytes = trace_id.as_bytes();
    let trace_len = trace_bytes.len().min(u16::MAX as usize);

    // Length = type(4) + trace_len_field(2) + trace_bytes + payload
    let length = 4 + 2 + trace_len + payload.len();

    let mut buf = BytesMut::with_capacity(8 + 2 + trace_len + payload.len());
    buf.put_u32(length as u32);
    buf.put_u32(msg_type as u32);
    buf.put_u16(trace_len as u16);
    if trace_len > 0 {
        buf.extend_from_slice(&trace_bytes[..trace_len]);
    }
    buf.extend_from_slice(payload);

    // Name the frame in the error: on a torn-down connection the resulting
    // "Connection closed by peer" warn must say which response/stream write
    // was lost, or the failure cannot be attributed (CORE-82).
    writer
        .write_all(&buf)
        .await
        .with_context(|| format!("failed to write {msg_type:?} message"))?;
    writer
        .flush()
        .await
        .with_context(|| format!("failed to flush {msg_type:?} message"))?;

    Ok(())
}

/// Writes an RPC response to the stream with a trace ID.
pub async fn write_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    response: &RpcResponse,
    trace_id: &str,
) -> Result<()> {
    let payload = response.encode_payload();
    write_message(writer, response.message_type(), trace_id, &payload).await
}

/// Parses an RPC request from message type and payload.
pub fn parse_request(msg_type: MessageType, payload: &[u8]) -> Result<RpcRequest> {
    match msg_type {
        MessageType::OpenInstanceAuthorityRequest => Ok(RpcRequest::OpenInstanceAuthority(
            OpenInstanceAuthorityRequest::decode_from_slice(payload)?,
        )),
        MessageType::ReplaceInstanceAuthorityRequest => Ok(RpcRequest::ReplaceInstanceAuthority(
            ReplaceInstanceAuthorityRequest::decode_from_slice(payload)?,
        )),
        MessageType::PingRequest => {
            let req = PingRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::Ping(req))
        }
        MessageType::GetSystemInfoRequest => Ok(RpcRequest::GetSystemInfo),
        MessageType::ListGuestWebServicesRequest => {
            let request = ListGuestWebServicesRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::ListGuestWebServices(request))
        }
        MessageType::ReconcileHttpsAccessRequest => Ok(RpcRequest::ReconcileHttpsAccess(
            ReconcileHttpsAccessRequest::decode_from_slice(payload)?,
        )),
        MessageType::EnsureRuntimeRequest => {
            let req = RuntimeEnsureRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::EnsureRuntime(req))
        }
        MessageType::RuntimeStatusRequest => {
            let req = RuntimeStatusRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::RuntimeStatus(req))
        }
        MessageType::KubernetesStartRequest => {
            let req = KubernetesStartRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::StartKubernetes(req))
        }
        MessageType::KubernetesStopRequest => {
            let req = KubernetesStopRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::StopKubernetes(req))
        }
        MessageType::KubernetesDeleteRequest => {
            let req = KubernetesDeleteRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::DeleteKubernetes(req))
        }
        MessageType::KubernetesStatusRequest => {
            let req = KubernetesStatusRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::KubernetesStatus(req))
        }
        MessageType::KubernetesKubeconfigRequest => {
            let req = KubernetesKubeconfigRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::KubernetesKubeconfig(req))
        }
        MessageType::KubernetesLoadBalancersRequest => {
            let req = KubernetesLoadBalancersRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::KubernetesLoadBalancers(req))
        }
        MessageType::ShutdownRequest => {
            let req = ShutdownRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::Shutdown(req))
        }
        MessageType::MmapReadFileRequest => {
            let req = MmapReadFileRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::MmapReadFile(req))
        }
        MessageType::DiskTrimRequest => {
            let req = DiskTrimRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::DiskTrim(req))
        }
        MessageType::ContainerFsPathsRequest => {
            let req = ContainerFsPathsRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::ContainerFsPaths(req))
        }
        MessageType::ImageFsPathsRequest => {
            let req = ImageFsPathsRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::ImageFsPaths(req))
        }
        MessageType::EnsureNfsExportRequest => {
            let req = EnsureNfsExportRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::EnsureNfsExport(req))
        }
        MessageType::EnsureMachineExportRequest => {
            let req = EnsureMachineExportRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::EnsureMachineExport(req))
        }
        MessageType::WatchReadinessRequest => {
            let req = WatchReadinessRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::WatchReadiness(req))
        }
        MessageType::WatchMemoryPressureRequest => {
            let req = WatchMemoryPressureRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::WatchMemoryPressure(req))
        }
        MessageType::WatchStatsRequest => {
            let req = WatchStatsRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::WatchStats(req))
        }
        MessageType::WatchStorageHealthRequest => {
            let req = WatchStorageHealthRequest::decode_from_slice(payload)?;
            Ok(RpcRequest::WatchStorageHealth(req))
        }
        MessageType::StorageCheckRequest => Ok(RpcRequest::StorageCheck(
            StorageCheckRequest::decode_from_slice(payload)?,
        )),
        MessageType::KillAgentRequest => Ok(RpcRequest::KillAgent),
        _ => anyhow::bail!("unexpected message type: {:?}", msg_type),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn malformed_frame_does_not_consume_next_frame() {
        for body in [&[][..], &[0, 0, 0, 1, 0], &[0, 0, 0, 1, 0, 2, b'a']] {
            let (mut reader, mut writer) = tokio::io::duplex(128);
            writer
                .write_all(&(body.len() as u32).to_be_bytes())
                .await
                .unwrap();
            writer.write_all(body).await.unwrap();
            write_message(&mut writer, MessageType::PingRequest, "next", b"payload")
                .await
                .unwrap();
            assert!(read_message(&mut reader).await.is_err());
            assert_eq!(
                read_message(&mut reader).await.unwrap(),
                (MessageType::PingRequest, "next".into(), b"payload".to_vec())
            );
        }
    }

    #[tokio::test]
    async fn invalid_trace_encoding_is_rejected() {
        let (mut reader, mut writer) = tokio::io::duplex(128);
        writer
            .write_all(&[0, 0, 0, 7, 0, 0, 0, 1, 0, 1, 0xff])
            .await
            .unwrap();
        assert!(
            read_message(&mut reader)
                .await
                .unwrap_err()
                .to_string()
                .contains("UTF-8")
        );
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_reading_body() {
        let (mut reader, mut writer) = tokio::io::duplex(128);
        writer.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        let error =
            tokio::time::timeout(std::time::Duration::from_secs(1), read_message(&mut reader))
                .await
                .expect("oversized frame must not wait for a body")
                .unwrap_err();
        assert!(format!("{error:#}").contains("frame too large"));
    }

    #[tokio::test]
    async fn truncated_frame_is_rejected() {
        let (mut reader, mut writer) = tokio::io::duplex(128);
        writer.write_all(&[0, 0, 0, 6, 0, 0, 0, 1]).await.unwrap();
        drop(writer);
        assert!(read_message(&mut reader).await.is_err());
    }

    #[test]
    fn test_message_type_from_u32_requests() {
        assert_eq!(
            MessageType::from_u32(0x0001),
            Some(MessageType::PingRequest)
        );
        assert_eq!(
            MessageType::from_u32(0x0002),
            Some(MessageType::GetSystemInfoRequest)
        );
        assert_eq!(
            MessageType::from_u32(0x0003),
            Some(MessageType::EnsureRuntimeRequest)
        );
        assert_eq!(
            MessageType::from_u32(0x0004),
            Some(MessageType::RuntimeStatusRequest)
        );
    }

    #[test]
    fn test_message_type_from_u32_responses() {
        assert_eq!(
            MessageType::from_u32(0x1001),
            Some(MessageType::PingResponse)
        );
        assert_eq!(
            MessageType::from_u32(0x1002),
            Some(MessageType::GetSystemInfoResponse)
        );
        assert_eq!(
            MessageType::from_u32(0x1003),
            Some(MessageType::EnsureRuntimeResponse)
        );
        assert_eq!(
            MessageType::from_u32(0x1004),
            Some(MessageType::RuntimeStatusResponse)
        );
    }

    #[test]
    fn test_message_type_from_u32_special() {
        assert_eq!(MessageType::from_u32(0x0000), Some(MessageType::Empty));
        assert_eq!(MessageType::from_u32(0xFFFF), Some(MessageType::Error));
    }

    #[test]
    fn test_message_type_from_u32_invalid() {
        assert_eq!(MessageType::from_u32(0x9999), None);
        assert_eq!(MessageType::from_u32(0x0FFF), None);
        assert_eq!(MessageType::from_u32(0x1F00), None);
    }

    #[test]
    fn test_error_response_roundtrip() {
        let err = ErrorResponse::new(500, "internal error");
        let encoded = err.encode();
        let decoded = ErrorResponse::decode(&encoded).unwrap();

        assert_eq!(decoded.code, 500);
        assert_eq!(decoded.message, "internal error");
    }

    #[test]
    fn test_parse_request_ping() {
        let req = PingRequest {
            message: "ping".to_string(),
            ..Default::default()
        };
        let payload = req.encode_to_vec();

        let parsed = parse_request(MessageType::PingRequest, &payload).unwrap();
        match parsed {
            RpcRequest::Ping(p) => assert_eq!(p.message, "ping"),
            _ => panic!("Expected Ping request"),
        }
    }

    #[test]
    fn test_parse_request_shutdown() {
        let req = ShutdownRequest {
            timeout_seconds: 10,
            ..Default::default()
        };
        let payload = req.encode_to_vec();

        let parsed = parse_request(MessageType::ShutdownRequest, &payload).unwrap();
        match parsed {
            RpcRequest::Shutdown(s) => assert_eq!(s.timeout_seconds, 10),
            _ => panic!("Expected Shutdown request"),
        }
    }
}
