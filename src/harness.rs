pub mod spec;
pub use spec::{by_name, known, launchable, spec};

use crate::config::{HarnessKind, Policy, ResolvedJob};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invocation {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub timeout_s: f64,
}

pub trait Harness {
    fn compile(&self, job: &ResolvedJob) -> Result<Invocation>;
    fn resume(&self, session_id: &str, cwd: &Path) -> Result<std::process::Command>;
    /// Open a session that is still running elsewhere in this terminal; resume would refuse it.
    fn attach(&self, session_id: &str, cwd: &Path) -> Result<std::process::Command>;
    fn transcript(&self, session_id: &str, cwd: &Path) -> Result<PathBuf>;
}

pub struct Claude;
pub fn adapter(kind: HarnessKind) -> Result<Box<dyn Harness>> {
    match (
        kind,
        spec(kind).execution.enforcement,
        spec(kind).execution.result,
    ) {
        (HarnessKind::Claude, spec::Support::Supported, spec::Support::Supported) => {
            Ok(Box::new(Claude))
        }
        _ => bail!("{kind} has no execution adapter in v0.1. Use harness: claude"),
    }
}

/// Whether cones can supervise a run on this harness. `adapter` remains the one place that
/// decides, so a job the config accepts is a job the installer and the runner can carry.
pub fn executes(kind: HarnessKind) -> bool {
    adapter(kind).is_ok()
}

/// Every harness a job may name, for a message that offers the answer with the refusal.
pub fn executing() -> Vec<HarnessKind> {
    known().iter().copied().filter(|k| executes(*k)).collect()
}

/// Native session launch with harness-owned permissions and lifetime.
/// Background commands return after launch; foreground commands are daemon clients.
/// See docs/harness.md and docs/dashboard.md for the ownership boundary.
pub enum Start {
    Background(std::process::Command),
    Foreground(std::process::Command),
}

pub fn start(kind: HarnessKind, dir: &Path, prompt: &str, policy: &Policy) -> Result<Start> {
    let launch = spec(kind)
        .launch
        .as_ref()
        .context("harness has no launch operation")?;
    let name = kind.to_string();
    let path = executable(&name, &launch_path())
        .ok_or_else(|| anyhow::anyhow!("{name} not found on the launch PATH"))?;
    let path = native_executable(kind, path);
    leave_and_return(kind)?;
    let mut start = match launch.handler {
        spec::LaunchHandler::ClaudeBackground => {
            let mut c = std::process::Command::new(path);
            c.args(session_args(kind, None, prompt, policy)?)
                .current_dir(dir);
            Start::Background(c)
        }
        spec::LaunchHandler::CodexRemote => {
            let (path, remote) =
                codex_remote(&path, &crate::codex::home(&crate::fleet::claude_dir()?))?;
            let mut c = std::process::Command::new(path);
            c.args(session_args(kind, Some((&remote, dir)), prompt, policy)?)
                .current_dir(dir);
            Start::Foreground(c)
        }
        spec::LaunchHandler::Terminal => {
            let mut c = std::process::Command::new(path);
            c.args(session_args(kind, None, prompt, policy)?)
                .current_dir(dir);
            if kind == HarnessKind::Opencode {
                c.env(crate::opencode::reporting::ENABLE, "1");
            }
            Start::Foreground(c)
        }
    };
    let (Start::Background(c) | Start::Foreground(c)) = &mut start;
    provider_env(
        c,
        launch,
        policy.bedrock,
        policy.aws_profile.as_deref(),
        policy.aws_region.as_deref(),
    );
    drop_host_identity(c);
    if launch.stdin_prompt && !prompt.is_empty() {
        let Start::Foreground(command) = start else {
            bail!("stdin prompts require a terminal harness");
        };
        return Ok(Start::Foreground(stdin_prompt(command, prompt)?));
    }
    Ok(start)
}

/// Copilot's npm shim waits on a native child. Spawn that same packaged binary so
/// the viewer PID is the client PID; arbitrary user wrappers keep their behavior.
fn native_executable(kind: HarnessKind, path: PathBuf) -> PathBuf {
    if kind != HarnessKind::Copilot || !cfg!(target_os = "macos") {
        return path;
    }
    let Some(loader) = path
        .canonicalize()
        .ok()
        .filter(|p| p.ends_with("@github/copilot/npm-loader.js"))
    else {
        return path;
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        _ => return path,
    };
    let Some(scope) = loader.parent().and_then(Path::parent) else {
        return path;
    };
    let package = scope.join(format!("copilot-darwin-{arch}"));
    executable("copilot", &package.to_string_lossy()).unwrap_or(path)
}

fn stdin_prompt(mut command: std::process::Command, prompt: &str) -> Result<std::process::Command> {
    command.env("CONES_LAUNCH_STDIN", prompt);
    bind_stdin_prompt(command, prompt)
}

/// Restore the anonymous stdin binding when a command crosses the terminal-host
/// boundary. Remove the transport marker before executing the native harness.
#[doc(hidden)]
pub fn restore_stdin_prompt(
    mut command: std::process::Command,
) -> std::io::Result<std::process::Command> {
    let prompt = command
        .get_envs()
        .find(|(key, _)| *key == "CONES_LAUNCH_STDIN")
        .and_then(|(_, value)| value)
        .map(|value| value.to_string_lossy().into_owned());
    command.env_remove("CONES_LAUNCH_STDIN");
    match prompt {
        Some(prompt) => bind_stdin_prompt(command, &prompt).map_err(std::io::Error::other),
        None => Ok(command),
    }
}

fn bind_stdin_prompt(
    mut command: std::process::Command,
    prompt: &str,
) -> Result<std::process::Command> {
    use std::{
        io::{Seek, Write},
        os::{fd::AsRawFd, unix::process::CommandExt},
    };
    let mut input = tempfile::tempfile()?;
    input.write_all(prompt.as_bytes())?;
    input.rewind()?;
    // The captured anonymous file is dropped if launch is cancelled. In the child,
    // only async-signal-safe descriptor operations run before exec.
    unsafe {
        command.pre_exec(move || {
            if libc::lseek(input.as_raw_fd(), 0, libc::SEEK_SET) < 0
                || libc::dup2(input.as_raw_fd(), libc::STDIN_FILENO) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command)
}

/// Fork through the native CLI. No transcript copying and no synthetic user turn.
pub fn fork(entry: &crate::history::Entry, new_id: Option<&str>, policy: &Policy) -> Result<Start> {
    let mut entry = entry.clone();
    if entry.key.harness == "opencode" {
        entry.transcript = crate::opencode::session_database(
            &entry.key.home,
            &entry.key.session_id,
            &entry.cwd,
            (!entry.transcript.as_os_str().is_empty()).then_some(entry.transcript.as_path()),
        )?;
    }
    ensure!(entry.cwd.is_dir(), "the session folder no longer exists");
    ensure!(
        entry.transcript.is_file(),
        "the source transcript no longer exists"
    );
    ensure!(
        !entry.archived,
        "resume an archived session before forking it"
    );
    let spec = by_name(&entry.key.harness).context("unknown fork harness")?;
    check_operation(spec, &spec.operations.fork, "fork")?;
    if spec.kind == HarnessKind::Pi {
        ensure!(
            new_id.is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()),
            "pi fork requires an exact new session UUID"
        );
    }
    let template = &spec.operations.fork.as_ref().expect("checked fork").args;
    let path = executable(&spec.name, &launch_path())
        .with_context(|| format!("{} not found", spec.name))?;
    let mut command = if spec.kind == HarnessKind::Codex {
        codex_client(
            &entry.key.home,
            &entry.key.session_id,
            &entry.cwd,
            template,
            false,
        )?
    } else {
        let mut command = std::process::Command::new(path);
        command
            .args(spec::args(
                template,
                &[
                    ("id", entry.key.session_id.as_ref()),
                    ("transcript", entry.transcript.as_os_str()),
                    ("new_id", new_id.unwrap_or("").as_ref()),
                ],
            )?)
            .current_dir(&entry.cwd);
        if spec.kind == HarnessKind::Opencode {
            crate::opencode::require_session(&entry.transcript, &entry.key.session_id)?;
            command
                .env("OPENCODE_DB", &entry.transcript)
                .env(crate::opencode::reporting::ENABLE, "1");
        }
        command
    };
    spec.home.set_command_home(&mut command, &entry.key.home);
    if let Some(launch) = &spec.launch {
        provider_env(
            &mut command,
            launch,
            policy.bedrock,
            policy.aws_profile.as_deref(),
            policy.aws_region.as_deref(),
        );
    }
    drop_host_identity(&mut command);
    Ok(Start::Foreground(command))
}

/// The environment of the terminal cones was started from names that terminal and, when cones
/// itself was launched from inside an agent, that agent's live session: its IDE socket, its
/// messaging socket, its identifiers and the provider its own launcher chose. A pane is neither,
/// so passing those on makes a session report a host it does not run in. A machine-wide
/// preference is not identity and stays, and a value cones set on this command is this launch's
/// own policy and always wins.
const HOST_IDENTITY: [&str; 14] = [
    "AI_AGENT",
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_EFFORT",
    "CLAUDE_JOB_DIR",
    "CLAUDE_PID",
];

/// `TERM_PROGRAM` decides which terminal a native CLI adapts its keys to, and the viewer answers
/// for none of them: it encodes shift+enter as a plain return, so an inherited name makes a CLI
/// offer a newline binding the pane cannot deliver.
const HOST_TERMINAL: [&str; 2] = ["TERM_PROGRAM", "TERM_PROGRAM_VERSION"];

fn drop_host_identity(command: &mut std::process::Command) {
    let ours: Vec<OsString> = command
        .get_envs()
        .map(|(name, _)| name.to_owned())
        .collect();
    for name in HOST_IDENTITY.iter().chain(&HOST_TERMINAL) {
        if !ours.iter().any(|ours| ours == OsStr::new(name)) {
            command.env_remove(name);
        }
    }
}

/// AWS_PROFILE and AWS_REGION go to every harness: each resolves AWS through the same
/// chain, so a profile and region are credentials, not a Claude setting. Only the
/// Bedrock switch belongs to one harness, and only a definition that names one gets it;
/// a harness without one refuses `bedrock` in validation instead of ignoring it.
/// A native settings file can still override the switch cones passes.
fn provider_env(
    c: &mut std::process::Command,
    launch: &spec::Launch,
    bedrock: Option<bool>,
    profile: Option<&str>,
    region: Option<&str>,
) {
    for (key, set) in [("AWS_PROFILE", profile), ("AWS_REGION", region)] {
        if let Some(v) = set {
            c.env(key, v);
        }
    }
    if let Some(switch) = &launch.bedrock {
        match bedrock {
            Some(true) => {
                c.env(switch, "1");
            }
            Some(false) => {
                c.env_remove(switch);
            }
            None => {}
        }
    }
}

/// Model, provider and effort overrides for native sessions; Claude selects its provider
/// through env.
pub fn session_args(
    kind: HarnessKind,
    remote: Option<(&str, &Path)>,
    prompt: &str,
    policy: &Policy,
) -> Result<Vec<OsString>> {
    let launch = spec(kind)
        .launch
        .as_ref()
        .context("harness has no launch operation")?;
    let mut args: Vec<OsString> = launch.prefix.iter().map(OsString::from).collect();
    if let Some((remote, dir)) = remote {
        args.extend(
            spec::args(
                &launch.remote,
                &[("remote", remote.as_ref()), ("cwd", dir.as_os_str())],
            )
            .expect("validated remote template"),
        );
    }
    for (flag, value) in [
        (&launch.model, policy.model_for(kind)),
        (&launch.provider, policy.provider_for(kind)),
        (&launch.effort, policy.effort_for(kind)),
    ] {
        if let (Some(flag), Some(value)) = (flag, value) {
            args.extend([OsString::from(flag), value.into()]);
        }
    }
    if kind.terminal_only() && prompt.is_empty() {
        return Ok(args);
    }
    if launch.stdin_prompt {
        // This payload is attached to stdin after constructing the native command.
    } else if let Some(flag) = &launch.prompt_flag {
        args.push(format!("{flag}={prompt}").into());
    } else if kind == HarnessKind::Opencode {
        // OpenCode's positional argument is a project. Assignment also keeps a
        // prompt beginning with "--" from being interpreted as another option.
        args.push(format!("--prompt={prompt}").into());
    } else {
        args.extend(
            spec::args(&launch.prompt, &[("prompt", prompt.as_ref())])
                .expect("validated prompt template"),
        );
    }
    Ok(args)
}

/// Check that the installed harness can run a session the dashboard starts, and
/// say how long that session lives once its viewer is gone.
/// Never add `--bg` to a Claude help probe: it takes precedence over `--help`
/// and starts a real background session.
pub fn leave_and_return(kind: HarnessKind) -> Result<String> {
    let probe = &spec(kind)
        .launch
        .as_ref()
        .context("harness has no launch operation")?
        .probe;
    probe_harness(kind, probe)
}

fn probe_harness(kind: HarnessKind, probe: &spec::Probe) -> Result<String> {
    let name = kind.to_string();
    let path = executable(&name, &launch_path())
        .ok_or_else(|| anyhow::anyhow!("{name} not found on the launch PATH"))?;
    let output = std::process::Command::new(&path)
        .args(&probe.args)
        .output()
        .with_context(|| format!("{name} {}", probe.args.join(" ")))?;
    probe.report(
        output.status.success(),
        &String::from_utf8_lossy(match probe.output {
            spec::ProbeOutput::Stdout => &output.stdout,
            spec::ProbeOutput::Stderr => &output.stderr,
        }),
    )
}

/// Resume a thread against the daemon of the home that holds it, not the ambient one:
/// a home pinned to another provider region keeps its own daemon and its own socket.
pub fn codex_resume(home: &Path, id: &str, cwd: &Path) -> Result<std::process::Command> {
    let definition = spec(HarnessKind::Codex);
    check_operation(definition, &definition.operations.resume, "resume")?;
    codex_client(
        home,
        id,
        cwd,
        &spec(HarnessKind::Codex).commands.resume,
        false,
    )
}

fn codex_client(
    home: &Path,
    id: &str,
    cwd: &Path,
    template: &[String],
    existing_only: bool,
) -> Result<std::process::Command> {
    let path = executable("codex", &launch_path())
        .ok_or_else(|| anyhow::anyhow!("codex not found on the launch PATH"))?;
    if existing_only {
        // Preserve the native liveness guard used by hover before resolving its address.
        ensure!(
            crate::codex::daemon_pid(home).is_some(),
            "Codex daemon is no longer running"
        );
    }
    let (path, remote) = codex_remote(&path, home)?;
    let mut c = std::process::Command::new(path);
    let spec = spec(HarnessKind::Codex);
    c.env(&spec.home.env, home);
    c.args(spec::args(
        template,
        &[("remote", remote.as_ref()), ("id", id.as_ref())],
    )?)
    .current_dir(cwd);
    Ok(c)
}

/// The native home a row was discovered in. A Codex thread names its own through the rollout
/// it writes, because a home pinned to another provider region keeps its own daemon.
pub fn home_of(session: &crate::fleet::Session, claude: &Path) -> PathBuf {
    if session.harness == HarnessKind::Codex.to_string()
        && let Some(rollout) = &session.transcript_path
        && let Some(home) = crate::codex::home_of(rollout)
    {
        return home.to_owned();
    }
    by_name(&session.harness).map_or_else(|| claude.to_owned(), |spec| spec.home.resolve(claude))
}

/// One note to a live session, through the delivery command its harness declares.
///
/// A harness with no `message` operation cannot be written to from here and says so. Faking it
/// by typing into the session's terminal would put words in the owner's input line, which is
/// not a message from a peer and is not cones' to do.
pub fn message(
    session: &crate::fleet::Session,
    home: &Path,
    text: &str,
) -> Result<std::process::Command> {
    let spec = by_name(&session.harness).context("unknown session harness")?;
    check_operation(spec, &spec.operations.message, "message")?;
    let template = &spec.operations.message.as_ref().expect("checked").args;
    let name = spec.kind.to_string();
    let path = executable(&name, &launch_path())
        .ok_or_else(|| anyhow::anyhow!("{name} not found on the launch PATH"))?;
    // Codex delivery goes to the daemon that owns the thread, the address its resume already
    // uses. Every other harness addresses the session directly and needs no socket.
    let (path, remote) = match spec.kind {
        HarnessKind::Codex => codex_remote(&path, home)?,
        _ => (path, String::new()),
    };
    let mut c = std::process::Command::new(path);
    if !spec.home.env.is_empty() {
        c.env(&spec.home.env, home);
    }
    c.args(spec::args(
        template,
        &[
            ("remote", remote.as_ref()),
            ("id", session.session_id.as_ref()),
            ("text", text.as_ref()),
        ],
    )?)
    .current_dir(&session.cwd);
    Ok(c)
}

/// One native join contract for Enter and hover. Hover is forbidden from launching a session.
pub fn join(
    session: &crate::fleet::Session,
    home: &Path,
    speculative: bool,
) -> Result<std::process::Command> {
    let spec = by_name(&session.harness).context("unknown session harness")?;
    let mut command = match spec.session(session.kind.as_deref()).join {
        spec::Join::Unavailable => bail!(
            "{} runs in its own terminal and cannot be joined from here",
            spec.name
        ),
        spec::Join::Attach => Claude.attach(&session.session_id, &session.cwd),
        spec::Join::CodexRemote => {
            check_operation(spec, &spec.operations.attach, "attach")?;
            codex_client(
                home,
                &session.session_id,
                &session.cwd,
                &spec.commands.attach,
                speculative,
            )
        }
    }?;
    drop_host_identity(&mut command);
    Ok(command)
}

pub fn can_peek(session: &crate::fleet::Session, home: &Path) -> bool {
    let Some(spec) = by_name(&session.harness) else {
        return false;
    };
    if !spec.permits_peek(session) {
        return false;
    }
    match spec.viewer.peek {
        // A settled background session has no process; joining it wakes one, which a hover
        // must never do. It waits for enter, like a saved Codex thread whose daemon is gone.
        spec::Peek::Join => session.pid.is_some(),
        spec::Peek::ExistingDaemon => crate::codex::daemon_pid(home).is_some(),
        spec::Peek::Unavailable => false,
    }
}

/// Historical resume always carries the entry's canonical native home.
pub fn resume_history(entry: &crate::history::Entry) -> Result<std::process::Command> {
    ensure!(
        entry.cwd.is_dir(),
        "session directory no longer exists: {}",
        entry.cwd.display()
    );
    ensure!(
        entry.transcript.is_file(),
        "session transcript no longer exists; reload history"
    );
    let spec = by_name(&entry.key.harness).context("unknown history harness")?;
    let mut command = match spec.commands.resume_handler {
        spec::Resume::BackgroundThenAttach => Claude.resume(&entry.key.session_id, &entry.cwd)?,
        spec::Resume::CodexRemote => {
            codex_resume(&entry.key.home, &entry.key.session_id, &entry.cwd)?
        }
        spec::Resume::Transcript => {
            check_operation(spec, &spec.operations.resume, "resume")?;
            let path = executable(&spec.name, &launch_path())
                .with_context(|| format!("{} not found", spec.name))?;
            let mut c = std::process::Command::new(path);
            c.args(spec::args(
                &spec.commands.resume,
                &[("transcript", entry.transcript.as_os_str())],
            )?)
            .current_dir(&entry.cwd);
            c
        }
        spec::Resume::SessionId => {
            check_operation(spec, &spec.operations.resume, "resume")?;
            crate::opencode::require_session(&entry.transcript, &entry.key.session_id)?;
            let path = executable(&spec.name, &launch_path())
                .with_context(|| format!("{} not found", spec.name))?;
            let mut c = std::process::Command::new(path);
            c.args(spec::args(
                &spec.commands.resume,
                &[("id", entry.key.session_id.as_ref())],
            )?)
            .env("OPENCODE_DB", &entry.transcript)
            .env(crate::opencode::reporting::ENABLE, "1")
            .current_dir(&entry.cwd);
            c
        }
    };
    spec.home.set_command_home(&mut command, &entry.key.home);
    drop_host_identity(&mut command);
    if entry.archived && spec.commands.resume_handler != spec::Resume::SessionId {
        check_operation(spec, &spec.operations.unarchive, "unarchive")?;
        ensure!(
            !spec.commands.unarchive.is_empty(),
            "this harness cannot unarchive a session"
        );
        command = then_exec(
            spec::args(
                &spec.commands.unarchive,
                &[("id", entry.key.session_id.as_ref())],
            )?,
            command,
        );
    }
    Ok(command)
}

pub fn check_operation(
    spec: &spec::HarnessSpec,
    operation: &Option<spec::Operation>,
    name: &str,
) -> Result<()> {
    let operation = operation
        .as_ref()
        .with_context(|| format!("{} has no {name} operation", spec.name))?;
    if let Some(probe) = &operation.probe {
        probe_harness(spec.kind, probe)?;
    }
    Ok(())
}

/// Sequence two invocations of one native program. Values are positional shell arguments.
pub(crate) fn then_exec(
    before: Vec<OsString>,
    after: std::process::Command,
) -> std::process::Command {
    let first = before.len();
    let second = after.get_args().len();
    let refs = |from: usize, len: usize| {
        (from..from + len)
            .map(|i| format!("\"${{{i}}}\""))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut c = std::process::Command::new("/bin/sh");
    c.arg("-c")
        .arg(format!(
            "\"$0\" {} >/dev/null && exec \"$0\" {}",
            refs(1, first),
            refs(first + 1, second)
        ))
        .arg(after.get_program())
        .args(before)
        .args(after.get_args());
    if let Some(cwd) = after.get_current_dir() {
        c.current_dir(cwd);
    }
    for (name, value) in after.get_envs() {
        if let Some(value) = value {
            c.env(name, value);
        } else {
            c.env_remove(name);
        }
    }
    c
}

/// Start the daemon idempotently and read its `socketPath` response.
fn codex_remote(codex: &Path, home: &Path) -> Result<(PathBuf, String)> {
    // Reuse reported addresses only while their sockets accept connections.
    type Addresses = BTreeMap<(PathBuf, PathBuf), String>;
    static ADDRESSES: std::sync::Mutex<Addresses> = std::sync::Mutex::new(BTreeMap::new());
    let key = (codex.to_owned(), home.to_owned());
    let cached = ADDRESSES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned();
    if let Some(remote) = cached
        && let Some(socket) = remote.strip_prefix("unix://")
        && std::os::unix::net::UnixStream::connect(socket).is_ok()
    {
        return Ok((codex.to_owned(), remote));
    }
    let out = std::process::Command::new(codex)
        .env(&spec(HarnessKind::Codex).home.env, home)
        .args(["app-server", "daemon", "start"])
        .output()
        .map_err(|e| anyhow::anyhow!("codex app-server daemon start: {e}"))?;
    let sock = socket_path(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| {
        anyhow::anyhow!(
            "codex app-server daemon start gave no socketPath: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    })?;
    let remote = format!("unix://{sock}");
    ADDRESSES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, remote.clone());
    Ok((codex.to_owned(), remote))
}

pub fn socket_path(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find_map(|v| v["socketPath"].as_str().map(str::to_owned))
}

pub fn executable(name: &str, path: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path).map(|d| d.join(name)).find(|p| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// Fixed harness PATH, independent of the shell. Tests must install fakes under a
/// temporary HOME; prepending the caller's PATH does not override this lookup.
pub fn launch_path() -> String {
    let home = dirs::home_dir().unwrap_or_default();
    [
        home.join(".local/bin"),
        home.join(".cargo/bin"),
        home.join(".opencode/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/sbin"),
    ]
    .iter()
    .map(|p| p.to_string_lossy())
    .collect::<Vec<_>>()
    .join(":")
}

/// Embedded coordinator plugin, loaded only for the session that starts it. The skill is prose
/// and nothing else: the plumbing it used to ship as shell and Python is `cones coordinator`.
pub const COORDINATOR_SKILL: &str = "start-coordinator";
const COORDINATOR_FILES: [(&str, &str); 3] = [
    (
        ".claude-plugin/plugin.json",
        include_str!("../assets/coordinator/.claude-plugin/plugin.json"),
    ),
    (
        "skills/start-coordinator/SKILL.md",
        include_str!("../assets/coordinator/skills/start-coordinator/SKILL.md"),
    ),
    (
        "skills/dispatch/SKILL.md",
        include_str!("../assets/coordinator/skills/dispatch/SKILL.md"),
    ),
];

/// The bundled skills by name, in the order the plugin lists them. Nothing loads a skill into a
/// session that is already running, so an agent already holding a task reads the prose from
/// stdout instead: `cones skill dispatch`.
pub fn skills() -> impl Iterator<Item = (&'static str, &'static str)> {
    COORDINATOR_FILES.into_iter().filter_map(|(rel, text)| {
        Some((
            rel.strip_prefix("skills/")?.strip_suffix("/SKILL.md")?,
            text,
        ))
    })
}

/// The folder's live coordinator record, including one claimed outside cones.
pub fn coordinator_status(state: &Path, dir: &Path) -> Option<Value> {
    crate::coordinator::status(state, dir)
}

/// Rewrite the embedded plugin on each start so upgrades include the current skill.
pub fn coordinator_plugin(state: &Path) -> Result<PathBuf> {
    let plugin = state.join("coordinator/plugin");
    // An upgrade that drops a file must take it away as well: a coordinator loading a plugin
    // directory left with yesterday's helpers would follow instructions this build no longer has.
    let _ = std::fs::remove_dir_all(plugin.join("skills").join(COORDINATOR_SKILL).join("bin"));
    for (rel, text) in COORDINATOR_FILES {
        let path = plugin.join(rel);
        std::fs::create_dir_all(path.parent().unwrap())?;
        // A coordinator may be reading the skill while another folder starts one.
        let temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        std::fs::write(temporary.path(), text)?;
        temporary.persist(&path)?;
    }
    Ok(plugin)
}

/// The skill itself prevents duplicate coordinators for a folder.
pub fn coordinator(dir: &Path, state: &Path) -> Result<std::process::Command> {
    let plugin = coordinator_plugin(state)?;
    let path =
        executable("claude", &launch_path()).ok_or_else(|| anyhow::anyhow!("claude not found"))?;
    let mut cmd = std::process::Command::new(path);
    cmd.arg("--bg")
        .arg("--plugin-dir")
        .arg(plugin)
        .arg(format!("/cones:{COORDINATOR_SKILL}"))
        .current_dir(dir);
    Ok(cmd)
}

pub fn environment(job: &ResolvedJob) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    env.insert(
        "HOME".into(),
        dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("missing home directory"))?
            .to_string_lossy()
            .into(),
    );
    env.insert("PATH".into(), launch_path());
    env.insert("LANG".into(), "en_US.UTF-8".into());
    if let Ok(user) = std::env::var("USER") {
        env.insert("USER".into(), user);
    }
    if let Ok(tmp) = std::env::var("TMPDIR") {
        env.insert("TMPDIR".into(), tmp);
    }
    for key in &job.env {
        env.insert(
            key.clone(),
            std::env::var(key).map_err(|_| {
                anyhow::anyhow!(
                    "job {} requires environment variable {key}; export it before install/run",
                    job.name
                )
            })?,
        );
    }
    if job.bedrock == Some(true)
        && let Some(switch) = spec(job.harness).bedrock_switch()
    {
        env.insert(switch.into(), "1".into());
    }
    // A configured profile or region is this job's AWS, whichever harness runs it. A run
    // starts from a cleared environment, so inherit the credentials themselves first and
    // let the validated values override them.
    if job.aws_profile.is_some() || job.aws_region.is_some() {
        env.extend(std::env::vars().filter(|(k, _)| k.starts_with("AWS_")));
        for (key, set) in [
            ("AWS_PROFILE", &job.aws_profile),
            ("AWS_REGION", &job.aws_region),
        ] {
            if let Some(v) = set {
                env.insert(key.into(), v.clone());
            }
        }
    }
    env.insert("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN".into(), "1".into());
    Ok(env)
}

impl Harness for Claude {
    fn compile(&self, job: &ResolvedJob) -> Result<Invocation> {
        ensure!(
            job.harness == HarnessKind::Claude,
            "Claude adapter requires a Claude job"
        );
        // A job is a scheduled launch of the same agent the owner runs by hand: its own
        // settings, its own MCP servers, no prompts to answer and no second permission
        // engine here. timeout_min is the only limit cones puts on the run.
        //
        // It runs as a background session rather than a headless print, so the run is the
        // agent: the dashboard peeks it in a native viewer while it works and enter joins
        // the conversation to steer it, exactly as it does for a session the owner started.
        let mut args: Vec<String> = [
            "--bg",
            "--dangerously-skip-permissions",
            "--name",
            &job.name,
        ]
        .into_iter()
        .map(String::from)
        .collect();
        if let Some(model) = &job.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(effort) = &job.effort {
            args.extend(["--effort".into(), effort.clone()]);
        }
        args.extend(["--".into(), job.prompt.clone()]);
        // Resolve from the same PATH that launchd will use. Never rely on a shell alias.
        let env = environment(job)?;
        let program = executable("claude", &env["PATH"])
            .ok_or_else(|| anyhow::anyhow!("claude is missing from the launchd PATH"))?;
        Ok(Invocation {
            program,
            args,
            env,
            cwd: job.cwd.clone(),
            timeout_s: job.timeout_min * 60.0,
        })
    }
    fn resume(&self, session_id: &str, cwd: &Path) -> Result<std::process::Command> {
        uuid::Uuid::parse_str(session_id)?;
        // Resume in the background so ctrl+z detaches without suspending the agent.
        let spec = spec(HarnessKind::Claude);
        check_operation(spec, &spec.operations.resume, "resume")?;
        Ok(then_exec(
            spec::args(
                &spec.commands.resume,
                &[
                    ("id", session_id.as_ref()),
                    ("short_id", session_id[..8].as_ref()),
                ],
            )?,
            self.attach(session_id, cwd)?,
        ))
    }
    fn attach(&self, session_id: &str, cwd: &Path) -> Result<std::process::Command> {
        uuid::Uuid::parse_str(session_id)?;
        let definition = spec(HarnessKind::Claude);
        check_operation(definition, &definition.operations.attach, "attach")?;
        let path = executable("claude", &launch_path())
            .ok_or_else(|| anyhow::anyhow!("claude not found"))?;
        let mut cmd = std::process::Command::new(path);
        // `claude attach` takes the short id, the first block of the UUID.
        cmd.args(spec::args(
            &spec(HarnessKind::Claude).commands.attach,
            &[
                ("id", session_id.as_ref()),
                ("short_id", session_id[..8].as_ref()),
            ],
        )?)
        .current_dir(cwd);
        Ok(cmd)
    }
    fn transcript(&self, session_id: &str, cwd: &Path) -> Result<PathBuf> {
        let home = dirs::home_dir()
            .context("missing home directory")?
            .join(".claude");
        claude_transcript(&home, session_id, cwd)
    }
}

/// Where Claude keeps one session's conversation under a given native home. Separate from
/// the adapter method so a caller that already knows the home, such as a dashboard reading
/// a custom `CLAUDE_CONFIG_DIR`, does not have to assume `~/.claude`.
pub fn claude_transcript(home: &Path, session_id: &str, cwd: &Path) -> Result<PathBuf> {
    uuid::Uuid::parse_str(session_id)?;
    let project: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Ok(spec(HarnessKind::Claude)
        .transcript
        .live_path(home)
        .join(project)
        .join(format!("{session_id}.jsonl")))
}

pub fn policy_hash(job: &ResolvedJob, invocation: &Invocation) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&compiled_policy(job, invocation)?)?)
    ))
}

pub fn compiled_policy(job: &ResolvedJob, invocation: &Invocation) -> Result<Value> {
    // Exclude task text and secret values from the policy hash. The session identity is the
    // harness's own: `--bg` names the conversation, and cones records what it returned.
    let mut args = invocation.args.clone();
    if let Some(i) = args.iter().position(|a| a == "--name") {
        args[i + 1] = "<job-name>".into();
    }
    args.pop(); // The final positional prompt is task content, not policy.
    let policy = serde_json::json!({
        "v":5, "harness":job.harness, "program":invocation.program,
        "enforcement":"native-flags",
        "args":args,
        "cwd":job.cwd,
        "permission_mode":"bypassPermissions",
        "timeout_s":invocation.timeout_s, "overlap":job.overlap,
        "model":job.model, "env_names":job.env,
    });
    Ok(policy)
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub result_seen: bool,
    pub failed: bool,
    pub permission_denied: bool,
    pub reason: Option<String>,
    pub session_mismatch: bool,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub cost_usd: Option<f64>,
}
impl Outcome {
    pub fn observe(&mut self, line: &str, session_id: &str) -> Result<()> {
        let event: Value = serde_json::from_str(line)?;
        ensure!(event.is_object(), "harness event must be an object");
        if let Some(id) = event.get("session_id").and_then(Value::as_str) {
            self.session_mismatch |= id != session_id;
        }
        if event
            .get("permission_denials")
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty())
        {
            self.permission_denied = true;
        }
        if event["type"] == "system" && event["subtype"] == "permission_denied" {
            self.permission_denied = true;
        }
        // A background session prints no headless result. Its supervisor watches the state
        // the harness reports for the session and says how that session ended.
        if event["type"] == "cones_result" {
            self.result_seen = true;
            let state = event["state"].as_str().unwrap_or("unreported");
            self.failed = state != "done";
            self.reason = (state != "done").then(|| format!("session_{state}"));
        }
        if event["type"] == "result" {
            // Claude ends a headless turn with a result and then answers again when a
            // background task of its own completes, so one run can report several. The last
            // result is the run's verdict; the totals are what every result spent together.
            self.result_seen = true;
            self.session_mismatch |= event["session_id"].as_str() != Some(session_id);
            self.failed =
                event["is_error"].as_bool().unwrap_or(true) || event["subtype"] != "success";
            self.reason = event["subtype"]
                .as_str()
                .filter(|s| *s != "success")
                .map(str::to_owned);
            let cost = event["total_cost_usd"]
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0);
            if !self.failed && cost.is_none() {
                self.failed = true;
                self.reason = Some("missing_cost".into());
            }
            if let Some(cost) = cost {
                *self.cost_usd.get_or_insert(0.0) += cost;
            }
            let tokens_in = event["usage"]["input_tokens"].as_u64().map(|n| {
                n.saturating_add(
                    event["usage"]["cache_creation_input_tokens"]
                        .as_u64()
                        .unwrap_or(0),
                )
                .saturating_add(
                    event["usage"]["cache_read_input_tokens"]
                        .as_u64()
                        .unwrap_or(0),
                )
            });
            if let Some(tokens) = tokens_in {
                let total = self.tokens_in.get_or_insert(0);
                *total = total.saturating_add(tokens);
            }
            if let Some(tokens) = event["usage"]["output_tokens"].as_u64() {
                let total = self.tokens_out.get_or_insert(0);
                *total = total.saturating_add(tokens);
            }
        }
        Ok(())
    }
}

/// Tested Claude version range, reported on a job that would run an untested Claude.
pub const TESTED_CLAUDE_RANGE: &str = ">=2.1, <3";

/// `None` when the output has no leading version.
pub fn claude_version_tested(output: &str) -> Option<bool> {
    let mut parts = output
        .split_whitespace()
        .next()?
        .split('.')
        .map(|p| p.parse::<u64>().ok());
    let (major, minor) = (parts.next()??, parts.next()??);
    Some((major, minor) >= (2, 1) && major < 3)
}

/// Compiled switches before the prompt delimiter, for the flag probes of the dashboard's
/// doctor panel; the `cones doctor` command they were written for is gone.
pub fn compiled_flags(args: &[String]) -> Vec<&str> {
    args.iter()
        .take_while(|a| *a != "--")
        .filter(|a| a.starts_with("--"))
        .map(String::as_str)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_composer_launch_carries_the_defaults_model_and_provider() {
        let p = Policy {
            model: Some("opus".into()),
            codex_model: Some("gpt-5.6-luna".into()),
            bedrock: Some(true),
            ..Policy::default()
        };
        assert_eq!(
            session_args(HarnessKind::Claude, None, "fix it", &p).unwrap(),
            ["--bg", "--model", "opus", "--", "fix it"]
        );
        assert_eq!(
            session_args(
                HarnessKind::Codex,
                Some(("unix:///s.sock", Path::new("/repo"))),
                "fix it",
                &p
            )
            .unwrap(),
            [
                "--remote",
                "unix:///s.sock",
                "-C",
                "/repo",
                "-m",
                "gpt-5.6-luna",
                "--",
                "fix it"
            ]
        );
        assert_eq!(
            session_args(HarnessKind::Claude, None, "x", &Policy::default()).unwrap(),
            ["--bg", "--", "x"]
        );
        // The app-server daemon keeps the provider it started with, so cones passes
        // no provider of its own; config refuses `bedrock` on a Codex job for that reason.
        let direct = Policy {
            bedrock: Some(false),
            codex_model: Some("a-model".into()),
            ..Policy::default()
        };
        assert_eq!(
            session_args(HarnessKind::Codex, None, "x", &direct).unwrap(),
            ["-m", "a-model", "--", "x"]
        );
        // Each harness receives only its own configured model and provider.
        assert_eq!(
            session_args(HarnessKind::Pi, None, "fix it", &p).unwrap(),
            ["--", "fix it"]
        );
        let pi = Policy {
            pi_model: Some("native-model".into()),
            pi_provider: Some("native-provider".into()),
            ..p
        };
        assert_eq!(
            session_args(HarnessKind::Pi, None, "fix it", &pi).unwrap(),
            [
                "--model",
                "native-model",
                "--provider",
                "native-provider",
                "--",
                "fix it"
            ]
        );
    }

    #[test]
    fn a_composer_launch_carries_each_harness_s_own_effort_flag() {
        let p = Policy {
            effort: Some("high".into()),
            pi_thinking: Some("minimal".into()),
            codex_model: Some("gpt-5.6-luna".into()),
            opencode_model: Some("provider/model".into()),
            ..Policy::default()
        };
        assert_eq!(
            session_args(HarnessKind::Claude, None, "fix it", &p).unwrap(),
            ["--bg", "--effort", "high", "--", "fix it"]
        );
        assert_eq!(
            session_args(HarnessKind::Pi, None, "fix it", &p).unwrap(),
            ["--thinking", "minimal", "--", "fix it"]
        );
        // Codex takes reasoning effort only through a configuration override and OpenCode
        // takes none at all, so a level set for Claude or pi reaches neither.
        assert_eq!(
            session_args(HarnessKind::Codex, None, "fix it", &p).unwrap(),
            ["-m", "gpt-5.6-luna", "--", "fix it"]
        );
        assert_eq!(
            session_args(HarnessKind::Opencode, None, "fix it", &p).unwrap(),
            ["--model", "provider/model", "--prompt=fix it"]
        );
    }

    #[test]
    fn the_composer_offers_every_harness_claude_first() {
        assert_eq!(
            known().iter().map(ToString::to_string).collect::<Vec<_>>(),
            [
                "claude",
                "codex",
                "pi",
                "opencode",
                "gemini",
                "cursor-agent",
                "copilot",
                "amp",
                "droid",
                "kimi"
            ],
            "shift+tab cycles in this order and start.harness defaults to the first"
        );
    }

    #[test]
    fn a_bedrock_job_gets_the_switch_and_the_shell_s_aws_variables() {
        let dir = std::env::temp_dir();
        let mut job = crate::config::adhoc(None, "p", &dir).unwrap();
        // SAFETY: a name no other test reads, set before the environment is built.
        unsafe { std::env::set_var("AWS_CONES_TEST_REGION", "us-west-2") };
        let env = environment(&job).unwrap();
        assert!(!env.contains_key("CLAUDE_CODE_USE_BEDROCK"));
        assert!(!env.contains_key("AWS_CONES_TEST_REGION"));
        job.bedrock = Some(true);
        job.aws_profile = Some("claude".into());
        job.aws_region = Some("us-east-1".into());
        let env = environment(&job).unwrap();
        assert_eq!(
            env["CLAUDE_CODE_USE_BEDROCK"], "1",
            "the switch is the one claude.yaml names"
        );
        assert_eq!(env["AWS_CONES_TEST_REGION"], "us-west-2");
        assert_eq!(env["AWS_PROFILE"], "claude");
        assert_eq!(env["AWS_REGION"], "us-east-1");
    }

    #[test]
    fn aws_credentials_reach_a_harness_that_takes_no_bedrock_switch() {
        let dir = std::env::temp_dir();
        let mut job = crate::config::adhoc(None, "p", &dir).unwrap();
        job.harness = HarnessKind::Pi;
        job.aws_profile = Some("claude".into());
        job.aws_region = Some("us-east-1".into());
        let env = environment(&job).unwrap();
        assert_eq!(env["AWS_PROFILE"], "claude");
        assert_eq!(env["AWS_REGION"], "us-east-1");
        assert!(
            !env.keys().any(|k| k.contains("BEDROCK")),
            "pi declares no switch, so none is invented for it"
        );
    }

    #[test]
    fn a_composer_session_carries_aws_to_every_harness_and_the_switch_to_claude_alone() {
        // The definitions alone answer this, so no harness is launched or probed.
        for kind in known() {
            let launch = spec(*kind).launch.as_ref().expect("every harness launches");
            let env = |bedrock| {
                let mut c = std::process::Command::new("true");
                provider_env(&mut c, launch, bedrock, Some("claude"), Some("us-east-1"));
                c.get_envs()
                    .map(|(k, v)| {
                        (
                            k.to_string_lossy().into_owned(),
                            v.map(|v| v.to_string_lossy().into_owned()),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let set = env(Some(true));
            for (key, value) in [("AWS_PROFILE", "claude"), ("AWS_REGION", "us-east-1")] {
                assert!(
                    set.contains(&(key.to_owned(), Some(value.to_owned()))),
                    "{kind} resolves AWS like every other harness: {set:?}"
                );
            }
            let switch = |env: &[(String, Option<String>)]| {
                env.iter()
                    .find(|(k, _)| k == "CLAUDE_CODE_USE_BEDROCK")
                    .map(|(_, v)| v.clone())
            };
            assert_eq!(
                switch(&set),
                (*kind == HarnessKind::Claude).then_some(Some("1".to_owned())),
                "{kind} gets the switch only if its definition names one"
            );
            // false removes the switch a native settings file may have set.
            assert_eq!(
                switch(&env(Some(false))),
                (*kind == HarnessKind::Claude).then_some(None),
            );
            assert_eq!(switch(&env(None)), None, "unset passes no switch at all");
        }
    }

    #[test]
    fn a_configured_provider_outranks_the_identity_a_launch_drops() {
        let launch = spec(HarnessKind::Claude)
            .launch
            .as_ref()
            .expect("claude launches");
        let switch = |bedrock| {
            let mut c = std::process::Command::new("true");
            provider_env(&mut c, launch, bedrock, None, None);
            drop_host_identity(&mut c);
            c.get_envs()
                .find(|(name, _)| *name == OsStr::new("CLAUDE_CODE_USE_BEDROCK"))
                .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
        };
        // The config chose the provider, so the pane keeps it; with nothing configured the
        // launcher's own switch is identity like the rest and the pane starts without it.
        assert_eq!(switch(Some(true)), Some(Some("1".to_owned())));
        assert_eq!(switch(Some(false)), Some(None));
        assert_eq!(switch(None), Some(None));
    }

    #[test]
    fn the_daemon_socket_comes_from_the_first_json_line() {
        let out = "warning: experimental\n{\"status\":\"alreadyRunning\",\"socketPath\":\"/u/.codex/app-server-control/app-server-control.sock\"}\n";
        assert_eq!(
            socket_path(out).as_deref(),
            Some("/u/.codex/app-server-control/app-server-control.sock")
        );
        assert_eq!(socket_path("not json"), None);
    }

    #[test]
    fn a_live_daemon_address_is_reused_and_a_stale_one_is_rediscovered() {
        use std::{
            fs,
            os::unix::{fs::PermissionsExt, net::UnixListener},
        };
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("codex");
        let first_path = dir.path().join("first.sock");
        let first = UnixListener::bind(&first_path).unwrap();
        let advertise = |socket: &Path| {
            let json = serde_json::json!({"socketPath": socket})
                .to_string()
                .replace('\'', "'\\''");
            fs::write(&program, format!("#!/bin/sh\nprintf '%s\\n' '{json}'\n")).unwrap();
            fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        };
        advertise(&first_path);
        let home = dir.path().join("home");
        let (_, address) = codex_remote(&program, &home).unwrap();
        fs::write(&program, "#!/bin/sh\nexit 99\n").unwrap();
        assert_eq!(
            codex_remote(&program, &home).unwrap().1,
            address,
            "an existing daemon does not require another CLI startup"
        );
        drop(first);
        fs::remove_file(first_path).unwrap();
        let second_path = dir.path().join("second.sock");
        let _second = UnixListener::bind(&second_path).unwrap();
        advertise(&second_path);
        assert_eq!(
            codex_remote(&program, &home).unwrap().1,
            format!("unix://{}", second_path.display())
        );
    }
    #[test]
    fn stdin_prompt_preserves_literal_bytes_in_an_anonymous_file() {
        let prompt = "--model fake\n$(touch should-not-exist) `echo nope` 'quoted'\n";
        let mut cat = std::process::Command::new("/bin/cat");
        cat.env("CONES_TEST_STDIN", "yes");
        let mut command = stdin_prompt(cat, prompt).unwrap();
        assert_eq!(command.get_program(), "/bin/cat");
        assert_eq!(command.get_args().count(), 0);
        let out = command.output().unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, prompt.as_bytes());
        assert_eq!(
            command.output().unwrap().stdout,
            prompt.as_bytes(),
            "reusing a prepared command rewinds its input"
        );
    }

    #[test]
    fn terminal_harness_prompts_remain_single_native_operands_without_permission_bypasses() {
        let prompt = "--model=other; $(echo surprise)\nsecond line";
        for &kind in known().iter().filter(|k| k.terminal_only()) {
            let args = session_args(kind, None, prompt, &Policy::default()).unwrap();
            let args: Vec<_> = args.iter().map(|s| s.to_str().unwrap()).collect();
            match kind {
                HarnessKind::Gemini => assert_eq!(args, [format!("--prompt-interactive={prompt}")]),
                HarnessKind::Copilot => assert_eq!(args, [format!("--interactive={prompt}")]),
                HarnessKind::Kimi => assert_eq!(args, [format!("--prompt={prompt}")]),
                HarnessKind::Amp => assert!(args.is_empty()),
                HarnessKind::Cursor | HarnessKind::Droid => assert_eq!(args, ["--", prompt]),
                _ => unreachable!(),
            }
            assert!(adapter(kind).is_err());
            assert!(!spec(kind).transcript.available);
            assert!(spec(kind).operations.resume.is_none());
            assert!(spec(kind).operations.fork.is_none());
        }
    }
}
