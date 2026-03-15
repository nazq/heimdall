//! Terminal utilities: ANSI sequences, status bar rendering, termios guard.

use nix::sys::termios;
use std::os::fd::{AsRawFd, BorrowedFd};
use tokio::io::AsyncWriteExt;

// -- ANSI escape sequences --
// Named consts instead of inline literals for readability.

/// Save cursor position.
const SAVE_CURSOR: &str = "\x1b7";
/// Restore cursor position.
const RESTORE_CURSOR: &str = "\x1b8";
/// Reset all attributes.
const RESET: &str = "\x1b[0m";
/// Switch to alternate screen buffer.
const ALT_SCREEN_ON: &str = "\x1b[?1049h";
/// Leave alternate screen buffer.
const ALT_SCREEN_OFF: &str = "\x1b[?1049l";
/// Clear entire screen.
const CLEAR_SCREEN: &str = "\x1b[2J";
/// Move cursor to home position (1,1).
const CURSOR_HOME: &str = "\x1b[H";
/// Reset scroll region to full screen.
const SCROLL_REGION_RESET: &str = "\x1b[r";
/// CSI prefix for parameterized sequences.
const CSI: &str = "\x1b[";

// Status bar colors — SGR sequences.
const GREEN_BG_BLACK_FG: &str = "\x1b[42;30m";
const DARK_GRAY_BG_WHITE_FG: &str = "\x1b[48;5;236;37m";
const YELLOW_BG_BLACK_FG: &str = "\x1b[43;30m";
const BLUE_BG_WHITE_FG: &str = "\x1b[44;37m";
const MAGENTA_BG_WHITE_FG: &str = "\x1b[45;37m";
const CYAN_BG_BLACK_FG: &str = "\x1b[46;30m";
const RED_BG_WHITE_FG: &str = "\x1b[41;37m";
const WHITE_BG_BLACK_FG: &str = "\x1b[47;30m";
const GRAY_BG_WHITE_FG: &str = "\x1b[100;37m";

/// Info from a STATUS_RESP used to render the right side of the bar.
pub struct StatusInfo {
    pub state_byte: u8,
    pub state_ms: u32,
}

/// Get current terminal size via ioctl.
pub fn terminal_size() -> std::io::Result<(u16, u16)> {
    unsafe {
        let mut ws: nix::libc::winsize = std::mem::zeroed();
        if nix::libc::ioctl(std::io::stdin().as_raw_fd(), nix::libc::TIOCGWINSZ, &mut ws) == 0 {
            Ok((ws.ws_col, ws.ws_row))
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

/// Set up the scroll region, alt screen, and draw the initial status bar.
///
/// Returns the inner row count (total rows minus the status bar line).
/// Callers should use this for pty RESIZE frames so the child sees the
/// correct usable height.
pub async fn setup_status_bar(
    stdout: &mut tokio::io::Stdout,
    session_id: &str,
    cols: u16,
    rows: u16,
    info: Option<&StatusInfo>,
) -> std::io::Result<u16> {
    let inner_rows = rows.saturating_sub(1).max(1);

    let setup = format!("{ALT_SCREEN_ON}{CLEAR_SCREEN}{CURSOR_HOME}{CSI}1;{inner_rows}r");
    stdout.write_all(setup.as_bytes()).await?;

    draw_status_bar(stdout, session_id, cols, rows, info).await?;
    Ok(inner_rows)
}

/// Update scroll region and redraw status bar after a terminal resize.
///
/// Unlike [`setup_status_bar`], this does not switch to the alt screen or
/// clear — it just adjusts the scroll region to the new dimensions and
/// redraws the bar.
///
/// Returns the inner row count for the RESIZE frame.
pub async fn resize_status_bar(
    stdout: &mut tokio::io::Stdout,
    session_id: &str,
    cols: u16,
    rows: u16,
    info: Option<&StatusInfo>,
) -> std::io::Result<u16> {
    let inner_rows = rows.saturating_sub(1).max(1);

    let region = format!("{CSI}1;{inner_rows}r");
    stdout.write_all(region.as_bytes()).await?;

    draw_status_bar(stdout, session_id, cols, rows, info).await?;
    Ok(inner_rows)
}

/// Draw (or redraw) the status bar on the last line.
///
/// Layout:
///   Left  (green bg):  [hm] session-id
///   Right (state color): state-name duration
///   Middle: dark fill
///
/// Uses a single pre-sized buffer and `write!` to minimize allocations.
/// This runs every second for status bar updates.
pub async fn draw_status_bar(
    stdout: &mut tokio::io::Stdout,
    session_id: &str,
    cols: u16,
    rows: u16,
    info: Option<&StatusInfo>,
) -> std::io::Result<()> {
    use std::fmt::Write as FmtWrite;

    // Pre-size: ANSI escapes (~100 bytes) + session_id + fill (up to cols) + state name.
    // 256 covers most terminals without reallocation.
    let mut bar = String::with_capacity(256 + cols as usize);

    let (state_name, state_color) = match info {
        Some(si) => match si.state_byte {
            0x00 => ("idle", GREEN_BG_BLACK_FG),
            0x01 => ("thinking", YELLOW_BG_BLACK_FG),
            0x02 => ("streaming", BLUE_BG_WHITE_FG),
            0x03 => ("tool_use", MAGENTA_BG_WHITE_FG),
            0x04 => ("active", CYAN_BG_BLACK_FG),
            0xFF => ("dead", RED_BG_WHITE_FG),
            _ => ("unknown", WHITE_BG_BLACK_FG),
        },
        None => ("...", GRAY_BG_WHITE_FG),
    };

    // Compute left/right content lengths for fill calculation.
    let left_len = " [hm]  ".len() + session_id.len();
    let mut right_len = 1 + state_name.len(); // " " + state_name

    // Compute duration suffix length without allocating.
    let duration_secs = info.map(|si| si.state_ms / 1000);
    if let Some(secs) = duration_secs {
        if secs >= 60 {
            // " Xm Ys " — estimate digit count
            right_len += 2 + digit_count(secs / 60) + 1 + digit_count(secs % 60) + 2;
        } else {
            // " Xs "
            right_len += 1 + digit_count(secs) + 2;
        }
    }

    let fill_len = (cols as usize).saturating_sub(left_len + right_len);

    // Build the bar in one pass.
    let _ = write!(
        bar,
        "{SAVE_CURSOR}{CSI}{rows};1H{GREEN_BG_BLACK_FG} [hm] {session_id} {RESET}{DARK_GRAY_BG_WHITE_FG}"
    );
    for _ in 0..fill_len {
        bar.push(' ');
    }
    let _ = write!(bar, "{RESET}{state_color} {state_name}");
    if let Some(secs) = duration_secs {
        if secs >= 60 {
            let _ = write!(bar, " {}m{}s ", secs / 60, secs % 60);
        } else {
            let _ = write!(bar, " {}s ", secs);
        }
    }
    let _ = write!(bar, "{RESET}{RESTORE_CURSOR}");

    stdout.write_all(bar.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

/// Count decimal digits in a u32 (used for status bar layout calculation).
fn digit_count(n: u32) -> usize {
    if n == 0 {
        return 1;
    }
    let mut count = 0;
    let mut v = n;
    while v > 0 {
        count += 1;
        v /= 10;
    }
    count
}

/// Reset scroll region and switch back to the main screen buffer.
pub async fn reset_scroll_region(stdout: &mut tokio::io::Stdout) -> std::io::Result<()> {
    stdout.write_all(SCROLL_REGION_RESET.as_bytes()).await?;
    stdout.write_all(ALT_SCREEN_OFF.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

/// RAII guard to restore terminal settings on drop.
pub struct RestoreTermios {
    pub fd: i32,
    pub original: termios::Termios,
}

impl Drop for RestoreTermios {
    fn drop(&mut self) {
        let fd = unsafe { BorrowedFd::borrow_raw(self.fd) };
        let _ = termios::tcsetattr(fd, termios::SetArg::TCSANOW, &self.original);
    }
}
