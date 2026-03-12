//! Heimdall — PTY session supervisor.
//!
//! Owns the pty, manages process lifecycle, exposes a Unix socket for IPC.
//! Everything else is a client.

mod broadcast;
mod classify;
mod config;
mod protocol;
mod pty;
mod socket;

use broadcast::OutputState;
use bytes::Bytes;
use clap::{Parser, Subcommand};
use nix::sys::termios;
use socket::ServerState;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

#[derive(Parser)]
#[command(
    name = "hm",
    about = "PTY session supervisor",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("HM_BUILD_TIME"), ")")
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Path to config file.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Launch a supervised session and attach to it.
    Run {
        /// Session identifier (used for socket filename).
        #[arg(long)]
        id: String,
        /// Working directory for the child process.
        #[arg(long, default_value = ".")]
        workdir: PathBuf,
        /// Directory for socket and pid files (overrides config).
        #[arg(long)]
        socket_dir: Option<PathBuf>,
        /// Terminal columns.
        #[arg(long, default_value_t = 220)]
        cols: u16,
        /// Terminal rows.
        #[arg(long, default_value_t = 50)]
        rows: u16,
        /// Run the supervisor in the background without attaching.
        #[arg(long)]
        detach: bool,
        /// Child command and arguments (everything after --).
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
    },
    /// Attach to a running session (terminal passthrough).
    Attach {
        /// Session identifier to attach to.
        id: String,
        /// Directory for socket files (overrides config).
        #[arg(long)]
        socket_dir: Option<PathBuf>,
    },
    /// Query status of a session.
    Status {
        /// Session identifier.
        id: String,
        /// Directory for socket files (overrides config).
        #[arg(long)]
        socket_dir: Option<PathBuf>,
    },
    /// List active sessions.
    #[command(name = "ls")]
    List {
        /// Directory for socket files (overrides config).
        #[arg(long)]
        socket_dir: Option<PathBuf>,
    },
    /// Kill a session (graceful shutdown).
    Kill {
        /// Session identifier.
        id: String,
        /// Directory for socket files (overrides config).
        #[arg(long)]
        socket_dir: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    // Parse CLI before any Tokio runtime — fork() must happen single-threaded.
    let cli = Cli::parse();
    let cfg = config::load(cli.config.as_deref())?;

    match cli.command {
        Command::Run {
            id,
            workdir,
            socket_dir,
            cols,
            rows,
            detach,
            cmd,
        } => {
            let dir = socket_dir.unwrap_or_else(|| cfg.socket_dir.clone());
            if detach {
                run_supervisor(id, workdir, dir, cols, rows, cmd, cfg)
            } else {
                run_and_attach(id, workdir, dir, cols, rows, cmd, cfg)
            }
        }
        Command::Attach { id, socket_dir } => {
            let dir = socket_dir.unwrap_or_else(|| cfg.socket_dir.clone());
            run_attach(id, dir, &cfg)
        }
        Command::Status { id, socket_dir } => {
            let dir = socket_dir.unwrap_or_else(|| cfg.socket_dir.clone());
            run_status(id, dir, &cfg)
        }
        Command::List { socket_dir } => {
            let dir = socket_dir.unwrap_or_else(|| cfg.socket_dir.clone());
            run_list(dir)
        }
        Command::Kill { id, socket_dir } => {
            let dir = socket_dir.unwrap_or_else(|| cfg.socket_dir.clone());
            run_kill(id, dir)
        }
    }
}

/// Launch the supervisor as a background process and attach to it.
///
/// Spawns `hm run --detach` as a child process, waits for the socket to
/// appear, then runs the normal attach flow. When the attach disconnects
/// (Ctrl-\ or session exit), the supervisor keeps running in the background.
fn run_and_attach(
    id: String,
    workdir: PathBuf,
    socket_dir: PathBuf,
    cols: u16,
    rows: u16,
    cmd: Vec<String>,
    cfg: config::Config,
) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let mut child_args = vec![
        "run".to_string(),
        "--id".to_string(),
        id.clone(),
        "--workdir".to_string(),
        workdir.to_string_lossy().into_owned(),
        "--socket-dir".to_string(),
        socket_dir.to_string_lossy().into_owned(),
        "--cols".to_string(),
        cols.to_string(),
        "--rows".to_string(),
        rows.to_string(),
        "--detach".to_string(),
        "--".to_string(),
    ];
    child_args.extend(cmd);

    // Spawn supervisor in background. Redirect stdio to /dev/null and
    // call setsid() in the child so it becomes its own session leader.
    // Without setsid(), closing the terminal (X button) sends SIGHUP to
    // the entire session, killing the supervisor along with the attach client.
    let _supervisor = unsafe {
        std::process::Command::new(exe)
            .args(&child_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .pre_exec(|| {
                // Create a new session so the supervisor isn't killed when
                // the parent terminal closes.
                nix::unistd::setsid().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                Ok(())
            })
            .spawn()?
    };

    // Wait for the socket to appear (supervisor needs a moment to bind).
    let socket_path = socket_dir.join(format!("{id}.sock"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !socket_path.exists() {
        if std::time::Instant::now() > deadline {
            anyhow::bail!(
                "Timed out waiting for supervisor socket at {}",
                socket_path.display()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // Attach to the now-running session.
    run_attach(id, socket_dir, &cfg)
}

/// RAII guard that removes socket and PID files on drop.
/// Ensures cleanup even on panic or early `?` return.
struct CleanupGuard {
    socket_path: PathBuf,
    pid_path: PathBuf,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.pid_path);
    }
}

fn run_supervisor(
    id: String,
    workdir: PathBuf,
    socket_dir: PathBuf,
    cols: u16,
    rows: u16,
    cmd: Vec<String>,
    cfg: config::Config,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(&socket_dir)?;

    let socket_path = socket_dir.join(format!("{id}.sock"));
    let pid_path = socket_dir.join(format!("{id}.pid"));

    // Acquire exclusive lock on PID file to prevent TOCTOU races.
    // Two `hm run` with the same ID will serialize on this lock.
    use std::io::Write;
    let pid_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&pid_path)?;
    use nix::fcntl::{Flock, FlockArg};
    let mut lock = match Flock::lock(pid_file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => lock,
        Err(_) => {
            eprintln!(
                "Session '{id}' is already running (PID file locked). \
                 Use `hm kill {id}` first.",
            );
            std::process::exit(1);
        }
    };

    // Check if existing PID in the file is still alive.
    if let Ok(contents) = std::fs::read_to_string(&pid_path)
        && let Ok(pid) = contents.trim().parse::<i32>()
    {
        let alive = unsafe { nix::libc::kill(pid, 0) } == 0;
        if alive {
            eprintln!(
                "Session '{id}' is already running (pid {pid}). \
                 Use `hm kill {id}` first.",
            );
            std::process::exit(1);
        }
    }

    // Clean up stale socket
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }

    let workdir = workdir.canonicalize()?;

    // Fork child BEFORE starting Tokio runtime (single-threaded requirement).
    let pty_child = pty::spawn(&cmd, &workdir, &id, cols, rows, &cfg)?;
    let child_pid = pty_child.pid;
    let master_raw_fd = pty_child.master.as_raw_fd();

    // Write PID to the locked file.
    // Flock<File> derefs to File, so we can use it directly.
    {
        use std::io::Seek;
        let f: &mut std::fs::File = &mut lock;
        f.set_len(0)?;
        f.seek(std::io::SeekFrom::Start(0))?;
        write!(f, "{}", child_pid.as_raw())?;
    }

    // RAII cleanup — removes socket + PID on drop (panic, early return, normal exit).
    let _cleanup = CleanupGuard {
        socket_path: socket_path.clone(),
        pid_path: pid_path.clone(),
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("heimdall=info".parse().unwrap()),
        )
        .with_target(false)
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

    let exit_code = rt.block_on(async move {
        let listener = UnixListener::bind(&socket_path)?;

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

        // Transfer ownership of the master fd to AsyncFd.
        // into_raw_fd() consumes the OwnedFd without closing it.
        let owned_fd = pty_child.master.into_raw_fd();
        // SAFETY: we just consumed the only owner; no double-close possible.
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(owned_fd) };

        // AsyncFd requires the fd to be non-blocking. openpty() returns
        // blocking fds, so set O_NONBLOCK before wrapping. Without this,
        // libc::read inside try_io blocks the entire runtime when the pty
        // has no data (instead of returning EAGAIN).
        let raw = owned.as_fd().as_raw_fd();
        let flags = unsafe { nix::libc::fcntl(raw, nix::libc::F_GETFL) };
        if flags == -1 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { nix::libc::fcntl(raw, nix::libc::F_SETFL, flags | nix::libc::O_NONBLOCK) } == -1
        {
            return Err(std::io::Error::last_os_error());
        }

        let master_async = tokio::io::unix::AsyncFd::new(owned)?;

        // Signal handlers
        let mut sigchld = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

        // Spawn socket server
        let server_state_clone = Arc::clone(&server_state);
        tokio::spawn(async move {
            socket::serve(listener, server_state_clone).await;
        });

        // Main event loop
        let mut buf = [0u8; 4096];
        let exit_code: i32;

        loop {
            tokio::select! {
                // Read from pty master
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
                            let chunk = Bytes::copy_from_slice(&buf[..n]);
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

                // SIGCHLD — child exited
                _ = sigchld.recv() => {
                    tracing::info!("SIGCHLD received");
                    exit_code = pty::wait_child(child_pid).unwrap_or(-1);
                    break;
                }

                // SIGTERM — graceful shutdown
                _ = sigterm.recv() => {
                    tracing::info!("SIGTERM received, shutting down");
                    let _ = pty::send_sigterm(child_pid, cfg.kill_process_group);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    let _ = pty::send_sigkill(child_pid, cfg.kill_process_group);
                    exit_code = pty::wait_child(child_pid).unwrap_or(-1);
                    break;
                }
            }
        }

        // Mark as dead and broadcast exit
        alive.store(false, Ordering::Relaxed);
        output.set_dead();
        socket::broadcast_exit(&server_state, exit_code).await;

        // Brief delay for clients to receive the exit frame
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        tracing::info!(exit_code, "supervisor exiting");

        Ok::<i32, std::io::Error>(exit_code)
    })?;

    // Explicit cleanup before process::exit (which skips destructors).
    // The CleanupGuard is a safety net for panics and early `?` returns only.
    drop(_cleanup);
    std::process::exit(exit_code);
}

// ---- Attach subcommand ----

fn run_attach(id: String, socket_dir: PathBuf, _cfg: &config::Config) -> anyhow::Result<()> {
    let socket_path = socket_dir.join(format!("{id}.sock"));
    if !socket_path.exists() {
        eprintln!("No session found: {id}");
        eprintln!("Socket not found at {}", socket_path.display());
        std::process::exit(1);
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let stream = tokio::net::UnixStream::connect(&socket_path).await?;
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);

        // Read mode byte
        let mut mode = [0u8; 1];
        reader.read_exact(&mut mode).await?;
        assert_eq!(mode[0], protocol::MODE_BINARY, "expected binary mode");

        // Save terminal state and set raw mode
        let stdin_raw_fd = std::io::stdin().as_raw_fd();
        let stdin_borrowed = unsafe { BorrowedFd::borrow_raw(stdin_raw_fd) };
        let original_termios = termios::tcgetattr(stdin_borrowed)
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
        let mut raw = original_termios.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(stdin_borrowed, termios::SetArg::TCSANOW, &raw)
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;

        // Restore terminal on exit
        let _restore = RestoreTermios(stdin_raw_fd, original_termios);

        let mut stdout = tokio::io::stdout();

        // Set up status bar: reserve the bottom line via scroll region.
        let (cols, rows) = terminal_size();
        let inner_rows = rows.saturating_sub(1).max(1);
        setup_status_bar(&mut stdout, &id, cols, rows, None).await?;

        // Send RESIZE with inner_rows so the child sees the reduced height.
        let mut resize_payload = [0u8; 4];
        resize_payload[0..2].copy_from_slice(&cols.to_be_bytes());
        resize_payload[2..4].copy_from_slice(&inner_rows.to_be_bytes());
        protocol::write_frame(&mut write_half, protocol::RESIZE, &resize_payload).await?;

        // Now subscribe — scrollback replay happens at the correct size.
        protocol::write_frame(&mut write_half, protocol::SUBSCRIBE, &[]).await?;

        // Signal handlers
        let mut sigwinch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
        let mut sighup =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut sigint =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

        // Periodic status poll for the status bar (1 second).
        let mut status_tick = tokio::time::interval(std::time::Duration::from_secs(1));
        status_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Track current terminal dimensions for status bar redraws.
        let mut cur_cols = cols;
        let mut cur_rows = rows;

        // Async stdin reader
        let stdin = tokio::io::stdin();
        let mut stdin_reader = tokio::io::BufReader::new(stdin);
        let mut stdin_buf = [0u8; 1024];

        // Second socket for STATUS polling (the main socket is in SUBSCRIBE mode).
        let status_stream = tokio::net::UnixStream::connect(&socket_path).await?;
        let (status_read, mut status_write) = status_stream.into_split();
        let mut status_reader = tokio::io::BufReader::new(status_read);
        // Read mode byte.
        let mut smode = [0u8; 1];
        status_reader.read_exact(&mut smode).await?;

        loop {
            tokio::select! {
                // Socket -> stdout (pty output)
                result = protocol::read_frame(&mut reader) => {
                    let (msg_type, payload): (u8, Bytes) = result?;
                    match msg_type {
                        protocol::OUTPUT => {
                            stdout.write_all(&payload).await?;
                            stdout.flush().await?;
                        }
                        protocol::EXIT => {
                            let code = if payload.len() >= 4 {
                                i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                            } else {
                                0
                            };
                            // Reset scroll region before exiting
                            reset_scroll_region(&mut stdout).await?;
                            drop(_restore);
                            eprintln!("\r\n[session exited with code {code}]");
                            std::process::exit(code);
                        }
                        _ => {}
                    }
                }

                // stdin -> socket (user input)
                n = stdin_reader.read(&mut stdin_buf) => {
                    let n = n?;
                    if n == 0 {
                        break;
                    }
                    // Detach key: Ctrl-\ (0x1C)
                    if stdin_buf[..n].contains(&0x1C) {
                        reset_scroll_region(&mut stdout).await?;
                        drop(_restore);
                        eprintln!("\r\n[detached from session {id}]");
                        std::process::exit(0);
                    }
                    protocol::write_frame(&mut write_half, protocol::INPUT, &stdin_buf[..n]).await?;
                }

                // SIGWINCH -> resize
                _ = sigwinch.recv() => {
                    let (new_cols, new_rows) = terminal_size();
                    cur_cols = new_cols;
                    cur_rows = new_rows;
                    let inner_rows = new_rows.saturating_sub(1).max(1);
                    setup_status_bar(&mut stdout, &id, new_cols, new_rows, None).await?;

                    let mut payload = [0u8; 4];
                    payload[0..2].copy_from_slice(&new_cols.to_be_bytes());
                    payload[2..4].copy_from_slice(&inner_rows.to_be_bytes());
                    protocol::write_frame(&mut write_half, protocol::RESIZE, &payload).await?;
                }

                // Periodic status poll -> update status bar
                _ = status_tick.tick() => {
                    // Send STATUS request on the dedicated connection.
                    if protocol::write_frame(&mut status_write, protocol::STATUS, &[]).await.is_ok()
                        && let Ok((msg_type, payload)) = protocol::read_frame(&mut status_reader).await
                        && msg_type == protocol::STATUS_RESP
                        && payload.len() >= 14
                    {
                        let state_byte = payload[9];
                        let state_ms = u32::from_be_bytes([
                            payload[10], payload[11], payload[12], payload[13],
                        ]);
                        let info = StatusInfo { state_byte, state_ms };
                        draw_status_bar(&mut stdout, &id, cur_cols, cur_rows, Some(&info)).await?;
                    }
                }

                // SIGHUP/SIGTERM/SIGINT — terminal closed, killed, or interrupted.
                _ = sighup.recv() => {
                    // Terminal is gone (X close) — can't write to stdout.
                    // Just restore termios and exit.
                    drop(_restore);
                    std::process::exit(0);
                }
                _ = sigterm.recv() => {
                    let _ = reset_scroll_region(&mut stdout).await;
                    drop(_restore);
                    eprintln!("\r\n[terminated]");
                    std::process::exit(0);
                }
                _ = sigint.recv() => {
                    // Forward Ctrl-C to the session instead of exiting.
                    protocol::write_frame(&mut write_half, protocol::INPUT, &[0x03]).await?;
                }
            }
        }

        // Clean up: reset scroll region
        reset_scroll_region(&mut stdout).await?;

        Ok::<(), std::io::Error>(())
    })?;

    Ok(())
}

/// Info from a STATUS_RESP used to render the right side of the bar.
struct StatusInfo {
    state_byte: u8,
    state_ms: u32,
}

/// Set up the scroll region, alt screen, and draw the initial status bar.
async fn setup_status_bar(
    stdout: &mut tokio::io::Stdout,
    session_id: &str,
    cols: u16,
    rows: u16,
    info: Option<&StatusInfo>,
) -> std::io::Result<()> {
    let inner_rows = rows.saturating_sub(1).max(1);

    // Switch to alternate screen buffer, clear, home cursor, set scroll region.
    let setup = format!("\x1b[?1049h\x1b[2J\x1b[H\x1b[1;{inner_rows}r");
    stdout.write_all(setup.as_bytes()).await?;

    draw_status_bar(stdout, session_id, cols, rows, info).await
}

/// Draw (or redraw) the status bar on the last line.
///
/// Layout:
///   Left  (green bg):  [hm] session-id
///   Right (state color): state-name duration
///   Middle: dark fill
async fn draw_status_bar(
    stdout: &mut tokio::io::Stdout,
    session_id: &str,
    cols: u16,
    rows: u16,
    info: Option<&StatusInfo>,
) -> std::io::Result<()> {
    // Left segment: green background, black text.
    let left = format!(" [hm] {session_id} ");

    // Right segment: state with colored background.
    let (state_name, state_color) = match info {
        Some(si) => match si.state_byte {
            0x00 => ("idle", "\x1b[42;30m"),      // green bg
            0x01 => ("thinking", "\x1b[43;30m"),  // yellow bg
            0x02 => ("streaming", "\x1b[44;37m"), // blue bg
            0x03 => ("tool_use", "\x1b[45;37m"),  // magenta bg
            0x04 => ("active", "\x1b[46;30m"),    // cyan bg
            0xFF => ("dead", "\x1b[41;37m"),      // red bg
            _ => ("unknown", "\x1b[47;30m"),      // white bg
        },
        None => ("...", "\x1b[100;37m"), // gray, waiting for first poll
    };

    let duration = info.map_or(String::new(), |si| {
        let secs = si.state_ms / 1000;
        if secs >= 60 {
            format!(" {}m{}s ", secs / 60, secs % 60)
        } else {
            format!(" {}s ", secs)
        }
    });

    let right = format!(" {state_name}{duration}");
    let right_visible_len = right.len();
    let left_visible_len = left.len();

    // Middle fill: dark background.
    let fill_len = (cols as usize).saturating_sub(left_visible_len + right_visible_len);
    let fill = " ".repeat(fill_len);

    // Compose: save cursor, jump to last line, draw segments, restore cursor.
    // \x1b[42;30m = green bg + black fg (left)
    // \x1b[0m\x1b[48;5;236m = reset then dark gray bg (middle)
    // state_color (right)
    // \x1b[0m = reset
    let bar = format!(
        "\x1b7\x1b[{rows};1H\x1b[42;30m{left}\x1b[0m\x1b[48;5;236;37m{fill}\x1b[0m{state_color}{right}\x1b[0m\x1b8"
    );

    stdout.write_all(bar.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

/// Reset scroll region and switch back to the main screen buffer.
async fn reset_scroll_region(stdout: &mut tokio::io::Stdout) -> std::io::Result<()> {
    // Reset scroll region, then leave alternate screen buffer.
    // The original terminal content is restored (like exiting vim/tmux).
    stdout.write_all(b"\x1b[r\x1b[?1049l").await?;
    stdout.flush().await?;
    Ok(())
}

/// RAII guard to restore terminal settings on drop.
struct RestoreTermios(i32, termios::Termios);

impl Drop for RestoreTermios {
    fn drop(&mut self) {
        let fd = unsafe { BorrowedFd::borrow_raw(self.0) };
        let _ = termios::tcsetattr(fd, termios::SetArg::TCSANOW, &self.1);
    }
}

/// Get current terminal size via ioctl.
fn terminal_size() -> (u16, u16) {
    unsafe {
        let mut ws: nix::libc::winsize = std::mem::zeroed();
        if nix::libc::ioctl(std::io::stdin().as_raw_fd(), nix::libc::TIOCGWINSZ, &mut ws) == 0 {
            (ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }
}

// ---- Status subcommand ----

fn run_status(id: String, socket_dir: PathBuf, cfg: &config::Config) -> anyhow::Result<()> {
    let socket_path = socket_dir.join(format!("{id}.sock"));
    if !socket_path.exists() {
        eprintln!("No session found: {id}");
        std::process::exit(1);
    }

    let classifier_config = cfg.classifier.clone();
    let idle_threshold = cfg.idle_threshold_ms;
    let debounce = cfg.debounce_ms;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let stream = tokio::net::UnixStream::connect(&socket_path).await?;
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);

        let mut mode = [0u8; 1];
        reader.read_exact(&mut mode).await?;

        protocol::write_frame(&mut write_half, protocol::STATUS, &[]).await?;

        let (msg_type, payload) = protocol::read_frame(&mut reader).await?;
        if msg_type == protocol::STATUS_RESP && payload.len() >= 9 {
            let pid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            let idle_ms = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
            let alive = payload[8] != 0;
            println!("session:  {id}");
            println!("pid:      {pid}");
            println!("idle_ms:  {idle_ms}");
            println!("alive:    {alive}");

            // Use the configured classifier for state name resolution.
            let cls = classify::from_config(&classifier_config, idle_threshold, debounce);

            if payload.len() >= 15 {
                let state_byte = payload[9];
                let state_ms =
                    u32::from_be_bytes([payload[10], payload[11], payload[12], payload[13]]);
                let state_name = cls.state_name(state_byte);
                println!("state:    {state_name} ({state_ms}ms)");
            } else if idle_ms > 3000 {
                println!("state:    idle");
            } else {
                println!("state:    active");
            }
        } else {
            eprintln!("unexpected response");
        }

        Ok::<(), std::io::Error>(())
    })?;

    Ok(())
}

// ---- List subcommand ----

fn run_list(socket_dir: PathBuf) -> anyhow::Result<()> {
    if !socket_dir.exists() {
        println!("No sessions directory found at {}", socket_dir.display());
        return Ok(());
    }

    let mut sessions: Vec<String> = std::fs::read_dir(&socket_dir)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "sock") {
                path.file_stem().and_then(|s| s.to_str()).map(String::from)
            } else {
                None
            }
        })
        .collect();

    sessions.sort();

    // Filter to live sessions: check PID file liveness, clean up stale entries.
    let mut live = Vec::new();
    for id in &sessions {
        let pid_path = socket_dir.join(format!("{id}.pid"));
        let socket_path = socket_dir.join(format!("{id}.sock"));

        let is_alive = pid_path
            .exists()
            .then(|| std::fs::read_to_string(&pid_path).ok())
            .flatten()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .is_some_and(|pid| unsafe { nix::libc::kill(pid, 0) } == 0);

        if is_alive {
            live.push(id.clone());
        } else {
            // Clean up stale socket + PID.
            let _ = std::fs::remove_file(&socket_path);
            let _ = std::fs::remove_file(&pid_path);
        }
    }

    if live.is_empty() {
        println!("No active sessions");
    } else {
        println!("Active sessions:");
        for id in &live {
            println!("  {id}");
        }
    }

    Ok(())
}

// ---- Kill subcommand ----

fn run_kill(id: String, socket_dir: PathBuf) -> anyhow::Result<()> {
    let socket_path = socket_dir.join(format!("{id}.sock"));
    if !socket_path.exists() {
        eprintln!("No session found: {id}");
        std::process::exit(1);
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let stream = tokio::net::UnixStream::connect(&socket_path).await?;
        let (_read_half, mut write_half) = stream.into_split();

        let mut mode = [0u8; 1];
        let mut reader = tokio::io::BufReader::new(_read_half);
        reader.read_exact(&mut mode).await?;

        protocol::write_frame(&mut write_half, protocol::KILL, &[]).await?;
        println!("Kill signal sent to session {id}");

        Ok::<(), std::io::Error>(())
    })?;

    Ok(())
}
