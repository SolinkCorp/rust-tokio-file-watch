//! Watches a JSON file and updates when it changes.

use std::path::Path;

use serde::de::DeserializeOwned;
use tokio::{
    fs::{self, File},
    io::{AsyncBufReadExt, BufReader},
    sync::watch,
};
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
    serde_watch::<T, _, StringLoader<T>, _>(path, |path, data| {
        serde_json::from_str(data)
            .map_err(|e| Error::DecodeError(path.to_string_lossy().to_string(), e.to_string()))
    })
    .await
}

pub async fn jsonlines_watch<T>(path: impl AsRef<Path>) -> Result<JsonWatch<Vec<T>>, Error>
where
    T: Clone + DeserializeOwned + Send + Sync + 'static,
{
    serde_watch::<T, _, LinesLoader<T>, Vec<T>>(path, |path, data| {
        serde_json::from_str(data)
            .map_err(|e| Error::DecodeError(path.to_string_lossy().to_string(), e.to_string()))
    })
    .await
}

struct StringLoader<T> {
    _marker: std::marker::PhantomData<T>,
}

impl<T: Send> Loader<T, T> for StringLoader<T> {
    type Parse = fn(&Path, &str) -> Result<T, Error>;

    async fn load(path: &Path, parse: &Self::Parse) -> Option<T> {
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
}

struct LinesLoader<T> {
    _marker: std::marker::PhantomData<T>,
}

impl<T> Loader<T, Vec<T>> for LinesLoader<T>
where
    T: Clone + DeserializeOwned + Send + Sync + 'static,
{
    type Parse = fn(&Path, &str) -> Result<T, Error>;

    async fn load(path: &Path, parse: &Self::Parse) -> Option<Vec<T>> {
        let Ok(file) = File::open(path)
            .await
            .map_err(|err| error!(%err, path = %path.display(), "Error opening file"))
        else {
            return None;
        };
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut output = Vec::new();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if let Ok(parsed) = parse(path, &line)
                        .map_err(|err| error!(%err, path =% path.display(), "Error parsing data"))
                    {
                        output.push(parsed);
                    }
                }
                Ok(None) => break, // EOF reached
                Err(err) => {
                    error!(%err, path =% path.display(), "Error reading line from file");
                    break;
                }
            }
        }
        Some(output)
    }
}

#[trait_variant::make(Loader: Send)]
trait LocalLoader<T, R> {
    type Parse;
    #[allow(dead_code)]
    async fn load(path: &Path, parse: &Self::Parse) -> Option<R>;
}

async fn serde_watch<T, F, L, R>(
    path: impl AsRef<Path>,
    parse: F,
) -> Result<watch::Receiver<Option<R>>, Error>
where
    T: Clone + DeserializeOwned + Send + Sync + 'static,
    F: (Fn(&Path, &str) -> Result<T, Error>) + Send + Sync + 'static,
    L: Loader<T, R, Parse = F>,
    R: Send + Sync + 'static,
{
    let path = path.as_ref();
    let initial_value = L::load(path, &parse).await;
    if initial_value.is_none() {
        error!(path = %path.display(), "File does not exist or is empty.");
    }
    let (tx, rx) = watch::channel(initial_value);
    let mut watch = AsyncFsWatch::watch(&path).await?;

    let path = path.to_path_buf();
    tokio::task::spawn(async move {
        loop {
            if let Err(err) = watch.changed().await {
                error!(%err, path = %path.display(), "Error watching file");
                break;
            }

            let new_value = L::load(&path, &parse).await;
            if new_value.is_some() {
                let result = tx.send(new_value);
                if result.is_err() {
                    info!(path = %path.display(), "Watch for file is closed.");
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

    #[tokio::test]
    async fn test_update_lines() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("file.txt");

        // Create the file with multiple JSON lines
        fs::write(
            &file_path,
            r#"{"message": "Hello World!"}
{"message": "Hello Again!"}"#,
        )
        .await
        .unwrap();

        let mut watcher = jsonlines_watch::<Value>(&file_path).await.unwrap();
        {
            let value = watcher.borrow();
            let vec = value.as_ref().unwrap();
            assert_eq!(vec.len(), 2);
            assert_eq!(vec[0]["message"], "Hello World!");
            assert_eq!(vec[1]["message"], "Hello Again!");
        }

        // Update the file with multiple JSON lines
        fs::write(
            &file_path,
            r#"{"message": "Hello World 2!"}
{"message": "Hello Again 2!"}"#,
        )
        .await
        .unwrap();

        // Wait for the file to change
        watcher.changed().await.unwrap();
        {
            let value = watcher.borrow();
            let vec = value.as_ref().unwrap();
            assert_eq!(vec.len(), 2);
            assert_eq!(vec[0]["message"], "Hello World 2!");
            assert_eq!(vec[1]["message"], "Hello Again 2!");
        }
    }
}
