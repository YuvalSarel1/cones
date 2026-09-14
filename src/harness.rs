use crate::config::{HarnessKind, ResolvedJob};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
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
    match kind {
        HarnessKind::Claude => Ok(Box::new(Claude)),
        _ => bail!(
            "{kind} execution is not available in v0.1; its dollar budget cannot yet be enforced. Use harness: claude"
        ),
    }
}

/// Every harness cones knows, in the order the dashboard's `n` prompt offers them.
pub const KNOWN: [HarnessKind; 2] = [HarnessKind::Claude, HarnessKind::Codex];

/// The harness started natively in `dir`, as typing its name in a shell there would: no
/// policy, no ledger, the harness's own permission prompts. Only a harness whose session
/// outlives the viewer opens from the dashboard, because leaving must keep it working. Claude
/// starts as a background session with `claude attach` on it; `--bg` picks the id itself, so
/// the launcher reads it from the `backgrounded · <id>` line. Codex starts as a thread of its
/// app-server daemon with the TUI as a `--remote` client; leaving the client keeps the thread,
/// and the dashboard records its id from the rollout to resume it.
///
/// Two other ways to leave a harness were tried and rejected: stopping the client with SIGTSTP
/// parks the agent, which freezes it until re-entered, and a cones-owned pty proxy (dtach
/// style) keeps it running but makes cones the owner of the agent's terminal and lifetime,
/// which belong to the harness. A harness with no mode whose session outlives the viewer is
/// refused here with the reason, not parked or proxied.
pub fn interactive(kind: HarnessKind, dir: &Path) -> Result<std::process::Command> {
    let name = kind.to_string();
    let path = executable(&name, &launch_path())
        .ok_or_else(|| anyhow::anyhow!("{name} not found on the launch PATH"))?;
    leave_and_return(kind)?;
    let mut cmd = match kind {
        HarnessKind::Claude => {
            let mut c = std::process::Command::new("/bin/sh");
            c.arg("-c").arg(BG_THEN_ATTACH).arg(path);
            c
        }
        HarnessKind::Codex => {
            let (path, remote) = codex_remote(&path)?;
            let mut c = std::process::Command::new(path);
            c.args(["--remote", &remote, "-C"]).arg(dir);
            c
        }
    };
    cmd.current_dir(dir);
    Ok(cmd)
}

/// Whether this build of the harness can be opened from the dashboard and left running: Claude
/// needs `--bg` and `attach`, Codex its app-server daemon (0.154 and later, experimental
/// there). The message is the doctor line; the error is what the `n` prompt shows instead of
/// opening a session that could not be left.
pub fn leave_and_return(kind: HarnessKind) -> Result<String> {
    let name = kind.to_string();
    let path = executable(&name, &launch_path())
        .ok_or_else(|| anyhow::anyhow!("{name} not found on the launch PATH"))?;
    let run = |args: &[&str]| {
        std::process::Command::new(&path)
            .args(args)
            .output()
            .map(|o| {
                (
                    o.status.success(),
                    String::from_utf8_lossy(&o.stdout).into_owned(),
                )
            })
            .map_err(|e| anyhow::anyhow!("{name} {}: {e}", args.join(" ")))
    };
    match kind {
        HarnessKind::Claude => {
            let (_, help) = run(&["--help"])?;
            ensure!(
                help.contains("--bg") && help.contains("attach"),
                "this claude has no --bg or attach, so a session opened here could not be left running; update Claude Code"
            );
            Ok("claude: background sessions with a viewer (--bg, attach)".into())
        }
        HarnessKind::Codex => {
            let (ok, json) = run(&["app-server", "daemon", "version"])?;
            ensure!(
                ok,
                "this codex has no app-server daemon, so a session opened here could not be left running; Codex 0.154 or later has one"
            );
            let ver = json
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
                .find_map(|v| v["cliVersion"].as_str().map(str::to_owned))
                .unwrap_or_default();
            Ok(format!(
                "codex {ver}: threads behind the app-server daemon (experimental in Codex)"
            ))
        }
    }
}

/// The harness's own list of its agents, opened without picking a row first: `claude agents`,
/// or Codex's `resume` picker as a client of the app-server daemon, so a thread picked there
/// keeps working after the client is left, as a thread opened with `n` does. A thread resumed
/// this way is not recorded: `launched` reads rollouts started after the launch only, and
/// the daemon has no thread list yet, so once left it is a row again only when a client shows
/// it.
pub fn agents(kind: HarnessKind) -> Result<std::process::Command> {
    let name = kind.to_string();
    let path = executable(&name, &launch_path())
        .ok_or_else(|| anyhow::anyhow!("{name} not found on the launch PATH"))?;
    Ok(match kind {
        HarnessKind::Claude => {
            let mut c = std::process::Command::new(path);
            c.arg("agents");
            c
        }
        HarnessKind::Codex => {
            let (path, remote) = codex_remote(&path)?;
            let mut c = std::process::Command::new(path);
            c.args(["--remote", &remote, "resume", "--all"]);
            c
        }
    })
}

/// The client that reopens a daemon thread in its directory.
pub fn codex_resume(id: &str, cwd: &Path) -> Result<std::process::Command> {
    let path = executable("codex", &launch_path())
        .ok_or_else(|| anyhow::anyhow!("codex not found on the launch PATH"))?;
    let (path, remote) = codex_remote(&path)?;
    let mut c = std::process::Command::new(path);
    c.args(["--remote", &remote, "resume", id]).current_dir(cwd);
    Ok(c)
}

/// The daemon's address, starting it if it is not running: `codex app-server daemon start` is
/// idempotent and prints JSON with `socketPath` either way. Experimental in Codex 0.154.
fn codex_remote(codex: &Path) -> Result<(PathBuf, String)> {
    let out = std::process::Command::new(codex)
        .args(["app-server", "daemon", "start"])
        .output()
        .map_err(|e| anyhow::anyhow!("codex app-server daemon start: {e}"))?;
    let sock = socket_path(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| {
        anyhow::anyhow!(
            "codex app-server daemon start gave no socketPath: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    })?;
    Ok((codex.to_owned(), format!("unix://{sock}")))
}

/// `socketPath` from the first JSON line the daemon command prints.
pub fn socket_path(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find_map(|v| v["socketPath"].as_str().map(str::to_owned))
}

/// `$0` is the claude binary. The id is the first eight-hex-digit word of `claude --bg`'s
/// output, colors and all; anything else is an error shown as is.
pub const BG_THEN_ATTACH: &str = r#"out=$("$0" --bg 2>&1); id=$(printf '%s' "$out" | grep -oE '[0-9a-f]{8}' | head -1); [ -n "$id" ] || { printf '%s\n' "$out" >&2; exit 1; }; exec "$0" attach "$id""#;

pub fn executable(name: &str, path: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path).map(|d| d.join(name)).find(|p| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

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

/// The coordinator is a skill, not cones: one Claude Code session per folder that greets the
/// agents working there, gates their commits and relays findings. cones ships it as a plugin
/// embedded in the binary and loads it for that session only; nothing lands in ~/.claude.
pub const COORDINATOR_SKILL: &str = "start-orchestrator";
const COORDINATOR_FILES: [(&str, &str); 5] = [
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
];

/// The skill's status file for `dir`, when it names a live process. The skill writes it under
/// ~/.claude/orchestrator whether cones or a hand-typed /start-orchestrator started it, so
/// either guard sees the other.
pub fn coordinator_status(dir: &Path) -> Option<Value> {
    let files = std::fs::read_dir(dirs::home_dir()?.join(".claude/orchestrator")).ok()?;
    files.flatten().find_map(|entry| {
        let status: Value = serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
        let pid = status.get("pid")?.as_u64()? as u32;
        (status.get("cwd")?.as_str()? == dir.to_str()? && crate::fleet::alive(pid))
            .then_some(status)
    })
}

/// Write the embedded plugin under `state` (rewritten on every start, so an upgraded binary
/// carries its skill along) and return the plugin directory.
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

/// `claude --bg --plugin-dir <plugin> /cones:start-orchestrator` in `dir`. The skill refuses a
/// second instance per folder itself, so a blind launch is safe.
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
    env.insert("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN".into(), "1".into());
    Ok(env)
}

pub fn effective_tools(job: &ResolvedJob) -> Result<Vec<String>> {
    let mut tools = BTreeSet::new();
    for rule in &job.tools {
        let base = rule.split('(').next().unwrap_or("");
        ensure!(
            matches!(base, "Read" | "Grep" | "Glob" | "Edit" | "Write" | "Bash"),
            "job {}: unsupported Claude tool {rule}; supported tools: Read, Grep, Glob, Edit, Write, Bash",
            job.name
        );
        if rule != base {
            ensure!(
                base == "Bash"
                    && rule.starts_with("Bash(")
                    && rule.ends_with(')')
                    && rule.len() > 6
                    && !rule[5..rule.len() - 1].contains(['(', ')', '\n', '\r', '\0', ',']),
                "job {}: invalid tool rule {rule}; only Bash(command pattern) rules are supported",
                job.name
            );
        }
        ensure!(
            !job.write || base != "Bash" || matches!(rule.as_str(), "Bash" | "Bash(*)"),
            "job {}: Claude's scoped Bash rules are pre-approvals, not an exclusive command allowlist; use Read/Grep/Glob or explicitly allow sandboxed Bash",
            job.name
        );
        if job.write || matches!(base, "Read" | "Grep" | "Glob") {
            tools.insert(rule.clone());
        }
    }
    Ok(tools.into_iter().collect())
}

impl Harness for Claude {
    fn compile(&self, job: &ResolvedJob, session_id: &str) -> Result<Invocation> {
        ensure!(
            job.harness == HarnessKind::Claude,
            "Claude adapter requires a Claude job"
        );
        uuid::Uuid::parse_str(session_id)?;
        let tools = effective_tools(job)?;
        let bases: BTreeSet<_> = tools.iter().map(|t| t.split('(').next().unwrap()).collect();
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
            &bases.into_iter().collect::<Vec<_>>().join(","),
            "--allowedTools",
            &tools.join(","),
            "--session-id",
            session_id,
            "--max-budget-usd",
            &job.budget_usd.to_string(),
            "--name",
            &job.name,
        ]
        .into_iter()
        .map(String::from)
        .collect();
        if tools.iter().any(|t| t == "Bash" || t.starts_with("Bash(")) {
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
        if let Some(turns) = job.max_turns {
            args.extend(["--max-turns".into(), turns.to_string()]);
        }
        args.extend(["--".into(), job.prompt.clone()]);
        // Resolve from the same PATH that launchd will use. Never rely on a shell alias.
        let env = environment(job)?;
        let program = executable("claude", &env["PATH"]).ok_or_else(|| {
            anyhow::anyhow!("claude is missing from the launchd PATH; run cones doctor")
        })?;
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
        let path = executable("claude", &launch_path())
            .ok_or_else(|| anyhow::anyhow!("claude not found"))?;
        // Resume in the background, then attach: ctrl-z detaches instead of suspending a
        // foreground process group, so the dashboard always gets its terminal back. The session
        // outlives the terminal until it is exited or stopped, like any background session.
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(r#""$0" --bg --resume "$1" >/dev/null && exec "$0" attach "$2""#)
            .arg(path)
            .arg(session_id)
            .arg(&session_id[..8])
            .current_dir(cwd);
        Ok(cmd)
    }
    fn attach(&self, session_id: &str, cwd: &Path) -> Result<std::process::Command> {
        uuid::Uuid::parse_str(session_id)?;
        let path = executable("claude", &launch_path())
            .ok_or_else(|| anyhow::anyhow!("claude not found"))?;
        let mut cmd = std::process::Command::new(path);
        // `claude attach` takes the short id, the first block of the UUID.
        cmd.args(["attach", &session_id[..8]]).current_dir(cwd);
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
            .join(".claude/projects")
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
    // Session IDs, prompts and secret values are not policy. Tool restrictions, executable,
    // working directory, budget, timeout, model and named environment imports are.
    let mut args = invocation.args.clone();
    // Normalize generated session identity and the task text, retaining the actual compiled
    // switches and settings. Changes to a compiler flag therefore change the audit hash.
    if let Some(i) = args.iter().position(|a| a == "--session-id") {
        args[i + 1] = "<session-id>".into();
    }
    if let Some(i) = args.iter().position(|a| a == "--name") {
        args[i + 1] = "<job-name>".into();
    }
    args.pop(); // The final positional prompt is task content, not policy.
    let policy = serde_json::json!({
        "v":2, "harness":job.harness, "program":invocation.program,
        "enforcement":"native-flags",
        "args":args,
        "cwd":job.cwd, "write":job.write, "tools":effective_tools(job)?,
        "permission_mode":"dontAsk", "permission_prompts":"none",
        "safe_mode":true, "restricted":true, "mcp":false,
        "timeout_s":invocation.timeout_s, "budget_usd":job.budget_usd,
        "overlap":job.overlap,
        "daily_budget_usd":job.daily_budget_usd, "max_turns":job.max_turns,
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
            // Budget-stop results can zero the aggregate usage while retaining per-model
            // accounting. Use the harness's reported totals rather than recording false zeros.
            if self.tokens_in.unwrap_or(0) == 0
                && self.tokens_out.unwrap_or(0) == 0
                && let Some(models) = event["modelUsage"].as_object()
            {
                let mut input = 0u64;
                let mut output = 0u64;
                for usage in models.values() {
                    for key in [
                        "inputTokens",
                        "cacheReadInputTokens",
                        "cacheCreationInputTokens",
                    ] {
                        input = input.saturating_add(usage[key].as_u64().unwrap_or(0));
                    }
                    output = output.saturating_add(usage["outputTokens"].as_u64().unwrap_or(0));
                }
                if input > 0 || output > 0 {
                    self.tokens_in = Some(input);
                    self.tokens_out = Some(output);
                }
            }
        }
        Ok(())
    }
}

/// Claude Code versions the compiled flags above were tested against; `cones doctor` warns
/// when the installed version leaves the range.
pub const TESTED_CLAUDE_RANGE: &str = ">=2.1, <3";

/// Whether `claude --version` output such as `2.1.269 (Claude Code)` falls inside the tested
/// range. `None` when the text has no leading version.
pub fn claude_version_tested(output: &str) -> Option<bool> {
    // ponytail: major.minor compare against the constant above; a semver crate if the range
    // ever needs pre-release or patch bounds.
    let mut parts = output
        .split_whitespace()
        .next()?
        .split('.')
        .map(|p| p.parse::<u64>().ok());
    let (major, minor) = (parts.next()??, parts.next()??);
    Some((major, minor) >= (2, 1) && major < 3)
}

/// Every switch in a compiled argv up to the `--` that starts the prompt, so the set doctor
/// probes against `claude --help` is whatever the compiler currently emits.
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
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn launch(bin: &Path) -> std::process::Output {
        std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(BG_THEN_ATTACH)
            .arg(bin)
            .output()
            .unwrap()
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
    fn claude_launcher_reads_the_id_from_bg_output_and_attaches_with_it() {
        let d = tempfile::tempdir().unwrap();
        let fake = script(
            d.path(),
            "claude",
            "#!/bin/sh\ncase \"$1\" in\n--bg) printf 'backgrounded \\302\\267 \\033[36m7890c11a\\033[39m (idle)\\n';;\nattach) echo \"attached $2\";;\nesac\n",
        );
        let out = launch(&fake);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "attached 7890c11a"
        );
        let broken = script(
            d.path(),
            "broken",
            "#!/bin/sh\necho 'no daemon' >&2; exit 2\n",
        );
        let out = launch(&broken);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("no daemon"));
    }
}
