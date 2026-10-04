//! kitty-tmux runs a command on a pseudo-terminal and wraps the kitty graphics
//! commands in its output in tmux's DCS passthrough, so the images reach the
//! terminal outside tmux.
//!
//! Written for Claude Code, which draws images with kitty Unicode placeholders
//! but writes the graphics commands raw even inside tmux, where tmux drops
//! them. The placeholder cells are plain text, which tmux already carries.

mod rewrite;

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::{Winsize, openpty};
use nix::sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr};
use signal_hook::consts::{SIGCHLD, SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGWINCH};

use rewrite::Rewriter;

const USAGE: &str = "usage: kitty-tmux [--] <command> [args...]\n\n\
Runs <command> on a pty and wraps kitty graphics commands in its output in\n\
tmux passthrough (needs `set -g allow-passthrough on`). Outside tmux, or when\n\
stdin or stdout is not a terminal, it execs <command> directly.\n";

/// How long to keep reading the child's last output after it has exited.
/// A background process can hold the pty open long after the child is gone;
/// this is long enough for the final repaint to land.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

/// Signals passed on to the child rather than acted on. The child's exit is
/// what ends us, so it gets to run its own cleanup first.
const FORWARDED: [i32; 4] = [SIGINT, SIGTERM, SIGHUP, SIGQUIT];

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.first().and_then(|a| a.to_str()) {
        Some("--") => {
            args.remove(0);
        }
        Some("-h" | "--help") => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    let Some((program, rest)) = args.split_first() else {
        eprint!("kitty-tmux: no command given\n\n{USAGE}");
        return ExitCode::from(2);
    };

    let mut command = Command::new(program);
    command.args(rest);

    let interactive = is_tty(io::stdin().as_fd()) && is_tty(io::stdout().as_fd());
    if std::env::var_os("TMUX").is_none() || !interactive {
        let err = command.exec();
        eprintln!("kitty-tmux: {}: {err}", program.to_string_lossy());
        return ExitCode::from(127);
    }

    match run(command) {
        Ok(status) => ExitCode::from(exit_code(status)),
        Err(err) => {
            eprintln!("kitty-tmux: {err}");
            ExitCode::from(1)
        }
    }
}

fn run(mut command: Command) -> io::Result<ExitStatus> {
    let stdin = io::stdin();
    let tty = stdin.as_fd();
    // Copying the outer termios and size onto the pty before the child starts
    // means it never sees default line settings or a 0x0 terminal.
    let outer = tcgetattr(tty).ok();
    let pty = openpty(winsize(tty).as_ref(), outer.as_ref())?;
    fcntl(&pty.master, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;

    command
        .stdin(Stdio::from(pty.slave.try_clone()?))
        .stdout(Stdio::from(pty.slave.try_clone()?))
        .stderr(Stdio::from(pty.slave));
    // SAFETY: setsid and ioctl are async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    // The Command holds our copies of the slave; the master only sees EOF
    // once they are closed.
    drop(command);

    let signals = Signals::register()?;
    let _raw = outer.map(|t| RawMode::enter(tty, t)).transpose()?;
    let mut master = File::from(pty.master);
    proxy(&mut child, &mut master, &signals)
}

fn proxy(child: &mut Child, master: &mut File, signals: &Signals) -> io::Result<ExitStatus> {
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut rewriter = Rewriter::default();
    let mut buf = vec![0u8; 64 * 1024];
    let mut out = Vec::with_capacity(buf.len());
    let mut stdin_open = true;

    let exited = loop {
        let mut fds = vec![
            PollFd::new(master.as_fd(), PollFlags::POLLIN),
            PollFd::new(signals.pipe.as_fd(), PollFlags::POLLIN),
        ];
        if stdin_open {
            fds.push(PollFd::new(stdin.as_fd(), PollFlags::POLLIN));
        }
        match poll(&mut fds, PollTimeout::NONE) {
            Err(Errno::EINTR) => continue,
            Err(err) => return Err(err.into()),
            Ok(_) => {}
        }
        let ready = |i: usize| {
            fds.get(i)
                .and_then(PollFd::revents)
                .is_some_and(|r| !r.is_empty())
        };
        let (master_ready, signal_ready, stdin_ready) = (ready(0), ready(1), ready(2));
        drop(fds);

        if signal_ready {
            signals.drain();
            if signals.winch.swap(false, Ordering::Relaxed) {
                // Setting the size on the master sends SIGWINCH to the child.
                if let Some(ws) = winsize(stdin.as_fd()) {
                    set_winsize(master.as_raw_fd(), &ws);
                }
            }
            for (sig, flag) in &signals.forwarded {
                if flag.swap(false, Ordering::Relaxed) {
                    // SAFETY: kill has no memory-safety preconditions.
                    unsafe { libc::kill(child.id() as i32, *sig) };
                }
            }
            if signals.chld.swap(false, Ordering::Relaxed)
                && let Some(status) = child.try_wait()?
            {
                break Some(status);
            }
        }

        if master_ready {
            match read_retrying(master, &mut buf)? {
                // EOF on darwin, EIO on linux: the last slave fd is gone.
                None | Some(0) => break None,
                Some(n) => {
                    out.clear();
                    rewriter.feed(&buf[..n], &mut out);
                    stdout.write_all(&out)?;
                    stdout.flush()?;
                }
            }
        }

        if stdin_ready {
            match read_retrying(&mut stdin, &mut buf)? {
                None | Some(0) => stdin_open = false,
                Some(n) => master.write_all(&buf[..n])?,
            }
        }
    };

    if exited.is_some() {
        drain(master, &mut rewriter, &mut stdout, &mut buf)?;
    }
    out.clear();
    rewriter.flush(&mut out);
    stdout.write_all(&out)?;
    stdout.flush()?;

    match exited {
        Some(status) => Ok(status),
        None => child.wait(),
    }
}

/// Copies whatever the child left on the pty, for at most DRAIN_TIMEOUT.
fn drain(
    master: &mut File,
    rewriter: &mut Rewriter,
    stdout: &mut impl Write,
    buf: &mut [u8],
) -> io::Result<()> {
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    let mut out = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let timeout = PollTimeout::try_from(left).unwrap_or(PollTimeout::ZERO);
        let mut fds = [PollFd::new(master.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, timeout) {
            Err(Errno::EINTR) => continue,
            Ok(0) | Err(_) => return Ok(()),
            Ok(_) => {}
        }
        match read_retrying(master, buf)? {
            None | Some(0) => return Ok(()),
            Some(n) => {
                out.clear();
                rewriter.feed(&buf[..n], &mut out);
                stdout.write_all(&out)?;
            }
        }
    }
}

/// Reads once, retrying on EINTR. A pty master reports the far side closing
/// as EIO on some systems, so that comes back as `None` rather than an error.
fn read_retrying(r: &mut impl Read, buf: &mut [u8]) -> io::Result<Option<usize>> {
    loop {
        match r.read(buf) {
            Ok(n) => return Ok(Some(n)),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if e.raw_os_error() == Some(libc::EIO) => return Ok(None),
            Err(e) => return Err(e),
        }
    }
}

/// Signal flags plus a self-pipe that wakes the poll loop when any is raised.
struct Signals {
    pipe: UnixStream,
    winch: Arc<AtomicBool>,
    chld: Arc<AtomicBool>,
    forwarded: Vec<(i32, Arc<AtomicBool>)>,
}

impl Signals {
    fn register() -> io::Result<Self> {
        let (read, write) = UnixStream::pair()?;
        read.set_nonblocking(true)?;
        write.set_nonblocking(true)?;
        let watch = |sig: i32| -> io::Result<Arc<AtomicBool>> {
            let flag = Arc::new(AtomicBool::new(false));
            signal_hook::flag::register(sig, Arc::clone(&flag))?;
            signal_hook::low_level::pipe::register(sig, write.try_clone()?)?;
            Ok(flag)
        };
        Ok(Self {
            winch: watch(SIGWINCH)?,
            chld: watch(SIGCHLD)?,
            forwarded: FORWARDED
                .iter()
                .map(|&s| Ok((s, watch(s)?)))
                .collect::<io::Result<_>>()?,
            pipe: read,
        })
    }

    fn drain(&self) {
        let mut sink = [0u8; 64];
        while matches!((&self.pipe).read(&mut sink), Ok(n) if n > 0) {}
    }
}

/// Puts the outer terminal in raw mode and restores it on drop.
struct RawMode<'a> {
    fd: BorrowedFd<'a>,
    saved: Termios,
}

impl<'a> RawMode<'a> {
    fn enter(fd: BorrowedFd<'a>, saved: Termios) -> io::Result<Self> {
        let mut raw = saved.clone();
        cfmakeraw(&mut raw);
        tcsetattr(fd, SetArg::TCSANOW, &raw)?;
        Ok(Self { fd, saved })
    }
}

impl Drop for RawMode<'_> {
    fn drop(&mut self) {
        let _ = tcsetattr(self.fd, SetArg::TCSADRAIN, &self.saved);
    }
}

fn is_tty(fd: BorrowedFd) -> bool {
    // SAFETY: isatty only inspects the descriptor.
    unsafe { libc::isatty(fd.as_raw_fd()) == 1 }
}

fn winsize(fd: BorrowedFd) -> Option<Winsize> {
    let mut ws: Winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes one winsize into the pointer we pass.
    (unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } == 0).then_some(ws)
}

fn set_winsize(fd: RawFd, ws: &Winsize) {
    // SAFETY: TIOCSWINSZ reads one winsize from the pointer we pass.
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, ws) };
}

/// A shell-style status: the exit code, or 128 plus the killing signal.
fn exit_code(status: ExitStatus) -> u8 {
    match (status.code(), status.signal()) {
        (Some(code), _) => code as u8,
        (None, Some(sig)) => 128u8.wrapping_add(sig as u8),
        (None, None) => 1,
    }
}
