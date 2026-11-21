//! Pseudo-terminal allocation and spawning the login command on it.

use std::{
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    process::{ExitStatus, Stdio},
};

use rustix::{
    process::{Pid, Signal},
    pty::OpenptFlags,
    termios::Winsize,
};
use tokio::{io::unix::AsyncFd, process::Child};

/// The master side of a pty with a child process attached to the slave.
pub struct Pty {
    master: AsyncFd<OwnedFd>,
}

pub struct Spawned {
    pub pty: Pty,
    pub child: Child,
}

fn winsize(rows: u16, cols: u16) -> Winsize {
    Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}

/// Opens a new pty and runs `cmd` as a new session leader with the pty slave
/// as its controlling terminal and stdio.
pub fn spawn(cmd: &[String], env: &[(String, String)], rows: u16, cols: u16) -> io::Result<Spawned> {
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    let master = rustix::pty::openpt(flags)?;
    rustix::pty::grantpt(&master)?;
    rustix::pty::unlockpt(&master)?;
    let slave = rustix::pty::ioctl_tiocgptpeer(&master, flags)?;
    rustix::termios::tcsetwinsize(&master, winsize(rows, cols))?;
    rustix::io::ioctl_fionbio(&master, true)?;

    let (prog, args) = cmd.split_first().ok_or_else(|| io::Error::other("empty login command"))?;
    let mut command = tokio::process::Command::new(prog);
    command
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: the closure only calls async-signal-safe syscalls (setsid,
    // ioctl) and does not allocate.
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()?;
            rustix::process::ioctl_tiocsctty(BorrowedFd::borrow_raw(0))?;
            Ok(())
        });
    }
    let child = command.spawn()?;
    // `command` holds the parent's copies of the slave fd; drop them so that
    // reading the master reports EOF once the session's processes are gone.
    drop(command);

    // SAFETY: `master` is an open fd owned by the AsyncFd for its whole
    // lifetime; OwnedFd always reports the same raw fd.
    let master = unsafe { AsyncFd::register(master) }.map_err(|e| e.into_parts().1)?;
    Ok(Spawned { pty: Pty { master }, child })
}

impl Pty {
    /// Reads from the master. Returns 0 once no process holds the slave open
    /// any more (Linux reports that as EIO).
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.master.readable().await?;
            match guard.try_io(|fd| rustix::io::read(fd, &mut *buf).map_err(io::Error::from)) {
                Ok(Ok(n)) => return Ok(n),
                Ok(Err(e)) if e.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) => return Ok(0),
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }

    pub async fn write_all(&self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let mut guard = self.master.writable().await?;
            match guard.try_io(|fd| rustix::io::write(fd, buf).map_err(io::Error::from)) {
                Ok(Ok(n)) => buf = &buf[n..],
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    /// Sets the window size; the kernel sends SIGWINCH to the foreground
    /// process group.
    pub fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        rustix::termios::tcsetwinsize(self.master.get_ref().as_fd(), winsize(rows, cols))?;
        Ok(())
    }
}

/// Sends `sig` to the process group led by `child` (it is a session leader,
/// so its pid is also its process group id).
pub fn signal_group(child: &Child, sig: Signal) {
    if let Some(pid) = child.id().and_then(|id| Pid::from_raw(id as i32)) {
        let _ = rustix::process::kill_process_group(pid, sig);
    }
}

/// Exit status as a shell would report it: the exit code, or 128 + signal.
pub fn status_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().or_else(|| status.signal().map(|s| 128 + s)).unwrap_or(255)
}
