# solink-tokio-file-watch

Provides a library for watching a JSON file and reloading it when it changes.

Example:

```rust
let config_watch: JsonWatch<Config> = solink_tokio_file_watch::json_watch("./config.json").await?;
```
