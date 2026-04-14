//! Async file-based block backend (uses tokio).

use std::path::PathBuf;

/// Async file-based block backend using tokio.
#[allow(dead_code)]
pub struct AsyncFileBackend {
    /// Path to the backing file.
    path: PathBuf,
    /// Capacity in sectors.
    capacity: u64,
    /// Block size.
    block_size: u32,
    /// Read-only mode.
    read_only: bool,
}

impl AsyncFileBackend {
    /// Creates a new async file backend.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened.
    pub fn new(path: impl Into<PathBuf>, read_only: bool) -> std::io::Result<Self> {
        let path = path.into();

        let file = std::fs::File::open(&path)?;
        let metadata = file.metadata()?;
        let capacity = metadata.len() / 512;

        tracing::info!(
            "Created async file backend: {}, capacity={} sectors",
            path.display(),
            capacity
        );

        Ok(Self {
            path,
            capacity,
            block_size: 512,
            read_only,
        })
    }

    /// Performs async read.
    pub async fn async_read(&self, sector: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let mut file = tokio::fs::File::open(&self.path).await?;
        let offset = sector * u64::from(self.block_size);
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.read(buf).await
    }

    /// Performs async write.
    pub async fn async_write(&self, sector: u64, buf: &[u8]) -> std::io::Result<usize> {
        use tokio::io::{AsyncSeekExt, AsyncWriteExt};

        if self.read_only {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Device is read-only",
            ));
        }

        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&self.path)
            .await?;

        let offset = sector * u64::from(self.block_size);
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.write(buf).await
    }

    /// Performs async flush.
    pub async fn async_flush(&self) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;

        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&self.path)
            .await?;
        file.flush().await?;
        file.sync_all().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_async_file_backend_creation() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(&vec![0u8; 8192]).unwrap();

        let backend = AsyncFileBackend::new(temp_file.path(), false).unwrap();
        assert_eq!(backend.capacity, 16); // 8192 / 512
    }

    #[tokio::test]
    async fn test_async_file_backend_read() {
        let mut temp_file = NamedTempFile::new().unwrap();
        let mut data = vec![0u8; 4096];
        data[0..5].copy_from_slice(b"Hello");
        temp_file.write_all(&data).unwrap();

        let backend = AsyncFileBackend::new(temp_file.path(), true).unwrap();

        let mut buf = vec![0u8; 5];
        let read = backend.async_read(0, &mut buf).await.unwrap();
        assert_eq!(read, 5);
        assert_eq!(&buf, b"Hello");
    }

    #[tokio::test]
    async fn test_async_file_backend_write() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(&vec![0u8; 4096]).unwrap();

        let backend = AsyncFileBackend::new(temp_file.path(), false).unwrap();

        let written = backend.async_write(0, b"AsyncTest").await.unwrap();
        assert_eq!(written, 9);

        let mut buf = vec![0u8; 9];
        backend.async_read(0, &mut buf).await.unwrap();
        assert_eq!(&buf, b"AsyncTest");
    }

    #[tokio::test]
    async fn test_async_file_backend_read_only() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(&vec![0u8; 4096]).unwrap();

        let backend = AsyncFileBackend::new(temp_file.path(), true).unwrap();

        let result = backend.async_write(0, b"test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_async_file_backend_flush() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(&vec![0u8; 4096]).unwrap();

        let backend = AsyncFileBackend::new(temp_file.path(), false).unwrap();
        backend.async_write(0, b"flush test").await.unwrap();

        assert!(backend.async_flush().await.is_ok());
    }
}
