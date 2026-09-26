//! Why the server's files — its keys and the client config — could not be
//! set up.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{}: {source}", path.display())]
    Key {
        path: PathBuf,
        #[source]
        source: russh::keys::ssh_key::Error,
    },
    #[error(transparent)]
    Write(#[from] arcbox_atomic_file::AtomicWriteError),
    #[error("{}: ssh_config cannot hold a path with a quote or a control character", .0.display())]
    UnquotablePath(PathBuf),
}
