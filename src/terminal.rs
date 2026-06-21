//! Terminal geometry helpers: report the controlling terminal's size in
//! character cells so command output can be formatted to fit.

use supports_color::Stream;
use terminal_size::{Height, Width, terminal_size};

/// Columns/rows assumed when the terminal size is unavailable — i.e. stdout is
/// not a TTY (piped or redirected to a file). 80x24 is the conventional default.
pub const DEFAULT_COLS: u16 = 80;
pub const DEFAULT_ROWS: u16 = 24;

/// The terminal's `(width, height)` in character cells, querying stdout.
/// Falls back to [`DEFAULT_COLS`]x[`DEFAULT_ROWS`] when the size can't be
/// determined (non-TTY output), so callers always get usable, non-zero values.
pub fn terminal_dimensions() -> (u16, u16) {
    dimensions_or_fallback(terminal_size())
}

/// Map a queried terminal size to usable `(cols, rows)`, substituting
/// [`DEFAULT_COLS`]x[`DEFAULT_ROWS`] when the size is missing or degenerate.
/// Split out from [`terminal_dimensions`] so the fallback can be tested without
/// depending on whether the test harness has a real TTY attached.
fn dimensions_or_fallback(size: Option<(Width, Height)>) -> (u16, u16) {
    match size {
        Some((Width(w), Height(h))) if w > 0 && h > 0 => (w, h),
        _ => (DEFAULT_COLS, DEFAULT_ROWS),
    }
}

/// The pixel size of a single character cell, `(width, height)`, or `None` when
/// the terminal doesn't report it (or on unsupported platforms). Used to size a
/// kitty-graphics image to the terminal's actual pixel canvas.
#[cfg(target_os = "linux")]
pub fn terminal_cell_pixels() -> Option<(u16, u16)> {
    // SAFETY: a zeroed `winsize` is a valid value, and `TIOCGWINSZ` only writes
    // into it. We ignore the call on any error or non-positive field.
    let ws = unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) != 0 {
            return None;
        }
        ws
    };
    if ws.ws_col == 0 || ws.ws_row == 0 || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 {
        return None;
    }
    Some((ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row))
}

#[cfg(not(target_os = "linux"))]
pub fn terminal_cell_pixels() -> Option<(u16, u16)> {
    None
}

/// Whether the terminal speaks the kitty graphics protocol. Detected by sending
/// a graphics query (`a=q`) plus a primary device-attributes request and seeing
/// whether a graphics response (`_G…OK`) comes back before the DA reply.
///
/// Only meaningful on Linux with a TTY on both stdin and stdout; otherwise
/// (non-TTY, or unsupported platform) returns `false`.
#[cfg(target_os = "linux")]
pub fn supports_kitty_graphics() -> bool {
    use std::io::Write;
    use std::os::fd::AsRawFd;

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let in_fd = stdin.as_raw_fd();
    let out_fd = stdout.as_raw_fd();

    // SAFETY: `isatty` only reads the fd.
    if unsafe { libc::isatty(in_fd) } != 1 || unsafe { libc::isatty(out_fd) } != 1 {
        return false;
    }

    // Put the terminal in raw mode so the response isn't echoed or line-buffered;
    // the guard restores the original attributes on drop.
    let _raw = match RawMode::enable(in_fd) {
        Some(guard) => guard,
        None => return false,
    };

    // The empty-image graphics query, followed by a DA request that every
    // terminal answers — a non-kitty terminal replies to the DA request only.
    let query = "\x1b_Gi=1,a=q,s=1,v=1,f=24,t=d;AAAA\x1b\\\x1b[c";
    {
        let mut out = stdout.lock();
        if out.write_all(query.as_bytes()).is_err() || out.flush().is_err() {
            return false;
        }
    }

    // Read until the DA reply (ends in 'c') arrives or we time out. A kitty
    // response (`\x1b_G...OK...\x1b\\`) precedes it when supported.
    //
    // Read straight from the fd via `libc::read` rather than through `Stdin`:
    // its internal `BufReader` would slurp every available byte on the first
    // read and hand back only one, leaving `wait_readable` to poll an
    // already-drained kernel buffer and falsely time out.
    let mut buf = Vec::with_capacity(64);
    let mut chunk = [0u8; 64];
    loop {
        // 200ms budget for the next batch of reply bytes to arrive.
        if !wait_readable(in_fd, 200) {
            break;
        }
        // SAFETY: `read` writes at most `chunk.len()` bytes into the valid buffer.
        let n = unsafe {
            libc::read(in_fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len())
        };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        // The DA reply is the terminal's last response and ends in 'c'; once we
        // see that final byte we have everything, including any graphics frame.
        if buf.last() == Some(&b'c') {
            break;
        }
    }

    contains_graphics_ok(&buf)
}

#[cfg(not(target_os = "linux"))]
pub fn supports_kitty_graphics() -> bool {
    false
}

/// True if `buf` contains a kitty graphics response acknowledging support: an
/// APC graphics frame (`\x1b_G`) carrying `OK`.
#[cfg(target_os = "linux")]
fn contains_graphics_ok(buf: &[u8]) -> bool {
    let Some(start) = buf.windows(2).position(|w| w == b"\x1b_") else {
        return false;
    };
    let rest = &buf[start..];
    rest.len() > 2 && rest[2] == b'G' && rest.windows(2).any(|w| w == b"OK")
}

/// Block until `fd` is readable or `timeout_ms` elapses; returns whether data is
/// ready. Used to bound the wait for a terminal query response.
#[cfg(target_os = "linux")]
fn wait_readable(fd: i32, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pfd` points to a single valid pollfd for the duration of the call.
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    rc > 0 && (pfd.revents & libc::POLLIN) != 0
}

/// RAII guard that puts a terminal fd into raw mode and restores the original
/// `termios` attributes when dropped.
#[cfg(target_os = "linux")]
struct RawMode {
    fd: i32,
    original: libc::termios,
}

#[cfg(target_os = "linux")]
impl RawMode {
    fn enable(fd: i32) -> Option<Self> {
        // SAFETY: `tcgetattr` only writes a valid `termios` into the zeroed value.
        let original = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return None;
            }
            t
        };
        let mut raw = original;
        // Disable canonical mode and echo so the response is delivered raw and
        // isn't printed back to the screen.
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        // SAFETY: `raw` is a valid, fully-initialized `termios`.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(RawMode { fd, original })
    }
}

#[cfg(target_os = "linux")]
impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restoring the previously-captured, valid attributes.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ColorMode {
    BW,
    HiColor,
    TrueColor,
}

pub fn terminal_color_mode() -> ColorMode {
    if let Some(support) = supports_color::on(Stream::Stdout) {
        if support.has_16m {
            ColorMode::TrueColor
        } else if support.has_256 {
            ColorMode::HiColor
        } else {
            ColorMode::BW
        }
    } else {
        ColorMode::BW
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_when_size_unavailable() {
        // A missing or degenerate size maps to the documented default, while a
        // valid size is passed through unchanged. Tested through the pure helper
        // so the result doesn't depend on whether the runner has a real TTY.
        assert_eq!(dimensions_or_fallback(None), (DEFAULT_COLS, DEFAULT_ROWS));
        assert_eq!(
            dimensions_or_fallback(Some((Width(0), Height(0)))),
            (DEFAULT_COLS, DEFAULT_ROWS)
        );
        assert_eq!(
            dimensions_or_fallback(Some((Width(120), Height(40)))),
            (120, 40)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn graphics_ok_detected_only_with_apc_frame() {
        // A real kitty reply: graphics frame with OK, then the DA response.
        assert!(contains_graphics_ok(b"\x1b_Gi=1;OK\x1b\\\x1b[?62;c"));
        // DA reply alone (non-kitty terminal): no graphics frame.
        assert!(!contains_graphics_ok(b"\x1b[?62;c"));
        // "OK" appearing outside an APC graphics frame must not count.
        assert!(!contains_graphics_ok(b"OK"));
        assert!(!contains_graphics_ok(b""));
    }

    #[test]
    fn dimensions_are_nonzero() {
        // Callers lay out against / divide by these, so both must be positive
        // regardless of environment.
        let (w, h) = terminal_dimensions();
        assert!(w > 0);
        assert!(h > 0);
    }
}
