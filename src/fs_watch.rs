use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};
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
        let canonical_watch_dir: PathBuf = path_to_watch.canonicalize().map_err(|e| {
            Error::InvalidPath(format!("Could not canonicalize {path_to_watch:?}: {e}"))
        })?;
        let files = files.to_vec();

        let mut debouncer = {
            let path_to_watch = path_to_watch.clone();
            notify_debouncer_mini::new_debouncer(
                debounce,
                move |res: DebounceEventResult| match res {
                    Ok(events) => {
                        // Ignore any events not for our watched file.
                        //
                        // We canonicalize the watch directory once, before this closure runs.
                        // The directory still exists after the file is deleted, so this
                        // canonicalization cannot fail because of the delete.
                        //
                        // See `is_watched` for how we match an event to a watched file.

                        // TODO: If we upgrade notify to 7.0.0 and notify-debouncer-mini to 0.5.0,
                        // this will start firing more or less continuously on the QNAP,
                        // even though the file isn't being modified.  :(
                        for event in events {
                            if Self::is_watched(&event.path, &canonical_watch_dir, &files) {
                                if let Err(err) = tx.send(()) {
                                    warn!(?err, ?event.path, "Error sending notification");
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

    /// Return true if the event path is one of the watched files.
    fn is_watched(event_path: &Path, canonical_watch_dir: &Path, files: &[OsString]) -> bool {
        // Match by name. We canonicalize only the parent directory, because the
        // file may not exist anymore. This match finds deletes.
        let same_dir = event_path
            .parent()
            .and_then(|dir| dir.canonicalize().ok())
            .is_some_and(|dir| dir == canonical_watch_dir);
        if same_dir
            && event_path
                .file_name()
                .is_some_and(|name| files.iter().any(|f| f == name))
        {
            return true;
        }
        // Match by resolved target. This match finds changes to a file that a watched
        // name links to, in the same directory. Both paths must exist, so this match
        // cannot find a delete. We resolve the watched file on each event, because the
        // link can change to a different target.
        let Ok(canonical_event) = event_path.canonicalize() else {
            return false;
        };
        files.iter().any(|f| {
            canonical_watch_dir
                .join(f)
                .canonicalize()
                .is_ok_and(|p| p == canonical_event)
        })
    }
}

#[cfg(test)]
mod test {
    use tempfile::TempDir;

    use super::*;
    use crate::test_utils::settle;
    use std::fs;

    #[tokio::test]
    async fn test_update() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("file.txt");

        // Create the file
        fs::write(&file_path, "Hello, world!").unwrap();

        let mut watcher = AsyncFsWatch::watch(&file_path).await.unwrap();
        settle(&mut watcher.rx).await;

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
    async fn test_delete() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("file.txt");

        // Create the file
        fs::write(&file_path, "Hello, world!").unwrap();

        let mut watcher = AsyncFsWatch::watch(&file_path).await.unwrap();
        settle(&mut watcher.rx).await;

        // Delete the file
        fs::remove_file(&file_path).unwrap();

        // Wait for the file to change
        watcher.changed().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_symlink_target_write() {
        let temp_dir = TempDir::new().unwrap();
        let target_path = temp_dir.path().join("config.v2.json");
        let link_path = temp_dir.path().join("config.json");

        // Create the target, and a symlink to it in the same folder
        fs::write(&target_path, "Hello, world!").unwrap();
        std::os::unix::fs::symlink(&target_path, &link_path).unwrap();

        let mut watcher = AsyncFsWatch::watch(&link_path).await.unwrap();
        settle(&mut watcher.rx).await;

        // Update the target directly, not through the symlink
        fs::write(&target_path, "Hello, world 2!").unwrap();

        // Wait for the file to change. Use a timeout so that a missed event fails the test.
        tokio::time::timeout(Duration::from_secs(5), watcher.changed())
            .await
            .expect("no change seen for symlink target")
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_symlink_write_through_link() {
        let temp_dir = TempDir::new().unwrap();
        let target_path = temp_dir.path().join("config.v2.json");
        let link_path = temp_dir.path().join("config.json");

        // Create the target, and a symlink to it in the same folder
        fs::write(&target_path, "Hello, world!").unwrap();
        std::os::unix::fs::symlink(&target_path, &link_path).unwrap();

        let mut watcher = AsyncFsWatch::watch(&link_path).await.unwrap();
        settle(&mut watcher.rx).await;

        // Update the target through the symlink
        fs::write(&link_path, "Hello, world 2!").unwrap();

        // Wait for the file to change. Use a timeout so that a missed event fails the test.
        tokio::time::timeout(Duration::from_secs(5), watcher.changed())
            .await
            .expect("no change seen when writing through symlink")
            .unwrap();
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
