//! Unix socket listener and per-client connection handler.

use crate::broadcast::OutputState;
use crate::protocol::{self, INPUT, KILL, OUTPUT, RESIZE, STATUS, SUBSCRIBE};
use nix::unistd::Pid;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

/// Shared state passed to each client handler.
pub struct ServerState {
    pub output: Arc<OutputState>,
    pub child_pid: Pid,
    pub master_fd: i32,
    pub alive: Arc<AtomicBool>,
    /// Exit code set by the supervisor before dropping the broadcast sender.
    pub exit_code: AtomicI32,
    /// Whether kill signals target the process group or just the child.
    pub kill_process_group: bool,
}

/// Accept client connections and spawn handlers.
pub async fn serve(listener: UnixListener, state: Arc<ServerState>) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, &state).await {
                        tracing::debug!("client disconnected: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::error!("accept error: {e}");
            }
        }
    }
}

/// Handle a single client connection.
async fn handle_client(stream: UnixStream, state: &ServerState) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Send mode byte: binary framing.
    write_half.write_all(&[protocol::MODE_BINARY]).await?;

    // Read frames in a loop until the client disconnects or subscribes.
    loop {
        let (msg_type, payload) = protocol::read_frame(&mut reader).await?;

        match msg_type {
            INPUT => {
                write_to_pty(state, payload.to_vec()).await?;
            }
            SUBSCRIBE => {
                handle_subscribed(state, reader, &mut write_half).await?;
                return Ok(());
            }
            STATUS => {
                send_status(state, &mut write_half).await?;
            }
            RESIZE => {
                handle_resize(state, &payload)?;
            }
            KILL => {
                let _ = crate::pty::send_sigterm(state.child_pid, state.kill_process_group);
            }
            _ => {
                tracing::warn!("unknown message type: 0x{msg_type:02x}");
            }
        }
    }
}

/// After SUBSCRIBE: stream output to client while still accepting input/resize frames.
async fn handle_subscribed(
    state: &ServerState,
    mut reader: BufReader<OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
) -> std::io::Result<()> {
    // Send scrollback first
    let snapshot = state.output.scrollback_snapshot();
    for chunk in snapshot {
        protocol::write_frame(writer, OUTPUT, &chunk).await?;
    }

    // Subscribe to live broadcast
    let mut rx = state.output.tx.subscribe();

    // Concurrent loop: broadcast output + client frames
    loop {
        tokio::select! {
            // Broadcast -> client (pty output)
            result = rx.recv() => {
                match result {
                    Ok(chunk) => {
                        if !state.alive.load(Ordering::Relaxed) {
                            // After alive=false, the only broadcast is the EXIT frame.
                            // Write it directly (it's already fully framed).
                            writer.write_all(&chunk).await?;
                            return Ok(());
                        }
                        protocol::write_frame(writer, OUTPUT, &chunk).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("subscriber lagged, dropped {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Channel closed — send EXIT frame if we haven't already.
                        let code = state.exit_code.load(Ordering::Relaxed);
                        if !state.alive.load(Ordering::Relaxed) {
                            let exit_frame = protocol::pack_exit(code);
                            let _ = writer.write_all(&exit_frame).await;
                        }
                        return Ok(());
                    }
                }
            }

            // Client -> supervisor (input, resize, etc.)
            result = protocol::read_frame(&mut reader) => {
                let (msg_type, payload) = result?;
                match msg_type {
                    INPUT => {
                        write_to_pty(state, payload.to_vec()).await?;
                    }
                    RESIZE => {
                        handle_resize(state, &payload)?;
                    }
                    KILL => {
                        let _ = crate::pty::send_sigterm(state.child_pid, state.kill_process_group);
                    }
                    STATUS => {
                        send_status(state, writer).await?;
                    }
                    _ => {
                        tracing::warn!("unknown message type in subscribed mode: 0x{msg_type:02x}");
                    }
                }
            }
        }
    }
}

/// Write raw bytes to the pty master fd.
/// Checks `alive` before writing to avoid writing to a closed/reused fd.
async fn write_to_pty(state: &ServerState, data: Vec<u8>) -> std::io::Result<()> {
    if !state.alive.load(Ordering::Relaxed) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "child process has exited",
        ));
    }
    let fd = state.master_fd;
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let ret = unsafe { nix::libc::write(fd, data.as_ptr().cast(), data.len()) };
        if ret < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Send a STATUS response with process state.
async fn send_status(state: &ServerState, writer: &mut OwnedWriteHalf) -> std::io::Result<()> {
    let pid = state.child_pid.as_raw() as u32;
    let idle_ms = state.output.idle_ms();
    let alive = state.alive.load(Ordering::Relaxed);
    let process_state = state.output.process_state() as u8;
    let state_ms = state.output.state_ms();
    let frame = protocol::pack_status(pid, idle_ms, alive, process_state, state_ms);
    writer.write_all(&frame).await
}

/// Handle a RESIZE frame.
fn handle_resize(state: &ServerState, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() >= 4 {
        let cols = u16::from_be_bytes([payload[0], payload[1]]);
        let rows = u16::from_be_bytes([payload[2], payload[3]]);
        crate::pty::set_winsize_raw(state.master_fd, cols, rows)?;
        let _ = crate::pty::send_sigwinch(state.child_pid);
    }
    Ok(())
}

/// Broadcast an exit notification to all subscribers.
///
/// Stores the exit code and sends the fully-framed EXIT message through the
/// broadcast channel. Subscribers write it directly (not wrapped in OUTPUT).
pub async fn broadcast_exit(state: &Arc<ServerState>, exit_code: i32) {
    state.exit_code.store(exit_code, Ordering::Relaxed);
    let _ = state.output.tx.send(protocol::pack_exit(exit_code));
}
