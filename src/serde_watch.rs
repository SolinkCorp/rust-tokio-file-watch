//! Watches a JSON file and updates when it changes.

use std::path::Path;

use serde::de::DeserializeOwned;
use tokio::{fs, sync::watch};
use tracing::{error, info};

use crate::Error;

use super::fs_watch::AsyncFsWatch;

pub type JsonWatch<T> = watch::Receiver<Option<T>>;

/// Watch a JSON file for changes.
///
/// This will update the value in the returned receiver whenever the file changes.
/// If the contents of the file are removed or are invalid, the value in the receiver
/// will not change.
///
pub async fn json_watch<T>(path: impl AsRef<Path>) -> Result<JsonWatch<T>, Error>
where
    T: Clone + DeserializeOwned + Send + Sync + 'static,
{
    serde_watch(path, |path, data| {
        serde_json::from_str(data)
            .map_err(|e| Error::DecodeError(path.to_string_lossy().to_string(), e.to_string()))
    })
    .await
}

async fn load_file<T, F>(path: impl AsRef<Path>, parse: &F) -> Option<T>
where
    T: Clone,
    F: (Fn(&Path, &str) -> Result<T, Error>) + Send + Sync + 'static,
{
    let path = path.as_ref();
    match fs::read_to_string(path).await {
        Ok(data) => parse(path, &data)
            .map_err(|err| error!(?err, ?path, "Error parsing data"))
            .ok(),
        Err(err) => {
            error!(?err, ?path, "Error reading file");
            None
        }
    }
}

async fn serde_watch<T, F>(
    path: impl AsRef<Path>,
    parse: F,
) -> Result<watch::Receiver<Option<T>>, Error>
where
    T: Clone + DeserializeOwned + Send + Sync + 'static,
    F: (Fn(&Path, &str) -> Result<T, Error>) + Send + Sync + 'static,
{
    let initial_value = load_file(&path, &parse).await;
    let (tx, rx) = watch::channel(initial_value);
    let mut watch = AsyncFsWatch::watch(&path).await?;

    let path = path.as_ref().to_path_buf();
    tokio::spawn(async move {
        loop {
            if let Err(err) = watch.changed().await {
                error!("Error watching file {path:?}: {err}");
                break;
            }

            let new_value = load_file(&path, &parse).await;
            if new_value.is_some() {
                let result = tx.send(new_value);
                if result.is_err() {
                    info!(?path, "Watch for file is closed.");
                    break;
                }
            }
        }
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn test_update() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("file.txt");

        // Create the file
        fs::write(&file_path, r#"{"message": "Hello World!"}"#)
            .await
            .unwrap();

        let mut watcher = json_watch::<Value>(&file_path).await.unwrap();
        {
            let value = watcher.borrow();
            assert_eq!(value.as_ref().unwrap()["message"], "Hello World!");
        }

        // Update the file
        fs::write(&file_path, r#"{"message": "Hello World 2!"}"#)
            .await
            .unwrap();

        // Wait for the file to change
        watcher.changed().await.unwrap();
        {
            let value = watcher.borrow();
            assert_eq!(value.as_ref().unwrap()["message"], "Hello World 2!");
        }
    }
}
