//! Downloading the QEMU build on demand.

use std::fs::{File, TryLockError};
use std::path::PathBuf;
use std::time::Duration;

use arcbox_asset::AssetError;
use tokio_util::sync::CancellationToken;

use super::{QemuRuntime, install};
use crate::error::{ImageError, Result};

/// What [`QemuRuntime::ensure`] is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QemuProgress {
    /// Receiving the archive. `downloaded` includes bytes kept from earlier
    /// attempts; `total` is the archive's size once the server reports it.
    Transferring { downloaded: u64, total: Option<u64> },
    /// Checking the archive's SHA-256 against the pin.
    Verifying,
    /// Extracting, checking signatures and moving the build into place.
    Installing,
    /// An attempt failed with `error`; the next starts after `delay` and
    /// resumes from the `downloaded` bytes now in the partial archive, which
    /// may come from an earlier daemon run.
    WaitingToRetry {
        error: String,
        delay: Duration,
        downloaded: u64,
    },
}

/// Backoff between attempts: doubling from `first`, capped at `max`.
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    pub first: Duration,
    pub max: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            first: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

impl QemuRuntime {
    /// Returns the installed build's executable, downloading and installing
    /// the build first when it is missing.
    ///
    /// A network failure or a 5xx, 408 or 429 answer is retried after the
    /// [`Retry`] delay until `cancel` fires, resuming from the bytes already
    /// in `<root>/.downloads`, which also survive a daemon restart. Every
    /// attempt first rechecks the install, so an import ends the wait. A
    /// resumed archive that fails its checksum is discarded and downloaded
    /// again; a whole one that fails, any other status, a failed install
    /// check or a local I/O error is returned.
    pub async fn ensure(
        &self,
        retry: Retry,
        cancel: &CancellationToken,
        progress: impl Fn(QemuProgress) + Send + Sync,
    ) -> Result<PathBuf> {
        let mut delay = retry.first;
        loop {
            match self.attempt(cancel, &progress).await {
                Err(ImageError::Asset(error)) if error.is_transient() => {
                    // Read from disk: the attempt may have failed before any
                    // progress, or emptied or deleted the partial archive.
                    let downloaded = tokio::fs::metadata(self.partial())
                        .await
                        .map_or(0, |metadata| metadata.len());
                    progress(QemuProgress::WaitingToRetry {
                        error: error.to_string(),
                        delay,
                        downloaded,
                    });
                    tokio::select! {
                        () = cancel.cancelled() => return Err(ImageError::Cancelled),
                        () = tokio::time::sleep(delay) => {}
                    }
                    delay = (delay * 2).min(retry.max);
                }
                outcome => return outcome,
            }
        }
    }

    async fn attempt(
        &self,
        cancel: &CancellationToken,
        progress: &(impl Fn(QemuProgress) + Send + Sync),
    ) -> Result<PathBuf> {
        let lock = self.lock(cancel).await?;
        if let Some(executable) = self.installed_executable() {
            return Ok(executable);
        }
        let partial = self.partial();
        tokio::fs::create_dir_all(self.root.join(install::DOWNLOADS)).await?;
        let resumed = tokio::fs::metadata(&partial)
            .await
            .is_ok_and(|metadata| metadata.len() > 0);
        let url = self.artifact.url();
        tokio::select! {
            () = cancel.cancelled() => return Err(ImageError::Cancelled),
            transferred = arcbox_asset::download::download_resumable(
                &url,
                &partial,
                |downloaded, total| progress(QemuProgress::Transferring { downloaded, total }),
            ) => transferred?,
        }

        progress(QemuProgress::Verifying);
        let runtime = self.clone();
        let archive = partial.clone();
        let checked =
            tokio::task::spawn_blocking(move || install::check_sha256(&archive, &runtime.artifact))
                .await
                .map_err(|error| ImageError::config(format!("QEMU verify task failed: {error}")))?;
        if let Err(error) = checked {
            tokio::fs::remove_file(&partial).await?;
            if resumed {
                // The bytes kept from an earlier attempt are suspect, not the
                // build: start over from zero.
                return Err(
                    AssetError::Download(format!("resumed download was corrupt: {error}")).into(),
                );
            }
            return Err(error);
        }

        progress(QemuProgress::Installing);
        let runtime = self.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = lock;
            install::install(
                &runtime.root,
                &runtime.artifact,
                &partial,
                runtime.verifier.as_ref(),
            )
        })
        .await
        .map_err(|error| ImageError::config(format!("QEMU install task failed: {error}")))?
    }

    /// The archive being downloaded, kept across attempts and daemon runs.
    fn partial(&self) -> PathBuf {
        self.root
            .join(install::DOWNLOADS)
            .join(format!("{}.part", self.artifact.sha256))
    }

    /// Takes the root's lock without blocking a runtime thread: an import
    /// holds it for seconds.
    async fn lock(&self, cancel: &CancellationToken) -> Result<File> {
        let file = install::open_lock(&self.root)?;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) => {
                    tokio::select! {
                        () = cancel.cancelled() => return Err(ImageError::Cancelled),
                        () = tokio::time::sleep(Duration::from_millis(100)) => {}
                    }
                }
                Err(TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::qemu::tests::{SHA, entries, names, runtime_at, write_archive};

    const FAST: Retry = Retry {
        first: Duration::from_millis(1),
        max: Duration::from_millis(1),
    };

    #[derive(Clone, Copy)]
    enum Cdn {
        /// Honors `Range`, but stops the first response after this many bytes.
        CutFirstAt(usize),
        /// Answers every request with this status.
        Status(u16),
        /// Serves the archive.
        Serve,
    }

    /// Serves `body` at every path and records each request's `Range`.
    async fn serve(body: Vec<u8>, mode: Cdn) -> (String, Arc<Mutex<Vec<Option<String>>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cdn = format!("http://{}/qemu", listener.local_addr().unwrap());
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&ranges);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0; 2048];
                let n = socket.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..n]).to_ascii_lowercase();
                let start: Option<usize> = request.lines().find_map(|line| {
                    line.strip_prefix("range: bytes=")?
                        .strip_suffix('-')?
                        .parse()
                        .ok()
                });
                let first = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(start.map(|start| format!("bytes={start}-")));
                    seen.len() == 1
                };
                let len = body.len();
                let (head, payload) = match (mode, start) {
                    (Cdn::Status(status), _) => {
                        (format!("{status} Failed\r\nContent-Length: 0"), &body[..0])
                    }
                    (_, Some(start)) => (
                        format!(
                            "206 Partial Content\r\nContent-Range: bytes {start}-{}/{len}\r\nContent-Length: {}",
                            len - 1,
                            len - start
                        ),
                        &body[start..],
                    ),
                    (_, None) => (format!("200 OK\r\nContent-Length: {len}"), &body[..]),
                };
                let payload = match mode {
                    Cdn::CutFirstAt(cut) if first => &payload[..cut],
                    _ => payload,
                };
                let head = format!("HTTP/1.1 {head}\r\nConnection: close\r\n\r\n");
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(payload).await;
            }
        });
        (cdn, ranges)
    }

    #[tokio::test]
    async fn a_cut_transfer_resumes_and_installs() {
        let dir = tempfile::tempdir().unwrap();
        let (archive, root) = (dir.path().join("qemu.tar.gz"), dir.path().join("qemu"));
        let sha256 = write_archive(&archive, &entries("0.1.0"), None);
        let body = std::fs::read(&archive).unwrap();
        let (len, cut) = (body.len() as u64, body.len() / 2);
        let (cdn, ranges) = serve(body, Cdn::CutFirstAt(cut)).await;
        let (runtime, _) = runtime_at(&root, &sha256, &cdn);

        let events = Mutex::new(Vec::new());
        let executable = runtime
            .ensure(FAST, &CancellationToken::new(), |event| {
                events.lock().unwrap().push(event);
            })
            .await
            .unwrap();
        assert_eq!(runtime.installed_executable(), Some(executable));
        assert_eq!(
            *ranges.lock().unwrap(),
            [None, Some(format!("bytes={cut}-"))]
        );
        // The download is gone once the build is installed.
        assert_eq!(names(&root), [".lock", "0.1.0"]);

        let events = events.into_inner().unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, QemuProgress::WaitingToRetry { .. })),
            "{events:?}"
        );
        assert_eq!(
            events[events.len() - 3..],
            [
                QemuProgress::Transferring {
                    downloaded: len,
                    total: Some(len)
                },
                QemuProgress::Verifying,
                QemuProgress::Installing,
            ]
        );
    }

    #[tokio::test]
    async fn a_wrong_archive_or_a_missing_build_is_not_retried() {
        for (case, mode, wrong_pin) in [
            ("wrong archive", Cdn::Serve, true),
            ("missing build", Cdn::Status(404), false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (archive, root) = (dir.path().join("qemu.tar.gz"), dir.path().join("qemu"));
            let sha256 = write_archive(&archive, &entries("0.1.0"), None);
            let (cdn, ranges) = serve(std::fs::read(&archive).unwrap(), mode).await;
            let (runtime, _) = runtime_at(&root, if wrong_pin { SHA } else { &sha256 }, &cdn);

            let error = runtime
                .ensure(FAST, &CancellationToken::new(), |_| {})
                .await
                .unwrap_err();
            assert!(
                matches!(&error, ImageError::Asset(e) if !e.is_transient()),
                "{case}: {error}"
            );
            assert_eq!(ranges.lock().unwrap().len(), 1, "{case} was retried");
            assert_eq!(
                names(&root),
                [".downloads", ".lock"],
                "{case} kept a download"
            );
        }
    }

    #[tokio::test]
    async fn an_import_or_shutdown_ends_the_wait_for_an_unreachable_cdn() {
        let dir = tempfile::tempdir().unwrap();
        let (archive, root) = (dir.path().join("qemu.tar.gz"), dir.path().join("qemu"));
        let sha256 = write_archive(&archive, &entries("0.1.0"), None);
        let (cdn, _) = serve(Vec::new(), Cdn::Status(503)).await;
        let (runtime, _) = runtime_at(&root, &sha256, &cdn);
        // Kept by an earlier daemon run; an outage must not report it as lost.
        std::fs::create_dir_all(root.join(install::DOWNLOADS)).unwrap();
        std::fs::write(
            root.join(install::DOWNLOADS).join(format!("{sha256}.part")),
            [0; 1234],
        )
        .unwrap();
        let retry = Retry {
            first: Duration::from_millis(20),
            max: Duration::from_millis(20),
        };

        let start = |cancel: CancellationToken| {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let runtime = runtime.clone();
            let task = tokio::spawn(async move {
                let progress = move |event| {
                    let _ = tx.send(event);
                };
                runtime.ensure(retry, &cancel, progress).await
            });
            (task, rx)
        };
        async fn retrying(rx: &mut tokio::sync::mpsc::UnboundedReceiver<QemuProgress>) -> u64 {
            loop {
                match rx.recv().await {
                    Some(QemuProgress::WaitingToRetry { downloaded, .. }) => return downloaded,
                    Some(_) => {}
                    None => panic!("ensure ended without waiting to retry"),
                }
            }
        }

        let cancel = CancellationToken::new();
        let (task, mut events) = start(cancel.clone());
        assert_eq!(retrying(&mut events).await, 1234);
        cancel.cancel();
        assert!(matches!(task.await.unwrap(), Err(ImageError::Cancelled)));

        let (task, mut events) = start(CancellationToken::new());
        retrying(&mut events).await;
        let imported = runtime.import(&archive).await.unwrap();
        assert_eq!(task.await.unwrap().unwrap(), imported);
    }
}
