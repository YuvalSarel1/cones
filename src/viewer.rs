//! PTY-backed viewers rendered through vt100 into the dashboard. Viewer output never
//! reaches the real terminal directly.
//!
//! The vendored vt100 patch lets a scroll region beginning at row zero contribute
//! history while a fixed prompt occupies the rows below it. Interior regions and
//! alternate screens add no history. Unscrolled cell access also lets dashboard
//! layout inspect the live input box without moving the user's history view.
use ratatui::{
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
    layout::Rect,
    style::{Color, Modifier, Style},
};
use std::{
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    process::{Child, ChildStderr, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};
pub use vt100::MouseProtocolMode;

const STDERR_TAIL: usize = 16 * 1024;
/// Input the viewer has not read yet; beyond this it is not reading at all.
const INPUT_CAP: usize = 8 * 1024 * 1024;
/// Bound pump time so output floods cannot block input. Wait briefly between reads
/// to collect macOS's 1 KiB PTY chunks into a frame.
const PUMP_MAX: Duration = Duration::from_millis(50);
const PUMP_WAIT: Duration = Duration::from_millis(1);
const SCROLLBACK: usize = 1000;
/// Draw an unfinished synchronized update after this timeout to avoid freezing the pane.
const SYNC_MAX: Duration = Duration::from_millis(150);

/// Real terminal colors in xterm `rgb:RRRR/GGGG/BBBB` form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Colors {
    pub fg: String,
    pub bg: String,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            fg: "rgb:e4e4/e4e4/e4e4".into(),
            bg: "rgb:1414/1414/1414".into(),
        }
    }
}

/// Probe OSC 10/11 after entering raw mode, before crossterm can treat replies as keys.
/// Keep defaults on timeout; unrelated input read during the probe is dropped.
pub fn probe_colors(timeout: Duration) -> Colors {
    let mut out = io::stdout().lock();
    if out
        .write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\")
        .and_then(|()| out.flush())
        .is_err()
    {
        return Colors::default();
    }
    let mut deadline = Instant::now() + timeout;
    let mut extended = false;
    let mut bytes = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut fd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut fd, 1, left.as_millis() as libc::c_int) };
        if ready < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if ready <= 0 {
            break;
        }
        let mut chunk = [0u8; 256];
        let n = unsafe { libc::read(0, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n as usize]);
        let (fg, bg) = parse_color_replies(&bytes);
        if fg.is_some() && bg.is_some() {
            break;
        }
        // A reply under way is worth one more wait: cut short, its rest would land as keys.
        if !extended && bytes.windows(2).any(|w| w == b"\x1b]") {
            extended = true;
            deadline = Instant::now() + timeout;
        }
    }
    let (fg, bg) = parse_color_replies(&bytes);
    let defaults = Colors::default();
    Colors {
        fg: fg.unwrap_or(defaults.fg),
        bg: bg.unwrap_or(defaults.bg),
    }
}

fn parse_color_replies(bytes: &[u8]) -> (Option<String>, Option<String>) {
    let find = |code: &[u8]| -> Option<String> {
        let start = bytes.windows(code.len()).position(|w| w == code)? + code.len();
        let rest = &bytes[start..];
        let end = rest
            .iter()
            .position(|&b| b == 0x07 || b == 0x1b)
            .unwrap_or(rest.len());
        let value = std::str::from_utf8(&rest[..end]).ok()?;
        value.starts_with("rgb:").then(|| value.to_owned())
    };
    (find(b"]10;"), find(b"]11;"))
}

/// Answer terminal queries vt100 delegates. Leave kitty keyboard queries unanswered
/// because input uses classic encoding; DA1 lets clients stop waiting.
#[derive(Default)]
pub(crate) struct Replies {
    pub(crate) out: Vec<u8>,
    colors: Colors,
    title: Option<String>,
    return_to_list: bool,
    /// Pre-update screen held until synchronized output ends or times out.
    frozen: Option<(Instant, vt100::Screen)>,
}

impl Replies {
    pub(crate) fn new(colors: Colors) -> Self {
        Self {
            out: Vec::new(),
            colors,
            title: None,
            return_to_list: false,
            frozen: None,
        }
    }

    fn shown<'a>(&'a self, live: &'a vt100::Screen) -> &'a vt100::Screen {
        match &self.frozen {
            Some((since, screen)) if since.elapsed() < SYNC_MAX => screen,
            _ => live,
        }
    }
}

impl vt100::Callbacks for Replies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        match (i1, i2, params, c) {
            (None, None, _, 'c') => self.out.extend_from_slice(b"\x1b[?62;22c"),
            (None, None, [[6]], 'n') => {
                let (row, col) = screen.cursor_position();
                self.out
                    .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            (Some(b'>'), None, [[0]], 'q') => self.out.extend_from_slice(
                format!("\x1bP>|cones {}\x1b\\", env!("CARGO_PKG_VERSION")).as_bytes(),
            ),
            (Some(b'?'), None, [[2026]], 'h') => {
                if self.frozen.is_none() {
                    self.frozen = Some((Instant::now(), screen.clone()));
                }
            }
            (Some(b'?'), None, [[2026]], 'l') => self.frozen = None,
            _ => {}
        }
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        let reply = match params {
            [b"777", b"cones", b"return"] => {
                self.return_to_list = true;
                return;
            }
            [b"10", b"?"] => format!("\x1b]10;{}\x1b\\", self.colors.fg),
            [b"11", b"?"] => format!("\x1b]11;{}\x1b\\", self.colors.bg),
            _ => return,
        };
        self.out.extend_from_slice(reply.as_bytes());
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
    }
}

pub struct Viewer {
    child: Child,
    reaped: bool,
    master: File,
    master_open: bool,
    stderr: Option<ChildStderr>,
    errors: Vec<u8>,
    parser: vt100::Parser<Replies>,
    /// The tail of the last read that ended inside a UTF-8 sequence, fed first next time.
    partial: Vec<u8>,
    pending_input: Vec<u8>,
    spawned: Instant,
    first_paint: Option<Duration>,
    status: Option<ExitStatus>,
}

fn nonblocking(fd: i32) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Do not let later harness children inherit PTY descriptors and keep viewers alive.
fn cloexec(fd: i32) -> io::Result<()> {
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(2),
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

impl Viewer {
    /// Spawn with shell termios and default signals. Capture stderr separately from the screen.
    pub fn spawn(
        command: Command,
        rows: u16,
        cols: u16,
        normal: Option<&libc::termios>,
        colors: Colors,
    ) -> io::Result<Viewer> {
        Self::spawn_pty(command, rows, cols, normal, colors, false)
    }

    /// Shell prompts and command errors belong on the same terminal as stdout.
    pub fn spawn_terminal(
        command: Command,
        rows: u16,
        cols: u16,
        normal: Option<&libc::termios>,
        colors: Colors,
    ) -> io::Result<Viewer> {
        Self::spawn_pty(command, rows, cols, normal, colors, true)
    }

    fn spawn_pty(
        mut command: Command,
        rows: u16,
        cols: u16,
        normal: Option<&libc::termios>,
        colors: Colors,
        terminal: bool,
    ) -> io::Result<Viewer> {
        let (mut master, mut slave) = (-1, -1);
        let mut size = winsize(rows, cols);
        let mut normal = normal.copied();
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                normal.as_mut().map_or(std::ptr::null_mut(), |t| t),
                &mut size,
            )
        };
        if opened < 0 {
            return Err(io::Error::last_os_error());
        }
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        cloexec(master.as_raw_fd())?;
        cloexec(slave.as_raw_fd())?;
        command
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(if terminal {
                Stdio::from(slave.try_clone()?)
            } else {
                Stdio::piped()
            });
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
        let spawned = Instant::now();
        let mut child = command.spawn()?;
        drop(slave);
        nonblocking(master.as_raw_fd())?;
        let stderr = child.stderr.take();
        if let Some(stderr) = &stderr {
            nonblocking(stderr.as_raw_fd())?;
        }
        Ok(Viewer {
            child,
            reaped: false,
            master,
            master_open: true,
            stderr,
            errors: Vec::new(),
            parser: vt100::Parser::new_with_callbacks(
                size.ws_row,
                size.ws_col,
                SCROLLBACK,
                Replies::new(colors),
            ),
            partial: Vec::new(),
            pending_input: Vec::new(),
            spawned,
            first_paint: None,
            status: None,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    fn kill(&mut self) {
        if !self.reaped {
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            let _ = self.child.kill();
        }
    }

    /// Pump output, replies and queued input; return whether the screen changed.
    /// Kill stopped clients without affecting the daemon-owned agent.
    pub fn pump(&mut self) -> io::Result<bool> {
        if self.status.is_none() {
            let mut status = 0;
            let waited = unsafe {
                libc::waitpid(
                    self.child.id() as i32,
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
                    self.kill();
                } else {
                    self.reaped = true;
                    self.status = Some(ExitStatus::from_raw(status));
                }
            }
        }
        let mut dirty = false;
        let mut bytes = [0u8; 8192];
        let started = Instant::now();
        while self.master_open {
            match self.master.read(&mut bytes) {
                Ok(0) => self.master_open = false,
                Ok(n) => {
                    self.ingest(&bytes[..n]);
                    dirty = true;
                }
                Err(e) if e.raw_os_error() == Some(libc::EIO) => self.master_open = false,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if started.elapsed() >= PUMP_MAX
                        || !readable(self.master.as_raw_fd(), PUMP_WAIT)
                    {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        // After the exit, drain what is there and stop: a descendant that inherited stderr
        // is not waited for.
        while let Some(stderr) = &mut self.stderr {
            match stderr.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => {
                    self.errors.extend_from_slice(&bytes[..n]);
                    if self.errors.len() > STDERR_TAIL {
                        let extra = self.errors.len() - STDERR_TAIL;
                        self.errors.drain(..extra);
                        if self.status.is_some() {
                            break;
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        self.flush();
        Ok(dirty && self.parser.callbacks().frozen.is_none())
    }

    /// Feed complete UTF-8 code points: splitting a two-byte character across vte calls
    /// can discard the following ASCII byte.
    fn ingest(&mut self, read: &[u8]) {
        let mut chunk = std::mem::take(&mut self.partial);
        chunk.extend_from_slice(read);
        let keep = utf8_tail(&chunk);
        self.partial = chunk.split_off(chunk.len() - keep);
        if chunk.is_empty() {
            return;
        }
        if self.first_paint.is_none() && has_text(&chunk) {
            self.first_paint = Some(self.spawned.elapsed());
        }
        self.parser.process(&chunk);
        let replies = std::mem::take(&mut self.parser.callbacks_mut().out);
        self.pending_input.extend_from_slice(&replies);
    }

    fn drain(&mut self) {
        while !self.pending_input.is_empty() && self.master_open && self.status.is_none() {
            match self.master.write(&self.pending_input) {
                Ok(0) => break,
                Ok(n) => {
                    self.pending_input.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }

    /// Queue input; terminate a viewer that exceeds `INPUT_CAP` without consuming it.
    pub fn write(&mut self, bytes: &[u8]) {
        self.parser.screen_mut().set_scrollback(0);
        self.pending_input.extend_from_slice(bytes);
        self.flush();
    }

    /// A shell widget requests navigation only after checking its live editor buffer.
    pub fn take_return_to_list(&mut self) -> bool {
        std::mem::take(&mut self.parser.callbacks_mut().return_to_list)
    }

    /// Write queued input, and stop a viewer that has let it pile past `INPUT_CAP`,
    /// whether the queue is user keys or the parser's replies to the client's queries.
    fn flush(&mut self) {
        self.drain();
        if self.pending_input.len() > INPUT_CAP {
            self.pending_input.clear();
            self.errors
                .extend_from_slice(b"\nviewer input queue exceeded 8 MiB\n");
            self.kill();
        }
    }

    /// Scroll back by `lines` (negative moves forward); return whether the view moved.
    pub fn scroll(&mut self, lines: i32) -> bool {
        let before = self.parser.screen().scrollback();
        let after = (before as i64 + i64::from(lines)).clamp(0, SCROLLBACK as i64);
        self.parser.screen_mut().set_scrollback(after as usize);
        self.parser.screen().scrollback() != before
    }

    /// Resize both emulator and PTY, sending SIGWINCH only when the size changes.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let size = winsize(rows, cols);
        if self.parser.screen().size() == (size.ws_row, size.ws_col) {
            return;
        }
        self.parser.screen_mut().set_size(size.ws_row, size.ws_col);
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size);
        }
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.callbacks().shown(self.parser.screen())
    }

    pub fn exited(&self) -> Option<ExitStatus> {
        self.status
    }

    pub fn stderr_tail(&self) -> &[u8] {
        &self.errors
    }

    pub fn first_paint(&self) -> Option<Duration> {
        self.first_paint
    }

    pub fn title(&self) -> Option<&str> {
        self.parser.callbacks().title.as_deref()
    }
}

const REAP: Duration = Duration::from_secs(2);

impl Drop for Viewer {
    /// Drain the PTY while reaping: macOS can keep a killed leader in exit state
    /// until its unread output is consumed, blocking a plain wait.
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        self.kill();
        let pid = self.child.id() as i32;
        let deadline = Instant::now() + REAP;
        let mut bytes = [0u8; 8192];
        loop {
            let mut status = 0;
            let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            let interrupted =
                waited < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted;
            if waited != 0 && !interrupted {
                return;
            }
            while self.master_open {
                match self.master.read(&mut bytes) {
                    Ok(0) => self.master_open = false,
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => self.master_open = false,
                }
            }
            if let Some(stderr) = &mut self.stderr {
                while let Ok(n) = stderr.read(&mut bytes) {
                    if n == 0 {
                        break;
                    }
                }
            }
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn readable(fd: i32, timeout: Duration) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut poll, 1, timeout.as_millis() as libc::c_int) > 0 }
}

fn utf8_tail(bytes: &[u8]) -> usize {
    for back in 1..=3.min(bytes.len()) {
        let b = bytes[bytes.len() - back];
        if b & 0xc0 == 0x80 {
            continue;
        }
        let need = match b {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => return 0,
        };
        return if back < need { back } else { 0 };
    }
    0
}

/// Approximate first text for timing; a split escape sequence may count one read early.
fn has_text(chunk: &[u8]) -> bool {
    let mut i = 0;
    while i < chunk.len() {
        let b = chunk[i];
        if b == 0x1b {
            i += 1;
            match chunk.get(i) {
                Some(b'[') => {
                    i += 1;
                    while i < chunk.len() && !(0x40..=0x7e).contains(&chunk[i]) {
                        i += 1;
                    }
                }
                Some(b']' | b'P' | b'_' | b'^') => {
                    i += 1;
                    while i < chunk.len() && chunk[i] != 0x07 && chunk[i] != 0x1b {
                        i += 1;
                    }
                    if chunk.get(i) == Some(&0x1b) {
                        i += 1;
                    }
                }
                // ESC, intermediates, one final byte: charset designations and the like.
                Some(_) => {
                    while i < chunk.len() && (0x20..=0x2f).contains(&chunk[i]) {
                        i += 1;
                    }
                }
                None => return false,
            }
            i += 1;
        } else if b >= 0x20 && b != 0x7f {
            return true;
        } else {
            i += 1;
        }
    }
    false
}

fn color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Render clipped cells; the caller draws the real terminal cursor.
pub fn render(screen: &vt100::Screen, area: Rect, buf: &mut Buffer) {
    let (rows, cols) = screen.size();
    for row in 0..rows.min(area.height) {
        for col in 0..cols.min(area.width) {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            let Some(target) = buf.cell_mut((area.x + col, area.y + row)) else {
                continue;
            };
            let mut style = Style::reset()
                .fg(color(cell.fgcolor()))
                .bg(color(cell.bgcolor()));
            for (on, modifier) in [
                (cell.bold(), Modifier::BOLD),
                (cell.dim(), Modifier::DIM),
                (cell.italic(), Modifier::ITALIC),
                (cell.underline(), Modifier::UNDERLINED),
                (cell.inverse(), Modifier::REVERSED),
            ] {
                if on {
                    style = style.add_modifier(modifier);
                }
            }
            target.set_style(style);
            if cell.is_wide_continuation() {
                target.set_symbol("");
            } else if cell.has_contents() {
                target.set_symbol(cell.contents());
            } else {
                target.set_symbol(" ");
            }
        }
    }
}

/// The xterm modifier parameter: 1 plus shift, alt and control bits.
fn modifier_param(mods: KeyModifiers) -> u8 {
    1 + u8::from(mods.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(mods.contains(KeyModifiers::ALT))
        + 4 * u8::from(mods.contains(KeyModifiers::CONTROL))
}

/// True when the caret sits at an empty prompt: nothing to its left on that row
/// but box art, a prompt marker and blanks. A client with text typed, or drawing
/// a full-screen view, fails the test and keeps the key.
pub fn at_empty_prompt(screen: &vt100::Screen, input: &crate::harness::spec::Input) -> bool {
    if input.empty_prompt == crate::harness::spec::EmptyPrompt::Bordered {
        return at_empty_bordered_prompt(screen);
    }
    if screen.hide_cursor() || screen.scrollback() != 0 {
        return false;
    }
    let (row, col) = screen.cursor_position();
    let left = screen.contents_between(row, 0, row, col);
    // Codex sprinkles braille dots around its composer as an ambient animation; one
    // can land between the marker and the caret, so they count as blanks.
    let mut marks = left
        .chars()
        .filter(|c| {
            !c.is_whitespace() && !(input.ignore_braille && ('\u{2800}'..='\u{28ff}').contains(c))
        })
        .peekable();
    // ponytail: a marker has to be there, so a full-screen client parking the caret
    // on blank space keeps its key. Add a client's marker here when one is missing.
    marks.peek().is_some() && marks.all(|c| input.markers.contains(c))
}

/// A definition owns which native keys return to cones and the screen condition for each.
pub fn returns_to_list(
    screen: &vt100::Screen,
    input: &crate::harness::spec::Input,
    code: KeyCode,
    mods: KeyModifiers,
) -> bool {
    use crate::harness::spec::{ReturnKey, ReturnWhen};
    input.return_to_list.iter().any(|binding| {
        let matches = match binding.key {
            ReturnKey::CtrlZ => code == KeyCode::Char('z') && mods == KeyModifiers::CONTROL,
            ReturnKey::Tab => code == KeyCode::Tab && mods.is_empty(),
            ReturnKey::Left => code == KeyCode::Left && mods.is_empty(),
        };
        matches
            && match binding.when {
                ReturnWhen::Always => true,
                ReturnWhen::EmptyPrompt => at_empty_prompt(screen, input),
            }
    })
}

/// pi's standard editor has horizontal borders, no prompt marker, and an inverse-video
/// software caret. Its terminal cursor is positioned there even when hidden.
fn at_empty_bordered_prompt(screen: &vt100::Screen) -> bool {
    let (row, col) = screen.cursor_position();
    let (height, width) = screen.size();
    if screen.scrollback() != 0 || row == 0 || row + 1 >= height {
        return false;
    }
    let line = |r| screen.contents_between(r, 0, r, width);
    let top = line(row - 1);
    let bottom = line(row + 1);
    // One empty editor line only. A wrapped/multiline draft, completion menu or another
    // component keeps Left; a hidden hardware caret alone is not an input report.
    top.starts_with("──")
        && top.ends_with('─')
        && bottom.chars().count() == usize::from(width)
        && bottom.chars().all(|c| c == '─')
        && line(row).trim().is_empty()
        && screen
            .cell(row, col)
            .is_some_and(|c| c.inverse() && c.contents().trim().is_empty())
}

/// Classic xterm encoding. DECCKM selects SS3 arrows/Home/End; control digits follow
/// xterm's table, matching crossterm's decoding of bytes 0x1c to 0x1f.
pub fn encode_key(code: KeyCode, mods: KeyModifiers, app_cursor: bool) -> Vec<u8> {
    let alt = mods.contains(KeyModifiers::ALT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let m = modifier_param(mods);
    let with_alt = |mut bytes: Vec<u8>| {
        if alt {
            bytes.insert(0, 0x1b);
        }
        bytes
    };
    let csi = |plain: &str, number: u8, final_byte: char| -> Vec<u8> {
        if m == 1 {
            plain.as_bytes().to_vec()
        } else {
            format!("\x1b[{number};{m}{final_byte}").into_bytes()
        }
    };
    let cursor = |letter: char| -> Vec<u8> {
        if m > 1 {
            format!("\x1b[1;{m}{letter}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    match code {
        KeyCode::Char(c) if ctrl => {
            let byte = match c.to_ascii_lowercase() {
                l @ 'a'..='z' => l as u8 - b'a' + 1,
                '@' | ' ' | '2' => 0,
                '[' | '3' => 27,
                '\\' | '4' => 28,
                ']' | '5' => 29,
                '^' | '6' => 30,
                '_' | '7' | '/' => 31,
                '?' | '8' => 127,
                other => return with_alt(other.to_string().into_bytes()),
            };
            with_alt(vec![byte])
        }
        KeyCode::Char(c) => with_alt(c.to_string().into_bytes()),
        KeyCode::Enter => with_alt(b"\r".to_vec()),
        KeyCode::Tab => with_alt(b"\t".to_vec()),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => with_alt(vec![0x7f]),
        KeyCode::Esc => with_alt(vec![0x1b]),
        KeyCode::Delete => csi("\x1b[3~", 3, '~'),
        KeyCode::Insert => csi("\x1b[2~", 2, '~'),
        KeyCode::PageUp => csi("\x1b[5~", 5, '~'),
        KeyCode::PageDown => csi("\x1b[6~", 6, '~'),
        KeyCode::Home => cursor('H'),
        KeyCode::End => cursor('F'),
        KeyCode::Up => cursor('A'),
        KeyCode::Down => cursor('B'),
        KeyCode::Right => cursor('C'),
        KeyCode::Left => cursor('D'),
        KeyCode::F(n @ 1..=4) => {
            let letter = (b'P' + n - 1) as char;
            if m == 1 {
                format!("\x1bO{letter}").into_bytes()
            } else {
                format!("\x1b[1;{m}{letter}").into_bytes()
            }
        }
        KeyCode::F(n @ 5..=12) => {
            let number = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                _ => 24,
            };
            csi(&format!("\x1b[{number}~"), number, '~')
        }
        _ => Vec::new(),
    }
}

/// SGR mouse encoding with 1-based coordinates relative to `origin`, filtered by reporting mode.
pub fn encode_mouse(ev: MouseEvent, origin: (u16, u16), mode: MouseProtocolMode) -> Vec<u8> {
    use MouseProtocolMode as M;
    if mode == M::None {
        return Vec::new();
    }
    let button = |b: MouseButton| match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (code, release) = match ev.kind {
        MouseEventKind::Down(b) => (button(b), false),
        MouseEventKind::Up(b) if mode != M::Press => (button(b), true),
        MouseEventKind::Drag(b) if matches!(mode, M::ButtonMotion | M::AnyMotion) => {
            (button(b) + 32, false)
        }
        MouseEventKind::Moved if mode == M::AnyMotion => (3 + 32, false),
        MouseEventKind::ScrollUp if mode != M::Press => (64, false),
        MouseEventKind::ScrollDown if mode != M::Press => (65, false),
        MouseEventKind::ScrollLeft if mode != M::Press => (66, false),
        MouseEventKind::ScrollRight if mode != M::Press => (67, false),
        _ => return Vec::new(),
    };
    let mods = 4 * u8::from(ev.modifiers.contains(KeyModifiers::SHIFT))
        + 8 * u8::from(ev.modifiers.contains(KeyModifiers::ALT))
        + 16 * u8::from(ev.modifiers.contains(KeyModifiers::CONTROL));
    let x = ev.column.saturating_sub(origin.0) + 1;
    let y = ev.row.saturating_sub(origin.1) + 1;
    format!(
        "\x1b[<{};{x};{y}{}",
        code + mods,
        if release { 'm' } else { 'M' }
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_has_three_tty_streams_and_displays_stderr_in_order() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "test -t 0 && test -t 1 && test -t 2 || exit 1; printf 'out-'; printf 'err-' >&2; printf 'end'",
        ]);
        let mut viewer = Viewer::spawn_terminal(command, 4, 80, None, Colors::default()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while viewer.exited().is_none() {
            viewer.pump().unwrap();
            assert!(Instant::now() < deadline, "terminal did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(viewer.exited().unwrap().success());
        assert!(viewer.screen().contents().contains("out-err-end"));
        assert!(viewer.stderr_tail().is_empty());
    }

    fn text(screen: &vt100::Screen, row: u16) -> String {
        let (_, cols) = screen.size();
        (0..cols)
            .filter_map(|c| screen.cell(row, c))
            .map(|c| c.contents())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    #[test]
    fn a_pty_nobody_sized_wraps_without_panicking() {
        // A zero-size pty clamps to one row, and then every wrap scrolls the row
        // being written off the top, leaving none to mark as wrapped.
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "printf 'aaaaaaaaaa'; sleep 0.2"]);
        let mut v = Viewer::spawn(c, 0, 0, None, Colors::default()).unwrap();
        assert_eq!(v.screen().size(), (1, 2), "one row is the clamped floor");
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            v.pump().unwrap();
            if v.screen().contents().contains('a') {
                return;
            }
        }
        panic!("the child never drew");
    }

    #[test]
    fn a_viewer_draws_on_the_emulated_screen_and_its_cursor_query_is_answered() {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "printf 'hello\\033[6n'; sleep 0.2"]);
        let mut v = Viewer::spawn(c, 4, 20, None, Colors::default()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while text(v.screen(), 0) != "hello" {
            v.pump().unwrap();
            assert!(
                Instant::now() < deadline,
                "screen: {:?}",
                v.screen().contents()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(v.first_paint().is_some());
        assert_eq!(v.screen().cursor_position(), (0, 5));
        assert!(v.pending_input.is_empty());
        assert!(v.exited().is_none());
        while v.exited().is_none() {
            v.pump().unwrap();
            assert!(Instant::now() < deadline, "the viewer did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(v.exited().unwrap().success());
    }

    #[test]
    fn a_flooding_viewer_nobody_pumped_closes_at_once() {
        let mut c = Command::new("/bin/sh");
        c.args([
            "-c",
            "while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done",
        ]);
        let v = Viewer::spawn(c, 4, 20, None, Colors::default()).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        std::thread::spawn(move || {
            drop(v);
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("drop did not return");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "close took {:?}: the child was not reaped once its output was drained",
            started.elapsed()
        );
    }

    #[test]
    fn replies_answer_cursor_position_device_attributes_version_and_colors_but_not_kitty() {
        let mut p = vt100::Parser::new_with_callbacks(4, 20, 0, Replies::new(Colors::default()));
        p.process(b"ab\x1b[6n");
        assert_eq!(p.callbacks().out, b"\x1b[1;3R");
        p.callbacks_mut().out.clear();
        p.process(b"\x1b[c\x1b[?u\x1b[>0q");
        let out = String::from_utf8(p.callbacks().out.clone()).unwrap();
        assert!(out.starts_with("\x1b[?62;22c"), "{out:?}");
        assert!(out.ends_with("\x1b\\"), "{out:?}");
        assert!(out.contains("\x1bP>|cones "), "{out:?}");
        assert!(
            !out.contains("u"),
            "the kitty query is left unanswered: {out:?}"
        );
        p.callbacks_mut().out.clear();
        p.process(b"\x1b]10;?\x1b\\\x1b]11;?\x07\x1b]2;a title\x07");
        assert_eq!(
            String::from_utf8(p.callbacks().out.clone()).unwrap(),
            "\x1b]10;rgb:e4e4/e4e4/e4e4\x1b\\\x1b]11;rgb:1414/1414/1414\x1b\\"
        );
        assert_eq!(p.callbacks().title.as_deref(), Some("a title"));
    }

    #[test]
    fn render_keeps_wide_continuations_empty_and_carries_color_and_weight() {
        let mut p = vt100::Parser::new(2, 10, 0);
        p.process(b"a\xe4\xb8\xadb \x1b[1;31mR");
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 2));
        render(p.screen(), buf.area, &mut buf);
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "a");
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), "中");
        assert_eq!(buf.cell((2, 0)).unwrap().symbol(), "");
        assert_eq!(buf.cell((3, 0)).unwrap().symbol(), "b");
        let r = buf.cell((5, 0)).unwrap();
        assert_eq!(r.symbol(), "R");
        assert_eq!(r.fg, Color::Indexed(1));
        assert!(r.modifier.contains(Modifier::BOLD));
        assert_eq!(buf.cell((6, 0)).unwrap().symbol(), " ");
    }

    #[test]
    fn keys_are_encoded_as_a_classic_xterm_sends_them() {
        use KeyModifiers as K;
        let plain = |code| encode_key(code, K::NONE, false);
        assert_eq!(plain(KeyCode::Char('a')), b"a");
        assert_eq!(plain(KeyCode::Char('é')), "é".as_bytes());
        assert_eq!(encode_key(KeyCode::Char('c'), K::CONTROL, false), [3]);
        assert_eq!(encode_key(KeyCode::Char('z'), K::CONTROL, false), [26]);
        assert_eq!(encode_key(KeyCode::Char(' '), K::CONTROL, false), [0]);
        assert_eq!(encode_key(KeyCode::Char('?'), K::CONTROL, false), [127]);
        assert_eq!(encode_key(KeyCode::Char('['), K::CONTROL, false), [27]);
        assert_eq!(encode_key(KeyCode::Char('4'), K::CONTROL, false), [28]);
        assert_eq!(encode_key(KeyCode::Char('7'), K::CONTROL, false), [31]);
        assert_eq!(encode_key(KeyCode::Char('/'), K::CONTROL, false), [31]);
        assert_eq!(encode_key(KeyCode::Char('8'), K::CONTROL, false), [127]);
        assert_eq!(encode_key(KeyCode::Char('9'), K::CONTROL, false), b"9");
        assert_eq!(encode_key(KeyCode::Char('x'), K::ALT, false), b"\x1bx");
        assert_eq!(plain(KeyCode::Enter), b"\r");
        assert_eq!(encode_key(KeyCode::Enter, K::ALT, false), b"\x1b\r");
        assert_eq!(plain(KeyCode::Tab), b"\t");
        assert_eq!(encode_key(KeyCode::Tab, K::ALT, false), b"\x1b\t");
        assert_eq!(plain(KeyCode::BackTab), b"\x1b[Z");
        assert_eq!(plain(KeyCode::Backspace), [0x7f]);
        assert_eq!(plain(KeyCode::Esc), [0x1b]);
        assert_eq!(encode_key(KeyCode::Esc, K::ALT, false), b"\x1b\x1b");
        assert_eq!(plain(KeyCode::Delete), b"\x1b[3~");
        assert_eq!(encode_key(KeyCode::Delete, K::SHIFT, false), b"\x1b[3;2~");
        assert_eq!(plain(KeyCode::Up), b"\x1b[A");
        assert_eq!(encode_key(KeyCode::Up, K::NONE, true), b"\x1bOA");
        assert_eq!(encode_key(KeyCode::Left, K::CONTROL, true), b"\x1b[1;5D");
        assert_eq!(
            encode_key(KeyCode::Right, K::CONTROL | K::SHIFT | K::ALT, false),
            b"\x1b[1;8C"
        );
        assert_eq!(plain(KeyCode::Home), b"\x1b[H");
        assert_eq!(encode_key(KeyCode::End, K::NONE, true), b"\x1bOF");
        assert_eq!(plain(KeyCode::PageDown), b"\x1b[6~");
        assert_eq!(plain(KeyCode::F(1)), b"\x1bOP");
        assert_eq!(encode_key(KeyCode::F(4), K::SHIFT, false), b"\x1b[1;2S");
        assert_eq!(plain(KeyCode::F(5)), b"\x1b[15~");
        assert_eq!(plain(KeyCode::F(11)), b"\x1b[23~");
        assert_eq!(encode_key(KeyCode::F(12), K::CONTROL, false), b"\x1b[24;5~");
        assert!(plain(KeyCode::Null).is_empty());
        assert!(plain(KeyCode::Menu).is_empty());
    }

    #[test]
    fn mouse_reports_take_the_sgr_form_and_follow_what_the_viewer_asked_for() {
        let at = |kind, column, row, modifiers| MouseEvent {
            kind,
            column,
            row,
            modifiers,
        };
        let click = at(
            MouseEventKind::Down(MouseButton::Left),
            4,
            2,
            KeyModifiers::NONE,
        );
        assert_eq!(
            encode_mouse(click, (0, 0), MouseProtocolMode::PressRelease),
            b"\x1b[<0;5;3M"
        );
        assert_eq!(
            encode_mouse(click, (2, 1), MouseProtocolMode::Press),
            b"\x1b[<0;3;2M",
            "relative to the pane"
        );
        assert!(encode_mouse(click, (0, 0), MouseProtocolMode::None).is_empty());
        let release = at(
            MouseEventKind::Up(MouseButton::Right),
            0,
            0,
            KeyModifiers::CONTROL,
        );
        assert_eq!(
            encode_mouse(release, (0, 0), MouseProtocolMode::PressRelease),
            b"\x1b[<18;1;1m"
        );
        assert!(encode_mouse(release, (0, 0), MouseProtocolMode::Press).is_empty());
        let wheel = at(MouseEventKind::ScrollDown, 9, 9, KeyModifiers::SHIFT);
        assert_eq!(
            encode_mouse(wheel, (0, 0), MouseProtocolMode::ButtonMotion),
            b"\x1b[<69;10;10M"
        );
        let drag = at(
            MouseEventKind::Drag(MouseButton::Left),
            1,
            1,
            KeyModifiers::NONE,
        );
        assert!(encode_mouse(drag, (0, 0), MouseProtocolMode::PressRelease).is_empty());
        assert_eq!(
            encode_mouse(drag, (0, 0), MouseProtocolMode::ButtonMotion),
            b"\x1b[<32;2;2M"
        );
        let moved = at(MouseEventKind::Moved, 1, 1, KeyModifiers::NONE);
        assert!(encode_mouse(moved, (0, 0), MouseProtocolMode::ButtonMotion).is_empty());
        assert_eq!(
            encode_mouse(moved, (0, 0), MouseProtocolMode::AnyMotion),
            b"\x1b[<35;2;2M"
        );
    }

    #[test]
    fn color_replies_parse_with_bel_or_st_and_missing_ones_stay_default() {
        let (fg, bg) =
            parse_color_replies(b"\x1b]10;rgb:ffff/ffff/ffff\x07\x1b]11;rgb:0000/0000/1111\x1b\\");
        assert_eq!(fg.as_deref(), Some("rgb:ffff/ffff/ffff"));
        assert_eq!(bg.as_deref(), Some("rgb:0000/0000/1111"));
        let (fg, bg) = parse_color_replies(b"junk\x1b]11;rgb:1/2/3\x07");
        assert_eq!(fg, None);
        assert_eq!(bg.as_deref(), Some("rgb:1/2/3"));
        assert_eq!(parse_color_replies(b"\x1b]10;#ffffff\x07"), (None, None));
    }

    #[test]
    fn a_read_that_ends_inside_a_code_point_keeps_its_tail_for_the_next_one() {
        assert_eq!(utf8_tail(b"abc"), 0);
        assert_eq!(utf8_tail(b"ab\xc2"), 1);
        assert_eq!(utf8_tail(b"ab\xc2\xb7"), 0);
        assert_eq!(utf8_tail(b"\xe2\x86"), 2);
        assert_eq!(utf8_tail(b"\xf0\x9f\x98"), 3);
        assert_eq!(utf8_tail(b"\xf0\x9f\x98\x80"), 0);
        assert_eq!(
            utf8_tail(b"\x80\x80\x80\x80"),
            0,
            "malformed bytes are not held"
        );
        assert_eq!(utf8_tail(b""), 0);
        // Regression: splitting a two-byte character can drop the next ASCII byte in vte.
        let mut broken = vt100::Parser::new(1, 10, 0);
        broken.process(b"\xc2");
        broken.process(b"\xb7 \xe2\x86\x90");
        let mut whole = vt100::Parser::new(1, 10, 0);
        whole.process(b"\xc2\xb7 \xe2\x86\x90");
        assert_eq!(text(whole.screen(), 0), "· ←");
        if text(broken.screen(), 0) == "· ←" {
            eprintln!("vte no longer splits code points; utf8_tail can go");
        }
    }

    #[test]
    fn a_synchronized_update_is_shown_whole_and_not_while_it_is_drawn() {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "printf 'one\\033[?2026h\\033[H\\033[2Ktwo\\033[10;10H'; sleep 0.3; printf '\\033[1;4H\\033[?2026l'; sleep 0.2"]);
        let mut v = Viewer::spawn(c, 12, 20, None, Colors::default()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while text(v.screen(), 0) != "one" {
            v.pump().unwrap();
            assert!(Instant::now() < deadline, "{:?}", v.screen().contents());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(v.parser.callbacks().frozen.is_some());
        assert_eq!(text(v.screen(), 0), "one");
        assert_eq!(v.screen().cursor_position(), (0, 3));
        assert_eq!(text(v.parser.screen(), 0), "two");
        while v.parser.callbacks().frozen.is_some() {
            let dirty = v.pump().unwrap();
            if v.parser.callbacks().frozen.is_some() {
                assert!(!dirty, "a frame still being drawn is not a change");
            }
            assert!(Instant::now() < deadline, "the update never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(text(v.screen(), 0), "two");
        assert_eq!(v.screen().cursor_position(), (0, 3));
    }

    #[test]
    fn a_burst_lands_in_a_few_pumps_not_a_kib_per_turn() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("burst");
        let lines: String = (1..=20000).map(|n| format!("L{n}\n")).collect();
        std::fs::write(&file, lines).unwrap();
        let mut c = Command::new("/bin/sh");
        c.args([
            "-c",
            &format!(
                "sleep 0.2; while read l; do echo \"$l\"; done < {}",
                file.display()
            ),
        ]);
        let mut v = Viewer::spawn(c, 12, 20, None, Colors::default()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut dirty_pumps = 0;
        while text(v.screen(), 10) != "L20000" {
            if v.pump().unwrap() {
                dirty_pumps += 1;
            }
            assert!(Instant::now() < deadline, "{:?}", v.screen().contents());
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(dirty_pumps <= 8, "{dirty_pumps} pumps for a 120 KiB burst");
    }

    #[test]
    fn a_synchronized_update_that_never_ends_is_drawn_after_sync_max() {
        let mut p = vt100::Parser::new_with_callbacks(4, 20, 0, Replies::new(Colors::default()));
        p.process(b"one\x1b[?2026h\x1b[H\x1b[2Ktwo");
        assert_eq!(text(p.callbacks().shown(p.screen()), 0), "one");
        p.callbacks_mut().frozen.as_mut().unwrap().0 = Instant::now() - SYNC_MAX;
        assert_eq!(text(p.callbacks().shown(p.screen()), 0), "two");
    }

    #[test]
    fn the_wheel_scrolls_back_through_what_left_the_top_and_input_comes_back_down() {
        let mut c = Command::new("/bin/sh");
        c.args([
            "-c",
            "for i in 1 2 3 4 5 6 7 8; do echo line$i; done; sleep 1",
        ]);
        let mut v = Viewer::spawn(c, 4, 20, None, Colors::default()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while text(v.screen(), 2) != "line8" {
            v.pump().unwrap();
            assert!(Instant::now() < deadline, "{:?}", v.screen().contents());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(v.scroll(2));
        assert_eq!(text(v.screen(), 0), "line4");
        assert!(v.scroll(100), "clamped to the five lines kept");
        assert_eq!(text(v.screen(), 0), "line1");
        assert!(!v.scroll(1), "nothing further back");
        assert!(v.scroll(-1));
        assert_eq!(text(v.screen(), 0), "line2");
        v.write(b"x");
        assert_eq!(v.screen().scrollback(), 0);
        assert_eq!(text(v.screen(), 2), "line8");
        assert!(!v.scroll(-1), "already at the bottom");
    }

    #[test]
    fn history_above_a_fixed_prompt_stays_in_scrollback() {
        for (alternate, top, expected) in [(false, 1, 2), (true, 1, 0), (false, 2, 0)] {
            let mut p = vt100::Parser::new(4, 20, 10);
            if alternate {
                p.process(b"\x1b[?1049h");
            }
            p.process(b"\x1b[31mline1\x1b[m\r\nline2\r\nline3\r\nPROMPT");
            // Codex writes history in a region starting at the top, leaving its prompt
            // below the region. Only lines leaving the normal screen's top are history.
            p.process(format!("\x1b[{top};3r\x1b[3;1H\r\nline4\r\nline5\x1b[r").as_bytes());
            assert_eq!(text(p.screen(), 3), "PROMPT");
            p.screen_mut().set_scrollback(10);
            assert_eq!(
                p.screen().scrollback(),
                expected,
                "alternate={alternate}, top={top}"
            );
            if expected > 0 {
                assert_eq!(text(p.screen(), 0), "line1");
                assert_eq!(text(p.screen(), 1), "line2");
                assert_eq!(
                    p.screen().cell(0, 0).unwrap().fgcolor(),
                    vt100::Color::Idx(1)
                );
                p.screen_mut().set_scrollback(0);
                assert_eq!(text(p.screen(), 0), "line3");
                assert_eq!(text(p.screen(), 3), "PROMPT");
            }
        }
    }

    #[test]
    fn empty_prompt_is_the_caret_behind_box_art_and_a_marker_only() {
        let at = |bytes: &[u8]| {
            let mut p = vt100::Parser::new(3, 20, 0);
            p.process(bytes);
            at_empty_prompt(
                p.screen(),
                &crate::harness::spec(crate::config::HarnessKind::Claude).input,
            )
        };
        assert!(at("\u{2502} > ".as_bytes()));
        assert!(at(b"$ "));
        assert!(!at("\u{2502} > hi".as_bytes()));
        // Codex's ambient braille sparkle drifting over the composer row.
        assert!(at("\u{203a}\u{2801} \u{2808}\x1b[2G".as_bytes()));
        assert!(!at(b"\x1b[?25l$ "));
        assert!(!at(b"> hi\x1b[H"));
        // A full-screen client parks the caret mid-screen, not behind a marker.
        assert!(!at(b"\x1b[2J\x1b[Hlines here\x1b[2;4H"));

        let input = &crate::harness::spec(crate::config::HarnessKind::Claude).input;
        let mut p = vt100::Parser::new(3, 20, 10);
        p.process(b"history\r\n> \r\n> \r\n> ");
        assert!(at_empty_prompt(p.screen(), input));
        p.screen_mut().set_scrollback(1);
        assert!(
            !at_empty_prompt(p.screen(), input),
            "a historical prompt cannot return focus"
        );
    }

    #[test]
    fn pi_empty_editor_uses_its_software_cursor_and_borders() {
        let input = &crate::harness::spec(crate::config::HarnessKind::Pi).input;
        // pi-tui Editor.render: full-width horizontal rules, no marker or side borders,
        // inverse space at the caret. MainScreenTUI positions a usually-hidden hardware cursor.
        let rule = "─".repeat(20);
        let at = |text: &str, col: u16, hidden: bool| {
            let mut p = vt100::Parser::new(8, 20, 0);
            p.process(
                format!(
                    "\x1b[2;1H{rule}\x1b[3;1H{text}\x1b[4;1H{rule}\x1b[3;{col}H{}",
                    if hidden { "\x1b[?25l" } else { "\x1b[?25h" }
                )
                .as_bytes(),
            );
            at_empty_prompt(p.screen(), input)
        };
        assert!(at("\x1b[7m \x1b[0m", 1, true));
        assert!(at(" \x1b[7m \x1b[0m", 2, true), "editor padding");
        assert!(at(" \x1b[7m \x1b[0m", 2, false), "hardware cursor enabled");
        assert!(
            !at(" \x1b[7mx\x1b[0m", 2, true),
            "cursor at start of a draft"
        );
        assert!(
            !at(" hello\x1b[7m \x1b[0m", 7, true),
            "cursor at end of a draft"
        );
        assert!(
            !at(" \x1b[7m \x1b[0m hidden text", 2, true),
            "text after cursor"
        );
        assert!(!at("  ", 2, true), "blank content without the native caret");

        let mut p = vt100::Parser::new(8, 20, 0);
        p.process(
            format!(
                "\x1b[2;1H{rule}\x1b[3;1Hdraft\x1b[4;1H\x1b[7m \x1b[0m\x1b[5;1H{rule}\x1b[4;1H"
            )
            .as_bytes(),
        );
        assert!(
            !at_empty_prompt(p.screen(), input),
            "multiline draft keeps Left"
        );
        p.process(b"\x1b[2J\x1b[4;1H\x1b[7m \x1b[0m\x1b[4;1H");
        assert!(
            !at_empty_prompt(p.screen(), input),
            "a fullscreen view is not an editor"
        );
    }

    #[test]
    fn return_keys_and_their_conditions_come_from_the_harness_definition() {
        use crate::harness::spec::{HarnessSpec, ReturnKey, ReturnWhen};
        let mut definition =
            HarnessSpec::parse(include_str!("../assets/harnesses/claude.yaml")).unwrap();
        let mut screen = vt100::Parser::new(3, 40, 0);
        screen.process(b"> draft");
        let check = |input: &crate::harness::spec::Input, code, mods| {
            returns_to_list(screen.screen(), input, code, mods)
        };
        assert!(!check(&definition.input, KeyCode::Tab, KeyModifiers::NONE));
        assert!(check(
            &definition.input,
            KeyCode::Char('z'),
            KeyModifiers::CONTROL
        ));
        assert!(!check(&definition.input, KeyCode::Left, KeyModifiers::NONE));
        // A definition can opt into returning on Tab even while its input is populated.
        definition
            .input
            .return_to_list
            .iter_mut()
            .find(|b| b.key == ReturnKey::Tab)
            .unwrap()
            .when = ReturnWhen::Always;
        definition.validate().unwrap();
        assert!(check(&definition.input, KeyCode::Tab, KeyModifiers::NONE));
        assert!(check(
            &definition.input,
            KeyCode::Char('z'),
            KeyModifiers::CONTROL
        ));
        // Omitting a key leaves it to the client even on an otherwise-recognized prompt.
        definition
            .input
            .return_to_list
            .retain(|b| b.key != ReturnKey::Left);
        screen.process(b"\x1b[2J\x1b[H> ");
        assert!(!returns_to_list(
            screen.screen(),
            &definition.input,
            KeyCode::Left,
            KeyModifiers::NONE
        ));
        assert!(returns_to_list(
            screen.screen(),
            &definition.input,
            KeyCode::Tab,
            KeyModifiers::NONE
        ));
        assert!(!returns_to_list(
            screen.screen(),
            &definition.input,
            KeyCode::BackTab,
            KeyModifiers::SHIFT
        ));
    }

    #[test]
    fn first_paint_ignores_setup_sequences_and_finds_the_first_text() {
        assert!(!has_text(b"\x1b[?1049h\x1b[2J\x1b[H\x1b]10;?\x07\x1b[?25l"));
        assert!(!has_text(b"\x1bP>|q\x1b\\\x1b(B"));
        assert!(has_text(b"\x1b[2J\x1b[HReady"));
        assert!(has_text(b"x"));
    }
}
