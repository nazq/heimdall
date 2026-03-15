//! PID file abstraction.
//!
//! The PID file stores two PIDs, one per line:
//!
//! ```text
//! <supervisor_pid>
//! <child_pid>
//! ```
//!
//! Line 1 is the supervisor (the `hm` process that holds the flock).
//! Line 2 is the child (the pty-forked process: claude, bash, etc.).
//! Between fork and the child PID write, only line 1 is present.

use std::io::{Seek, Write};
use std::path::Path;

/// Parsed contents of a PID file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PidFile {
    pub supervisor: i32,
    pub child: Option<i32>,
}

impl PidFile {
    /// Parse a PID file from disk. Returns `None` if the file is missing,
    /// empty, or does not contain a valid supervisor PID on line 1.
    pub fn read(path: &Path) -> Option<Self> {
        let contents = std::fs::read_to_string(path).ok()?;
        Self::parse(&contents)
    }

    /// Parse PID file contents from a string.
    fn parse(contents: &str) -> Option<Self> {
        let mut lines = contents.lines();

        let supervisor: i32 = lines.next()?.trim().parse().ok()?;
        if supervisor <= 0 {
            return None;
        }

        let child = lines
            .next()
            .and_then(|line| line.trim().parse::<i32>().ok())
            .filter(|&pid| pid > 0);

        Some(Self { supervisor, child })
    }

    /// Write the supervisor PID (line 1). Call this immediately after
    /// acquiring the flock, before fork.
    pub fn write_supervisor(f: &mut std::fs::File, pid: u32) -> std::io::Result<()> {
        f.set_len(0)?;
        f.seek(std::io::SeekFrom::Start(0))?;
        writeln!(f, "{pid}")?;
        f.flush()
    }

    /// Append the child PID (line 2). Call this after fork.
    pub fn write_child(f: &mut std::fs::File, pid: i32) -> std::io::Result<()> {
        write!(f, "{pid}")?;
        f.flush()
    }

    /// Check whether a PID is alive using `kill(pid, 0)`.
    ///
    /// kill(pid, 0) is a POSIX probe: signal 0 is never delivered. The kernel
    /// runs all existence and permission checks, then does nothing.
    ///
    /// Returns true if the process exists (even if owned by another user).
    pub fn is_pid_alive(pid: i32) -> bool {
        let ret = unsafe { nix::libc::kill(pid, 0) };
        // ret == 0        → process exists, we can signal it
        // ret == -1/EPERM → process exists, different owner
        // ret == -1/ESRCH → no such process
        ret == 0
            || (ret == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(nix::libc::EPERM))
    }

    /// Check whether the supervisor process is still alive.
    pub fn supervisor_alive(&self) -> bool {
        Self::is_pid_alive(self.supervisor)
    }

    /// Check whether the child process is still alive (false if no child PID).
    pub fn child_alive(&self) -> bool {
        self.child.is_some_and(Self::is_pid_alive)
    }

    /// Check whether either the supervisor or child is alive.
    pub fn any_alive(&self) -> bool {
        self.supervisor_alive() || self.child_alive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_both_pids() {
        let pf = PidFile::parse("1234\n5678").unwrap();
        assert_eq!(pf.supervisor, 1234);
        assert_eq!(pf.child, Some(5678));
    }

    #[test]
    fn parse_supervisor_only() {
        let pf = PidFile::parse("1234\n").unwrap();
        assert_eq!(pf.supervisor, 1234);
        assert_eq!(pf.child, None);
    }

    #[test]
    fn parse_supervisor_only_no_newline() {
        let pf = PidFile::parse("1234").unwrap();
        assert_eq!(pf.supervisor, 1234);
        assert_eq!(pf.child, None);
    }

    #[test]
    fn parse_trailing_newline() {
        let pf = PidFile::parse("1234\n5678\n").unwrap();
        assert_eq!(pf.supervisor, 1234);
        assert_eq!(pf.child, Some(5678));
    }

    #[test]
    fn parse_whitespace() {
        let pf = PidFile::parse("  1234 \n  5678 \n").unwrap();
        assert_eq!(pf.supervisor, 1234);
        assert_eq!(pf.child, Some(5678));
    }

    #[test]
    fn parse_empty_returns_none() {
        assert!(PidFile::parse("").is_none());
    }

    #[test]
    fn parse_garbage_returns_none() {
        assert!(PidFile::parse("not_a_pid").is_none());
    }

    #[test]
    fn parse_zero_supervisor_returns_none() {
        assert!(PidFile::parse("0\n5678").is_none());
    }

    #[test]
    fn parse_negative_supervisor_returns_none() {
        assert!(PidFile::parse("-1\n5678").is_none());
    }

    #[test]
    fn parse_zero_child_treated_as_none() {
        let pf = PidFile::parse("1234\n0").unwrap();
        assert_eq!(pf.supervisor, 1234);
        assert_eq!(pf.child, None);
    }

    #[test]
    fn parse_negative_child_treated_as_none() {
        let pf = PidFile::parse("1234\n-1").unwrap();
        assert_eq!(pf.supervisor, 1234);
        assert_eq!(pf.child, None);
    }

    #[test]
    fn parse_garbage_child_treated_as_none() {
        let pf = PidFile::parse("1234\ngarbage").unwrap();
        assert_eq!(pf.supervisor, 1234);
        assert_eq!(pf.child, None);
    }

    #[test]
    fn is_pid_alive_current_process() {
        let pid = std::process::id() as i32;
        assert!(PidFile::is_pid_alive(pid));
    }

    #[test]
    fn is_pid_alive_nonexistent() {
        // PID 2^22 - 1 is almost certainly not in use.
        assert!(!PidFile::is_pid_alive(4_194_303));
    }
}
