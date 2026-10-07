//! Recheck storage admission for input received after a stream opens.

use arcbox_connect::{sandbox_v1, v1};
use arcbox_core::{ExecSessionInput, Runtime, WriteFileChunk};
use connectrpc::{ConnectError, InboundStream, ServiceStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_stream::StreamExt as _;

use crate::ApiError;

pub(super) fn machine_output(
    input: impl Future<Output = Result<(), ConnectError>> + Send + 'static,
    mut output: ServiceStream<v1::MachineExecOutput>,
) -> ServiceStream<v1::MachineExecOutput> {
    // Start before the response is polled; slow output must not block stdin.
    // The response owns the JoinSet, which aborts input when the response drops.
    let mut tasks = JoinSet::new();
    tasks.spawn(input);
    Box::pin(async_stream::stream! {
        loop {
            let item = tokio::select! {
                Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                    match result {
                        Ok(Ok(())) => continue,
                        Ok(Err(error)) => Err(error),
                        Err(error) => Err(ConnectError::internal(format!("machine input task failed: {error}"))),
                    }
                }
                item = output.next() => {
                    let Some(item) = item else { break };
                    item
                }
            };
            let done = match &item {
                Ok(frame) => frame.done,
                Err(_) => true,
            };
            if done {
                tasks.abort_all();
            }
            yield item;
            if done { break; }
        }
    })
}

pub(super) async fn machine(
    runtime: &Runtime,
    machine: &str,
    mut requests: InboundStream<v1::MachineExecInput>,
    input: mpsc::Sender<ExecSessionInput>,
) -> Result<(), ConnectError> {
    while let Some(item) = requests.next().await {
        let message = match item?.to_owned_message().payload {
            Some(v1::machine_exec_input::Payload::Stdin(data)) => ExecSessionInput::Stdin(data),
            Some(v1::machine_exec_input::Payload::Resize(size)) => ExecSessionInput::Resize {
                width: u16::try_from(size.width).unwrap_or(u16::MAX),
                height: u16::try_from(size.height).unwrap_or(u16::MAX),
            },
            _ => continue,
        };
        let Ok(permit) = input.reserve().await else {
            return Ok(());
        };
        if !matches!(&message, ExecSessionInput::Stdin(data) if data.is_empty()) {
            runtime
                .ensure_storage_writes_available(machine)
                .map_err(ApiError::from)?;
        }
        permit.send(message);
    }
    // Dropping the sender asks the existing agent input pump to send EOF.
    Ok(())
}

pub(super) async fn file(
    runtime: &Runtime,
    machine: &str,
    mut requests: InboundStream<sandbox_v1::WriteFileRequest>,
    input: mpsc::Sender<WriteFileChunk>,
) -> Result<(), ConnectError> {
    while let Some(item) = requests.next().await {
        let Some(sandbox_v1::write_file_request::Payload::Chunk(chunk)) =
            item?.to_owned_message().payload
        else {
            continue;
        };
        let permit = input
            .reserve()
            .await
            .map_err(|_| ConnectError::unavailable("write_file: guest input closed"))?;
        runtime
            .ensure_storage_writes_available(machine)
            .map_err(ApiError::from)?;
        if !chunk.data.is_empty() {
            permit.send(WriteFileChunk::Data(chunk.data));
        }
        if chunk.done {
            return Ok(());
        }
    }
    Err(ConnectError::invalid_argument(
        "write_file: client stream ended before completion",
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::connect::{ConnectRuntimeExt as _, tests::storage_runtime};
    use arcbox_engine::machine::DEFAULT_MACHINE_NAME;
    use connectrpc::{ErrorCode, StreamMessage};
    use tokio_stream::wrappers::ReceiverStream;

    #[tokio::test]
    async fn machine_output_preserves_independent_input_and_cancels_it_on_drop() {
        let timeout = std::time::Duration::from_secs(2);
        for protect in [false, true] {
            let (directory, shared) = storage_runtime();
            let runtime = Arc::clone(shared.ready().unwrap());
            let (requests, receiver) = mpsc::channel(1);
            let (input, mut received) = mpsc::channel(1);
            let (guest_output, output_receiver) = mpsc::channel(1);
            let forwarded = Arc::clone(&runtime);
            let mut output = machine_output(
                async move {
                    machine(
                        &forwarded,
                        DEFAULT_MACHINE_NAME,
                        Box::pin(ReceiverStream::new(receiver)),
                        input,
                    )
                    .await
                },
                Box::pin(ReceiverStream::new(output_receiver)),
            );
            guest_output
                .send(Ok(v1::MachineExecOutput::default()))
                .await
                .unwrap();
            tokio::time::timeout(timeout, output.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let stdin = v1::MachineExecInput {
                payload: Some(v1::machine_exec_input::Payload::Stdin(vec![1])),
                ..Default::default()
            };
            requests
                .send(Ok(StreamMessage::from_message(&stdin)))
                .await
                .unwrap();
            // Do not poll output while waiting for this new input to reach the guest.
            let message = tokio::time::timeout(timeout, received.recv())
                .await
                .expect("output backpressure must not stop input");
            assert!(matches!(message, Some(ExecSessionInput::Stdin(data)) if data == [1]));
            if protect {
                let _reservation = runtime.machine_manager().reserve_storage().unwrap();
                requests
                    .send(Ok(StreamMessage::from_message(&stdin)))
                    .await
                    .unwrap();
                let error = tokio::time::timeout(timeout, output.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_err();
                assert_eq!(error.code, ErrorCode::FailedPrecondition);
                assert!(output.next().await.is_none());
            } else {
                drop(output);
            }
            tokio::time::timeout(timeout, requests.closed())
                .await
                .expect("closing output must cancel input");
            assert!(received.recv().await.is_none());
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test]
    async fn established_machine_input_rejects_new_stdin_and_resize_after_protection() {
        for payload in [
            v1::machine_exec_input::Payload::Stdin(vec![2]),
            v1::machine_exec_input::Payload::from(v1::TerminalSize::default()),
        ] {
            let (directory, shared) = storage_runtime();
            let runtime = Arc::clone(shared.ready().unwrap());
            let (requests, receiver) = mpsc::channel(1);
            let (input, mut received) = mpsc::channel(1);
            let forwarded = Arc::clone(&runtime);
            let task = tokio::spawn(async move {
                machine(
                    &forwarded,
                    DEFAULT_MACHINE_NAME,
                    Box::pin(ReceiverStream::new(receiver)),
                    input,
                )
                .await
            });
            let first = v1::MachineExecInput {
                payload: Some(v1::machine_exec_input::Payload::Stdin(vec![1])),
                ..Default::default()
            };
            requests
                .send(Ok(StreamMessage::from_message(&first)))
                .await
                .unwrap();
            assert!(
                matches!(received.recv().await, Some(ExecSessionInput::Stdin(data)) if data == [1])
            );
            let _reservation = runtime.machine_manager().reserve_storage().unwrap();
            let blocked = v1::MachineExecInput {
                payload: Some(payload),
                ..Default::default()
            };
            requests
                .send(Ok(StreamMessage::from_message(&blocked)))
                .await
                .unwrap();
            assert_eq!(
                task.await.unwrap().unwrap_err().code,
                ErrorCode::FailedPrecondition
            );
            assert!(received.recv().await.is_none());
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test]
    async fn established_file_input_rejects_new_data_and_empty_commit_after_protection() {
        for done in [false, true] {
            let (directory, shared) = storage_runtime();
            let runtime = Arc::clone(shared.ready().unwrap());
            let (requests, receiver) = mpsc::channel(1);
            let (input, mut received) = mpsc::channel(1);
            let forwarded = Arc::clone(&runtime);
            let task = tokio::spawn(async move {
                file(
                    &forwarded,
                    DEFAULT_MACHINE_NAME,
                    Box::pin(ReceiverStream::new(receiver)),
                    input,
                )
                .await
            });
            let chunk = |data, done| {
                StreamMessage::from_message(&sandbox_v1::WriteFileRequest {
                    payload: Some(sandbox_v1::write_file_request::Payload::from(
                        sandbox_v1::FileChunk {
                            data,
                            done,
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                })
            };
            requests.send(Ok(chunk(vec![1], false))).await.unwrap();
            assert!(
                matches!(received.recv().await, Some(WriteFileChunk::Data(data)) if data == [1])
            );
            let _reservation = runtime.machine_manager().reserve_storage().unwrap();
            requests
                .send(Ok(chunk(if done { vec![] } else { vec![2] }, done)))
                .await
                .unwrap();
            assert_eq!(
                task.await.unwrap().unwrap_err().code,
                ErrorCode::FailedPrecondition
            );
            assert!(received.recv().await.is_none());
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
}
