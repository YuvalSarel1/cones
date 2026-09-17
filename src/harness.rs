pub mod spec;
pub use spec::{by_name, known, spec};

use crate::config::{HarnessKind, Policy, ResolvedJob};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
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
    fn compile(&self, job: &ResolvedJob, session_id: &str) -> Result<Invocation>;
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

/// Native session launch with harness-owned permissions and lifetime.
/// Background commands return after launch; foreground commands are daemon clients.
/// See docs/harness.md and docs/dashboard.md for the ownership boundary.
pub enum Start {
    Background(std::process::Command),
    Foreground(std::process::Command),
}

pub fn start(kind: HarnessKind, dir: &Path, prompt: &str, policy: &Policy) -> Result<Start> {
    let name = kind.to_string();
    let path = executable(&name, &launch_path())
        .ok_or_else(|| anyhow::anyhow!("{name} not found on the launch PATH"))?;
    leave_and_return(kind)?;
    Ok(match spec(kind).launch.handler {
        spec::LaunchHandler::ClaudeBackground => {
            let mut c = std::process::Command::new(path);
            c.args(session_args(kind, None, prompt, policy))
                .current_dir(dir);
            // Claude settings.json `env` can override this provider switch.
            match policy.bedrock {
                Some(true) => {
                    c.env("CLAUDE_CODE_USE_BEDROCK", "1");
                    for (key, set) in [
                        ("AWS_PROFILE", &policy.aws_profile),
                        ("AWS_REGION", &policy.aws_region),
                    ] {
                        if let Some(v) = set {
                            c.env(key, v);
                        }
                    }
                }
                Some(false) => {
                    c.env_remove("CLAUDE_CODE_USE_BEDROCK");
                }
                None => {}
            }
            Start::Background(c)
        }
        spec::LaunchHandler::CodexRemote => {
            let (path, remote) =
                codex_remote(&path, &crate::codex::home(&crate::fleet::claude_dir()?))?;
            let mut c = std::process::Command::new(path);
            c.args(session_args(kind, Some((&remote, dir)), prompt, policy))
                .current_dir(dir);
            Start::Foreground(c)
        }
        spec::LaunchHandler::Terminal => {
            let mut c = std::process::Command::new(path);
            c.args(session_args(kind, None, prompt, policy))
                .current_dir(dir);
            Start::Foreground(c)
        }
    })
}

/// Model and provider overrides for native sessions; Claude selects its provider through env.
pub fn session_args(
    kind: HarnessKind,
    remote: Option<(&str, &Path)>,
    prompt: &str,
    policy: &Policy,
) -> Vec<OsString> {
    let launch = &spec(kind).launch;
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
    if let Some(model) = &launch.model {
        let value = match model.source {
            spec::ModelSource::Claude => &policy.model,
            spec::ModelSource::Codex => &policy.codex_model,
        };
        if let Some(value) = value {
            args.extend([OsString::from(&model.flag), value.into()]);
        }
    }
    args.extend(
        spec::args(&launch.prompt, &[("prompt", prompt.as_ref())])
            .expect("validated prompt template"),
    );
    args
}

/// Check that the installed harness can run a session the dashboard starts, and
/// say how long that session lives once its viewer is gone.
pub fn leave_and_return(kind: HarnessKind) -> Result<String> {
    let name = kind.to_string();
    let path = executable(&name, &launch_path())
        .ok_or_else(|| anyhow::anyhow!("{name} not found on the launch PATH"))?;
    let probe = &spec(kind).probe;
    let output = std::process::Command::new(&path)
        .args(&probe.args)
        .output()
        .with_context(|| format!("{name} {}", probe.args.join(" ")))?;
    probe.report(
        output.status.success(),
        &String::from_utf8_lossy(&output.stdout),
    )
}

/// Resume a thread against the daemon of the home that holds it, not the ambient one:
/// a home pinned to another provider region keeps its own daemon and its own socket.
pub fn codex_resume(home: &Path, id: &str, cwd: &Path) -> Result<std::process::Command> {
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

/// One native join contract for Enter and hover. Hover is forbidden from launching a session.
pub fn join(
    session: &crate::fleet::Session,
    home: &Path,
    speculative: bool,
) -> Result<std::process::Command> {
    let spec = by_name(&session.harness).context("unknown session harness")?;
    match spec.session(session.kind.as_deref()).join {
        spec::Join::Unavailable => bail!(
            "{} runs in its own terminal and cannot be joined from here",
            spec.name
        ),
        spec::Join::Attach => Claude.attach(&session.session_id, &session.cwd),
        spec::Join::CodexRemote => codex_client(
            home,
            &session.session_id,
            &session.cwd,
            &spec.commands.attach,
            speculative,
        ),
    }
}

pub fn can_peek(session: &crate::fleet::Session, home: &Path) -> bool {
    let Some(spec) = by_name(&session.harness) else {
        return false;
    };
    if !spec.permits_peek(session) {
        return false;
    }
    match spec.viewer.peek {
        spec::Peek::Join => true,
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
    };
    command.env(&spec.home.env, &entry.key.home);
    if entry.archived {
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

/// Embedded coordinator plugin, loaded only for the session that starts it.
pub const COORDINATOR_SKILL: &str = "start-orchestrator";
const COORDINATOR_FILES: [(&str, &str); 6] = [
    (
        ".claude-plugin/plugin.json",
        include_str!("../assets/coordinator/.claude-plugin/plugin.json"),
    ),
    (
        "skills/start-orchestrator/SKILL.md",
        include_str!("../assets/coordinator/skills/start-orchestrator/SKILL.md"),
    ),
    (
        "skills/start-orchestrator/bin/self.sh",
        include_str!("../assets/coordinator/skills/start-orchestrator/bin/self.sh"),
    ),
    (
        "skills/start-orchestrator/bin/sweep.sh",
        include_str!("../assets/coordinator/skills/start-orchestrator/bin/sweep.sh"),
    ),
    (
        "skills/start-orchestrator/bin/status.py",
        include_str!("../assets/coordinator/skills/start-orchestrator/bin/status.py"),
    ),
    (
        "skills/start-orchestrator/bin/codex.sh",
        include_str!("../assets/coordinator/skills/start-orchestrator/bin/codex.sh"),
    ),
];

/// Find the skill's live coordinator record, including coordinators started outside cones.
pub fn coordinator_status(dir: &Path) -> Option<Value> {
    let files = std::fs::read_dir(dirs::home_dir()?.join(".claude/orchestrator")).ok()?;
    files.flatten().find_map(|entry| {
        let status: Value = serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
        let pid = status.get("pid")?.as_u64()? as u32;
        (status.get("cwd")?.as_str()? == dir.to_str()? && crate::fleet::alive(pid))
            .then_some(status)
    })
}

/// Rewrite the embedded plugin on each start so upgrades include the current skill.
pub fn coordinator_plugin(state: &Path) -> Result<PathBuf> {
    let plugin = state.join("coordinator/plugin");
    let bin = plugin.join("skills").join(COORDINATOR_SKILL).join("bin");
    for (rel, text) in COORDINATOR_FILES {
        let path = plugin.join(rel);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            text.replace("__CONES_COORDINATOR_BIN__", &bin.to_string_lossy()),
        )?;
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
    if job.bedrock == Some(true) {
        // Inherit AWS credentials, then override profile and region with the validated job values.
        env.insert("CLAUDE_CODE_USE_BEDROCK".into(), "1".into());
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

/// Claude's complete tool allowlist. Scoped Bash rules only pre-approve commands;
/// they do not enforce an exclusive allowlist.
pub fn effective_tools(job: &ResolvedJob) -> &'static [&'static str] {
    if job.write {
        &["Read", "Grep", "Glob", "Edit", "Write", "Bash"]
    } else {
        &["Read", "Grep", "Glob"]
    }
}

impl Harness for Claude {
    fn compile(&self, job: &ResolvedJob, session_id: &str) -> Result<Invocation> {
        ensure!(
            job.harness == HarnessKind::Claude,
            "Claude adapter requires a Claude job"
        );
        uuid::Uuid::parse_str(session_id)?;
        let tools = effective_tools(job).join(",");
        let mut args: Vec<String> = [
            "--print",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            "dontAsk",
            "--permission-prompts",
            "none",
            "--safe-mode",
            "--restricted",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--mcp-config",
            "{\"mcpServers\":{}}",
            "--disable-slash-commands",
            "--tools",
            &tools,
            "--allowedTools",
            &tools,
            "--session-id",
            session_id,
            "--name",
            &job.name,
        ]
        .into_iter()
        .map(String::from)
        .collect();
        if job.write {
            // Require native filesystem/network isolation without expanding permissions
            // through sandbox auto-approval.
            args.extend([
                "--settings".into(),
                serde_json::json!({"sandbox":{
                    "enabled":true, "failIfUnavailable":true,
                    "autoAllowBashIfSandboxed":false, "allowUnsandboxedCommands":false,
                    "excludedCommands":[]
                }})
                .to_string(),
            ]);
        }
        if let Some(model) = &job.model {
            args.extend(["--model".into(), model.clone()]);
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
        uuid::Uuid::parse_str(session_id)?;
        let project: String = cwd
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        Ok(dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("missing home directory"))?
            .join(".claude")
            .join(spec(HarnessKind::Claude).transcript.live_root())
            .join(project)
            .join(format!("{session_id}.jsonl")))
    }
}

pub fn policy_hash(job: &ResolvedJob, invocation: &Invocation) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&compiled_policy(job, invocation)?)?)
    ))
}

pub fn compiled_policy(job: &ResolvedJob, invocation: &Invocation) -> Result<Value> {
    // Exclude task text, generated session identity and secret values from the policy hash.
    let mut args = invocation.args.clone();
    if let Some(i) = args.iter().position(|a| a == "--session-id") {
        args[i + 1] = "<session-id>".into();
    }
    if let Some(i) = args.iter().position(|a| a == "--name") {
        args[i + 1] = "<job-name>".into();
    }
    args.pop(); // The final positional prompt is task content, not policy.
    let policy = serde_json::json!({
        "v":3, "harness":job.harness, "program":invocation.program,
        "enforcement":"native-flags",
        "args":args,
        "cwd":job.cwd, "write":job.write, "tools":effective_tools(job),
        "permission_mode":"dontAsk", "permission_prompts":"none",
        "safe_mode":true, "restricted":true, "mcp":false,
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
        if event["type"] == "result" {
            ensure!(!self.result_seen, "duplicate Claude result");
            self.result_seen = true;
            self.session_mismatch |= event["session_id"].as_str() != Some(session_id);
            self.failed =
                event["is_error"].as_bool().unwrap_or(true) || event["subtype"] != "success";
            self.reason = event["subtype"]
                .as_str()
                .filter(|s| *s != "success")
                .map(str::to_owned);
            self.cost_usd = event["total_cost_usd"]
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0);
            if !self.failed && self.cost_usd.is_none() {
                self.failed = true;
                self.reason = Some("missing_cost".into());
            }
            self.tokens_in = event["usage"]["input_tokens"].as_u64().map(|n| {
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
            self.tokens_out = event["usage"]["output_tokens"].as_u64();
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
            session_args(HarnessKind::Claude, None, "fix it", &p),
            ["--bg", "--model", "opus", "--", "fix it"]
        );
        assert_eq!(
            session_args(
                HarnessKind::Codex,
                Some(("unix:///s.sock", Path::new("/repo"))),
                "fix it",
                &p
            ),
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
            session_args(HarnessKind::Claude, None, "x", &Policy::default()),
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
            session_args(HarnessKind::Codex, None, "x", &direct),
            ["-m", "a-model", "--", "x"]
        );
        // pi is started with the instruction alone: the model and provider defaults
        // name claude and codex, and a Claude alias is not a pi model pattern.
        assert_eq!(
            session_args(HarnessKind::Pi, None, "fix it", &p),
            ["--", "fix it"]
        );
    }

    #[test]
    fn the_composer_offers_every_harness_claude_first() {
        assert_eq!(
            known().iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["claude", "codex", "pi"],
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
        let env = environment(&job).unwrap();
        assert_eq!(env["CLAUDE_CODE_USE_BEDROCK"], "1");
        assert_eq!(env["AWS_CONES_TEST_REGION"], "us-west-2");
        job.aws_profile = Some("claude".into());
        job.aws_region = Some("us-east-1".into());
        let env = environment(&job).unwrap();
        assert_eq!(env["AWS_PROFILE"], "claude");
        assert_eq!(env["AWS_REGION"], "us-east-1");
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
}
