//! One detached PTY owner per interactive terminal. It runs no agent tools and
//! makes no permission decisions; attached dashboards send native terminal input.
use crate::{
    fleet::Session,
    viewer::{Colors, Viewer},
};
use fs2::FileExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    ffi::OsString,
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const VERSION: u32 = 1;
const CAP: usize = 8 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub id: String,
    pub socket: PathBuf,
    pub session: Session,
    pub what: String,
}

impl Record {
    /// Discovery can replace a launch id with a process or native conversation id.
    /// Only the same harness and its owned client can bind that row back to this host.
    pub(crate) fn matches_session(&self, id: &str, session: Option<&Session>) -> bool {
        self.session.session_id == id
            || session.is_some_and(|session| {
                session.harness == self.session.harness
                    && session.pid.is_some()
                    && session.pid == self.session.pid
            })
    }
}

#[derive(Serialize, Deserialize)]
struct Launch {
    program: Vec<u8>,
    args: Vec<Vec<u8>>,
    env: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    cwd: PathBuf,
    rows: u16,
    cols: u16,
    colors: Colors,
    shell: bool,
    record: Record,
    state: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Request {
    Attach { version: u32, id: String },
    Input(Vec<u8>),
    Resize(u16, u16),
    Scroll(i32),
    Update(Box<Session>),
    Stop { id: String },
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Reply {
    Attached(Box<Record>),
    Screen(Box<Snapshot>),
    Error(String),
    Stopped,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub rows: u16,
    pub cols: u16,
    pub bytes: Vec<u8>,
    pub reset: bool,
    pub alternate: bool,
    pub scrolled: Option<Vec<u8>>,
    pub title: Option<String>,
    pub return_to_list: bool,
    pub report: Option<serde_json::Value>,
    pub stderr: Vec<u8>,
    pub exit: Option<i32>,
}

pub(crate) fn frame(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > CAP {
        return Err(io::Error::other("terminal message exceeds 8 MiB"));
    }
    let mut frame = (bytes.len() as u32).to_be_bytes().to_vec();
    frame.extend(bytes);
    Ok(frame)
}

pub(crate) fn decode<T: DeserializeOwned>(buffer: &mut Vec<u8>) -> io::Result<Option<T>> {
    let Some(length) = buffer.get(..4) else {
        return Ok(None);
    };
    let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
    if length > CAP {
        return Err(io::Error::other("terminal message exceeds 8 MiB"));
    }
    if buffer.len() < length + 4 {
        return Ok(None);
    }
    let value = serde_json::from_slice(&buffer[4..length + 4]).map_err(io::Error::other)?;
    buffer.drain(..length + 4);
    Ok(Some(value))
}

fn read_one<T: DeserializeOwned>(stream: &mut impl Read) -> io::Result<T> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let len = u32::from_be_bytes(header) as usize;
    if len > CAP {
        return Err(io::Error::other("terminal message exceeds 8 MiB"));
    }
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn directory(state: &Path) -> PathBuf {
    state.join("terminals")
}
fn record_path(state: &Path, id: &str) -> PathBuf {
    directory(state).join(format!("{id}.json"))
}
fn lock_path(state: &Path, id: &str) -> PathBuf {
    directory(state).join(format!("{id}.lock"))
}

fn save(state: &Path, record: &Record) -> io::Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(directory(state))?;
    serde_json::to_writer(&mut temp, record).map_err(io::Error::other)?;
    temp.flush()?;
    temp.persist(record_path(state, &record.id))
        .map_err(io::Error::other)?;
    Ok(())
}

/// A held host lock establishes ownership; stale JSON and reused PIDs never do.
pub(crate) fn records(state: &Path) -> Vec<Record> {
    let Ok(entries) = fs::read_dir(directory(state)) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()? != "json" {
                return None;
            }
            let mut bytes = Vec::new();
            File::open(path)
                .ok()?
                .take(256 * 1024)
                .read_to_end(&mut bytes)
                .ok()?;
            let record: Record = serde_json::from_slice(&bytes).ok()?;
            if uuid::Uuid::parse_str(&record.id).is_err() {
                return None;
            }
            let file = File::open(lock_path(state, &record.id)).ok()?;
            match file.try_lock_exclusive() {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Some(record),
                _ => None,
            }
        })
        .collect()
}

pub(crate) fn connect(record: &Record) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(&record.socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(&frame(&Request::Attach {
        version: VERSION,
        id: record.id.clone(),
    })?)?;
    match read_one::<Reply>(&mut stream)? {
        Reply::Attached(found)
            if found.id == record.id && found.session.pid == record.session.pid => {}
        Reply::Error(error) => return Err(io::Error::other(error)),
        _ => return Err(io::Error::other("terminal host identity changed")),
    }
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    stream.set_nonblocking(true)?;
    Ok(stream)
}

pub(crate) fn stop(record: &Record) -> io::Result<()> {
    let mut stream = UnixStream::connect(&record.socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    stream.write_all(&frame(&Request::Stop {
        id: record.id.clone(),
    })?)?;
    match read_one::<Reply>(&mut stream)? {
        Reply::Stopped => Ok(()),
        Reply::Error(error) => Err(io::Error::other(error)),
        _ => Err(io::Error::other("terminal host did not confirm stop")),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn launch(
    exe: &Path,
    state: &Path,
    command: &Command,
    session: Session,
    what: &str,
    rows: u16,
    cols: u16,
    colors: Colors,
    shell: bool,
) -> io::Result<Record> {
    crate::private_dir(&directory(state)).map_err(io::Error::other)?;
    // macOS Unix socket paths are short. The state directory can be arbitrarily
    // deep; only this private endpoint lives in the system temporary directory.
    let sockets = PathBuf::from(format!("/tmp/cones-terminals-{}", unsafe {
        libc::geteuid()
    }));
    crate::private_dir(&sockets).map_err(io::Error::other)?;
    let id = uuid::Uuid::new_v4().to_string();
    let record = Record {
        socket: sockets.join(&id),
        id,
        session,
        what: what.into(),
    };
    let launch = Launch {
        program: command.get_program().as_bytes().to_vec(),
        args: command.get_args().map(|a| a.as_bytes().to_vec()).collect(),
        env: command
            .get_envs()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.map(|v| v.as_bytes().to_vec())))
            .collect(),
        cwd: command
            .get_current_dir()
            .map(Path::to_owned)
            .unwrap_or(std::env::current_dir()?),
        rows,
        cols,
        colors,
        shell,
        record,
        state: state.to_owned(),
    };
    let mut host = Command::new(exe);
    host.arg("__terminal-host")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    unsafe {
        host.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = host.spawn()?;
    let result = (|| {
        child.stdin.take().unwrap().write_all(&frame(&launch)?)?;
        // Startup has no model calls and returns as soon as the native process
        // exists. Poll the readiness pipe so failed startup cannot freeze the UI.
        let mut output = child.stdout.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            use std::os::fd::AsRawFd;
            let mut fd = libc::pollfd {
                fd: output.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut fd, 1, 20) } > 0 {
                let reply: Result<Record, String> = read_one(&mut output)?;
                return reply.map_err(io::Error::other);
            }
            if Instant::now() >= deadline || child.try_wait()?.is_some() {
                return Err(io::Error::other("terminal host did not start"));
            }
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    } else {
        // Reap when it eventually exits without holding up dashboard shutdown.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    result
}

struct Peer {
    stream: UnixStream,
    input: Vec<u8>,
    output: Vec<u8>,
    attached: bool,
    closing: bool,
    stop_pending: bool,
    since: Instant,
}

impl Peer {
    fn queue(&mut self, reply: &Reply) -> io::Result<()> {
        let bytes = frame(reply)?;
        if self.output.len() + bytes.len() > CAP {
            return Err(io::Error::other("terminal client is not reading"));
        }
        self.output.extend(bytes);
        Ok(())
    }

    fn read(&mut self) -> io::Result<()> {
        let mut bytes = [0; 65536];
        match self.stream.read(&mut bytes) {
            Ok(0) => Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) if self.input.len() + n <= CAP + 4 => {
                self.input.extend_from_slice(&bytes[..n]);
                Ok(())
            }
            Ok(_) => Err(io::Error::other("terminal input exceeds 8 MiB")),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.output.is_empty() {
            match self
                .stream
                .write(&self.output[..self.output.len().min(65536)])
            {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.output.drain(..n);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Hidden binary entry point. Commands arrive through an anonymous pipe; their
/// arguments, environment and prompts are never persisted in the host registry.
pub fn serve() -> io::Result<i32> {
    let launch: Launch = read_one(&mut io::stdin().lock())?;
    let state = launch.state.clone();
    let mut record = launch.record.clone();
    let prepared = (|| {
        let lock = crate::private_file(&lock_path(&state, &record.id)).map_err(io::Error::other)?;
        lock.try_lock_exclusive()?;
        let listener = UnixListener::bind(&record.socket)?;
        fs::set_permissions(&record.socket, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let mut command = Command::new(OsString::from_vec(launch.program));
        command
            .args(launch.args.into_iter().map(OsString::from_vec))
            .current_dir(launch.cwd);
        for (key, value) in launch.env {
            let key = OsString::from_vec(key);
            if let Some(value) = value {
                command.env(key, OsString::from_vec(value));
            } else {
                command.env_remove(key);
            }
        }
        let startup = if launch.shell {
            command
                .get_envs()
                .find(|(key, _)| *key == "ZDOTDIR")
                .and_then(|(_, value)| value)
                .map(PathBuf::from)
                .filter(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("cones-shell-"))
                })
                .map(|source| {
                    let copy = tempfile::Builder::new()
                        .prefix("shell-")
                        .tempdir_in(directory(&state))?;
                    fs::copy(source.join(".zshenv"), copy.path().join(".zshenv"))?;
                    command.env("ZDOTDIR", copy.path());
                    Ok::<_, io::Error>(copy)
                })
                .transpose()?
        } else {
            None
        };
        command = crate::harness::restore_stdin_prompt(command)?;
        let spawn = if launch.shell {
            Viewer::spawn_terminal
        } else {
            Viewer::spawn
        };
        let viewer = spawn(command, launch.rows, launch.cols, None, launch.colors)?;
        record.session.pid = Some(viewer.pid());
        save(&state, &record)?;
        Ok::<_, io::Error>((lock, listener, viewer, startup))
    })();
    match prepared {
        Err(error) => {
            let mut ready = io::stdout().lock();
            let _ = ready.write_all(&frame(&Err::<Record, _>(error.to_string()))?);
            let _ = ready.flush();
            let _ = fs::remove_file(&record.socket);
            Err(error)
        }
        Ok((_lock, listener, mut viewer, _startup)) => {
            let mut ready = io::stdout().lock();
            if ready
                .write_all(&frame(&Ok::<_, String>(&record))?)
                .and_then(|_| ready.flush())
                .is_err()
            {
                let _ = fs::remove_file(record_path(&state, &record.id));
                let _ = fs::remove_file(&record.socket);
                return Ok(1);
            }
            drop(ready);
            let result = host_loop(&listener, &mut viewer, &state, &mut record);
            drop(viewer);
            let _ = fs::remove_file(record_path(&state, &record.id));
            let _ = fs::remove_file(&record.socket);
            let _ = fs::remove_file(lock_path(&state, &record.id));
            result
        }
    }
}

fn host_loop(
    listener: &UnixListener,
    viewer: &mut Viewer,
    state: &Path,
    record: &mut Record,
) -> io::Result<i32> {
    let mut peers: Vec<Peer> = Vec::new();
    let mut previous = None;
    let mut last_report = Instant::now() - Duration::from_secs(1);
    let mut stopped = None;
    let mut ending = false;
    let mut retired = false;
    loop {
        while let Ok((stream, _)) = listener.accept() {
            if peers.len() >= 8 {
                continue;
            }
            stream.set_nonblocking(true)?;
            peers.push(Peer {
                stream,
                input: Vec::new(),
                output: Vec::new(),
                attached: false,
                closing: false,
                stop_pending: false,
                since: Instant::now(),
            });
        }
        let mut dirty = viewer.pump()?;
        peers.retain_mut(|peer| peer.read().is_ok());
        let mut attached = peers.iter().any(|peer| peer.attached);
        for peer in &mut peers {
            let result = (|| {
                while let Some(request) = decode::<Request>(&mut peer.input)? {
                    match request {
                        Request::Stop { id } if id == record.id => {
                            viewer.terminate();
                            peer.stop_pending = true;
                            ending = true;
                        }
                        Request::Attach { version, id }
                            if !peer.attached
                                && !attached
                                && !ending
                                && version == VERSION
                                && id == record.id =>
                        {
                            peer.attached = true;
                            attached = true;
                            viewer.scroll(-i32::MAX);
                            previous = None;
                            dirty = true;
                            peer.queue(&Reply::Attached(Box::new(record.clone())))?;
                        }
                        Request::Attach { .. } => {
                            peer.queue(&Reply::Error("terminal is already open in another dashboard, or the host protocol differs".into()))?;
                            peer.closing = true;
                        }
                        Request::Input(bytes) if peer.attached => viewer.write(&bytes),
                        Request::Resize(rows, cols) if peer.attached => {
                            viewer.resize(rows.min(512), cols.min(1024));
                            previous = None;
                            dirty = true;
                        }
                        Request::Scroll(lines) if peer.attached => {
                            dirty |= viewer.scroll(lines);
                        }
                        Request::Update(session)
                            if peer.attached
                                && !ending
                                && !retired
                                && session.pid == record.session.pid =>
                        {
                            record.session = *session;
                            save(state, record)?;
                        }
                        _ => return Err(io::Error::other("invalid terminal request")),
                    }
                }
                peer.flush()
            })();
            if result.is_err() {
                peer.closing = true;
                peer.output.clear();
            }
        }
        if viewer.exited().is_some() {
            if !retired {
                fs::remove_file(record_path(state, &record.id))?;
                retired = true;
                stopped = Some(Instant::now());
            }
            for peer in &mut peers {
                if std::mem::take(&mut peer.stop_pending) {
                    peer.queue(&Reply::Stopped)?;
                }
            }
        }
        if !ending && !retired && last_report.elapsed() >= Duration::from_secs(1) {
            if let Some(report) = viewer.opencode_report() {
                let previous_id = record.session.session_id.clone();
                report.apply(&mut record.session);
                if record.session.session_id != previous_id && previous_id.starts_with("ses_") {
                    record.session.forked_from = None;
                }
                save(state, record)?;
            } else if record.session.harness == "opencode" && record.session.state != "-" {
                record.session.state = "-".into();
                save(state, record)?;
            }
            dirty = true;
            last_report = Instant::now();
        }
        if dirty || viewer.exited().is_some() {
            if let Some(peer) = peers.iter_mut().find(|p| p.attached) {
                let (snapshot, live) = viewer.snapshot(previous.as_ref());
                previous = Some(live);
                if peer.queue(&Reply::Screen(Box::new(snapshot))).is_err() {
                    peer.closing = true;
                    peer.output.clear();
                }
            }
            if viewer.exited().is_some() {
                stopped.get_or_insert_with(Instant::now);
            }
        }
        peers.retain_mut(|peer| {
            peer.flush().is_ok()
                && !(peer.closing && peer.output.is_empty())
                && (peer.attached || peer.since.elapsed() < Duration::from_secs(2))
        });
        if stopped.is_some_and(|at| at.elapsed() >= Duration::from_millis(300)) {
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
