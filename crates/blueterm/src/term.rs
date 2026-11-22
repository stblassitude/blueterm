//! Local terminal: raw mode and window size.

use std::os::fd::BorrowedFd;

use rustix::{
    process::{Signal, getpid, kill_process},
    termios::{OptionalActions, Termios, isatty, tcgetattr, tcgetwinsize, tcsetattr},
};

fn stdin_fd() -> BorrowedFd<'static> {
    // SAFETY: fd 0 stays open for the life of the process.
    unsafe { BorrowedFd::borrow_raw(0) }
}

fn stdout_fd() -> BorrowedFd<'static> {
    // SAFETY: fd 1 stays open for the life of the process.
    unsafe { BorrowedFd::borrow_raw(1) }
}

pub fn stdin_is_tty() -> bool {
    isatty(stdin_fd())
}

/// Current window size (rows, cols), if stdout or stdin is a terminal.
pub fn window_size() -> Option<(u16, u16)> {
    [stdout_fd(), stdin_fd()]
        .into_iter()
        .find_map(|fd| tcgetwinsize(fd).ok())
        .filter(|ws| ws.ws_row > 0 && ws.ws_col > 0)
        .map(|ws| (ws.ws_row, ws.ws_col))
}

/// Puts stdin into raw mode; restores the previous mode when dropped.
pub struct RawMode {
    saved: Termios,
    raw: Termios,
}

impl RawMode {
    pub fn enable() -> std::io::Result<RawMode> {
        let saved = tcgetattr(stdin_fd())?;
        let mut raw = saved.clone();
        raw.make_raw();
        tcsetattr(stdin_fd(), OptionalActions::Now, &raw)?;
        let restore = saved.clone();
        // Restore the terminal even if we panic.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = tcsetattr(stdin_fd(), OptionalActions::Now, &restore);
            prev(info);
        }));
        Ok(RawMode { saved, raw })
    }
}

/// Stops the process like ^Z in a shell would, with the terminal in its
/// normal mode while we are stopped. Returns once we have been continued.
pub fn suspend(raw: Option<&RawMode>) -> std::io::Result<()> {
    if let Some(r) = raw {
        tcsetattr(stdin_fd(), OptionalActions::Now, &r.saved)?;
    }
    // The default SIGTSTP action stops all threads; kill() returns after SIGCONT.
    kill_process(getpid(), Signal::TSTP)?;
    if let Some(r) = raw {
        tcsetattr(stdin_fd(), OptionalActions::Now, &r.raw)?;
    }
    Ok(())
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = tcsetattr(stdin_fd(), OptionalActions::Now, &self.saved);
    }
}
