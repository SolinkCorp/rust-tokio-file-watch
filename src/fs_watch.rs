use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer};
use std::{ffi::OsString, path::Path, time::Duration};
use tokio::{fs, sync::watch};
use tracing::warn;

use crate::Error;

/// A file watcher that debounces events.
#[allow(clippy::module_name_repetitions)]
pub struct AsyncFsWatch {
    _debouncer: Debouncer<RecommendedWatcher>,
    rx: watch::Receiver<()>,
}

impl AsyncFsWatch {
    /// Watch a path, and be notified when it changes.
    pub async fn watch<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        let path = path.as_ref();
        // Note that we actually watch the parent of the path - if the path we're interested
        // in doesn't exist, we'll get notified when it's created.
        let path_to_watch = path.parent().unwrap_or(path.as_ref());
        let Some(filename) = path.file_name() else {
            return Err(Error::InvalidPath(format!(
                "Path has no filename: {path:?}"
            )));
        };

        Self::watch_with_debounce(
            path_to_watch,
            &[filename.to_owned()],
            Duration::from_millis(500),
        )
        .await
    }

    /// Watch a path, and be notified when it changes.
    pub async fn watch_with_debounce<P: AsRef<Path>>(
        folder: P,
        files: &[OsString],
        debounce: Duration,
    ) -> Result<Self, Error> {
        let (tx, rx) = watch::channel(());

        // Note that we actually watch the parent of the path - if the path we're interested
        // in doesn't exist, we'll get notified when it's created.
        let path_to_watch = folder.as_ref().to_path_buf();
        // Make sure the folder exists.
        fs::create_dir_all(&path_to_watch)
            .await
            .map_err(|e| Error::InvalidPath(format!("Could not create {path_to_watch:?}: {e}")))?;
        let files = files.to_vec();

        let mut debouncer = {
            let path_to_watch = path_to_watch.clone();
            notify_debouncer_mini::new_debouncer(
                debounce,
                move |res: DebounceEventResult| match res {
                    Ok(events) => {
                        // Ignore any events not for our desired path. We need to
                        // canonicalize the paths from the event here, because it
                        // might already be canonicalized.  We also need to canonicalize
                        // the path we're looking for - we can't do this above
                        // outside the debouncer, because canonicalization will fail
                        // if the path doesn't exist.

                        // TODO: If we upgrade notify to 7.0.0 and notify-debouncer-mini to 0.5.0,
                        // this will start firing more or less continuously on the QNAP,
                        // even though the file isn't being modified.  :(
                        for event in events {
                            if let Ok(event_path) = event.path.canonicalize() {
                                for file in &files {
                                    if let Ok(file_path) = path_to_watch.join(file).canonicalize() {
                                        if event_path == file_path {
                                            if let Err(err) = tx.send(()) {
                                                warn!(
                                                    ?err,
                                                    ?event_path,
                                                    "Error sending notification"
                                                );
                                            }
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(err) => warn!(path = ?path_to_watch, ?err, "Error watching path"),
                },
            )
            .map_err(Error::WatchError)?
        };

        debouncer
            .watcher()
            .watch(&path_to_watch, RecursiveMode::NonRecursive)?;

        Ok(Self {
            _debouncer: debouncer,
            rx,
        })
    }

    /// Wait for the watched file to change.
    pub async fn changed(&mut self) -> Result<(), Error> {
        self.rx.changed().await.map_err(|_| Error::WatcherStopped)
    }
}

#[cfg(test)]
mod test {
    use tempfile::TempDir;

    use super::*;
    use std::fs;

    #[tokio::test]
    async fn test_update() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("file.txt");

        // Create the file
        fs::write(&file_path, "Hello, world!").unwrap();

        let mut watcher = AsyncFsWatch::watch(&file_path).await.unwrap();

        // Update the file
        fs::write(&file_path, "Hello, world 2!").unwrap();

        // Wait for the file to change
        watcher.changed().await.unwrap();
    }

    #[tokio::test]
    async fn test_create() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("file.txt");

        let mut watcher = AsyncFsWatch::watch(&file_path).await.unwrap();

        // Create the file
        fs::write(&file_path, "Hello, world!").unwrap();

        // Wait for the file to change
        watcher.changed().await.unwrap();
    }

    #[tokio::test]
    async fn test_missing_folder() {
        let temp_dir = TempDir::new().unwrap();
        // Create the file in a subdirectory that doesn't exist yet.
        let file_path = temp_dir.path().join("foo/file.txt");

        let mut watcher = AsyncFsWatch::watch(&file_path).await.unwrap();

        // Create the file
        fs::write(&file_path, "Hello, world!").unwrap();

        // Wait for the file to change
        watcher.changed().await.unwrap();
    }
}
