//! A terminal for the native viewer while Cones keeps the physical alternate screen.
//! The agent stays in its harness's daemon. Ctrl+Z closes only this viewer.
use std::{
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    process::{Child, Command, ExitStatus, Stdio},
    time::Instant,
};

pub struct Outcome {
    pub status: Option<ExitStatus>,
    pub stderr: Vec<u8>,
    pub detached: bool,
}

struct Viewer {
    child: Child,
    reaped: bool,
}

impl Viewer {
    fn kill(&mut self) {
        if !self.reaped {
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            let _ = self.child.kill();
        }
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        if !self.reaped {
            self.kill();
            let _ = self.child.wait();
        }
    }
}

/// Alternate-screen controls belong to Cones. Handle split writes as well as whole frames.
#[derive(Default)]
struct ScreenFilter {
    pending: Vec<u8>,
}

impl ScreenFilter {
    fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        const SWITCHES: [&[u8]; 6] = [
            b"\x1b[?1049h",
            b"\x1b[?1049l",
            b"\x1b[?1047h",
            b"\x1b[?1047l",
            b"\x1b[?47h",
            b"\x1b[?47l",
        ];
        let mut out = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            self.pending.push(byte);
            loop {
                if SWITCHES.iter().any(|s| *s == self.pending) {
                    self.pending.clear();
                    break;
                }
                if SWITCHES.iter().any(|s| s.starts_with(&self.pending)) {
                    break;
                }
                out.push(self.pending.remove(0));
                if self.pending.is_empty() {
                    break;
                }
            }
        }
        out
    }
}

#[derive(Default)]
struct Input {
    tail: Vec<u8>,
    paste: bool,
}

/// Keep the opening frame visible through terminal queries and setup. Erase commands are
/// released with the first text, in the same write, instead of producing an empty frame.
#[derive(Default)]
struct FirstPaint {
    painted: bool,
    setup: Vec<u8>,
    sequence: Vec<u8>,
    mode: u8,
    string_escape: bool,
}

impl FirstPaint {
    fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if self.painted {
                out.push(byte);
            } else if self.mode == 0 {
                if byte == 27 {
                    self.sequence.push(byte);
                    self.mode = 1;
                } else if byte >= 32 && byte != 127 {
                    out.extend_from_slice(b"\x1b[2J\x1b[H");
                    out.append(&mut self.setup);
                    out.push(byte);
                    self.painted = true;
                } else {
                    self.setup.push(byte);
                    out.push(byte);
                }
            } else {
                self.sequence.push(byte);
                let (complete, query, erase) = match self.mode {
                    1 if self.sequence.len() == 2 && byte == b'[' => {
                        self.mode = 2;
                        (false, false, false)
                    }
                    1 if self.sequence.len() == 2 && matches!(byte, b']' | b'P' | b'_' | b'^') => {
                        self.mode = 3;
                        (false, false, false)
                    }
                    1 => (!(32..=47).contains(&byte), false, byte == b'c'),
                    2 if (64..=126).contains(&byte) => (
                        true,
                        matches!(byte, b'n' | b'c' | b't' | b'u') || self.sequence.ends_with(b"$p"),
                        matches!(
                            byte,
                            b'J' | b'K' | b'S' | b'T' | b'X' | b'L' | b'M' | b'@' | b'P'
                        ),
                    ),
                    3 if byte == 7 || (self.string_escape && byte == b'\\') => (
                        true,
                        self.sequence.contains(&b'?')
                            || self.sequence.starts_with(b"\x1bP+q")
                            || self.sequence.starts_with(b"\x1bP$q"),
                        false,
                    ),
                    3 => {
                        self.string_escape = byte == 27;
                        (false, false, false)
                    }
                    _ => (false, false, false),
                };
                if complete {
                    if query || !erase {
                        out.extend_from_slice(&self.sequence);
                    }
                    if !query {
                        self.setup.extend_from_slice(&self.sequence);
                    }
                    self.sequence.clear();
                    self.mode = 0;
                    self.string_escape = false;
                }
            }
            if self.setup.len() + self.sequence.len() > 65536 {
                out.append(&mut self.setup);
                out.append(&mut self.sequence);
                self.painted = true;
            }
        }
        out
    }
}

impl Input {
    fn detach_at(&mut self, bytes: &[u8]) -> Option<usize> {
        for (i, &byte) in bytes.iter().enumerate() {
            if byte == 26 && !self.paste {
                return Some(i);
            }
            self.tail.push(byte);
            if self.tail.len() > 6 {
                self.tail.remove(0);
            }
            if self.tail == b"\x1b[200~" {
                self.paste = true;
            } else if self.tail == b"\x1b[201~" {
                self.paste = false;
            }
        }
        None
    }
}

fn nonblocking(fd: i32) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn size() -> libc::winsize {
    let mut size = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(0, libc::TIOCGWINSZ, &mut size);
    }
    size
}

pub fn run(
    mut command: Command,
    normal: Option<&libc::termios>,
    mut debug: impl FnMut(String) + Send + 'static,
) -> io::Result<Outcome> {
    let (mut master, mut slave) = (-1, -1);
    let mut dimensions = size();
    let mut normal = normal.copied();
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            normal.as_mut().map_or(std::ptr::null_mut(), |t| t),
            &mut dimensions,
        )
    };
    if opened < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    command
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            for sig in [libc::SIGINT, libc::SIGTSTP, libc::SIGTTOU, libc::SIGTTIN] {
                libc::signal(sig, libc::SIG_DFL);
            }
            Ok(())
        });
    }
    let started = Instant::now();
    let child = command.spawn()?;
    drop(slave);
    let mut viewer = Viewer {
        child,
        reaped: false,
    };
    debug(format!(
        "viewer pid {}; terminal retained",
        viewer.child.id()
    ));
    nonblocking(master.as_raw_fd())?;
    let mut stderr = viewer.child.stderr.take().unwrap();
    nonblocking(stderr.as_raw_fd())?;
    let (mut screen, mut input) = (ScreenFilter::default(), Input::default());
    let (mut master_open, mut stderr_open) = (true, true);
    let mut errors = Vec::new();
    let mut detached = false;
    let mut first_paint = FirstPaint::default();
    let mut pending_input = Vec::new();
    let mut input_sent = 0;
    let mut output = io::stdout().lock();
    let mut keyboard = io::stdin().lock();
    output.write_all(b"\x1b[H")?;
    output.flush()?;
    loop {
        let mut status = 0;
        let waited = unsafe {
            libc::waitpid(
                viewer.child.id() as i32,
                &mut status,
                libc::WNOHANG | libc::WUNTRACED,
            )
        };
        if waited < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        } else if waited > 0 {
            if libc::WIFSTOPPED(status) {
                detached = true;
                debug("viewer stopped; closing its process group".into());
                viewer.kill();
                continue;
            }
            viewer.reaped = true;
            // Do not wait for descendants that inherited stderr after the viewer exited.
            let mut tail = [0; 4096];
            while let Ok(n) = stderr.read(&mut tail) {
                if n == 0 {
                    break;
                }
                errors.extend_from_slice(&tail[..n]);
                if errors.len() > 16384 {
                    errors.drain(..errors.len() - 16384);
                    break;
                }
            }
            debug(format!("viewer exited; detached={detached}"));
            return Ok(Outcome {
                status: Some(ExitStatus::from_raw(status)),
                stderr: errors,
                detached,
            });
        }
        let next_size = size();
        if dimensions.ws_col != next_size.ws_col || dimensions.ws_row != next_size.ws_row {
            dimensions = next_size;
            unsafe {
                libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &dimensions);
            }
        }
        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if master_open { master.as_raw_fd() } else { -1 },
                events: libc::POLLIN
                    | if input_sent < pending_input.len() {
                        libc::POLLOUT
                    } else {
                        0
                    },
                revents: 0,
            },
            libc::pollfd {
                fd: if stderr_open { stderr.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 16) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        let mut bytes = [0; 8192];
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let n = keyboard.read(&mut bytes)?;
            if n == 0 {
                viewer.kill();
                detached = true;
                continue;
            }
            if let Some(at) = input.detach_at(&bytes[..n]) {
                pending_input.extend_from_slice(&bytes[..=at]);
                debug("detach key; viewer cleanup continues off-screen".into());
                let remaining = pending_input[input_sent..].to_vec();
                std::thread::spawn(move || reap(viewer, master, stderr, remaining, debug));
                return Ok(Outcome {
                    status: None,
                    stderr: errors,
                    detached: true,
                });
            }
            pending_input.extend_from_slice(&bytes[..n]);
            if pending_input.len() - input_sent > 8 * 1024 * 1024 {
                return Err(io::Error::other("viewer input queue exceeded 8 MiB"));
            }
        }
        if input_sent < pending_input.len() {
            match master.write(&pending_input[input_sent..]) {
                Ok(n) => {
                    input_sent += n;
                    if input_sent == pending_input.len() {
                        pending_input.clear();
                        input_sent = 0;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            match master.read(&mut bytes) {
                Ok(0) => master_open = false,
                Ok(n) => {
                    let was_painted = first_paint.painted;
                    let frame = first_paint.feed(&screen.feed(&bytes[..n]));
                    if !was_painted && first_paint.painted {
                        debug(format!(
                            "timing viewer_first_paint ms={:.3}",
                            started.elapsed().as_secs_f64() * 1000.0
                        ));
                    }
                    if !frame.is_empty() {
                        output.write_all(&frame)?;
                        output.flush()?;
                    }
                }
                Err(e) if e.raw_os_error() == Some(libc::EIO) => master_open = false,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        if fds[2].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            match stderr.read(&mut bytes) {
                Ok(0) => stderr_open = false,
                Ok(n) => {
                    errors.extend_from_slice(&bytes[..n]);
                    if errors.len() > 16384 {
                        errors.drain(..errors.len() - 16384);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
    }
}

/// Let the native client handle its detach key and any preceding submission in order.
/// Its shutdown output stays on this private terminal, so cleanup cannot flash the shell.
fn reap(
    mut viewer: Viewer,
    mut master: File,
    mut stderr: std::process::ChildStderr,
    input: Vec<u8>,
    mut debug: impl FnMut(String),
) {
    let started = Instant::now();
    let mut sent = 0;
    loop {
        if sent < input.len() {
            match master.write(&input[sent..]) {
                Ok(n) => sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => viewer.kill(),
            }
        }
        let mut bytes = [0; 8192];
        for _ in 0..16 {
            if !matches!(master.read(&mut bytes), Ok(n) if n > 0) {
                break;
            }
        }
        for _ in 0..4 {
            if !matches!(stderr.read(&mut bytes), Ok(n) if n > 0) {
                break;
            }
        }
        let mut status = 0;
        let waited = unsafe {
            libc::waitpid(
                viewer.child.id() as i32,
                &mut status,
                libc::WNOHANG | libc::WUNTRACED,
            )
        };
        if waited > 0 {
            if libc::WIFSTOPPED(status) {
                viewer.kill();
            } else {
                viewer.reaped = true;
                debug(format!(
                    "timing viewer_cleanup ms={:.3}",
                    started.elapsed().as_secs_f64() * 1000.0
                ));
                return;
            }
        } else if waited < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
        if started.elapsed() >= std::time::Duration::from_secs(2) {
            debug("viewer cleanup exceeded 2s; closing its process group".into());
            return; // Viewer's drop kills and reaps this client, never the agent's daemon.
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewers_cannot_leave_the_outer_screen_even_when_escape_sequences_are_split() {
        let mut filter = ScreenFilter::default();
        let mut out = Vec::new();
        for byte in b"\x1b[?1049h\x1b[31mviewer\x1b[0m\x1b[?1049l" {
            out.extend(filter.feed(&[*byte]));
        }
        assert_eq!(out, b"\x1b[31mviewer\x1b[0m");
    }

    #[test]
    fn a_pasted_control_z_is_content_and_the_next_control_z_detaches() {
        let mut input = Input::default();
        assert_eq!(input.detach_at(b"\x1b[20"), None);
        assert_eq!(input.detach_at(b"0~text\x1a\x1b[201~"), None);
        assert_eq!(input.detach_at(b"\x1a"), Some(0));
    }

    #[test]
    fn setup_does_not_clear_the_dashboard_before_the_viewer_has_text() {
        let mut paint = FirstPaint::default();
        let setup = paint.feed(b"\x1b[2J\x1b[H\x1b]10;?\x07");
        assert!(!setup.windows(4).any(|w| w == b"\x1b[2J"));
        assert!(
            setup.ends_with(b"\x1b]10;?\x07"),
            "queries still reach the terminal"
        );
        assert!(!paint.painted);
        let visible = paint.feed(b"\x1b[32mReady");
        assert!(visible.windows(4).any(|w| w == b"\x1b[2J"));
        assert!(visible.ends_with(b"Ready"));
        assert!(paint.painted);
    }
}
