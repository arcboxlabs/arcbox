//! Streaming download with SHA-256 verification and atomic writes.

use crate::error::{AssetError, Result};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

/// HTTP request timeout in seconds.
const HTTP_TIMEOUT_SECS: u64 = 300;

/// User-Agent header sent with all requests.
const USER_AGENT: &str = "arcbox-asset/0.1";

/// Compute the SHA-256 hex digest of a file.
pub async fn sha256_file(path: &Path) -> Result<String> {
    let bytes = tokio::fs::read(path).await?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

/// Returns a sibling temp path unique per process and per call.
///
/// A fixed shared temp name (`dest.tmp`) lets two concurrent downloads of
/// the same destination truncate and interleave into one file — each
/// writer's *stream* hash still verifies, so a corrupt file gets renamed
/// into place as "verified". Uniqueness makes every writer's temp private;
/// the final `rename` stays atomic, so concurrent installs degrade to
/// last-writer-wins with a genuinely verified file.
fn unique_temp_path(dest: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dest.with_file_name(format!("{file_name}.{}.{seq}.tmp", std::process::id()))
}

/// Download `url` to `dest`, verifying that the content matches
/// `expected_sha256`. The hash is computed incrementally while streaming.
///
/// The file is first written to a `.tmp` sibling and atomically renamed on
/// success. On checksum failure the temp file is removed.
///
/// `on_progress` is called after every chunk with (bytes_downloaded, total_bytes).
pub async fn download_and_verify(
    url: &str,
    dest: &Path,
    expected_sha256: &str,
    name: &str,
    on_progress: impl Fn(u64, Option<u64>) + Send + Sync,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| AssetError::Download(format!("failed to create HTTP client: {e}")))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| AssetError::Download(format!("request failed for {url}: {e}")))?;

    if !response.status().is_success() {
        return Err(AssetError::Download(format!(
            "HTTP {} for {url}",
            response.status()
        )));
    }

    let total = response.content_length();
    let mut downloaded: u64 = 0;

    // Write to a private temp file, then rename atomically.
    let temp_path = unique_temp_path(dest);
    let outcome: Result<()> = async {
        let mut file = tokio::fs::File::create(&temp_path).await?;
        let mut stream = response.bytes_stream();
        let mut hasher = Sha256::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| AssetError::Download(format!("stream error: {e}")))?;
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            downloaded += chunk.len() as u64;
            on_progress(downloaded, total);
        }

        file.flush().await?;
        drop(file);

        let actual_sha = format!("{:x}", hasher.finalize());
        if actual_sha != expected_sha256 {
            return Err(AssetError::ChecksumMismatch {
                name: name.to_string(),
                expected: expected_sha256.to_string(),
                actual: actual_sha,
            });
        }

        tokio::fs::rename(&temp_path, dest).await?;
        Ok(())
    }
    .await;

    // Unique temp names would otherwise accumulate on failure.
    if outcome.is_err() {
        let _ = tokio::fs::remove_file(&temp_path).await;
    }
    outcome
}

/// Download `url` into `partial`, continuing after the bytes `partial`
/// already holds.
///
/// A non-empty `partial` is resumed with a `Range` request. A server that
/// ignores the range (200) restarts the file from zero; one that reports the
/// range unsatisfiable (416) for a file of exactly `partial`'s length leaves it
/// as is. Bytes received before a failure stay in `partial`, so the next call
/// continues from there. Nothing is verified: the caller checks the finished
/// file and deletes it when it does not match, and must serialize writers of
/// one `partial` path.
///
/// `on_progress` is called after every chunk with (bytes in `partial`,
/// entity length when known).
pub async fn download_resumable(
    url: &str,
    partial: &Path,
    on_progress: impl Fn(u64, Option<u64>) + Send + Sync,
) -> Result<()> {
    use reqwest::StatusCode;
    use reqwest::header::{CONTENT_RANGE, RANGE};

    // No whole-request timeout: a slow link may take longer than any fixed
    // bound. A stalled one trips the read timeout and resumes on the retry.
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(60))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| AssetError::Download(format!("failed to create HTTP client: {e}")))?;

    let held = match tokio::fs::metadata(partial).await {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error.into()),
    };
    let mut request = client.get(url);
    if held > 0 {
        request = request.header(RANGE, format!("bytes={held}-"));
    }
    let response = request
        .send()
        .await
        .map_err(|e| AssetError::Download(format!("request failed for {url}: {}", causes(&e))))?;
    let content_range = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let (offset, total) = match response.status() {
        StatusCode::OK => (0, response.content_length()),
        StatusCode::PARTIAL_CONTENT if held > 0 => {
            let (start, length) = content_range
                .as_deref()
                .and_then(parse_content_range)
                .ok_or_else(|| {
                    AssetError::Download(format!("unusable Content-Range from {url}"))
                })?;
            if start != held {
                return Err(AssetError::Download(format!(
                    "{url} resumed at byte {start}, expected {held}"
                )));
            }
            (held, length)
        }
        StatusCode::RANGE_NOT_SATISFIABLE if held > 0 => {
            let length = content_range
                .as_deref()
                .and_then(|range| range.strip_prefix("bytes */"))
                .and_then(|length| length.parse::<u64>().ok());
            if length == Some(held) {
                on_progress(held, Some(held));
                return Ok(());
            }
            // Longer than the entity: restart rather than keep stale bytes.
            tokio::fs::File::create(partial).await?;
            return Err(AssetError::Download(format!(
                "{url} is shorter than the {held} bytes already downloaded"
            )));
        }
        status => {
            return Err(AssetError::HttpStatus {
                url: url.to_owned(),
                status: status.as_u16(),
            });
        }
    };

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(offset == 0)
        .open(partial)
        .await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut downloaded = offset;
    let mut stream = response.bytes_stream();
    let streamed: Result<()> = async {
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| AssetError::Download(format!("stream error: {}", causes(&e))))?;
            file.write_all(&chunk).await?;
            downloaded += chunk.len() as u64;
            on_progress(downloaded, total);
        }
        Ok(())
    }
    .await;
    // Bytes written before a failure are what the next call resumes from.
    file.flush().await?;
    streamed?;
    match total {
        Some(total) if downloaded != total => Err(AssetError::Download(format!(
            "{url} ended after {downloaded} of {total} bytes"
        ))),
        _ => Ok(()),
    }
}

/// `error` and its sources: reqwest's own message ("error sending request")
/// does not say whether the connection was refused, reset or timed out.
fn causes(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

/// Parses `bytes <start>-<end>/<length>` into the start and, unless `*`, the
/// complete entity length.
fn parse_content_range(value: &str) -> Option<(u64, Option<u64>)> {
    let (range, length) = value.strip_prefix("bytes ")?.split_once('/')?;
    let start = range.split_once('-')?.0.parse().ok()?;
    let length = match length {
        "*" => None,
        length => Some(length.parse().ok()?),
    };
    Some((start, length))
}

/// Download `url` to `dest` without checksum verification.
///
/// Useful for fetching manifests or metadata files whose checksums are
/// validated after parsing.
pub async fn download_raw(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| AssetError::Download(format!("failed to create HTTP client: {e}")))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| AssetError::Download(format!("request failed for {url}: {e}")))?;

    if !response.status().is_success() {
        return Err(AssetError::Download(format!(
            "HTTP {} for {url}",
            response.status()
        )));
    }

    let temp_path = unique_temp_path(dest);
    let outcome: Result<()> = async {
        let mut file = tokio::fs::File::create(&temp_path).await?;
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| AssetError::Download(format!("stream error: {e}")))?;
            file.write_all(&chunk).await?;
        }

        file.flush().await?;
        drop(file);

        tokio::fs::rename(&temp_path, dest).await?;
        Ok(())
    }
    .await;

    if outcome.is_err() {
        let _ = tokio::fs::remove_file(&temp_path).await;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncReadExt;

    /// Serves `body` over HTTP to every incoming connection until dropped.
    async fn serve_fixture(body: &'static [u8]) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture listener");
        let addr = listener.local_addr().expect("fixture addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(header.as_bytes()).await;
                    let _ = sock.write_all(body).await;
                });
            }
        });
        format!("http://{addr}/asset")
    }

    /// How [`serve_ranged`] answers.
    #[derive(Clone, Copy)]
    enum Ranged {
        /// Honor `Range`, but stop the first response after this many bytes.
        CutFirstAt(usize),
        /// Answer every request with the whole body, as a server without
        /// range support does.
        IgnoreRange,
        /// Honor `Range`.
        Honor,
    }

    /// Serves `body`, recording each request's `Range` header.
    async fn serve_ranged(
        body: &'static [u8],
        mode: Ranged,
    ) -> (String, Arc<Mutex<Vec<Option<String>>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture listener");
        let addr = listener.local_addr().expect("fixture addr");
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&ranges);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                let range = request
                    .lines()
                    .find_map(|line| line.strip_prefix("range: "))
                    .map(str::to_owned);
                let first = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(range.clone());
                    seen.len() == 1
                };
                let start = range
                    .as_deref()
                    .and_then(|r| r.strip_prefix("bytes=")?.strip_suffix('-')?.parse().ok())
                    .filter(|_| !matches!(mode, Ranged::IgnoreRange));
                let len = body.len();
                let (head, payload) = match start {
                    Some(start) if start >= len => (
                        format!(
                            "416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0"
                        ),
                        &body[..0],
                    ),
                    Some(start) => (
                        format!(
                            "206 Partial Content\r\nContent-Range: bytes {start}-{}/{len}\r\nContent-Length: {}",
                            len - 1,
                            len - start
                        ),
                        &body[start..],
                    ),
                    None => (format!("200 OK\r\nContent-Length: {len}"), body),
                };
                let payload = match mode {
                    Ranged::CutFirstAt(cut) if first => &payload[..cut],
                    _ => payload,
                };
                let header = format!("HTTP/1.1 {head}\r\nConnection: close\r\n\r\n");
                let _ = sock.write_all(header.as_bytes()).await;
                let _ = sock.write_all(payload).await;
            }
        });
        (format!("http://{addr}/asset"), ranges)
    }

    const RANGED_BODY: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCD";

    #[tokio::test]
    async fn interrupted_download_resumes_from_the_bytes_already_received() {
        let (url, ranges) = serve_ranged(RANGED_BODY, Ranged::CutFirstAt(10)).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let partial = dir.path().join("asset.part");

        let error = download_resumable(&url, &partial, |_, _| {})
            .await
            .expect_err("a cut transfer must fail");
        assert!(error.is_transient(), "{error}");
        assert_eq!(std::fs::read(&partial).unwrap(), &RANGED_BODY[..10]);

        let last = Mutex::new((0, None));
        download_resumable(&url, &partial, |done, total| {
            *last.lock().unwrap() = (done, total);
        })
        .await
        .expect("resume");
        assert_eq!(std::fs::read(&partial).unwrap(), RANGED_BODY);
        assert_eq!(*last.lock().unwrap(), (40, Some(40)));
        assert_eq!(
            *ranges.lock().unwrap(),
            [None, Some("bytes=10-".to_owned())]
        );
    }

    #[tokio::test]
    async fn a_server_ignoring_the_range_restarts_the_file() {
        let (url, _) = serve_ranged(RANGED_BODY, Ranged::IgnoreRange).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let partial = dir.path().join("asset.part");
        std::fs::write(&partial, b"stale").unwrap();

        download_resumable(&url, &partial, |_, _| {})
            .await
            .expect("download");
        assert_eq!(std::fs::read(&partial).unwrap(), RANGED_BODY);
    }

    #[tokio::test]
    async fn a_complete_partial_is_left_as_is() {
        let (url, ranges) = serve_ranged(RANGED_BODY, Ranged::Honor).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let partial = dir.path().join("asset.part");
        std::fs::write(&partial, RANGED_BODY).unwrap();

        download_resumable(&url, &partial, |_, _| {})
            .await
            .expect("already complete");
        assert_eq!(std::fs::read(&partial).unwrap(), RANGED_BODY);
        assert_eq!(*ranges.lock().unwrap(), [Some("bytes=40-".to_owned())]);
    }

    fn leftover_temp_files(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .expect("read dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|ext| ext == "tmp"))
            .collect()
    }

    #[tokio::test]
    async fn checksum_mismatch_removes_temp_file() {
        const BODY: &[u8] = b"asset-bytes";
        let url = serve_fixture(BODY).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("asset.bin");

        let err = download_and_verify(&url, &dest, "deadbeef", "asset", |_, _| {})
            .await
            .expect_err("wrong sha must fail");
        assert!(matches!(err, AssetError::ChecksumMismatch { .. }));
        assert!(!dest.exists(), "dest must not be created on failure");
        assert!(
            leftover_temp_files(dir.path()).is_empty(),
            "failed download must not leak temp files"
        );
    }

    #[tokio::test]
    async fn concurrent_downloads_of_same_dest_yield_verified_file() {
        // Regression: a fixed `dest.tmp` let two concurrent writers truncate
        // and interleave the same temp file, then rename a corrupt file that
        // each writer's own stream hash had "verified".
        const BODY: &[u8] = b"shared-destination-bytes";
        let url = serve_fixture(BODY).await;
        let sha = format!("{:x}", Sha256::digest(BODY));
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("tool");

        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let url = url.clone();
                let sha = sha.clone();
                let dest = dest.clone();
                tokio::spawn(async move {
                    download_and_verify(&url, &dest, &sha, "tool", |_, _| {}).await
                })
            })
            .collect();
        for t in tasks {
            t.await.expect("task").expect("download");
        }

        let on_disk = tokio::fs::read(&dest).await.expect("read dest");
        assert_eq!(on_disk, BODY, "installed file must match the payload");
        assert!(
            leftover_temp_files(dir.path()).is_empty(),
            "successful downloads must not leak temp files"
        );
    }
}
