//! Read-only observations of the System VM's two persistent volumes.
//!
//! Mount flags are sampled because a filesystem's emergency read-only
//! transition does not necessarily notify mountinfo watchers. The watch runs
//! only while a host subscribes; observing it never writes to either volume.

use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use arcbox_connect::v1::storage_volume_health::{Role, State};
use arcbox_connect::v1::{StorageHealth, StorageVolumeHealth};
use arcbox_constants::devices::DOCKER_METADATA_BLOCK_DEVICE;
use buffa::Message as _;
use nix::sys::statvfs::{FsFlags, statvfs};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

use super::btrfs::BTRFS_TEMP_MOUNT;
use super::cmdline::{declared_docker_metadata_device, docker_data_device};
use super::metadata_volume::METADATA_MOUNT;
use crate::agent::ensure_runtime::{RuntimeState, runtime_guard};
use crate::rpc::{MessageType, write_message};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

struct Volume<'a> {
    role: Role,
    device: Option<String>,
    mount: &'a str,
    filesystem: &'a str,
}

fn volumes() -> &'static [Volume<'static>; 2] {
    static VOLUMES: OnceLock<[Volume<'static>; 2]> = OnceLock::new();
    VOLUMES.get_or_init(|| {
        let metadata = declared_docker_metadata_device().or_else(|| {
            Path::new(DOCKER_METADATA_BLOCK_DEVICE)
                .exists()
                .then(|| DOCKER_METADATA_BLOCK_DEVICE.to_owned())
        });
        [
            Volume {
                role: Role::Data,
                device: Some(docker_data_device()),
                mount: BTRFS_TEMP_MOUNT,
                filesystem: "btrfs",
            },
            Volume {
                role: Role::Metadata,
                device: metadata,
                mount: METADATA_MOUNT,
                filesystem: "ext4",
            },
        ]
    })
}

pub(super) async fn snapshot() -> StorageHealth {
    let runtime = runtime_guard();
    let initialized = matches!(
        &*runtime.state.lock().await,
        RuntimeState::Ready { .. } | RuntimeState::Failed { .. }
    );
    let mounts = std::fs::read_to_string("/proc/self/mounts").map_err(|e| e.to_string());
    StorageHealth {
        volumes: volumes()
            .iter()
            .map(|volume| {
                observe_volume(volume, mounts.as_deref(), initialized, mount_is_read_only)
            })
            .collect(),
        observed_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            }),
        ..Default::default()
    }
}

fn observe_volume(
    volume: &Volume<'_>,
    mounts: std::result::Result<&str, &String>,
    initialized: bool,
    read_only: impl FnOnce(&str) -> std::result::Result<bool, String>,
) -> StorageVolumeHealth {
    let mut observation = StorageVolumeHealth {
        role: volume.role.into(),
        device: volume.device.clone().unwrap_or_default(),
        mount_point: volume.mount.into(),
        filesystem: volume.filesystem.into(),
        ..Default::default()
    };
    let Some(device) = &volume.device else {
        observation.state = State::NotConfigured.into();
        observation.detail = "No metadata volume is configured for this guest.".into();
        return observation;
    };
    let mounts = match mounts {
        Ok(mounts) => mounts,
        Err(error) => {
            observation.detail = format!("Cannot read the guest mount table: {error}");
            return observation;
        }
    };
    let mounted = mounts.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let source = fields.next()?;
        let target = fields.next()?;
        let filesystem = fields.next()?;
        (target == volume.mount).then_some((source, filesystem))
    });
    match mounted {
        None if !initialized => {
            observation.detail = "Storage initialization has not completed.".into();
        }
        None => {
            observation.state = State::Unavailable.into();
            observation.detail = "The configured volume is not mounted.".into();
        }
        Some((source, filesystem)) if source != device || filesystem != volume.filesystem => {
            observation.state = State::Unavailable.into();
            observation.detail = format!(
                "Expected {device} ({}) but found {source} ({filesystem}).",
                volume.filesystem
            );
        }
        Some(_) => match read_only(volume.mount) {
            Ok(true) => {
                observation.state = State::ReadOnly.into();
                observation.detail =
                    "The filesystem is mounted read-only; writes are unavailable.".into();
            }
            Ok(false) => observation.state = State::MountedReadWrite.into(),
            Err(error) => {
                observation.detail = format!("Cannot inspect filesystem mount flags: {error}");
            }
        },
    }
    observation
}

fn mount_is_read_only(mount: &str) -> std::result::Result<bool, String> {
    statvfs(mount)
        .map(|stat| stat.flags().contains(FsFlags::ST_RDONLY))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
pub(super) fn observe_test_mount(
    device: &str,
    mount: &str,
    filesystem: &str,
) -> StorageVolumeHealth {
    let volume = Volume {
        role: Role::Data,
        device: Some(device.into()),
        mount,
        filesystem,
    };
    let mounts = std::fs::read_to_string("/proc/self/mounts").map_err(|error| error.to_string());
    observe_volume(&volume, mounts.as_deref(), true, mount_is_read_only)
}

pub(super) async fn watch<S>(stream: &mut S, trace_id: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut previous = None;
    let mut last_sent = tokio::time::Instant::now();
    let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut peer_byte = [0_u8; 1];
    loop {
        tokio::select! {
            result = stream.read(&mut peer_byte) => {
                anyhow::ensure!(result? == 0, "unexpected input on storage health watch");
                return Ok(());
            }
            _ = ticker.tick() => {}
        }
        let current = snapshot().await;
        if previous.as_ref() != Some(&current.volumes) || last_sent.elapsed() >= HEARTBEAT_INTERVAL
        {
            write_message(
                stream,
                MessageType::StorageHealth,
                trace_id,
                &current.encode_to_vec(),
            )
            .await?;
            previous = Some(current.volumes);
            last_sent = tokio::time::Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> Volume<'static> {
        Volume {
            role: Role::Data,
            device: Some("/dev/vdb".into()),
            mount: BTRFS_TEMP_MOUNT,
            filesystem: "btrfs",
        }
    }

    const MOUNTS: &str = "/dev/vdb /run/arcbox/data btrfs rw,relatime 0 0\n";

    #[test]
    fn live_readonly_flag_overrides_stale_mount_table_options() {
        let status = observe_volume(&data(), Ok(MOUNTS), true, |_| Ok(true));
        assert_eq!(status.state, State::ReadOnly);
        assert_eq!(
            observe_volume(&data(), Ok(MOUNTS), true, |_| Ok(false)).state,
            State::MountedReadWrite
        );
    }

    #[test]
    fn missing_and_wrong_mounts_are_not_the_writable_parent_tmpfs() {
        for mounts in [
            "tmpfs /run tmpfs rw 0 0\n",
            "tmpfs /run/arcbox/data tmpfs rw 0 0\n",
        ] {
            assert_eq!(
                observe_volume(&data(), Ok(mounts), true, |_| panic!(
                    "must not inspect a different filesystem"
                ))
                .state,
                State::Unavailable
            );
        }
    }

    #[test]
    fn metadata_failure_is_independent_from_data_mount_mode() {
        let metadata = Volume {
            role: Role::Metadata,
            device: Some("/dev/vdc".into()),
            mount: METADATA_MOUNT,
            filesystem: "ext4",
        };
        let mounts = format!("{MOUNTS}/dev/vdc /run/arcbox/metadata ext4 rw 0 0\n");
        let data = observe_volume(&data(), Ok(&mounts), true, |_| Ok(false));
        let metadata = observe_volume(&metadata, Ok(&mounts), true, |_| Ok(true));
        assert_eq!(data.state, State::MountedReadWrite);
        assert_eq!(metadata.role, Role::Metadata);
        assert_eq!(metadata.state, State::ReadOnly);
    }

    #[test]
    fn inspection_errors_are_unknown_and_optional_absence_is_explicit() {
        let error = "permission denied".to_owned();
        assert_eq!(
            observe_volume(&data(), Err(&error), true, |_| panic!("no mount table")).state,
            State::StateUnspecified
        );
        assert_eq!(
            observe_volume(&data(), Ok(MOUNTS), true, |_| Err(error.clone())).state,
            State::StateUnspecified
        );
        let metadata = Volume {
            role: Role::Metadata,
            device: None,
            mount: METADATA_MOUNT,
            filesystem: "ext4",
        };
        assert_eq!(
            observe_volume(&metadata, Err(&error), true, |_| panic!("not configured")).state,
            State::NotConfigured
        );
    }

    #[test]
    fn startup_missing_mount_is_unknown_but_observed_readonly_is_definitive() {
        assert_eq!(
            observe_volume(&data(), Ok(""), false, |_| panic!("not mounted")).state,
            State::StateUnspecified
        );
        assert_eq!(
            observe_volume(&data(), Ok(""), true, |_| panic!("not mounted")).state,
            State::Unavailable
        );
        assert_eq!(
            observe_volume(&data(), Ok(MOUNTS), false, |_| Ok(true)).state,
            State::ReadOnly
        );
    }

    #[tokio::test]
    async fn watch_sends_snapshot_and_stops_when_peer_closes() {
        let (server, mut client) = tokio::io::duplex(4096);
        let task = tokio::spawn(super::super::rpc::handle_connection(
            server,
            crate::agent::Guest::SystemVm,
        ));
        crate::rpc::write_message(
            &mut client,
            MessageType::WatchStorageHealthRequest,
            "storage-test",
            &[],
        )
        .await
        .unwrap();
        let (kind, trace, payload) = crate::rpc::read_message(&mut client).await.unwrap();
        assert_eq!(kind, MessageType::StorageHealth);
        assert_eq!(trace, "storage-test");
        assert_eq!(
            StorageHealth::decode_from_slice(&payload)
                .unwrap()
                .volumes
                .len(),
            2
        );
        drop(client);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
