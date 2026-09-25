use std::time::Duration;

use tokio::sync::watch;

/// Wait for the events from the test setup to arrive, then mark them as seen.
///
/// On macOS, these events can arrive after the watcher starts. This also occurs in CI
/// on `macos-latest`. Without this step, `changed()` can return because of the setup,
/// not because of the test action. Call this after you start the watcher, if the test
/// writes to the watched directory before it starts the watcher.
pub(crate) async fn settle<T>(rx: &mut watch::Receiver<T>) {
    tokio::time::sleep(Duration::from_secs(1)).await;
    rx.borrow_and_update();
}
