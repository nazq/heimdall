//! Attach subcommand: terminal passthrough with status bar.

use crate::cli::SessionParams;
use crate::config;
use crate::protocol;
use crate::terminal::{
    RestoreTermios, StatusInfo, draw_status_bar, reset_scroll_region, resize_status_bar,
    setup_status_bar, terminal_size,
};
use crate::util;
use bytes::Bytes;
use nix::sys::termios;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::process::CommandExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Num of seconds to wait for the supervisor socket to appear before giving up.
const SOCKET_DEADLINE_SECS: u64 = 5;
const SOCKET_DEADLINE: std::time::Duration = std::time::Duration::from_secs(SOCKET_DEADLINE_SECS);

// Interval for polling supervisor socket before giving up in SOCKET_DEADLINE.
const SOCKET_POLL_INTERVAL_MS: u64 = 5;
const SOCKET_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(SOCKET_POLL_INTERVAL_MS);

// How often to poll the supervisor for status bar updates.
const STATUS_POLL_INTERVAL_MS: u64 = 1000;
const STATUS_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(STATUS_POLL_INTERVAL_MS);

/// Launch the supervisor as a background process and attach to it.
///
/// Spawns `hm run --detach` as a child process, waits for the socket to
/// appear, then runs the normal attach flow. When the attach disconnects
/// (Ctrl-\ or session exit), the supervisor keeps running in the background.
pub fn launch_and_attach(params: SessionParams) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let child_args = params.to_detach_args()?;

    // Spawn supervisor in background with setsid() for terminal independence.
    let _supervisor = unsafe {
        std::process::Command::new(exe)
            .args(&child_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .pre_exec(|| {
                nix::unistd::setsid().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                Ok(())
            })
            .spawn()?
    };

    // Wait for the socket to appear.
    let socket_path = crate::util::socket_path(&params.socket_dir, &params.id);
    let deadline = std::time::Instant::now() + SOCKET_DEADLINE;
    while !socket_path.exists() {
        if std::time::Instant::now() > deadline {
            anyhow::bail!(
                "Timed out waiting for supervisor socket at {}",
                socket_path.display()
            );
        }
        // No tokio runtime yet, so just sleep the thread. The supervisor
        // should be up and creating the socket within a few milliseconds.
        std::thread::sleep(SOCKET_POLL_INTERVAL);
    }

    attach(params.id, params.socket_dir, &params.cfg)
}

pub fn attach(
    id: String,
    socket_dir: std::path::PathBuf,
    cfg: &config::Config,
) -> anyhow::Result<()> {
    let detach_key = cfg.detach_key;
    let socket_path = util::session_socket(&id, &socket_dir);

    util::with_runtime(async move {
        let mut sess = protocol::Session::connect(&socket_path).await?;

        // Save terminal state and set raw mode
        let stdin_raw_fd = std::io::stdin().as_raw_fd();
        let stdin_borrowed = unsafe { BorrowedFd::borrow_raw(stdin_raw_fd) };
        let original_termios = termios::tcgetattr(stdin_borrowed)
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
        let mut raw = original_termios.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(stdin_borrowed, termios::SetArg::TCSANOW, &raw)
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;

        let _restore = RestoreTermios {
            fd: stdin_raw_fd,
            original: original_termios,
        };

        let mut stdout = tokio::io::stdout();

        // Set up status bar: reserve the bottom line via scroll region.
        let (cols, rows) = terminal_size()?;
        let inner_rows = setup_status_bar(&mut stdout, &id, cols, rows, None).await?;

        // Send RESIZE with inner_rows so the child sees the reduced height,
        // then subscribe — order matters so scrollback replays at the right size.
        sess.send_resize(cols, inner_rows).await?;
        sess.subscribe().await?;

        // Destructure into raw halves for the select loop (borrow checker
        // requires independent borrows of reader and writer across arms).
        let protocol::Session {
            reader: mut main_reader,
            writer: mut main_writer,
        } = sess;

        // Signal handlers
        let mut sigwinch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

        // Periodic status poll for the status bar.
        let mut status_tick = tokio::time::interval(STATUS_POLL_INTERVAL);
        status_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut cur_cols = cols;
        let mut cur_rows = rows;

        // Async stdin reader
        let stdin = tokio::io::stdin();
        let mut stdin_reader = tokio::io::BufReader::new(stdin);
        let mut stdin_buf = [0u8; 1024];

        // Second socket for STATUS polling (the main socket is in SUBSCRIBE mode).
        let mut status_sess = protocol::Session::connect(&socket_path).await?;

        loop {
            tokio::select! {
                // Supervisor → client: child pty output or session exit notification.
                result = protocol::read_frame(&mut main_reader) => {
                    let (msg_type, payload): (u8, Bytes) = result?;
                    match msg_type {
                        protocol::OUTPUT => {
                            // Write raw child output to the terminal.
                            stdout.write_all(&payload).await?;
                            stdout.flush().await?;
                        }
                        protocol::EXIT => {
                            let code = protocol::parse_exit_code(&payload)?;
                            reset_scroll_region(&mut stdout).await?;
                            drop(_restore); // terminal back to cooked mode
                            eprintln!("[session exited with code {code}]");
                            std::process::exit(code);
                        }
                        _ => {}
                    }
                }

                // Client → supervisor: User input available on stdin — check for detach key, then forward to supervisor.
                n = stdin_reader.read(&mut stdin_buf) => {
                    let n = n?;
                    if n == 0 {
                        break;
                    }
                    // Detach key (default: Ctrl-\, configurable via detach_key).
                    // Only trigger on a lone keypress — ignore detach bytes buried in pastes.
                    if detach_key != 0 && n == 1 && stdin_buf[0] == detach_key {
                        reset_scroll_region(&mut stdout).await?;
                        drop(_restore); // terminal back to cooked mode
                        eprintln!("[detached from session {id}]");
                        std::process::exit(0);
                    }
                    protocol::write_frame(&mut main_writer, protocol::INPUT, &stdin_buf[..n]).await?;

                }

                // Terminal resized — update scroll region, status bar, and notify supervisor.
                _ = sigwinch.recv() => {
                    let (new_cols, new_rows) = terminal_size()?;
                    cur_cols = new_cols;
                    cur_rows = new_rows;
                    let inner_rows = resize_status_bar(&mut stdout, &id, new_cols, new_rows, None).await?;

                    protocol::write_frame(&mut main_writer, protocol::RESIZE, &protocol::pack_resize(new_cols, inner_rows)).await?;
                }

                // Periodic status poll (STATUS_POLL_INTERVAL) to refresh the status bar.
                _ = status_tick.tick() => {
                    if let Ok(status) = status_sess.recv_status().await {
                        let info = StatusInfo {
                            state_byte: status.state,
                            state_ms: status.state_ms,
                        };
                        draw_status_bar(&mut stdout, &id, cur_cols, cur_rows, Some(&info)).await?;
                    }
                }

                // Terminal gone (SSH disconnect, window closed) — nothing to clean up visually.
                _ = sighup.recv() => {
                    drop(_restore);
                    std::process::exit(0);
                }
                // Explicit kill of the attach client — terminal still exists, reset it.
                _ = sigterm.recv() => {
                    let _ = reset_scroll_region(&mut stdout).await;
                    drop(_restore); // terminal back to cooked mode
                    eprintln!("[terminated]");
                    std::process::exit(0);
                }
                // Raw mode swallows Ctrl-C; forward it as input to the child.
                _ = sigint.recv() => {
                    protocol::write_frame(&mut main_writer, protocol::INPUT, &[0x03]).await?;
                }
            }
        }

        reset_scroll_region(&mut stdout).await?;

        Ok(())
    })
}
