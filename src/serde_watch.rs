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

enum LoadOutcome<R> {
    Loaded(R), 
    Missing, 
    Invalid
}

/// Watch a JSON file for changes.
///
/// This function updates the value in the returned receiver each time the file changes.
///
/// If the file is deleted, the receiver's value becomes `None`.
/// If the file exists but its contents cannot be parsed, the value in the receiver
/// does not change.
///
pub async fn json_watch<T>(path: impl AsRef<Path>) -> Result<JsonWatch<T>, Error>
where
    T: Clone + DeserializeOwned + Send + Sync + 'static,
{
    serde_watch::<T, _, StringLoader, _>(path, |path, data| {
        serde_json::from_str(data)
            .map_err(|e| Error::DecodeError(path.to_string_lossy().to_string(), e.to_string()))
    })
    .await
}

/// Watch a JSON Lines file for changes.
///
/// A JSON Lines file is a file where each line holds one JSON object.
/// This function returns a Vec of the deserialized objects, one per line.
/// If a line contains invalid JSON, the function skips that line and processes the rest.
///
/// If the file is deleted, the receiver's value becomes `None`.
/// If the file exists but cannot be opened, the value in the receiver does not change.
///
/// This function uses a buffered reader to read the file and split it into lines. It
/// still returns all the deserialized objects at once. It is not a stream of updates,
/// so it may not suit very large files.
pub async fn jsonlines_watch<T>(path: impl AsRef<Path>) -> Result<JsonWatch<Vec<T>>, Error>
where
    T: Clone + DeserializeOwned + Send + Sync + 'static,
{
    serde_watch::<T, _, LinesLoader, Vec<T>>(path, |path, data| {
        serde_json::from_str(data)
            .map_err(|e| Error::DecodeError(path.to_string_lossy().to_string(), e.to_string()))
    })
    .await
}

struct StringLoader;

impl<T: Send> Loader<T, T> for StringLoader {
    type Parse = fn(&Path, &str) -> Result<T, Error>;

    async fn load(path: &Path, parse: &Self::Parse) -> LoadOutcome<T> {
        match fs::read_to_string(path).await {
            Ok(data) => match parse(path, &data) { 
                Ok(value) => LoadOutcome::Loaded(value),
                Err(err) => {
                    error!(%err, path = %path.display(), "Error parsing data");
                    LoadOutcome::Invalid
                },
            } 
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => LoadOutcome::Missing, 
            Err(err) => {
                error!(%err, path = %path.display(), "Error reading file");
                LoadOutcome::Invalid
            }
        }
    }
}

struct LinesLoader;

impl<T> Loader<T, Vec<T>> for LinesLoader
where
    T: Clone + DeserializeOwned + Send + Sync + 'static,
{
    type Parse = fn(&Path, &str) -> Result<T, Error>;

    async fn load(path: &Path, parse: &Self::Parse) -> LoadOutcome<Vec<T>> {
        let file = match File::open(path).await { 
            Ok(file) => file, 
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return LoadOutcome::Missing,
            Err(err) => {
                error!(%err, path = %path.display(), "Error opening file");
                return LoadOutcome::Invalid;       
            } 
        }; 
        
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut output = Vec::new();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue; // Skip empty lines
                    }
                    if let Ok(parsed) = parse(path, &line)
                        .map_err(|err| error!(%err, path = %path.display(), "Error parsing data"))
                    {
                        output.push(parsed);
                    }
                }
                Ok(None) => break, // EOF reached
                Err(err) => {
                    error!(%err, path = %path.display(), "Error reading line from file");
                    break;
                }
            }
        }
        LoadOutcome::Loaded(output)
    }
}

#[trait_variant::make(Loader: Send)]
trait LocalLoader<T, R> {
    type Parse;
    #[allow(dead_code)]
    async fn load(path: &Path, parse: &Self::Parse) -> LoadOutcome<R>;
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
    let initial_value = match L::load(path, &parse).await {
        LoadOutcome::Loaded(value) => Some(value), 
        LoadOutcome::Missing => {
            error!(path = %path.display(), "File does not exist."); 
            None
        }
        LoadOutcome::Invalid => {
            error!(path = %path.display(), "File exists but could not be loaded."); 
            None
        }
    };
    let (tx, rx) = watch::channel(initial_value);
    let mut watch = AsyncFsWatch::watch(&path).await?;

    let path = path.to_path_buf();
    tokio::task::spawn(async move {
        loop {
            if let Err(err) = watch.changed().await {
                error!(%err, path = %path.display(), "Error watching file");
                break;
            }

            let send_result = match L::load(&path, &parse).await {
                LoadOutcome::Loaded(value) => Some(tx.send(Some(value))), 
                LoadOutcome::Missing => Some(tx.send(None)),
                LoadOutcome::Invalid => None,
            }; 

            if let Some(Err(_)) = send_result {
                info!(path = %path.display(), "Watch for file is closed.");
                break;
            }
        }
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use tempfile::TempDir;
    use tracing_test::traced_test;

    use super::*;

    #[tokio::test]
    #[traced_test]
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
    #[traced_test]
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

    #[tokio::test]
    #[traced_test]
    async fn test_update_lines_with_some_parsing_issues() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("file.txt");

        fs::write(
            &file_path,
            r#"{"message": "Hello World!"}
            {"message": BROKEN}
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

        fs::write(
            &file_path,
            "{\"message\":\"Hello World 2!\"}\n{\"message\": \"Hello Again 2!\"}\r\nzzzz\n{\"message\": \"Hello Again Again 2!\"}\n\r",
        )
        .await
        .unwrap();

        // Wait for the file to change
        watcher.changed().await.unwrap();
        {
            let value = watcher.borrow();
            let vec = value.as_ref().unwrap();
            assert_eq!(vec.len(), 3);
            assert_eq!(vec[0]["message"], "Hello World 2!");
            assert_eq!(vec[1]["message"], "Hello Again 2!");
            assert_eq!(vec[2]["message"], "Hello Again Again 2!");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_delete() { 
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("file.txt");

        // create the file 
        fs::write(&file_path, r#"{"message": "Hello World!"}"#)
            .await
            .unwrap();
        let mut watcher = json_watch::<Value>(&file_path).await.unwrap();
        {
            let value = watcher.borrow();
            assert_eq!(value.as_ref().unwrap()["message"], "Hello World!"); 
        }

        // delete the file 
        fs::remove_file(&file_path).await.unwrap();
        watcher.changed().await.unwrap();
        {
            let value = watcher.borrow();
            assert!(value.is_none());
        }
    }
}
