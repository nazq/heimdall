//! Supervisor event loop: fork child, bind socket, multiplex I/O.

use crate::broadcast::OutputState;
use crate::cli::SessionParams;
use crate::socket::ServerState;
use crate::{pty, socket};
use bytes::BytesMut;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::net::UnixListener;

/// Grace period between SIGTERM and SIGKILL on shutdown.
const SIGKILL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
/// Delay after broadcasting exit to let in-flight socket writes drain.
const EXIT_DRAIN_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// Look up process details from `/proc` for a diagnostic message.
/// Returns a human-readable string like `"uptime 2h 15m, cmd: hm run --id foo -- bash"`.
/// Returns empty string if procfs is unavailable (non-Linux, permission denied, etc).
fn proc_detail(pid: i32) -> String {
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let mut parts = Vec::new();

    if let Some(uptime) = proc_uptime(&proc) {
        parts.push(format_uptime(uptime));
    }
    if let Some(cmd) = proc_cmdline(&proc) {
        parts.push(format!("cmd: {cmd}"));
    }

    parts.join(", ")
}

/// Read process uptime in seconds from `/proc/<pid>/stat` and `/proc/uptime`.
fn proc_uptime(proc: &Path) -> Option<u64> {
    let stat = std::fs::read_to_string(proc.join("stat")).ok()?;
    let uptime_s = std::fs::read_to_string("/proc/uptime").ok()?;

    let ticks_per_sec = unsafe { nix::libc::sysconf(nix::libc::_SC_CLK_TCK) };
    if ticks_per_sec <= 0 {
        return None;
    }

    // Field 22 (1-indexed) is starttime in clock ticks since boot.
    // Field 2 (comm) can contain spaces/parens, so find the closing ')' first.
    let after_comm = stat.rfind(')')? + 2;
    let fields: Vec<&str> = stat[after_comm..].split_whitespace().collect();
    // After ')': field 3 = index 0, so field 22 = index 19.
    let starttime: u64 = fields.get(19)?.parse().ok()?;

    let boot_secs = uptime_s.split_whitespace().next()?.parse::<f64>().ok()? as u64;
    let start_secs = starttime / ticks_per_sec as u64;

    Some(boot_secs.saturating_sub(start_secs))
}

/// Read the command line from `/proc/<pid>/cmdline`.
fn proc_cmdline(proc: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(proc.join("cmdline")).ok()?;
    let cmd = raw.replace('\0', " ").trim().to_string();
    if cmd.is_empty() { None } else { Some(cmd) }
}

/// Format seconds into a human-readable uptime string.
pub(crate) fn format_uptime(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        format!("uptime {days}d {hours}h")
    } else if hours > 0 {
        format!("uptime {hours}h {mins}m")
    } else {
        format!("uptime {mins}m")
    }
}

/// Called when flock fails — another supervisor holds the PID lock.
/// Retries reading the PID file (the holder may not have written yet),
/// then prints diagnostics and exits.
fn die_session_locked(id: &str, pid_path: &Path) -> ! {
    use crate::pidfile::PidFile;

    // The lock holder may not have written the PID yet — retry briefly.
    let mut pids = None;
    for _ in 0..20 {
        if let Some(pf) = PidFile::read(pid_path) {
            pids = Some(pf);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    match pids {
        Some(pf) => {
            let detail = proc_detail(pf.supervisor);
            if detail.is_empty() {
                eprintln!(
                    "Session '{id}' is locked by supervisor pid {}. \
                     Use `hm kill {id}` first.",
                    pf.supervisor,
                );
            } else {
                eprintln!(
                    "Session '{id}' is locked by supervisor pid {} ({detail}). \
                     Use `hm kill {id}` first.",
                    pf.supervisor,
                );
            }
        }
        None => {
            eprintln!(
                "Session '{id}' is locked by another process \
                 (could not read PID after 1s). Use `hm kill {id}` first.",
            );
        }
    }

    std::process::exit(1);
}

/// RAII guard that removes session files on drop.
/// Ensures cleanup even on panic or early `?` return.
struct CleanupGuard {
    socket_path: PathBuf,
    pid_path: PathBuf,
    config_path: PathBuf,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.pid_path);
        // Ephemeral config file written by to_detach_args() for re-exec.
        // May contain secrets from [[env]]. Clean up silently.
        let _ = std::fs::remove_file(&self.config_path);
    }
}

pub fn supervise(params: SessionParams) -> anyhow::Result<()> {
    let SessionParams {
        id,
        workdir,
        socket_dir,
        cols,
        rows,
        cmd,
        cfg,
        log_file,
    } = params;
    std::fs::create_dir_all(&socket_dir)?;

    let socket_path = crate::util::socket_path(&socket_dir, &id);
    let pid_path = crate::util::pid_path(&socket_dir, &id);

    // Acquire exclusive lock on PID file to prevent TOCTOU races.
    let pid_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&pid_path)?;
    use nix::fcntl::{Flock, FlockArg};
    let mut lock = match Flock::lock(pid_file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => lock,
        Err(_) => die_session_locked(&id, &pid_path),
    };

    // kill(pid, 0) is a POSIX probe: signal 0 is never delivered. The kernel
    // runs all existence and permission checks, then does nothing. It's the
    // standard Unix "is this PID alive?" idiom.
    //
    // Check if existing PIDs in the file are still alive.
    // kill(pid, 0) == 0        → process exists, we can signal it (alive → bail)
    // kill(pid, 0) == -1/EPERM → process exists, different owner  (alive → bail)
    // kill(pid, 0) == -1/ESRCH → no such process                  (stale → fall through)
    if let Some(pf) = crate::pidfile::PidFile::read(&pid_path)
        && pf.any_alive()
    {
        let display_pid = pf.child.unwrap_or(pf.supervisor);
        let detail = proc_detail(pf.supervisor);
        if detail.is_empty() {
            eprintln!(
                "Session '{id}' is already running (pid {display_pid}). \
                 Use `hm kill {id}` first.",
            );
        } else {
            eprintln!(
                "Session '{id}' is already running (pid {display_pid}, {detail}). \
                 Use `hm kill {id}` first.",
            );
        }
        std::process::exit(1);
    }

    // Clean up stale socket. Ignore NotFound (race or already gone),
    // but surface anything else (permissions, filesystem errors).
    if let Err(e) = std::fs::remove_file(&socket_path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(e.into());
    }

    let workdir = workdir.canonicalize()?;

    // Write supervisor PID (line 1) before fork — if we crash between here
    // and fork, the PID file still identifies who held the lock.
    {
        let f: &mut std::fs::File = &mut lock;
        crate::pidfile::PidFile::write_supervisor(f, std::process::id())?;
    }

    // Fork child BEFORE starting Tokio runtime (single-threaded requirement).
    let pty_child = pty::spawn(&cmd, &workdir, &id, cols, rows, &cfg)?;
    let child_pid = pty_child.pid;

    // Append child PID (line 2) after fork.
    {
        let f: &mut std::fs::File = &mut lock;
        crate::pidfile::PidFile::write_child(f, child_pid.as_raw())?;
    }

    // RAII cleanup — removes socket + PID + ephemeral config on drop.
    let _cleanup = CleanupGuard {
        socket_path: socket_path.clone(),
        pid_path: pid_path.clone(),
        config_path: socket_dir.join(format!("{id}.config.toml")),
    };

    // Log to file, never to stderr (which would corrupt the terminal or
    // vanish in detach mode). RUST_LOG env var takes precedence over both
    // log_level (heimdall's own level) and log_filter (dependency crates).
    let log_writer = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)?;
    let mut env_filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive(format!("heimdall={}", cfg.log_level).parse().unwrap());
    if let Some(ref filter) = cfg.log_filter {
        for directive in filter.split(',') {
            let directive = directive.trim();
            if !directive.is_empty() {
                if let Ok(d) = directive.parse() {
                    env_filter = env_filter.add_directive(d);
                } else {
                    eprintln!("warning: ignoring invalid log_filter directive: {directive}");
                }
            }
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_ansi(false)
        .with_writer(log_writer)
        .init();

    tracing::info!(
        session_id = %id,
        child_pid = child_pid.as_raw(),
        socket = %socket_path.display(),
        "supervisor started"
    );

    // Single-threaded runtime — sufficient for our I/O workload.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let exit_code = rt.block_on(event_loop(socket_path, cfg, child_pid, pty_child.master))?;

    drop(_cleanup);
    std::process::exit(exit_code);
}

/// Async event loop: bind socket, multiplex pty reads, signals, and client connections.
async fn event_loop(
    socket_path: PathBuf,
    cfg: crate::config::Config,
    child_pid: nix::unistd::Pid,
    master_fd: std::os::fd::OwnedFd,
) -> Result<i32, std::io::Error> {
    let listener = UnixListener::bind(&socket_path)?;
    let master_raw_fd = master_fd.as_raw_fd();

    let output = Arc::new(OutputState::new(&cfg));
    let alive = Arc::new(AtomicBool::new(true));

    let server_state = Arc::new(ServerState {
        output: Arc::clone(&output),
        child_pid,
        master_fd: master_raw_fd,
        alive: Arc::clone(&alive),
        exit_code: std::sync::atomic::AtomicI32::new(0),
        kill_process_group: cfg.kill_process_group,
    });

    // Prepare the master fd for tokio's async reactor. Four steps:
    //
    // 1. Consume the OwnedFd via into_raw_fd(). We already captured the
    //    raw fd number (line above) for ServerState before this point.
    // 2. Wrap the raw fd back into a fresh OwnedFd. This isn't a no-op —
    //    it transfers ownership from the function parameter into a local
    //    that AsyncFd will consume.
    // 3. Set O_NONBLOCK. openpty() returns blocking fds, but tokio's
    //    AsyncFd requires non-blocking so epoll can drive readiness
    //    without stalling the single-threaded runtime.
    // 4. Register with tokio's reactor via AsyncFd::new(). After this,
    //    the event loop can `await` readability on the master fd.
    let owned_fd = master_fd.into_raw_fd();
    // SAFETY: we just consumed the only owner; no double-close possible.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(owned_fd) };

    let raw = owned.as_fd().as_raw_fd();
    let flags = unsafe { nix::libc::fcntl(raw, nix::libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { nix::libc::fcntl(raw, nix::libc::F_SETFL, flags | nix::libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }

    let master_async = tokio::io::unix::AsyncFd::new(owned)?;

    // Signal handlers for the hm supervisor process.
    let mut sigchld = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
    // recv when kill, kill -15, or kill -TERM (graceful shutdown) are sent
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    // Spawn socket server
    let server_state_clone = Arc::clone(&server_state);
    tokio::spawn(async move {
        socket::serve(listener, server_state_clone).await;
    });

    // Main event loop
    // Use BytesMut so we can freeze() a zero-copy Bytes handle per read,
    // rather than Bytes::copy_from_slice which memcpy's into a new alloc.
    let mut read_buf = BytesMut::with_capacity(8192);
    let mut buf = [0u8; 4096];
    let exit_code: i32;

    loop {
        tokio::select! {
            // Child pty master is readable — drain available bytes into scrollback.
            ready = master_async.readable() => {
                let mut guard = ready?;
                match guard.try_io(|inner| {
                    let fd = inner.as_raw_fd();
                    let n = unsafe {
                        nix::libc::read(fd, buf.as_mut_ptr().cast(), buf.len())
                    };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(Ok(0)) => {
                        tracing::info!("pty EOF");
                        exit_code = pty::wait_child(child_pid).unwrap_or(-1);
                        break;
                    }
                    Ok(Ok(n)) => {
                        // Extend the BytesMut and split off a frozen Bytes.
                        // When read_buf has no other outstanding Bytes handles
                        // (the common case — broadcast subscribers hold their
                        // own refcounted views), this reuses the same backing
                        // allocation instead of malloc+memcpy per read.
                        read_buf.extend_from_slice(&buf[..n]);
                        let chunk = read_buf.split().freeze();
                        output.push(chunk);
                    }
                    Ok(Err(e)) => {
                        if e.raw_os_error() == Some(nix::libc::EIO) {
                            tracing::info!("pty EIO (child exited)");
                        } else {
                            tracing::error!("pty read error: {e}");
                        }
                        exit_code = pty::wait_child(child_pid).unwrap_or(-1);
                        break;
                    }
                    Err(_would_block) => {
                        continue;
                    }
                }
            }

            // Child exited — reap it and exit the loop.
            _ = sigchld.recv() => {
                tracing::info!("SIGCHLD received");
                exit_code = pty::wait_child(child_pid).unwrap_or(-1);
                break;
            }

            // Supervisor asked to stop — SIGTERM the child, SIGKILL after grace period.
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received, shutting down");
                let _ = pty::send_sigterm(child_pid, cfg.kill_process_group);
                tokio::time::sleep(SIGKILL_GRACE).await;
                let _ = pty::send_sigkill(child_pid, cfg.kill_process_group);
                exit_code = pty::wait_child(child_pid).unwrap_or(-1);
                break;
            }
        }
    }

    output.set_dead();
    // Store the exit code before broadcasting so subscribers see it on Acquire load.
    server_state.exit_code.store(exit_code, Ordering::Release);
    // Broadcast the EXIT frame first, then mark alive=false. This ordering
    // guarantees that any subscriber that observes alive=false has already
    // received (or will receive) the EXIT frame as the last channel message,
    // never raw OUTPUT chunks written without their frame wrapper.
    let exit_frame = crate::protocol::pack_exit(exit_code);
    let _ = server_state.output.tx.send(exit_frame);
    alive.store(false, Ordering::Release);

    tokio::time::sleep(EXIT_DRAIN_DELAY).await;

    tracing::info!(exit_code, "supervisor exiting");

    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_uptime_zero_seconds() {
        assert_eq!(format_uptime(0), "uptime 0m");
    }

    #[test]
    fn format_uptime_under_one_minute() {
        assert_eq!(format_uptime(59), "uptime 0m");
    }

    #[test]
    fn format_uptime_exactly_one_minute() {
        assert_eq!(format_uptime(60), "uptime 1m");
    }

    #[test]
    fn format_uptime_exactly_one_hour() {
        assert_eq!(format_uptime(3600), "uptime 1h 0m");
    }

    #[test]
    fn format_uptime_one_hour_one_minute_one_second() {
        assert_eq!(format_uptime(3661), "uptime 1h 1m");
    }

    #[test]
    fn format_uptime_exactly_one_day() {
        assert_eq!(format_uptime(86400), "uptime 1d 0h");
    }

    #[test]
    fn format_uptime_one_day_one_hour() {
        assert_eq!(format_uptime(90061), "uptime 1d 1h");
    }
}
