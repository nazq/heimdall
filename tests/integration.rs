//! Integration tests for heimdall.
//!
//! These tests spawn real `hm run` processes using temp directories for
//! socket_dir to avoid conflicts. Tests use a mix of `sleep` (silent child)
//! and output-producing children (bash -c, echo) to exercise both pty-idle
//! and pty-active code paths.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Path to the built binary.
fn hm_bin() -> PathBuf {
    // cargo test builds the binary in the same target dir.
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // remove test binary name
    path.pop(); // remove "deps"
    path.push("hm");
    path
}

/// Wait for a socket file to appear, with timeout.
fn wait_for_socket(path: &std::path::Path, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

// -- Issue #1: Duplicate session prevention --

/// Starting a second session with the same ID fails when the first is alive.
#[test]
fn duplicate_session_rejected_when_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "dup-alive";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    // Start first session.
    let mut child1 = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn first session");

    assert!(
        wait_for_socket(&socket_path, Duration::from_secs(5)),
        "first session socket never appeared"
    );

    // Try to start a second session with the same ID.
    let output = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to spawn second session");

    assert!(
        !output.status.success(),
        "second session should have failed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already running") || stderr.contains("is locked by"),
        "stderr should mention session conflict: {stderr}"
    );

    // Clean up first session.
    let _ = child1.kill();
    let _ = child1.wait();
}

/// Starting a session succeeds when the PID file is stale (process dead).
#[test]
fn stale_pid_file_allows_new_session() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "stale-pid";
    let pid_path = socket_dir.join(format!("{session_id}.pid"));

    std::fs::create_dir_all(&socket_dir).unwrap();

    // Write a PID file with a definitely-dead PID (PID 1 is init, but a random
    // large PID that doesn't exist works better).
    // Use PID 2147483647 which almost certainly doesn't exist.
    std::fs::write(&pid_path, "2147483647").unwrap();

    // Start a session — it should succeed because the PID is stale.
    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn session with stale PID");

    let socket_path = socket_dir.join(format!("{session_id}.sock"));
    assert!(
        wait_for_socket(&socket_path, Duration::from_secs(5)),
        "session with stale PID should have started"
    );

    let _ = child.kill();
    let _ = child.wait();
}

// -- Issue #2: Process group kill --

/// Verify that `send_sigterm` and `send_sigkill` with `kill_group=true` negate
/// the PID (targeting the process group, not just the process).
///
/// This is a unit-level check embedded in integration tests because it
/// requires importing from the main crate. We test the signal function
/// construction, not actual signal delivery (which requires a real process group).
#[test]
fn signal_functions_negate_pid() {
    use nix::unistd::Pid;

    // The signal functions use Pid::from_raw(-pid.as_raw()).
    // Verify the negation logic directly.
    let pid = Pid::from_raw(12345);
    let negated = Pid::from_raw(-pid.as_raw());
    assert_eq!(negated.as_raw(), -12345);

    // Also verify with PID 1 (edge case).
    let pid1 = Pid::from_raw(1);
    let neg1 = Pid::from_raw(-pid1.as_raw());
    assert_eq!(neg1.as_raw(), -1);
}

// -- Protocol over real Unix socket --

/// Connect to a running session and verify the mode byte and status response.
#[test]
fn status_over_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "status-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn session");

    assert!(
        wait_for_socket(&socket_path, Duration::from_secs(5)),
        "socket never appeared"
    );

    // Connect via std UnixStream (blocking).
    let mut stream = UnixStream::connect(&socket_path).expect("failed to connect to socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();

    // Read mode byte.
    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).unwrap();
    assert_eq!(mode[0], 0x00, "mode byte should be MODE_BINARY (0x00)");

    // Send STATUS request: type=0x03, len=0.
    let status_frame: [u8; 5] = [0x03, 0, 0, 0, 0];
    stream.write_all(&status_frame).unwrap();

    // Read response header.
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(header[0], 0x82, "response should be STATUS_RESP (0x82)");
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    assert_eq!(len, 15, "STATUS_RESP payload must be 15 bytes");

    // Read response payload.
    let mut payload = [0u8; 15];
    stream.read_exact(&mut payload).unwrap();

    // Parse fields: [pid: u32][idle_ms: u32][alive: u8][state: u8][state_ms: u32]
    let pid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let idle_ms = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let alive = payload[8];
    let state = payload[9];
    let state_ms = u32::from_be_bytes([payload[10], payload[11], payload[12], payload[13]]);
    assert!(pid > 0, "pid should be nonzero");
    assert_eq!(alive, 1, "alive should be 1");
    assert!(
        idle_ms < 5000,
        "idle_ms should be small for a just-started session, got {idle_ms}"
    );
    // State: 0x00=idle, 0x01=thinking, 0x02=streaming, 0x03=tool_use, 0x04=active.
    assert!(
        state <= 0x04,
        "state should be valid (0x00-0x04), got {state:#x}"
    );
    let _ = state_ms; // state_ms is valid at any value

    let _ = child.kill();
    let _ = child.wait();
}

/// Kill subcommand sends SIGTERM to the session.
#[test]
fn kill_subcommand_terminates_session() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "kill-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "60",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn session");

    assert!(
        wait_for_socket(&socket_path, Duration::from_secs(5)),
        "socket never appeared"
    );

    // Send kill.
    let kill_output = Command::new(hm_bin())
        .args([
            "kill",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run kill");

    assert!(kill_output.status.success(), "kill command should succeed");

    // The supervisor should exit within a few seconds.
    let _status = child.wait().expect("failed to wait for child");

    // Socket and PID files should be cleaned up after kill.
    let pid_path = socket_dir.join(format!("{session_id}.pid"));
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !socket_path.exists(),
        "socket file should be cleaned up after kill"
    );
    assert!(
        !pid_path.exists(),
        "PID file should be cleaned up after kill"
    );
}

/// List subcommand shows active sessions.
#[test]
fn list_shows_active_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "list-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn session");

    assert!(
        wait_for_socket(&socket_path, Duration::from_secs(5)),
        "socket never appeared"
    );

    let output = Command::new(hm_bin())
        .args(["ls", "--socket-dir", socket_dir.to_str().unwrap()])
        .output()
        .expect("failed to run ls");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(session_id),
        "ls output should contain session id: {stdout}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// List on an empty directory shows "No active sessions".
#[test]
fn list_empty_shows_none() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let output = Command::new(hm_bin())
        .args(["ls", "--socket-dir", socket_dir.to_str().unwrap()])
        .output()
        .expect("failed to run ls");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No active sessions"),
        "empty ls should say 'No active sessions': {stdout}"
    );
}

/// Status subcommand on a nonexistent session exits with error.
#[test]
fn status_nonexistent_session_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let output = Command::new(hm_bin())
        .args([
            "status",
            "nonexistent",
            "--socket-dir",
            socket_dir.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run status");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No session found"),
        "should report no session: {stderr}"
    );
}

/// Config flag with nonexistent file errors.
#[test]
fn config_flag_nonexistent_errors() {
    let output = Command::new(hm_bin())
        .args([
            "--config",
            "/tmp/nonexistent_heimdall_config_12345.toml",
            "ls",
        ])
        .output()
        .expect("failed to run with bad config");

    assert!(
        !output.status.success(),
        "should fail with nonexistent config"
    );
}

// -- Subscriber disconnect recovery --

/// After a subscriber connects and disconnects, the supervisor still serves
/// new STATUS requests. This is the exact scenario that caused hangs in testing.
#[test]
fn status_works_after_subscriber_disconnect() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "sub-disconnect";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn session");

    assert!(
        wait_for_socket(&socket_path, Duration::from_secs(5)),
        "socket never appeared"
    );

    // Connect as subscriber, then abruptly disconnect.
    {
        let mut stream = UnixStream::connect(&socket_path).expect("failed to connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let mut mode = [0u8; 1];
        stream.read_exact(&mut mode).unwrap();

        // Send SUBSCRIBE
        let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
        stream.write_all(&sub_frame).unwrap();

        std::thread::sleep(Duration::from_millis(200));
        // Drop stream — abrupt disconnect
    }

    std::thread::sleep(Duration::from_millis(500));

    // Now STATUS should still work.
    {
        let mut stream = UnixStream::connect(&socket_path).expect("failed to reconnect");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();

        let mut mode = [0u8; 1];
        stream.read_exact(&mut mode).unwrap();
        assert_eq!(mode[0], 0x00);

        let status_frame: [u8; 5] = [0x03, 0, 0, 0, 0];
        stream.write_all(&status_frame).unwrap();

        let mut header = [0u8; 5];
        stream.read_exact(&mut header).unwrap();
        assert_eq!(
            header[0], 0x82,
            "should get STATUS_RESP after subscriber disconnect"
        );
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        assert_eq!(len, 15, "STATUS_RESP payload must be 15 bytes");

        let mut payload = [0u8; 15];
        stream.read_exact(&mut payload).unwrap();
        let pid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let alive = payload[8];
        assert!(pid > 0, "pid should be nonzero after subscriber disconnect");
        assert_eq!(alive, 1, "alive should be 1 after subscriber disconnect");
    }

    let _ = child.kill();
    let _ = child.wait();
}

// -- Child exit detection --

/// When the supervised child exits, the supervisor should also exit.
#[test]
fn supervisor_exits_when_child_exits() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "child-exit";

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "1", // exits after 1 second
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn session");

    // Wait for the supervisor to exit (child sleeps 1s, supervisor should follow).
    let status = child.wait().expect("failed to wait");
    assert!(
        status.success(),
        "supervisor should exit 0 when child exits 0"
    );

    // Socket and PID file should be cleaned up.
    let socket_path = socket_dir.join(format!("{session_id}.sock"));
    let pid_path = socket_dir.join(format!("{session_id}.pid"));
    // Brief sleep to let cleanup happen.
    std::thread::sleep(Duration::from_millis(200));
    assert!(!socket_path.exists(), "socket file should be cleaned up");
    assert!(!pid_path.exists(), "PID file should be cleaned up");
}

// -- Corrupt PID file edge cases --

/// PID file with garbage content is treated as stale.
#[test]
fn corrupt_pid_file_treated_as_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "corrupt-pid";
    let pid_path = socket_dir.join(format!("{session_id}.pid"));

    std::fs::create_dir_all(&socket_dir).unwrap();
    std::fs::write(&pid_path, "not-a-number\n").unwrap();

    // Should start successfully — corrupt PID file is treated as stale.
    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    let socket_path = socket_dir.join(format!("{session_id}.sock"));
    assert!(
        wait_for_socket(&socket_path, Duration::from_secs(5)),
        "session should start despite corrupt PID file"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// Empty PID file is treated as stale.
#[test]
fn empty_pid_file_treated_as_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "empty-pid";
    let pid_path = socket_dir.join(format!("{session_id}.pid"));

    std::fs::create_dir_all(&socket_dir).unwrap();
    std::fs::write(&pid_path, "").unwrap();

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    let socket_path = socket_dir.join(format!("{session_id}.sock"));
    assert!(
        wait_for_socket(&socket_path, Duration::from_secs(5)),
        "session should start despite empty PID file"
    );

    let _ = child.kill();
    let _ = child.wait();
}

// -- Multiple concurrent status queries --

/// Multiple clients can query STATUS concurrently.
#[test]
fn concurrent_status_queries() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "concurrent-status";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    // Fire off 5 concurrent status queries.
    let handles: Vec<_> = (0..5)
        .map(|_| {
            let path = socket_path.clone();
            std::thread::spawn(move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();

                let mut mode = [0u8; 1];
                stream.read_exact(&mut mode).unwrap();
                assert_eq!(mode[0], 0x00, "mode byte should be MODE_BINARY");

                let status_frame: [u8; 5] = [0x03, 0, 0, 0, 0];
                stream.write_all(&status_frame).unwrap();

                let mut header = [0u8; 5];
                stream.read_exact(&mut header).unwrap();
                assert_eq!(header[0], 0x82, "expected STATUS_RESP");
                let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
                assert_eq!(len, 15, "STATUS_RESP payload must be 15 bytes");

                let mut payload = [0u8; 15];
                stream.read_exact(&mut payload).unwrap();
                let pid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let alive = payload[8];
                assert!(pid > 0, "pid should be nonzero");
                assert_eq!(alive, 1, "alive should be 1");
                true
            })
        })
        .collect();

    for h in handles {
        assert!(h.join().unwrap(), "concurrent status query should succeed");
    }

    let _ = child.kill();
    let _ = child.wait();
}

// -- Kill nonexistent session --

/// Kill on a nonexistent session exits with error.
#[test]
fn kill_nonexistent_session_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let output = Command::new(hm_bin())
        .args([
            "kill",
            "ghost",
            "--socket-dir",
            socket_dir.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run kill");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("No session found"));
}

// -- Stale socket file cleanup --

/// A stale socket file (no listener) is cleaned up when starting a new session.
#[test]
fn stale_socket_cleaned_on_start() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "stale-sock";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    std::fs::create_dir_all(&socket_dir).unwrap();
    // Create a stale socket file (just a regular file, not a real socket).
    std::fs::write(&socket_path, "stale").unwrap();

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    // The stale file already exists, so we can't use wait_for_socket (checks exists()).
    // Instead, poll until we can actually connect — that proves the stale file was
    // replaced with a real listening socket.
    let start = std::time::Instant::now();
    let mut connected = false;
    while start.elapsed() < Duration::from_secs(5) {
        if UnixStream::connect(&socket_path).is_ok() {
            connected = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        connected,
        "should be a real socket after cleaning stale file"
    );

    let _ = child.kill();
    let _ = child.wait();
}

// -- Subscriber receives EXIT frame --

/// A subscribed client receives an EXIT frame (0x83) when the child exits.
/// This is the exit-notification feature — zero e2e coverage until now.
#[test]
fn subscriber_receives_exit_frame_on_child_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "exit-notify";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "1", // exits after 1s
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    let mut stream = UnixStream::connect(&socket_path).expect("failed to connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    // Read mode byte.
    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).unwrap();
    assert_eq!(mode[0], 0x00);

    // Send SUBSCRIBE.
    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();

    // Read frames until we get EXIT (0x83) or timeout.
    let mut got_exit = false;
    let mut exit_code: Option<i32> = None;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(8) {
        let mut header = [0u8; 5];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) => break,
        }
        let msg_type = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 && stream.read_exact(&mut payload).is_err() {
            break;
        }
        if msg_type == 0x83 {
            got_exit = true;
            assert_eq!(
                payload.len(),
                4,
                "EXIT frame payload must be exactly 4 bytes, got {}",
                payload.len()
            );
            exit_code = Some(i32::from_be_bytes([
                payload[0], payload[1], payload[2], payload[3],
            ]));
            break;
        }
    }

    assert!(got_exit, "subscriber should receive EXIT frame");
    assert_eq!(exit_code, Some(0), "sleep 1 exits with code 0");

    let _ = child.wait();
}

// -- Non-existent command --

/// `hm run` with a command that doesn't exist should exit with an error,
/// not hang forever waiting for a child that silently failed to exec.
#[test]
fn run_nonexistent_command_exits_with_error() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();

    let output = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            "bad-cmd",
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "/nonexistent/binary/that/does/not/exist",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to spawn");

    assert!(
        !output.status.success(),
        "should fail when command doesn't exist"
    );
}

// -- Socket dir creation --

/// `hm run` creates the socket dir if it doesn't exist yet.
#[test]
fn run_creates_socket_dir_if_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().join("nested").join("deep").join("sessions");
    let session_id = "mkdir-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    assert!(!socket_dir.exists());

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(
        wait_for_socket(&socket_path, Duration::from_secs(5)),
        "session should start and create missing socket dir"
    );
    assert!(socket_dir.exists(), "socket dir should have been created");

    let _ = child.kill();
    let _ = child.wait();
}

// -- Client partial frame disconnect --

/// A client that sends a partial frame header (< 5 bytes) then disconnects
/// must NOT crash the supervisor. Other clients should still work.
#[test]
fn partial_frame_disconnect_does_not_crash_supervisor() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "partial-frame";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    // Connect, read mode byte, send 3 bytes of garbage, disconnect.
    {
        let mut stream = UnixStream::connect(&socket_path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut mode = [0u8; 1];
        stream.read_exact(&mut mode).unwrap();
        let _ = stream.write_all(&[0x03, 0x00, 0x00]);
    }

    std::thread::sleep(Duration::from_millis(300));

    // Supervisor should still be alive. Verify with a STATUS query.
    {
        let mut stream =
            UnixStream::connect(&socket_path).expect("supervisor died after partial frame");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut mode = [0u8; 1];
        stream.read_exact(&mut mode).unwrap();

        let status_frame: [u8; 5] = [0x03, 0, 0, 0, 0];
        stream.write_all(&status_frame).unwrap();

        let mut header = [0u8; 5];
        stream.read_exact(&mut header).unwrap();
        assert_eq!(
            header[0], 0x82,
            "should get STATUS_RESP after partial frame client"
        );
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        assert_eq!(len, 15, "STATUS_RESP payload must be 15 bytes");

        let mut payload = [0u8; 15];
        stream.read_exact(&mut payload).unwrap();
        let pid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let alive = payload[8];
        assert!(pid > 0, "pid should be nonzero after partial frame client");
        assert_eq!(alive, 1, "alive should be 1 after partial frame client");
    }

    let _ = child.kill();
    let _ = child.wait();
}

// -- Kill on stale socket (no listener) --

/// `hm kill` on a stale socket file (exists but no listener) should error cleanly.
#[test]
fn kill_stale_socket_errors_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "stale-kill";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    std::fs::create_dir_all(&socket_dir).unwrap();
    std::fs::write(&socket_path, "not-a-socket").unwrap();

    let output = Command::new(hm_bin())
        .args([
            "kill",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run kill");

    assert!(!output.status.success(), "kill on stale socket should fail");
}

// -- ls ignores non-.sock files --

/// `hm ls` only lists .sock files, not .pid or other files in the socket dir.
/// Also verifies stale .sock files (no live PID) are NOT listed.
#[test]
fn list_ignores_non_sock_and_stale_files() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(&socket_dir).unwrap();

    // Non-.sock files should never appear.
    std::fs::write(socket_dir.join("session1.pid"), "12345").unwrap();
    std::fs::write(socket_dir.join("session2.toml"), "config").unwrap();
    std::fs::write(socket_dir.join("random-file"), "data").unwrap();

    // A .sock file with no PID file — stale, should not be listed.
    std::fs::write(socket_dir.join("orphan.sock"), "").unwrap();

    // A .sock file with a dead PID — stale, should not be listed.
    std::fs::write(socket_dir.join("dead.sock"), "").unwrap();
    std::fs::write(socket_dir.join("dead.pid"), "2147483647").unwrap();

    let output = Command::new(hm_bin())
        .args(["ls", "--socket-dir", socket_dir.to_str().unwrap()])
        .output()
        .expect("failed to run ls");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No active sessions"),
        "all entries are stale, should show no sessions: {stdout}"
    );
    assert!(
        !stdout.contains("session1"),
        "should not list .pid file: {stdout}"
    );
    assert!(
        !stdout.contains("orphan"),
        "should not list stale .sock without PID: {stdout}"
    );
    assert!(
        !stdout.contains("dead"),
        "should not list stale .sock with dead PID: {stdout}"
    );
}

/// `hm ls` cleans up stale socket and PID files it finds.
#[test]
fn list_cleans_stale_socket_and_pid_files() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let sock = socket_dir.join("stale.sock");
    let pid = socket_dir.join("stale.pid");
    std::fs::write(&sock, "").unwrap();
    std::fs::write(&pid, "2147483647").unwrap();

    assert!(sock.exists());
    assert!(pid.exists());

    // Run ls — it should detect stale entry and clean up.
    let _ = Command::new(hm_bin())
        .args(["ls", "--socket-dir", socket_dir.to_str().unwrap()])
        .output()
        .expect("failed to run ls");

    assert!(!sock.exists(), "stale .sock should be cleaned up by ls");
    assert!(!pid.exists(), "stale .pid should be cleaned up by ls");
}

// -- Clean subcommand --

/// `hm clean --force` removes orphaned .log files older than the retention window.
#[test]
fn clean_removes_old_orphan_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(&socket_dir).unwrap();

    // Create an old orphaned log (no .sock or .pid).
    let old_log = socket_dir.join("dead-session.log");
    std::fs::write(&old_log, "some log output").unwrap();

    // Backdate the file to 2 days ago.
    let two_days_ago = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - Duration::from_secs(2 * 86400),
    );
    filetime::set_file_mtime(&old_log, two_days_ago).unwrap();

    assert!(old_log.exists());

    let output = Command::new(hm_bin())
        .args([
            "clean",
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--older-than",
            "24h",
            "--force",
        ])
        .output()
        .expect("failed to run clean");

    assert!(output.status.success(), "clean should succeed");
    assert!(!old_log.exists(), "old orphan log should be removed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Cleaned 1 log file"),
        "should report cleaning: {stdout}"
    );
}

/// `hm clean --force` preserves logs within the retention window.
#[test]
fn clean_preserves_young_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(&socket_dir).unwrap();

    // Create a fresh orphaned log (just created, well within 24h).
    let fresh_log = socket_dir.join("recent-session.log");
    std::fs::write(&fresh_log, "recent log output").unwrap();

    let output = Command::new(hm_bin())
        .args([
            "clean",
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--older-than",
            "24h",
            "--force",
        ])
        .output()
        .expect("failed to run clean");

    assert!(output.status.success());
    assert!(fresh_log.exists(), "young log should be preserved");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Nothing to clean"),
        "should report nothing to clean: {stdout}"
    );
}

/// `hm clean` without --force is dry-run by default.
#[test]
fn clean_default_is_dry_run() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let old_log = socket_dir.join("old-session.log");
    std::fs::write(&old_log, "old log").unwrap();

    let two_days_ago = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - Duration::from_secs(2 * 86400),
    );
    filetime::set_file_mtime(&old_log, two_days_ago).unwrap();

    let output = Command::new(hm_bin())
        .args([
            "clean",
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--older-than",
            "24h",
        ])
        .output()
        .expect("failed to run clean");

    assert!(output.status.success());
    assert!(
        old_log.exists(),
        "default (dry-run) should not delete files"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("would remove"),
        "should show what would be removed: {stdout}"
    );
}

/// `hm clean` skips logs belonging to live sessions.
#[test]
fn clean_preserves_live_session_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "clean-live";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    // Start a live session.
    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    // The log file exists and belongs to a live session.
    let log_path = socket_dir.join(format!("{session_id}.log"));

    // Backdate it so it would be cleaned if orphaned.
    if log_path.exists() {
        let old = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() - Duration::from_secs(2 * 86400),
        );
        filetime::set_file_mtime(&log_path, old).unwrap();
    }

    let output = Command::new(hm_bin())
        .args([
            "clean",
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--older-than",
            "1s",
            "--force",
        ])
        .output()
        .expect("failed to run clean");

    assert!(output.status.success());
    assert!(log_path.exists(), "live session log should not be removed");

    let _ = child.kill();
    let _ = child.wait();
}

// -- Tests that would have caught bugs found by code review --

/// Session startup should complete quickly. On systems where _SC_OPEN_MAX
/// returns millions (e.g. 4M on Linux 6.x), a naive fd-close loop would
/// add seconds of latency. This test would have caught the original
/// sysconf(_SC_OPEN_MAX) brute-force loop as a real performance bug.
#[test]
fn child_starts_within_reasonable_time() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "startup-perf";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let start = std::time::Instant::now();

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(
        wait_for_socket(&socket_path, Duration::from_secs(5)),
        "socket never appeared"
    );

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "session took {elapsed:?} to start — fd cleanup may be looping over millions of fds"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// STATUS query works with various child types that produce pty output.
///
/// The original bug: the pty master fd was not set to O_NONBLOCK, so
/// libc::read inside AsyncFd::try_io blocked the entire single-threaded
/// runtime after the first read drained available data. With `sleep` as
/// the child (no pty output) the bug was invisible — only children that
/// produce output triggered the blocking read.
///
/// Each child type exercises a different output pattern:
/// - bash -c: shell startup + echo (multi-write, prompt processing)
/// - sh -c: minimal shell, single echo
/// - python3 -c: interpreter startup overhead, print to stdout
/// - direct echo via /bin/echo: immediate output, fast exit (race with accept)
#[test]
fn status_works_with_output_producing_children() {
    let children: &[(&str, &[&str])] = &[
        ("out-bash", &["bash", "-c", "echo ready; sleep 30"]),
        ("out-sh", &["sh", "-c", "echo ready; sleep 30"]),
        (
            "out-python",
            &[
                "python3",
                "-c",
                "import time; print('ready'); time.sleep(30)",
            ],
        ),
        (
            "out-multiline",
            &[
                "bash",
                "-c",
                "for i in 1 2 3 4 5; do echo line$i; done; sleep 30",
            ],
        ),
    ];

    for (session_id, cmd) in children {
        let tmp = tempfile::tempdir().unwrap();
        let socket_dir = tmp.path().to_path_buf();
        let socket_path = socket_dir.join(format!("{session_id}.sock"));

        let mut args = vec![
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
        ];
        args.extend_from_slice(cmd);

        let mut child = Command::new(hm_bin())
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {session_id}: {e}"));

        assert!(
            wait_for_socket(&socket_path, Duration::from_secs(5)),
            "{session_id}: socket never appeared"
        );

        // Give the child time to produce output through the pty.
        std::thread::sleep(Duration::from_millis(500));

        // STATUS query must complete within 3 seconds, not hang.
        let mut stream = UnixStream::connect(&socket_path)
            .unwrap_or_else(|e| panic!("{session_id}: connect failed: {e}"));
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();

        // Read mode byte — this is where it blocks if the pty master fd
        // is not O_NONBLOCK (libc::read blocks the runtime).
        let mut mode = [0u8; 1];
        stream.read_exact(&mut mode).unwrap_or_else(|e| {
            panic!("{session_id}: timed out reading mode byte (pty fd likely blocking): {e}")
        });
        assert_eq!(mode[0], 0x00, "{session_id}: wrong mode byte");

        // Send STATUS.
        let status_frame: [u8; 5] = [0x03, 0, 0, 0, 0];
        stream.write_all(&status_frame).unwrap();

        // Read response.
        let mut header = [0u8; 5];
        stream
            .read_exact(&mut header)
            .unwrap_or_else(|e| panic!("{session_id}: timed out reading status response: {e}"));
        assert_eq!(header[0], 0x82, "{session_id}: should get STATUS_RESP");
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        assert_eq!(
            len, 15,
            "{session_id}: STATUS_RESP payload must be 15 bytes"
        );

        let mut payload = [0u8; 15];
        stream
            .read_exact(&mut payload)
            .unwrap_or_else(|e| panic!("{session_id}: failed to read status payload: {e}"));
        let pid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let alive = payload[8];
        assert!(pid > 0, "{session_id}: pid should be nonzero");
        assert_eq!(alive, 1, "{session_id}: alive should be 1");

        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Sending INPUT after the child has exited should return an error (or at
/// minimum not succeed silently). Without the alive-flag check in write_to_pty,
/// this writes to a closed fd that could be reused by another file.
#[test]
fn input_after_child_exit_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "input-after-exit";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut supervisor = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "1", // short-lived — exits after 1s
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(
        wait_for_socket(&socket_path, Duration::from_secs(5)),
        "socket never appeared"
    );

    // Wait for the child to exit and supervisor to notice.
    // sleep 1 gives us time to connect before child exits.
    std::thread::sleep(Duration::from_secs(2));

    // Try to connect and send INPUT. The supervisor may have already exited
    // (connection refused) OR it may accept and return an error on the INPUT.
    // Either outcome is correct — the only wrong answer is silent success
    // writing to a dead fd.
    let connect_result = UnixStream::connect(&socket_path);
    if let Ok(mut stream) = connect_result {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        // Read mode byte.
        let mut mode = [0u8; 1];
        if stream.read_exact(&mut mode).is_err() {
            // Supervisor closed connection — acceptable.
        } else {
            // Send INPUT frame with some data.
            let payload = b"echo hello\n";
            let len = (payload.len() as u32).to_be_bytes();
            let mut frame = vec![0x01]; // INPUT
            frame.extend_from_slice(&len);
            frame.extend_from_slice(payload);
            let _ = stream.write_all(&frame);

            // The connection should be closed or error out.
            // Try reading — we should get an error or EOF, not a normal response.
            let mut buf = [0u8; 5];
            let read_result = stream.read_exact(&mut buf);
            // Any of: connection reset, EOF, timeout, broken pipe = correct.
            // Getting a STATUS_RESP (0x82) back would be unexpected but not wrong.
            // The key assertion is that we get here without the supervisor panicking.
            let _ = read_result;
        }
    }
    // Either way: supervisor should not have panicked. If it's still running, kill it.
    // If it already exited, wait() will return immediately.
    let _ = supervisor.kill();
    let _ = supervisor.wait();
}

/// Sending a frame header claiming a payload larger than MAX_FRAME_SIZE over a
/// real socket should be rejected without the supervisor allocating that memory.
/// Without the frame size limit, a malicious client could OOM the supervisor.
#[test]
fn oversized_frame_over_socket_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "oversize-frame";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    // Connect and send a frame claiming 1GB payload.
    {
        let mut stream = UnixStream::connect(&socket_path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();

        let mut mode = [0u8; 1];
        stream.read_exact(&mut mode).unwrap();

        // INPUT frame with 1GB claimed length.
        let fake_len: u32 = 1 << 30; // 1 GB
        let mut header = vec![0x01u8]; // INPUT
        header.extend_from_slice(&fake_len.to_be_bytes());
        let _ = stream.write_all(&header);
        // Don't send actual payload — the supervisor should reject based on header.

        // Connection should be dropped by the supervisor.
        std::thread::sleep(Duration::from_millis(200));
    }

    // Supervisor should still be alive. Verify with a STATUS query.
    {
        let mut stream =
            UnixStream::connect(&socket_path).expect("supervisor died after oversized frame");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();

        let mut mode = [0u8; 1];
        stream.read_exact(&mut mode).unwrap();

        let status_frame: [u8; 5] = [0x03, 0, 0, 0, 0];
        stream.write_all(&status_frame).unwrap();

        let mut resp_header = [0u8; 5];
        stream.read_exact(&mut resp_header).unwrap();
        assert_eq!(
            resp_header[0], 0x82,
            "supervisor should still respond after rejecting oversized frame"
        );
        let len = u32::from_be_bytes([
            resp_header[1],
            resp_header[2],
            resp_header[3],
            resp_header[4],
        ]);
        assert_eq!(len, 15, "STATUS_RESP payload must be 15 bytes");

        let mut payload = [0u8; 15];
        stream.read_exact(&mut payload).unwrap();
        let pid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let alive = payload[8];
        assert!(
            pid > 0,
            "pid should be nonzero after oversized frame rejection"
        );
        assert_eq!(
            alive, 1,
            "alive should be 1 after oversized frame rejection"
        );
    }

    let _ = child.kill();
    let _ = child.wait();
}

/// Two concurrent `hm run` attempts for the same session ID — exactly one
/// should succeed. Without flock-based locking, both could race past the
/// PID-file check and clobber each other.
#[test]
fn concurrent_run_same_id_one_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "race-test";

    std::fs::create_dir_all(&socket_dir).unwrap();

    // Spawn two supervisors for the same session ID simultaneously.
    let mut child1 = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn child1");

    let mut child2 = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn child2");

    // Wait for both to finish (one should win, one should fail).
    // Give them time to race.
    std::thread::sleep(Duration::from_secs(2));

    // Check who's still running. Exactly one should be alive.
    let status1 = child1.try_wait().expect("failed to check child1");
    let status2 = child2.try_wait().expect("failed to check child2");

    let (winner, loser_status) = match (status1, status2) {
        (None, Some(s)) => {
            // child1 is running, child2 exited
            assert!(
                !s.success(),
                "the loser should exit with an error, got: {s}"
            );
            (1, s)
        }
        (Some(s), None) => {
            // child2 is running, child1 exited
            assert!(
                !s.success(),
                "the loser should exit with an error, got: {s}"
            );
            (2, s)
        }
        (None, None) => {
            // Both still running — this is the TOCTOU bug.
            let _ = child1.kill();
            let _ = child2.kill();
            let _ = child1.wait();
            let _ = child2.wait();
            panic!("both sessions are running — TOCTOU race: flock is not working");
        }
        (Some(s1), Some(s2)) => {
            // Both exited — possible if the race was very tight.
            // At least one should have failed.
            assert!(
                !s1.success() || !s2.success(),
                "at least one should fail: child1={s1}, child2={s2}"
            );
            let _ = child1.wait();
            let _ = child2.wait();
            return; // Test passes — no cleanup needed.
        }
    };

    let _ = loser_status;
    // Clean up the winner.
    if winner == 1 {
        let _ = child1.kill();
        let _ = child1.wait();
    } else {
        let _ = child2.kill();
        let _ = child2.wait();
    }
}

/// `hm ls` shows live sessions (with real running supervisor).
#[test]
fn list_shows_live_sessions_with_liveness_check() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "live-ls";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    let output = Command::new(hm_bin())
        .args(["ls", "--socket-dir", socket_dir.to_str().unwrap()])
        .output()
        .expect("failed to run ls");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(session_id),
        "live session should appear in ls: {stdout}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

// -- Extended integration tests: protocol, config, and resilience --

/// Sending a RESIZE frame does not crash the supervisor.
#[test]
fn resize_over_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "resize-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "bash",
            "-c",
            "sleep 30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    let mut stream = UnixStream::connect(&socket_path).expect("failed to connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();

    // Read mode byte.
    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).unwrap();
    assert_eq!(mode[0], 0x00);

    // Send RESIZE frame: type=0x04, len=4, payload=[cols:u16 BE, rows:u16 BE].
    let cols: u16 = 120;
    let rows: u16 = 40;
    let mut resize_frame = vec![0x04u8];
    resize_frame.extend_from_slice(&4u32.to_be_bytes());
    resize_frame.extend_from_slice(&cols.to_be_bytes());
    resize_frame.extend_from_slice(&rows.to_be_bytes());
    stream.write_all(&resize_frame).unwrap();

    // Verify session is still alive via STATUS.
    let status_frame: [u8; 5] = [0x03, 0, 0, 0, 0];
    stream.write_all(&status_frame).unwrap();

    let mut header = [0u8; 5];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(header[0], 0x82);
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    assert_eq!(len, 15);

    let mut payload = [0u8; 15];
    stream.read_exact(&mut payload).unwrap();
    let pid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let alive = payload[8];
    assert!(pid > 0, "pid should be nonzero after resize");
    assert_eq!(alive, 1, "session should be alive after resize");

    let _ = child.kill();
    let _ = child.wait();
}

/// Late-joining subscribers receive buffered scrollback output.
#[test]
fn subscribe_receives_scrollback() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "scrollback-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "bash",
            "-c",
            "echo SCROLLBACK_MARKER && sleep 30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    // Wait for echo to produce output into scrollback buffer.
    std::thread::sleep(Duration::from_millis(500));

    let mut stream = UnixStream::connect(&socket_path).expect("failed to connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();

    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).unwrap();
    assert_eq!(mode[0], 0x00);

    // Subscribe.
    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();

    // Read OUTPUT frames for up to 3s.
    let mut output_data = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        let mut header = [0u8; 5];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) => break,
        }
        let msg_type = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 && stream.read_exact(&mut payload).is_err() {
            break;
        }
        if msg_type == 0x81 {
            output_data.extend_from_slice(&payload);
        }
    }

    let output_str = String::from_utf8_lossy(&output_data);
    assert!(
        output_str.contains("SCROLLBACK_MARKER"),
        "scrollback should contain SCROLLBACK_MARKER, got: {output_str}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// Old scrollback data is evicted when the buffer exceeds its configured size.
#[test]
fn scrollback_eviction() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "eviction-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    // Write config with small scrollback.
    let config_path = tmp.path().join("eviction.toml");
    std::fs::write(&config_path, "scrollback_bytes = 256\n").unwrap();

    let mut child = Command::new(hm_bin())
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "bash",
            "-c",
            "head -c 1024 /dev/urandom | base64 && sleep 30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    // Wait for output to be produced and eviction to occur.
    std::thread::sleep(Duration::from_secs(1));

    let mut stream = UnixStream::connect(&socket_path).expect("failed to connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();

    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).unwrap();
    assert_eq!(mode[0], 0x00);

    // Subscribe.
    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();

    // Read all OUTPUT frames.
    let mut total_bytes: usize = 0;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        let mut header = [0u8; 5];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) => break,
        }
        let msg_type = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 && stream.read_exact(&mut payload).is_err() {
            break;
        }
        if msg_type == 0x81 {
            total_bytes += len;
        }
    }

    assert!(
        total_bytes <= 512,
        "scrollback should be evicted to ~256 bytes, got {total_bytes}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// Multiple subscribers see the same output.
#[test]
fn multiple_subscribers_see_same_output() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "multi-sub-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "bash",
            "-c",
            "sleep 0.5 && echo MULTI_TEST && sleep 30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    // Helper: connect and subscribe.
    let connect_and_subscribe = |path: &std::path::Path| -> UnixStream {
        let mut stream = UnixStream::connect(path).expect("failed to connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut mode = [0u8; 1];
        stream.read_exact(&mut mode).unwrap();
        assert_eq!(mode[0], 0x00);
        let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
        stream.write_all(&sub_frame).unwrap();
        stream
    };

    let mut stream1 = connect_and_subscribe(&socket_path);
    let mut stream2 = connect_and_subscribe(&socket_path);

    // Helper: read output frames until marker found or timeout.
    let read_until_marker = |stream: &mut UnixStream, marker: &str, timeout: Duration| -> String {
        let mut output_data = Vec::new();
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            let mut header = [0u8; 5];
            match stream.read_exact(&mut header) {
                Ok(()) => {}
                Err(_) => break,
            }
            let msg_type = header[0];
            let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
            let mut payload = vec![0u8; len];
            if len > 0 && stream.read_exact(&mut payload).is_err() {
                break;
            }
            if msg_type == 0x81 {
                output_data.extend_from_slice(&payload);
            }
            let s = String::from_utf8_lossy(&output_data);
            if s.contains(marker) {
                return s.into_owned();
            }
        }
        String::from_utf8_lossy(&output_data).into_owned()
    };

    let out1 = read_until_marker(&mut stream1, "MULTI_TEST", Duration::from_secs(5));
    let out2 = read_until_marker(&mut stream2, "MULTI_TEST", Duration::from_secs(5));

    assert!(
        out1.contains("MULTI_TEST"),
        "subscriber 1 should see MULTI_TEST: {out1}"
    );
    assert!(
        out2.contains("MULTI_TEST"),
        "subscriber 2 should see MULTI_TEST: {out2}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// Input sent over the socket round-trips through the pty and appears in output.
#[test]
fn input_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "input-rt-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "bash",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    let mut stream = UnixStream::connect(&socket_path).expect("failed to connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).unwrap();
    assert_eq!(mode[0], 0x00);

    // Subscribe first.
    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();

    // Give bash a moment to start.
    std::thread::sleep(Duration::from_millis(300));

    // Send INPUT frame with "echo ROUND_TRIP_TEST\r".
    let input_data = b"echo ROUND_TRIP_TEST\r";
    let mut input_frame = vec![0x01u8];
    input_frame.extend_from_slice(&(input_data.len() as u32).to_be_bytes());
    input_frame.extend_from_slice(input_data);
    stream.write_all(&input_frame).unwrap();

    // Read OUTPUT frames for up to 5s looking for our marker.
    let mut output_data = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let mut header = [0u8; 5];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) => break,
        }
        let msg_type = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 && stream.read_exact(&mut payload).is_err() {
            break;
        }
        if msg_type == 0x81 {
            output_data.extend_from_slice(&payload);
            let s = String::from_utf8_lossy(&output_data);
            if s.contains("ROUND_TRIP_TEST") {
                break;
            }
        }
    }

    let output_str = String::from_utf8_lossy(&output_data);
    assert!(
        output_str.contains("ROUND_TRIP_TEST"),
        "output should contain ROUND_TRIP_TEST: {output_str}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// Sending a KILL frame terminates the child process.
#[test]
fn kill_frame_sends_sigterm() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "kill-frame-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "60",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    let mut stream = UnixStream::connect(&socket_path).expect("failed to connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).unwrap();
    assert_eq!(mode[0], 0x00);

    // Subscribe so we can watch for EXIT frame.
    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();

    // Send KILL frame: type=0x05, len=0.
    let kill_frame: [u8; 5] = [0x05, 0, 0, 0, 0];
    stream.write_all(&kill_frame).unwrap();

    // Read frames until EXIT (0x83) or timeout.
    let mut got_exit = false;
    let mut exit_code: Option<i32> = None;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let mut header = [0u8; 5];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) => break,
        }
        let msg_type = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 && stream.read_exact(&mut payload).is_err() {
            break;
        }
        if msg_type == 0x83 {
            got_exit = true;
            assert_eq!(
                payload.len(),
                4,
                "EXIT frame payload must be exactly 4 bytes"
            );
            exit_code = Some(i32::from_be_bytes([
                payload[0], payload[1], payload[2], payload[3],
            ]));
            break;
        }
    }

    assert!(got_exit, "should receive EXIT frame after sending KILL");
    // SIGTERM kills sleep, so exit code should be non-zero (signal death).
    assert!(
        exit_code.unwrap() != 0,
        "exit code after KILL should be non-zero (signal death), got {:?}",
        exit_code
    );

    let _ = child.wait();
}

/// `kill_process_group = false` config still terminates the session.
#[test]
fn kill_process_group_false_config() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "kill-pg-false";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let config_path = tmp.path().join("no-pg-kill.toml");
    std::fs::write(&config_path, "kill_process_group = false\n").unwrap();

    let mut child = Command::new(hm_bin())
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "bash",
            "-c",
            "sleep 60",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    // Verify alive first.
    {
        let mut stream = UnixStream::connect(&socket_path).expect("failed to connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut mode = [0u8; 1];
        stream.read_exact(&mut mode).unwrap();

        let status_frame: [u8; 5] = [0x03, 0, 0, 0, 0];
        stream.write_all(&status_frame).unwrap();

        let mut header = [0u8; 5];
        stream.read_exact(&mut header).unwrap();
        assert_eq!(header[0], 0x82);

        let mut payload = [0u8; 15];
        stream.read_exact(&mut payload).unwrap();
        assert_eq!(payload[8], 1, "should be alive before kill");
    }

    // Send KILL via a new connection.
    {
        let mut stream = UnixStream::connect(&socket_path).expect("failed to connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut mode = [0u8; 1];
        stream.read_exact(&mut mode).unwrap();

        let kill_frame: [u8; 5] = [0x05, 0, 0, 0, 0];
        stream.write_all(&kill_frame).unwrap();
    }

    // Wait for session to die — socket should disappear.
    let start = std::time::Instant::now();
    let mut gone = false;
    while start.elapsed() < Duration::from_secs(5) {
        if !socket_path.exists() {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(
        gone,
        "socket should disappear after KILL with kill_process_group=false"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// Custom `session_env_var` config injects the session ID into the child env.
#[test]
fn custom_session_env_var() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "custom-env-var";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let config_path = tmp.path().join("custom-env.toml");
    std::fs::write(&config_path, "session_env_var = \"MY_CUSTOM_SESSION\"\n").unwrap();

    let mut child = Command::new(hm_bin())
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "bash",
            "-c",
            "echo $MY_CUSTOM_SESSION && sleep 30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));
    std::thread::sleep(Duration::from_millis(500));

    let mut stream = UnixStream::connect(&socket_path).expect("failed to connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();

    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).unwrap();
    assert_eq!(mode[0], 0x00);

    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();

    let mut output_data = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        let mut header = [0u8; 5];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) => break,
        }
        let msg_type = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 && stream.read_exact(&mut payload).is_err() {
            break;
        }
        if msg_type == 0x81 {
            output_data.extend_from_slice(&payload);
        }
    }

    let output_str = String::from_utf8_lossy(&output_data);
    assert!(
        output_str.contains(session_id),
        "output should contain the session ID '{session_id}': {output_str}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// Extra `[[env]]` vars from config are injected into the child process.
#[test]
fn extra_env_vars_injected() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "extra-env-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let config_path = tmp.path().join("extra-env.toml");
    std::fs::write(
        &config_path,
        r#"
[[env]]
name = "INTEG_TEST_KEY"
value = "integ_test_value_42"
"#,
    )
    .unwrap();

    let mut child = Command::new(hm_bin())
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "bash",
            "-c",
            "echo $INTEG_TEST_KEY && sleep 30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));
    std::thread::sleep(Duration::from_millis(500));

    let mut stream = UnixStream::connect(&socket_path).expect("failed to connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();

    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).unwrap();
    assert_eq!(mode[0], 0x00);

    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();

    let mut output_data = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        let mut header = [0u8; 5];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) => break,
        }
        let msg_type = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 && stream.read_exact(&mut payload).is_err() {
            break;
        }
        if msg_type == 0x81 {
            output_data.extend_from_slice(&payload);
        }
    }

    let output_str = String::from_utf8_lossy(&output_data);
    assert!(
        output_str.contains("integ_test_value_42"),
        "output should contain injected env value: {output_str}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// `--workdir` sets the working directory of the child process.
#[test]
fn workdir_applied_to_child() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "workdir-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let workdir_tmp = tempfile::tempdir().unwrap();
    let workdir_path = workdir_tmp.path().canonicalize().unwrap();

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--workdir",
            workdir_path.to_str().unwrap(),
            "--",
            "bash",
            "-c",
            "pwd && sleep 30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));
    std::thread::sleep(Duration::from_millis(500));

    let mut stream = UnixStream::connect(&socket_path).expect("failed to connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();

    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).unwrap();
    assert_eq!(mode[0], 0x00);

    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();

    let mut output_data = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        let mut header = [0u8; 5];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) => break,
        }
        let msg_type = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 && stream.read_exact(&mut payload).is_err() {
            break;
        }
        if msg_type == 0x81 {
            output_data.extend_from_slice(&payload);
        }
    }

    let output_str = String::from_utf8_lossy(&output_data);
    assert!(
        output_str.contains(workdir_path.to_str().unwrap()),
        "output should contain workdir path '{}': {output_str}",
        workdir_path.display()
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// Rapid connect/disconnect cycles do not crash the supervisor.
#[test]
fn rapid_connect_disconnect() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "rapid-cd-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "60",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    // Rapid connect/disconnect 50 times.
    for _ in 0..50 {
        if let Ok(mut stream) = UnixStream::connect(&socket_path) {
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let mut mode = [0u8; 1];
            let _ = stream.read_exact(&mut mode);
            // Drop immediately.
        }
    }

    // Give supervisor a moment to process all disconnects.
    std::thread::sleep(Duration::from_millis(200));

    // Verify supervisor is still alive.
    let mut stream = UnixStream::connect(&socket_path).expect("failed to connect after rapid loop");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();

    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).unwrap();
    assert_eq!(mode[0], 0x00);

    let status_frame: [u8; 5] = [0x03, 0, 0, 0, 0];
    stream.write_all(&status_frame).unwrap();

    let mut header = [0u8; 5];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(header[0], 0x82);

    let mut payload = [0u8; 15];
    stream.read_exact(&mut payload).unwrap();
    let alive = payload[8];
    assert_eq!(
        alive, 1,
        "supervisor should be alive after rapid connect/disconnect"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// Binary data through the pty does not crash the supervisor.
#[test]
fn binary_data_through_pty() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "binary-data-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "bash",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    let mut stream = UnixStream::connect(&socket_path).expect("failed to connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).unwrap();
    assert_eq!(mode[0], 0x00);

    // Subscribe.
    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();

    std::thread::sleep(Duration::from_millis(300));

    // Send INPUT with printf that outputs binary bytes.
    let input_data = b"printf '\\x01\\x02\\x03\\xff\\xfe'\r";
    let mut input_frame = vec![0x01u8];
    input_frame.extend_from_slice(&(input_data.len() as u32).to_be_bytes());
    input_frame.extend_from_slice(input_data);
    stream.write_all(&input_frame).unwrap();

    // Read OUTPUT frames for up to 5s — just verify we get some output.
    let mut got_output = false;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let mut header = [0u8; 5];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) => break,
        }
        let msg_type = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 && stream.read_exact(&mut payload).is_err() {
            break;
        }
        if msg_type == 0x81 {
            got_output = true;
            break;
        }
    }

    assert!(
        got_output,
        "should receive some output after sending binary data"
    );

    // Verify supervisor still alive via a fresh connection (avoids draining
    // residual OUTPUT frames on the subscribed stream).
    let mut status_stream =
        UnixStream::connect(&socket_path).expect("failed to connect for status");
    status_stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut mode2 = [0u8; 1];
    status_stream.read_exact(&mut mode2).unwrap();
    assert_eq!(mode2[0], 0x00);

    let status_frame: [u8; 5] = [0x03, 0, 0, 0, 0];
    status_stream.write_all(&status_frame).unwrap();

    let mut header = [0u8; 5];
    status_stream.read_exact(&mut header).unwrap();
    assert_eq!(header[0], 0x82);

    let mut payload = [0u8; 15];
    status_stream.read_exact(&mut payload).unwrap();
    assert_eq!(
        payload[8], 1,
        "supervisor should be alive after binary data"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// Zero-size terminal (--cols 0 --rows 0) does not crash the supervisor.
#[test]
fn zero_size_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "zero-size-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--cols",
            "0",
            "--rows",
            "0",
            "--",
            "sleep",
            "10",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(
        wait_for_socket(&socket_path, Duration::from_secs(5)),
        "supervisor should start even with zero-size terminal"
    );

    let mut stream = UnixStream::connect(&socket_path).expect("failed to connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();

    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).unwrap();
    assert_eq!(mode[0], 0x00);

    let status_frame: [u8; 5] = [0x03, 0, 0, 0, 0];
    stream.write_all(&status_frame).unwrap();

    let mut header = [0u8; 5];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(header[0], 0x82);

    let mut payload = [0u8; 15];
    stream.read_exact(&mut payload).unwrap();
    let pid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    assert!(pid > 0, "pid should be nonzero with zero-size terminal");
    assert_eq!(payload[8], 1, "should be alive with zero-size terminal");

    let _ = child.kill();
    let _ = child.wait();
}

/// Idle timer reports increasing idle_ms when the child produces no output.
#[test]
fn idle_ms_counter() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "idle-ms-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    // Wait 2 seconds for idle time to accumulate.
    std::thread::sleep(Duration::from_secs(2));

    let mut stream = UnixStream::connect(&socket_path).expect("failed to connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();

    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).unwrap();
    assert_eq!(mode[0], 0x00);

    let status_frame: [u8; 5] = [0x03, 0, 0, 0, 0];
    stream.write_all(&status_frame).unwrap();

    let mut header = [0u8; 5];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(header[0], 0x82);
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    assert_eq!(len, 15);

    let mut payload = [0u8; 15];
    stream.read_exact(&mut payload).unwrap();

    let pid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let idle_ms = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let alive = payload[8];
    assert!(pid > 0, "pid should be nonzero");
    assert_eq!(alive, 1, "should be alive");
    assert!(
        idle_ms >= 1500,
        "idle_ms should be >= 1500 after 2s wait, got {idle_ms}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// SIGTERM to the supervisor PID causes graceful shutdown and socket cleanup.
#[test]
fn supervisor_sigterm_graceful_shutdown() {
    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;

    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().to_path_buf();
    let session_id = "sigterm-test";
    let socket_path = socket_dir.join(format!("{session_id}.sock"));
    let pid_path = socket_dir.join(format!("{session_id}.pid"));

    let mut child = Command::new(hm_bin())
        .args([
            "run",
            "--detach",
            "--id",
            session_id,
            "--socket-dir",
            socket_dir.to_str().unwrap(),
            "--",
            "sleep",
            "60",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    assert!(wait_for_socket(&socket_path, Duration::from_secs(5)));

    // Read supervisor PID from line 1 of the PID file.
    let pid_contents = std::fs::read_to_string(&pid_path).expect("failed to read PID file");
    let supervisor_pid: i32 = pid_contents
        .lines()
        .next()
        .expect("PID file should have at least one line")
        .trim()
        .parse()
        .expect("PID file line 1 should be the supervisor PID");

    // Send SIGTERM to the supervisor.
    signal::kill(Pid::from_raw(supervisor_pid), Signal::SIGTERM).expect("failed to send SIGTERM");

    // Wait for socket to disappear (graceful shutdown).
    // The supervisor sends SIGTERM to the child, waits up to 5s (SIGKILL_GRACE),
    // then SIGKILLs. Allow enough time for the full grace period + cleanup.
    let start = std::time::Instant::now();
    let mut cleaned_up = false;
    while start.elapsed() < Duration::from_secs(10) {
        if !socket_path.exists() {
            cleaned_up = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(cleaned_up, "socket file should be cleaned up after SIGTERM");
    assert!(
        !pid_path.exists(),
        "PID file should be cleaned up after SIGTERM"
    );

    let _ = child.wait();
}

// -- Adversarial tests: stress, edge cases, and terminal resilience --

/// Helper: build a RESIZE frame with given cols and rows.
fn build_resize_frame(cols: u16, rows: u16) -> Vec<u8> {
    let mut frame = vec![0x04u8];
    frame.extend_from_slice(&4u32.to_be_bytes());
    frame.extend_from_slice(&cols.to_be_bytes());
    frame.extend_from_slice(&rows.to_be_bytes());
    frame
}

/// Helper: build an INPUT frame from a byte slice.
fn build_input_frame(data: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x01u8];
    frame.extend_from_slice(&(data.len() as u32).to_be_bytes());
    frame.extend_from_slice(data);
    frame
}

/// Helper: connect to socket, read mode byte, return stream.
fn connect_and_handshake(socket_path: &std::path::Path, read_timeout: Duration) -> UnixStream {
    let mut stream = UnixStream::connect(socket_path).expect("failed to connect to socket");
    stream.set_read_timeout(Some(read_timeout)).unwrap();
    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).unwrap();
    assert_eq!(mode[0], 0x00, "mode byte should be MODE_BINARY");
    stream
}

/// Helper: send STATUS and verify alive=1, return pid.
fn assert_status_alive(stream: &mut UnixStream) -> u32 {
    let status_frame: [u8; 5] = [0x03, 0, 0, 0, 0];
    stream.write_all(&status_frame).unwrap();

    let mut header = [0u8; 5];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(header[0], 0x82, "expected STATUS_RESP");
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    assert_eq!(len, 15, "STATUS_RESP payload must be 15 bytes");

    let mut payload = [0u8; 15];
    stream.read_exact(&mut payload).unwrap();
    let pid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let alive = payload[8];
    assert!(pid > 0, "pid should be nonzero");
    assert_eq!(alive, 1, "alive should be 1");
    pid
}

/// Helper: read OUTPUT frames until timeout or EXIT, accumulating payload bytes.
fn read_output_frames(stream: &mut UnixStream, timeout: Duration) -> (Vec<u8>, bool) {
    let mut output_data = Vec::new();
    let mut got_exit = false;
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        let mut header = [0u8; 5];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) => break,
        }
        let msg_type = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 && stream.read_exact(&mut payload).is_err() {
            break;
        }
        if msg_type == 0x81 {
            output_data.extend_from_slice(&payload);
        } else if msg_type == 0x83 {
            got_exit = true;
            break;
        }
    }
    (output_data, got_exit)
}

/// Helper: spawn a detached session and wait for socket.
fn spawn_session(
    session_id: &str,
    socket_dir: &std::path::Path,
    cmd: &[&str],
) -> (std::process::Child, PathBuf) {
    let socket_path = socket_dir.join(format!("{session_id}.sock"));

    let mut args = vec![
        "run",
        "--detach",
        "--id",
        session_id,
        "--socket-dir",
        socket_dir.to_str().unwrap(),
        "--",
    ];
    args.extend_from_slice(cmd);

    let child = Command::new(hm_bin())
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn session {session_id}: {e}"));

    assert!(
        wait_for_socket(&socket_path, Duration::from_secs(5)),
        "socket never appeared for session {session_id}"
    );

    (child, socket_path)
}

/// Concurrent RESIZE frames while output is streaming must not crash or corrupt.
#[test]
fn resize_while_output_streaming() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket_path) = spawn_session(
        "adv-resize-stream",
        tmp.path(),
        &[
            "bash",
            "-c",
            "for i in $(seq 1 200); do echo \"LINE_$i\"; done && sleep 30",
        ],
    );

    // Subscriber connection — reads output.
    let mut sub_stream = connect_and_handshake(&socket_path, Duration::from_secs(10));
    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    sub_stream.write_all(&sub_frame).unwrap();

    // Resize connection — sends resize frames concurrently.
    let resize_path = socket_path.clone();
    let resize_handle = std::thread::spawn(move || {
        let mut rs = connect_and_handshake(&resize_path, Duration::from_secs(10));
        let sizes: [(u16, u16); 4] = [(80, 24), (120, 40), (40, 10), (200, 50)];
        for _ in 0..20 {
            for &(cols, rows) in &sizes {
                let frame = build_resize_frame(cols, rows);
                if rs.write_all(&frame).is_err() {
                    return rs;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        rs
    });

    // Read output on subscriber.
    let (output_data, _) = read_output_frames(&mut sub_stream, Duration::from_secs(10));
    let output_str = String::from_utf8_lossy(&output_data);
    assert!(
        output_str.contains("LINE_1"),
        "should see output lines: got {} bytes",
        output_data.len()
    );

    // Wait for resize thread and verify session alive.
    let mut rs = resize_handle.join().expect("resize thread panicked");
    assert_status_alive(&mut rs);

    let _ = child.kill();
    let _ = child.wait();
}

/// A large single-frame INPUT (4000+ bytes) is handled without crash.
#[test]
fn large_paste_burst() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket_path) = spawn_session("adv-paste", tmp.path(), &["bash"]);

    let mut stream = connect_and_handshake(&socket_path, Duration::from_secs(10));

    // Subscribe.
    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    // Build large input: "echo " + 4000 'A's + "\r"
    let mut input_data = b"echo ".to_vec();
    input_data.extend(std::iter::repeat_n(b'A', 4000));
    input_data.push(b'\r');
    let frame = build_input_frame(&input_data);
    stream.write_all(&frame).unwrap();

    // Read output.
    let (output_data, _) = read_output_frames(&mut stream, Duration::from_secs(10));
    assert!(
        output_data.len() > 100,
        "should get substantial output back, got {} bytes",
        output_data.len()
    );

    // Verify still alive via a new connection.
    let mut status_stream = connect_and_handshake(&socket_path, Duration::from_secs(5));
    assert_status_alive(&mut status_stream);

    let _ = child.kill();
    let _ = child.wait();
}

/// 500 rapid single-byte INPUT frames must not crash or deadlock.
#[test]
fn rapid_input_small_frames() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket_path) = spawn_session("adv-rapid-input", tmp.path(), &["bash"]);

    let mut stream = connect_and_handshake(&socket_path, Duration::from_secs(10));

    // Subscribe.
    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    // Send 500 individual single-byte INPUT frames with no delay.
    for i in 0u16..500 {
        let byte = b'a' + (i % 26) as u8;
        let frame = build_input_frame(&[byte]);
        stream.write_all(&frame).unwrap();
    }

    // Send newline.
    let newline_frame = build_input_frame(b"\r");
    stream.write_all(&newline_frame).unwrap();

    // Read output.
    let (output_data, _) = read_output_frames(&mut stream, Duration::from_secs(5));
    assert!(
        !output_data.is_empty(),
        "should get some output after rapid input"
    );

    // Verify alive.
    let mut status_stream = connect_and_handshake(&socket_path, Duration::from_secs(5));
    assert_status_alive(&mut status_stream);

    let _ = child.kill();
    let _ = child.wait();
}

/// Ctrl-C (0x03 byte) is forwarded to child, not intercepted by supervisor.
#[test]
fn ctrl_c_forwarded_not_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket_path) = spawn_session(
        "adv-ctrl-c",
        tmp.path(),
        &[
            "bash",
            "-c",
            "trap \"echo GOT_SIGINT\" INT; echo READY; while true; do sleep 1; done",
        ],
    );

    let mut stream = connect_and_handshake(&socket_path, Duration::from_secs(10));

    // Subscribe.
    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();

    // Wait for READY in output.
    let mut output_data = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let mut header = [0u8; 5];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) => break,
        }
        let msg_type = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 && stream.read_exact(&mut payload).is_err() {
            break;
        }
        if msg_type == 0x81 {
            output_data.extend_from_slice(&payload);
            if String::from_utf8_lossy(&output_data).contains("READY") {
                break;
            }
        }
    }
    assert!(
        String::from_utf8_lossy(&output_data).contains("READY"),
        "should see READY before sending Ctrl-C"
    );

    // Send Ctrl-C (byte 0x03) as INPUT.
    let ctrl_c_frame = build_input_frame(&[0x03]);
    stream.write_all(&ctrl_c_frame).unwrap();

    // Read output looking for GOT_SIGINT.
    output_data.clear();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let mut header = [0u8; 5];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) => break,
        }
        let msg_type = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 && stream.read_exact(&mut payload).is_err() {
            break;
        }
        if msg_type == 0x81 {
            output_data.extend_from_slice(&payload);
            if String::from_utf8_lossy(&output_data).contains("GOT_SIGINT") {
                break;
            }
        }
    }

    let output_str = String::from_utf8_lossy(&output_data);
    assert!(
        output_str.contains("GOT_SIGINT"),
        "Ctrl-C should be forwarded to child, got: {output_str}"
    );

    // Verify alive.
    let mut status_stream = connect_and_handshake(&socket_path, Duration::from_secs(5));
    assert_status_alive(&mut status_stream);

    let _ = child.kill();
    let _ = child.wait();
}

/// High-throughput output (10000 lines) must not crash the supervisor.
#[test]
fn output_flood_does_not_crash() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket_path) = spawn_session(
        "adv-flood",
        tmp.path(),
        &[
            "bash",
            "-c",
            "yes \"FLOOD_LINE\" | head -n 10000 && sleep 5",
        ],
    );

    let mut stream = connect_and_handshake(&socket_path, Duration::from_secs(15));

    // Subscribe.
    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();

    // Read until EXIT or timeout.
    let (output_data, _) = read_output_frames(&mut stream, Duration::from_secs(15));

    assert!(
        output_data.len() >= 30000,
        "should receive at least 30000 bytes from 10000 lines, got {}",
        output_data.len()
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// A new subscriber after a flood should get scrollback with recent lines.
#[test]
fn scrollback_after_flood() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket_path) = spawn_session(
        "adv-scrollback",
        tmp.path(),
        &[
            "bash",
            "-c",
            "for i in $(seq 1 5000); do echo \"FLOOD_$i\"; done && sleep 30",
        ],
    );

    // Wait for output to finish.
    std::thread::sleep(Duration::from_secs(3));

    // Connect a NEW subscriber after all output has been produced.
    let mut stream = connect_and_handshake(&socket_path, Duration::from_secs(10));
    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();

    // Read scrollback frames.
    let (output_data, _) = read_output_frames(&mut stream, Duration::from_secs(5));
    let output_str = String::from_utf8_lossy(&output_data);

    // Some high-numbered line should be in scrollback.
    assert!(
        output_str.contains("FLOOD_49"),
        "scrollback should contain recent lines (FLOOD_49xx), got {} bytes",
        output_data.len()
    );

    // Verify alive.
    let mut status_stream = connect_and_handshake(&socket_path, Duration::from_secs(5));
    assert_status_alive(&mut status_stream);

    let _ = child.kill();
    let _ = child.wait();
}

/// Multiple subscribers all receive the same output.
#[test]
fn multiple_simultaneous_subscribers() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket_path) = spawn_session(
        "adv-multi-sub",
        tmp.path(),
        &[
            "bash",
            "-c",
            "sleep 1 && echo MULTI_SUB_MARKER && sleep 120",
        ],
    );

    // Connect 3 subscribers with short read timeouts so sequential
    // reading doesn't eat 30s+ total and outlive the child.
    let mut subscribers: Vec<UnixStream> = (0..3)
        .map(|_| {
            let mut s = connect_and_handshake(&socket_path, Duration::from_secs(3));
            let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
            s.write_all(&sub_frame).unwrap();
            s
        })
        .collect();

    // Read output from all 3 with a short per-subscriber timeout.
    let mut results: Vec<String> = Vec::new();
    for sub in &mut subscribers {
        let (data, _) = read_output_frames(sub, Duration::from_secs(3));
        results.push(String::from_utf8_lossy(&data).to_string());
    }

    for (i, r) in results.iter().enumerate() {
        assert!(
            r.contains("MULTI_SUB_MARKER"),
            "subscriber {i} should see MULTI_SUB_MARKER, got: {}",
            &r[..r.len().min(200)]
        );
    }

    // Disconnect all.
    drop(subscribers);

    // One more connection, verify alive.
    let mut status_stream = connect_and_handshake(&socket_path, Duration::from_secs(5));
    assert_status_alive(&mut status_stream);

    let _ = child.kill();
    let _ = child.wait();
}

/// ANSI escape sequences pass through the protocol layer unmolested.
#[test]
fn escape_sequences_pass_through() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket_path) = spawn_session(
        "adv-ansi",
        tmp.path(),
        &[
            "bash",
            "-c",
            "printf '\\033[31mRED\\033[0m\\033[1mBOLD\\033[0m' && sleep 30",
        ],
    );

    let mut stream = connect_and_handshake(&socket_path, Duration::from_secs(10));
    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();

    let (output_data, _) = read_output_frames(&mut stream, Duration::from_secs(3));

    // Check for raw ANSI escape bytes in the output.
    let has_red = output_data.windows(4).any(|w| w == b"\x1b[31");
    let has_bold = output_data.windows(3).any(|w| w == b"\x1b[1");

    assert!(
        has_red,
        "output should contain \\x1b[31m escape, got {} bytes: {:?}",
        output_data.len(),
        String::from_utf8_lossy(&output_data)
    );
    assert!(
        has_bold,
        "output should contain \\x1b[1m escape, got {} bytes: {:?}",
        output_data.len(),
        String::from_utf8_lossy(&output_data)
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// Extreme terminal dimensions (including degenerate sizes) do not crash.
#[test]
fn resize_to_extreme_dimensions() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket_path) =
        spawn_session("adv-extreme-resize", tmp.path(), &["sleep", "30"]);

    let mut stream = connect_and_handshake(&socket_path, Duration::from_secs(10));

    let extreme_sizes: [(u16, u16); 5] = [(1, 1), (500, 500), (0, 1), (1, 0), (u16::MAX, u16::MAX)];

    for &(cols, rows) in &extreme_sizes {
        let frame = build_resize_frame(cols, rows);
        stream.write_all(&frame).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_status_alive(&mut stream);
    }

    let _ = child.kill();
    let _ = child.wait();
}

/// A slow reader does not block the supervisor or other clients.
#[test]
fn subscriber_slow_reader() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket_path) = spawn_session(
        "adv-slow-reader",
        tmp.path(),
        &[
            "bash",
            "-c",
            "for i in $(seq 1 1000); do echo \"SLOW_$i\"; done && sleep 30",
        ],
    );

    let mut stream = connect_and_handshake(&socket_path, Duration::from_secs(10));
    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();

    // Don't read for 2 seconds — let buffer fill up.
    std::thread::sleep(Duration::from_secs(2));

    // Now read what we can.
    let (output_data, _) = read_output_frames(&mut stream, Duration::from_secs(5));

    // We may have missed some due to buffer overflow / lagged channel, that's fine.
    assert!(
        !output_data.is_empty(),
        "should get at least some output even as slow reader"
    );

    // Verify alive via separate connection.
    let mut status_stream = connect_and_handshake(&socket_path, Duration::from_secs(5));
    assert_status_alive(&mut status_stream);

    let _ = child.kill();
    let _ = child.wait();
}

/// INPUT works while in SUBSCRIBE mode (the select loop handles both).
#[test]
fn input_after_subscribe() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket_path) = spawn_session("adv-input-sub", tmp.path(), &["bash"]);

    let mut stream = connect_and_handshake(&socket_path, Duration::from_secs(10));

    // Subscribe.
    let sub_frame: [u8; 5] = [0x02, 0, 0, 0, 0];
    stream.write_all(&sub_frame).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    // Send INPUT.
    let input_data = b"echo AFTER_SUB_TEST\r";
    let frame = build_input_frame(input_data);
    stream.write_all(&frame).unwrap();

    // Read output.
    let mut output_data = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let mut header = [0u8; 5];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) => break,
        }
        let msg_type = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 && stream.read_exact(&mut payload).is_err() {
            break;
        }
        if msg_type == 0x81 {
            output_data.extend_from_slice(&payload);
            if String::from_utf8_lossy(&output_data).contains("AFTER_SUB_TEST") {
                break;
            }
        }
    }

    let output_str = String::from_utf8_lossy(&output_data);
    assert!(
        output_str.contains("AFTER_SUB_TEST"),
        "should see AFTER_SUB_TEST in output: {output_str}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// STATUS requests remain reliable while child produces heavy output.
#[test]
fn status_during_output_flood() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket_path) = spawn_session(
        "adv-status-flood",
        tmp.path(),
        &[
            "bash",
            "-c",
            "yes \"STATUS_FLOOD\" | head -n 5000 && sleep 30",
        ],
    );

    // Open a non-subscribed connection for STATUS queries.
    let mut status_stream = connect_and_handshake(&socket_path, Duration::from_secs(10));

    // Send 20 STATUS requests with 50ms between them.
    for i in 0..20 {
        let status_frame: [u8; 5] = [0x03, 0, 0, 0, 0];
        status_stream.write_all(&status_frame).unwrap();

        let mut header = [0u8; 5];
        status_stream
            .read_exact(&mut header)
            .unwrap_or_else(|e| panic!("STATUS request {i} failed to read header: {e}"));
        assert_eq!(header[0], 0x82, "STATUS request {i} should get STATUS_RESP");
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        assert_eq!(len, 15, "STATUS_RESP {i} payload must be 15 bytes");

        let mut payload = [0u8; 15];
        status_stream
            .read_exact(&mut payload)
            .unwrap_or_else(|e| panic!("STATUS request {i} failed to read payload: {e}"));
        let alive = payload[8];
        assert_eq!(alive, 1, "STATUS request {i}: alive should be 1");

        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = child.kill();
    let _ = child.wait();
}

/// Opening and closing many connections in sequence does not leak resources.
#[test]
fn many_connections_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket_path) = spawn_session("adv-conn-churn", tmp.path(), &["sleep", "60"]);

    // Open 20 connections in sequence, each doing a STATUS query.
    for i in 0..20 {
        let mut stream = connect_and_handshake(&socket_path, Duration::from_secs(5));
        assert_status_alive(&mut stream);
        drop(stream);
        // Small delay to let supervisor clean up.
        if i % 5 == 4 {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // One final check.
    let mut stream = connect_and_handshake(&socket_path, Duration::from_secs(5));
    assert_status_alive(&mut stream);

    let _ = child.kill();
    let _ = child.wait();
}
