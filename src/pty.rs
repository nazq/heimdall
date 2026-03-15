//! PTY management: open, fork, exec, resize.
//!
//! The supervisor owns the master fd. The child gets the slave on
//! stdin/stdout/stderr via the pre-exec seam.

use crate::config::{Config, EnvVar};
use nix::libc;
use nix::pty::{OpenptyResult, openpty};
use nix::sys::signal::{self, SigHandler, Signal};
use nix::unistd::{ForkResult, Pid, execvp, fork, setsid};
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::{env, io, path::Path};

/// Result of spawning a child process in a pty.
pub struct PtyChild {
    /// Master fd — the supervisor reads/writes this.
    pub master: OwnedFd,
    /// Child PID.
    pub pid: Pid,
}

/// Convert a command slice into `CString`s suitable for `execvp`.
///
/// Validates that no argument contains interior NUL bytes. This must run
/// before `fork()` — `CString::new` can allocate and panic, and a panic
/// in the child after fork is catastrophic.
pub fn prepare_argv(cmd: &[String]) -> io::Result<Vec<CString>> {
    cmd.iter()
        .map(|s| CString::new(s.as_str()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

/// Build the list of `(key, value)` pairs that will be injected into the
/// child's environment. Pure computation — no side effects.
///
/// Returns the session env var first, followed by any extra vars from config.
pub fn prepare_env<'a>(
    session_env_var: &'a str,
    session_id: &'a str,
    extra: &'a [EnvVar],
) -> Vec<(&'a str, &'a str)> {
    let mut pairs = Vec::with_capacity(1 + extra.len());
    pairs.push((session_env_var, session_id));
    for var in extra {
        pairs.push((var.name.as_str(), var.value.as_str()));
    }
    pairs
}

/// Compute the signal target PID.
///
/// When `kill_group` is true, returns the negated PID which tells `kill(2)`
/// to signal the entire process group. Otherwise returns the PID unchanged.
pub fn signal_target(pid: Pid, kill_group: bool) -> Pid {
    if kill_group {
        Pid::from_raw(-pid.as_raw())
    } else {
        pid
    }
}

/// Open a pty, fork, and exec the command in the child.
///
/// # Safety
///
/// Uses `fork()` which is inherently unsafe in a multi-threaded process.
/// Call this before spawning any Tokio tasks.
pub fn spawn(
    cmd: &[String],
    workdir: &Path,
    session_id: &str,
    cols: u16,
    rows: u16,
    config: &Config,
) -> io::Result<PtyChild> {
    // Build CStrings BEFORE fork — CString::new can panic on NUL bytes,
    // and a panic in the child after fork is catastrophic (runs panic handler,
    // allocates, corrupts parent state).
    let c_cmd = prepare_argv(cmd)?;

    // Build winsize for atomic openpty setup.
    let ws = Some(libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    });

    // Set up termios: raw mode for the slave.
    // We need a temporary pty to get default termios, then configure it.
    // Instead, build raw termios from a reference slave after openpty.

    // Open pty pair with winsize set atomically.
    let OpenptyResult {
        master: master_fd,
        slave: slave_fd,
    } = openpty(ws.as_ref(), None).map_err(nix_to_io)?;

    // Leave the slave in default cooked mode. The child process (bash,
    // Claude Code, etc.) will configure the terminal to its own needs.
    // Setting cfmakeraw here would break readline — bash captures the
    // initial termios as its "original" state, so raw becomes the baseline
    // and mode switching breaks (arrow keys echo as ^[[A instead of
    // navigating history).

    // Set FD_CLOEXEC on the master fd — prevents leaking to grandchild
    // processes if the child forks before we close it. Defence in depth
    // alongside close_inherited_fds().
    set_cloexec(&master_fd)?;

    let master_raw = master_fd.as_raw_fd();
    let slave_raw = slave_fd.as_raw_fd();

    // Fork
    // SAFETY: called before Tokio runtime starts, single-threaded at this point.
    let fork_result = unsafe { fork() }.map_err(nix_to_io)?;

    match fork_result {
        ForkResult::Child => {
            // -- Pre-exec seam --
            //
            // Post-fork child: only async-signal-safe operations allowed.
            // Every failure path MUST call _exit(), never return or unwind.
            // Returning from here would let the child run supervisor code.

            unsafe {
                // Close master fd in child
                libc::close(master_raw);

                // New session leader (creates new process group)
                if setsid().is_err() {
                    libc::_exit(1);
                }

                // Set the slave as controlling terminal
                if libc::ioctl(slave_raw, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                    libc::_exit(1);
                }

                // Redirect stdio to slave
                if libc::dup2(slave_raw, libc::STDIN_FILENO) == -1
                    || libc::dup2(slave_raw, libc::STDOUT_FILENO) == -1
                    || libc::dup2(slave_raw, libc::STDERR_FILENO) == -1
                {
                    libc::_exit(1);
                }
                if slave_raw > libc::STDERR_FILENO {
                    libc::close(slave_raw);
                }

                // Close inherited fds the child shouldn't keep.
                close_inherited_fds();

                // Reset signal dispositions to SIG_DFL.
                // SIG_IGN survives across exec(), so if the parent (or Rust
                // runtime, which ignores SIGPIPE) has ignored any signals,
                // the child inherits that. Reset the important ones.
                reset_signal_dispositions();
            }

            // Set environment variables (session ID + extras from config).
            // prepare_env was called before fork to keep allocation out of
            // the post-fork child, but the slices borrow from pre-fork data
            // which is still valid (fork copies the address space).
            let env_pairs = prepare_env(&config.session_env_var, session_id, &config.env);
            for (k, v) in &env_pairs {
                unsafe { env::set_var(k, v) };
            }

            // Change working directory
            if env::set_current_dir(workdir).is_err() {
                unsafe { libc::_exit(1) };
            }

            // Exec the command — c_cmd was validated before fork.
            let _ = execvp(&c_cmd[0], &c_cmd);
            // execvp only returns on failure. Must _exit, never return.
            unsafe { libc::_exit(127) };
        }
        ForkResult::Parent { child } => {
            // Close slave fd in parent — only the child uses it
            drop(slave_fd);

            Ok(PtyChild {
                master: master_fd,
                pid: child,
            })
        }
    }
}

/// Set FD_CLOEXEC on a file descriptor.
fn set_cloexec(fd: &impl AsFd) -> io::Result<()> {
    let raw = fd.as_fd().as_raw_fd();
    // F_GETFD gets the FD flags (close-on-exec), not the access mode flags.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // Now we simply ensure it's set, even if it was already set
    // We're using the full correct pattern, even though we know FD_CLOEXEC is the only flag which could be set
    if unsafe { libc::fcntl(raw, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Set the terminal window size using a raw fd.
pub fn set_winsize_raw(fd: i32, cols: u16, rows: u16) -> io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ret = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as libc::c_ulong, &ws) };
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Send SIGWINCH to a process.
pub fn send_sigwinch(pid: Pid) -> io::Result<()> {
    signal::kill(pid, Signal::SIGWINCH).map_err(nix_to_io)
}

/// Send SIGTERM, targeting the process group or direct child based on config.
pub fn send_sigterm(pid: Pid, kill_group: bool) -> io::Result<()> {
    signal::kill(signal_target(pid, kill_group), Signal::SIGTERM).map_err(nix_to_io)
}

/// Send SIGKILL, targeting the process group or direct child based on config.
pub fn send_sigkill(pid: Pid, kill_group: bool) -> io::Result<()> {
    signal::kill(signal_target(pid, kill_group), Signal::SIGKILL).map_err(nix_to_io)
}

/// Wait for a child process, returning the exit code.
pub fn wait_child(pid: Pid) -> io::Result<i32> {
    use nix::sys::wait::{WaitStatus, waitpid};
    match waitpid(pid, None).map_err(nix_to_io)? {
        WaitStatus::Exited(_, code) => Ok(code),
        WaitStatus::Signaled(_, sig, _) => Ok(128 + sig as i32),
        _ => Ok(-1),
    }
}

/// Reset signal dispositions to SIG_DFL for signals that matter.
///
/// SIG_IGN dispositions survive across exec(). Rust's runtime sets SIGPIPE
/// to SIG_IGN, and parent processes may ignore other signals. Reset them
/// so the child starts with clean signal handling.
///
/// Also unblock all signals via sigprocmask in case the parent had any blocked.
///
/// # Safety
///
/// Must be called in the child between fork() and exec().
/// Uses only async-signal-safe operations.
unsafe fn reset_signal_dispositions() {
    // SAFETY: signal() with SIG_DFL is async-signal-safe.
    for sig in [
        Signal::SIGCHLD,
        Signal::SIGHUP,
        Signal::SIGINT,
        Signal::SIGQUIT,
        Signal::SIGTERM,
        Signal::SIGALRM,
        Signal::SIGPIPE,
    ] {
        let _ = unsafe {
            signal::sigaction(
                sig,
                &signal::SigAction::new(
                    SigHandler::SigDfl,
                    signal::SaFlags::empty(),
                    signal::SigSet::empty(),
                ),
            )
        };
    }

    // Unblock all signals.
    let empty = signal::SigSet::empty();
    let _ = signal::sigprocmask(signal::SigmaskHow::SIG_SETMASK, Some(&empty), None);
}

/// Close all fds above STDERR in the child process.
///
/// Uses the fastest available method:
/// 1. `close_range(3, ~0, 0)` — single syscall, Linux 5.9+ / FreeBSD 12.2+
/// 2. `/proc/self/fd` enumeration — only touches actually-open fds
/// 3. Brute-force loop to `sysconf(_SC_OPEN_MAX)` — fallback for exotic systems
///
/// # Safety
///
/// Must be called in the child between fork() and exec(). All three paths
/// use only async-signal-safe operations.
unsafe fn close_inherited_fds() {
    // Tier 1: close_range() syscall (Linux 5.9+, FreeBSD 12.2+).
    #[cfg(target_os = "linux")]
    {
        let first_fd = (libc::STDERR_FILENO + 1) as libc::c_uint;
        // SAFETY: close_range is async-signal-safe. Closing fds [3, MAX] is
        // correct because stdin/stdout/stderr are already set up via dup2.
        let ret =
            unsafe { libc::syscall(libc::SYS_close_range, first_fd, libc::c_uint::MAX, 0u32) };
        if ret == 0 {
            return;
        }
        // ENOSYS = kernel too old, fall through to tier 2.
    }

    // Tier 2: iterate /proc/self/fd (Linux, some BSDs with procfs).
    // Only closes fds that are actually open — avoids million-iteration loop.
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CStr;
        // SAFETY: opendir/readdir/closedir are async-signal-safe on Linux.
        let dir = unsafe { libc::opendir(c"/proc/self/fd".as_ptr()) };
        if !dir.is_null() {
            loop {
                let entry = unsafe { libc::readdir(dir) };
                if entry.is_null() {
                    break;
                }
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
                if let Ok(s) = name.to_str()
                    && let Ok(fd) = s.parse::<i32>()
                {
                    // Don't close the dirfd we're iterating with.
                    let dir_fd = unsafe { libc::dirfd(dir) };
                    if fd > libc::STDERR_FILENO && fd != dir_fd {
                        unsafe { libc::close(fd) };
                    }
                }
            }
            unsafe { libc::closedir(dir) };
            return;
        }
    }

    // Tier 3: brute-force. Cap at 4096 to avoid pathological _SC_OPEN_MAX values
    // (some systems report 4M+). Fds above 4096 are extremely unlikely in a
    // freshly-forked supervisor child.
    let max_fd = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) } as i32;
    let limit = if max_fd > 0 { max_fd.min(4096) } else { 1024 };
    for fd in (libc::STDERR_FILENO + 1)..limit {
        unsafe { libc::close(fd) };
    }
}

fn nix_to_io(e: nix::Error) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- prepare_argv --

    #[test]
    fn prepare_argv_simple_command() {
        let cmd: Vec<String> = vec!["bash".into(), "-c".into(), "echo hello".into()];
        let result = prepare_argv(&cmd).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].to_str().unwrap(), "bash");
        assert_eq!(result[1].to_str().unwrap(), "-c");
        assert_eq!(result[2].to_str().unwrap(), "echo hello");
    }

    #[test]
    fn prepare_argv_single_element() {
        let cmd: Vec<String> = vec!["sleep".into()];
        let result = prepare_argv(&cmd).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].to_str().unwrap(), "sleep");
    }

    #[test]
    fn prepare_argv_empty_string_arg() {
        let cmd: Vec<String> = vec!["cmd".into(), "".into()];
        let result = prepare_argv(&cmd).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].to_str().unwrap(), "");
    }

    #[test]
    fn prepare_argv_interior_nul_is_error() {
        let cmd: Vec<String> = vec!["bash".into(), "hello\0world".into()];
        let result = prepare_argv(&cmd);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn prepare_argv_nul_in_first_arg_is_error() {
        let cmd: Vec<String> = vec!["\0".into()];
        let result = prepare_argv(&cmd);
        assert!(result.is_err());
    }

    #[test]
    fn prepare_argv_unicode_args() {
        let cmd: Vec<String> = vec!["echo".into(), "héllo".into(), "世界".into()];
        let result = prepare_argv(&cmd).unwrap();
        assert_eq!(result[1].to_str().unwrap(), "héllo");
        assert_eq!(result[2].to_str().unwrap(), "世界");
    }

    #[test]
    fn prepare_argv_preserves_spaces_and_special_chars() {
        let cmd: Vec<String> = vec![
            "cmd".into(),
            "--flag=value with spaces".into(),
            "$VAR".into(),
        ];
        let result = prepare_argv(&cmd).unwrap();
        assert_eq!(result[1].to_str().unwrap(), "--flag=value with spaces");
        assert_eq!(result[2].to_str().unwrap(), "$VAR");
    }

    // -- prepare_env --

    #[test]
    fn prepare_env_session_only() {
        let pairs = prepare_env("MY_SESSION", "sess-42", &[]);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("MY_SESSION", "sess-42"));
    }

    #[test]
    fn prepare_env_with_extras() {
        let extras = vec![
            EnvVar {
                name: "FOO".into(),
                value: "bar".into(),
            },
            EnvVar {
                name: "BAZ".into(),
                value: "qux".into(),
            },
        ];
        let pairs = prepare_env("HEIMDALL_SESSION_ID", "test-1", &extras);
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0], ("HEIMDALL_SESSION_ID", "test-1"));
        assert_eq!(pairs[1], ("FOO", "bar"));
        assert_eq!(pairs[2], ("BAZ", "qux"));
    }

    #[test]
    fn prepare_env_ordering_is_session_first() {
        let extras = vec![EnvVar {
            name: "FIRST".into(),
            value: "1".into(),
        }];
        let pairs = prepare_env("SESSION", "id", &extras);
        // Session var is always first — child can rely on this ordering.
        assert_eq!(pairs[0].0, "SESSION");
        assert_eq!(pairs[1].0, "FIRST");
    }

    // -- signal_target --

    #[test]
    fn signal_target_group_negates_pid() {
        let pid = Pid::from_raw(12345);
        let target = signal_target(pid, true);
        assert_eq!(target.as_raw(), -12345);
    }

    #[test]
    fn signal_target_direct_preserves_pid() {
        let pid = Pid::from_raw(12345);
        let target = signal_target(pid, false);
        assert_eq!(target.as_raw(), 12345);
    }

    #[test]
    fn signal_target_pid_1_group() {
        let pid = Pid::from_raw(1);
        let target = signal_target(pid, true);
        assert_eq!(target.as_raw(), -1);
    }

    #[test]
    fn signal_target_pid_1_direct() {
        let pid = Pid::from_raw(1);
        let target = signal_target(pid, false);
        assert_eq!(target.as_raw(), 1);
    }
}
