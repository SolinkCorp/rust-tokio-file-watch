use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Invalid path to watch: {0}")]
    InvalidPath(String),
    #[error("Error creating watch: {0}")]
    WatchError(#[from] notify::Error),
    #[error("Watcher stopped watching")]
    WatcherStopped,
    #[error("Error decoding file: {0} {1}")]
    DecodeError(String, String),
}