//! Shared utilities used across subcommands.

use std::path::{Path, PathBuf};

/// Socket path for a session: `<dir>/<id>.sock`.
pub fn socket_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.sock"))
}

/// PID file path for a session: `<dir>/<id>.pid`.
pub fn pid_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.pid"))
}

/// Resolve socket path and bail if the session doesn't exist.
pub fn session_socket(id: &str, socket_dir: &Path) -> PathBuf {
    let path = socket_path(socket_dir, id);
    if !path.exists() {
        eprintln!("No session found: {id}");
        std::process::exit(1);
    }
    path
}

/// Build a single-threaded tokio runtime and run an async closure.
pub fn with_runtime<F, T>(f: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(f)
}
