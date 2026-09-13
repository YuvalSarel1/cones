//! Fleet: the last known state of every harness session on this Mac, one JSON file per
//! session under `<state>/fleet/`. One global Claude Code hook writes them (`cones hook`);
//! `ls` and the dashboard read them. The hook only observes; it never gates a tool call.
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub v: u32,
    pub session_id: String,
    #[serde(default = "claude")]
    pub harness: String,
    pub cwd: PathBuf,
    /// `active`, `idle` (Stop fired, or the idle_prompt Notification), `blocked` (a permission or
    /// elicitation Notification) or `exited`.
    pub state: String,
    pub updated: DateTime<Utc>,
    /// When the session was first seen. Rows sort by this so they hold still while `updated` ticks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    /// Tokens in the context window at the last turn, and that window's size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Claude's own session title (`ai-title`, or a user-set `agent-name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// First line of the assistant's most recent text: what the session is doing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
}
fn claude() -> String {
    "claude".into()
}
pub const STATES: [&str; 4] = ["active", "idle", "blocked", "exited"];

/// Hook events the fleet hook subscribes to. PreToolUse is deliberately absent: cones observes
/// sessions, it does not sit on the tool-call path.
pub const EVENTS: [&str; 6] = [
    "SessionStart",
    "UserPromptSubmit",
    "PostToolUse",
    "Notification",
    "Stop",
    "SessionEnd",
];

pub fn dir(state: &Path) -> PathBuf {
    state.join("fleet")
}

/// Apply one hook payload (the JSON Claude Code writes to the hook's stdin) to the state file.
/// Tolerate a missing or unparsable transcript instead of returning an error: every live
/// session runs this hook, so one failure here breaks all of them at once.
pub fn record(state: &Path, pid: u32, payload: &Value) -> Result<()> {
    let id = payload["session_id"]
        .as_str()
        .context("hook payload has no session_id")?;
    let event = payload["hook_event_name"].as_str().unwrap_or("");
    let transcript = payload["transcript_path"].as_str().map(PathBuf::from);
    // The hook's own file only: asking claude for its agent list on every hook event is too slow.
    let previous = sessions(state)?.into_iter().find(|s| s.session_id == id);
    // Counting tokens means reading the whole transcript, so do it once per turn, not per tool.
    let counted = match event {
        "Stop" | "SessionEnd" => transcript.as_deref().and_then(|t| usage(t).ok()),
        _ => None,
    };
    let counted = counted.unwrap_or_else(|| {
        previous.as_ref().map_or(Usage::default(), |p| Usage {
            tokens_in: p.tokens_in,
            tokens_out: p.tokens_out,
            context: p.context_tokens,
            window: p.context_window,
        })
    });
    let (title, mut last) = transcript
        .as_deref()
        .map_or((None, Vec::new()), |t| tail(t, 1));
    // ponytail: the title is only read from the tail. Claude writes it in the first turn and
    // again on every resume, and hooks fire per tool call, so it is seen before it scrolls out.
    let title = title.or_else(|| previous.as_ref().and_then(|p| p.title.clone()));
    let last = last
        .pop()
        .or_else(|| previous.as_ref().and_then(|p| p.last.clone()));
    write(
        state,
        &Session {
            v: 1,
            session_id: id.into(),
            harness: claude(),
            cwd: payload["cwd"].as_str().unwrap_or("").into(),
            state: match event {
                // Nothing has been asked yet after SessionStart: waiting, not working.
                "Stop" | "SessionStart" => "idle".into(),
                "Notification" => match payload["notification_type"].as_str().unwrap_or("") {
                    "permission_prompt" | "elicitation_dialog" | "elicitation_url_dialog" => {
                        "blocked".into()
                    }
                    // Fires a minute after a turn ends with nothing typed: still idle.
                    "idle_prompt" => "idle".into(),
                    // auth_success, agent_completed, quota_*: informational, state unchanged.
                    _ => previous
                        .as_ref()
                        .map_or_else(|| "idle".to_owned(), |p| p.state.clone()),
                },
                "SessionEnd" => "exited".into(),
                _ => "active".into(),
            },
            updated: Utc::now(),
            started: previous
                .as_ref()
                .and_then(|p| p.started)
                .or_else(|| Some(Utc::now())),
            event: Some(event.into()),
            tool: payload["tool_name"].as_str().map(Into::into),
            pid: Some(pid),
            transcript_path: transcript,
            tokens_in: counted.tokens_in,
            tokens_out: counted.tokens_out,
            context_tokens: counted.context,
            context_window: counted.window,
            cost_usd: None,
            title,
            last,
        },
    )
}

/// Session title and the last `n` assistant texts (first line each) from a Claude transcript.
/// Read from the end in growing windows, so a long session costs about as much as a short one.
pub fn tail(transcript: &Path, n: usize) -> (Option<String>, Vec<String>) {
    use std::io::{Read, Seek, SeekFrom};
    let mut out = (None, Vec::new());
    let Ok(mut file) = fs::File::open(transcript) else {
        return out;
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let mut window: u64 = 256 * 1024;
    loop {
        let mut bytes = Vec::new();
        if file
            .seek(SeekFrom::Start(len.saturating_sub(window)))
            .is_err()
            || file.read_to_end(&mut bytes).is_err()
        {
            return out;
        }
        let text = String::from_utf8_lossy(&bytes);
        // A window that starts mid-file begins with a partial line; skip it.
        let start = if len > window {
            text.find('\n').map_or(text.len(), |i| i + 1)
        } else {
            0
        };
        out = scan(&text[start..]);
        // ponytail: one tool result can be a megabyte, so grow until both are found or 16 MiB.
        if (out.0.is_some() && out.1.len() >= n) || window >= len || window >= 16 << 20 {
            break;
        }
        window *= 4;
    }
    let keep = out.1.len().saturating_sub(n);
    out.1.drain(..keep);
    out
}

/// Print the last assistant lines of a session transcript and, with `follow`, each new one as it
/// lands. Ctrl+C returns; the session in its own terminal is untouched.
pub fn follow(transcript: &Path, follow: bool) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut out = std::io::stdout();
    for line in tail(transcript, 20).1 {
        writeln!(out, "· {line}")?;
    }
    if !follow {
        return Ok(());
    }
    let cancelled = std::sync::Arc::new(AtomicBool::new(false));
    let sigint = signal_hook::flag::register(signal_hook::consts::SIGINT, cancelled.clone())?;
    let mut pos = fs::metadata(transcript)?.len();
    let mut pending = String::new();
    while !cancelled.load(Ordering::Relaxed) {
        let mut file = fs::File::open(transcript)?;
        file.seek(SeekFrom::Start(pos))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        pos += bytes.len() as u64;
        pending.push_str(&String::from_utf8_lossy(&bytes));
        if let Some(i) = pending.rfind('\n') {
            for line in scan(&pending[..=i]).1 {
                writeln!(out, "· {line}")?;
            }
            pending.drain(..=i);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    signal_hook::low_level::unregister(sigint);
    Ok(())
}

/// The last user prompt and the assistant's full reply to it, as pane lines: prompt lines
/// quoted with `> `, a blank, then every assistant text since. Tool results are user messages
/// too; only text counts as a prompt.
pub fn exchange(transcript: &Path) -> Vec<String> {
    // ponytail: the last MiB is enough; an exchange older than that is not what the pane is for.
    let Ok(text) = crate::output::tail(transcript, 1 << 20) else {
        return Vec::new();
    };
    let (mut prompt, mut reply) = (None, Vec::new());
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let content = &v["message"]["content"];
        let texts: Vec<&str> = match content {
            Value::String(s) => vec![s.as_str()],
            _ => content
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|b| b["text"].as_str())
                .collect(),
        };
        match v["type"].as_str() {
            Some("user") if !texts.concat().trim().is_empty() => {
                prompt = Some(texts.join("\n"));
                reply.clear();
            }
            Some("assistant") => reply.extend(texts.into_iter().map(str::to_owned)),
            _ => {}
        }
    }
    let mut out: Vec<String> = prompt
        .iter()
        .flat_map(|p| p.trim().lines())
        .map(|l| format!("> {l}"))
        .collect();
    out.push(String::new());
    out.extend(reply.join("\n\n").lines().map(|l| l.replace("**", "")));
    out
}

fn scan(lines: &str) -> (Option<String>, Vec<String>) {
    let mut out = (None, Vec::new());
    for line in lines.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v["type"].as_str() {
            Some("ai-title") => out.0 = v["aiTitle"].as_str().map(Into::into),
            Some("agent-name") => out.0 = v["agentName"].as_str().map(Into::into),
            Some("assistant") => {
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    if let Some(first) = block["text"]
                        .as_str()
                        .and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty()))
                    {
                        out.1.push(first.replace("**", ""));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[derive(Default, Clone, Copy)]
struct Usage {
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
    context: Option<u64>,
    window: Option<u64>,
}

/// Total input and output tokens in a Claude transcript, plus the last message's prompt size as
/// the context in use. Streaming writes one line per content block with the same message id and
/// usage, so each message is counted once.
fn usage(transcript: &Path) -> Result<Usage> {
    let mut seen = HashSet::new();
    let (mut input, mut output) = (0, 0);
    let mut last = None;
    for line in std::io::BufReader::new(fs::File::open(transcript)?).lines() {
        let Ok(event) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        let message = &event["message"];
        let Some(u) = message.get("usage") else {
            continue;
        };
        if let Some(id) = message["id"].as_str()
            && !seen.insert(id.to_owned())
        {
            continue;
        }
        let n = |k: &str| u[k].as_u64().unwrap_or(0);
        let prompt =
            n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
        input += prompt;
        output += n("output_tokens");
        // ponytail: Claude writes no window size; 200k unless the model id says [1m].
        let window = if message["model"]
            .as_str()
            .is_some_and(|m| m.contains("[1m]"))
        {
            1_000_000
        } else {
            200_000
        };
        last = Some((prompt, window));
    }
    Ok(Usage {
        tokens_in: Some(input),
        tokens_out: Some(output),
        context: last.map(|(p, _)| p),
        window: last.map(|(_, w)| w),
    })
}

/// Atomically replace the session's file. The id comes from a hook payload, so it is
/// checked before it becomes a file name.
pub fn write(state: &Path, session: &Session) -> Result<()> {
    ensure!(
        !session.session_id.is_empty()
            && session
                .session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid session id"
    );
    let dir = dir(state);
    crate::private_dir(&dir)?;
    // Parallel tool calls fire hooks concurrently, so the temp name is per process.
    let tmp = dir.join(format!("{}.{}.tmp", session.session_id, std::process::id()));
    let mut file = crate::private_file(&tmp)?;
    file.set_len(0)?;
    file.write_all(serde_json::to_string(session)?.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, dir.join(format!("{}.json", session.session_id)))?;
    Ok(())
}

/// Oldest first by when the session started, so rows hold still while `updated` ticks. Unreadable
/// files are skipped; a session the hook is mid-write on is not a failure of the listing.
pub fn sessions(state: &Path) -> Result<Vec<Session>> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir(state)) else {
        return Ok(out);
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Some(s) = fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Session>(&b).ok())
        {
            // ponytail: exited sessions fall off the list after an hour; the files stay.
            if s.state != "exited" || Utc::now() - s.updated < chrono::Duration::hours(1) {
                out.push(s);
            }
        }
    }
    out.sort_by_key(|s| s.started.unwrap_or(s.updated));
    Ok(out)
}

pub fn alive(pid: u32) -> bool {
    // Signal 0 checks existence; EPERM means it exists under another user.
    unsafe { libc::kill(pid as i32, 0) == 0 || *libc::__error() == libc::EPERM }
}

/// Whether the fleet view may run `claude agents --json`. Off in the library so no test ever
/// runs claude; the binary turns it on.
pub static ASK_CLAUDE: AtomicBool = AtomicBool::new(false);

/// The hook's view plus what Claude itself lists: sessions the hook never saw are synthesized,
/// and a background job's one-line `detail` from `~/.claude/jobs/<id>/state.json` becomes the
/// last column.
pub fn with_agents(sessions: Vec<Session>) -> Vec<Session> {
    if !ASK_CLAUDE.load(Ordering::Relaxed) {
        return sessions;
    }
    let jobs = dirs::home_dir().unwrap_or_default().join(".claude/jobs");
    merge(sessions, &agents_json(), &jobs)
}

/// Pure: `agents` is the array `claude agents --json` prints, `jobs` the directory holding one
/// `<id>/state.json` per background job. Anything unparseable leaves the list as it was.
pub fn merge(mut sessions: Vec<Session>, agents: &str, jobs: &Path) -> Vec<Session> {
    let agents: Vec<Value> = serde_json::from_str(agents).unwrap_or_default();
    for a in &agents {
        let Some(id) = a["sessionId"].as_str() else {
            continue;
        };
        // The short id becomes a directory name, so it is checked first.
        let job: Value = a["id"]
            .as_str()
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric()))
            .and_then(|s| fs::read(jobs.join(s).join("state.json")).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let s = match sessions.iter().position(|s| s.session_id == id) {
            Some(i) => &mut sessions[i],
            None => {
                sessions.push(Session {
                    v: 1,
                    session_id: id.into(),
                    harness: claude(),
                    cwd: a["cwd"].as_str().unwrap_or("").into(),
                    state: if a["state"] == "working" {
                        "active"
                    } else {
                        "idle"
                    }
                    .into(),
                    updated: job["updatedAt"]
                        .as_str()
                        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                        .map(Into::into)
                        .or_else(|| {
                            a["startedAt"]
                                .as_i64()
                                .and_then(DateTime::from_timestamp_millis)
                        })
                        .unwrap_or_else(Utc::now),
                    started: a["startedAt"]
                        .as_i64()
                        .and_then(DateTime::from_timestamp_millis),
                    event: None,
                    tool: None,
                    pid: a["pid"].as_u64().map(|p| p as u32),
                    transcript_path: job["linkScanPath"].as_str().map(Into::into),
                    tokens_in: None,
                    tokens_out: None,
                    context_tokens: None,
                    context_window: None,
                    cost_usd: None,
                    title: None,
                    last: None,
                });
                sessions.last_mut().expect("just pushed")
            }
        };
        if let Some(d) = job["detail"].as_str().filter(|d| !d.trim().is_empty()) {
            s.last = Some(d.into());
        }
        if s.title.is_none() {
            s.title = a["name"].as_str().filter(|n| !n.is_empty()).map(Into::into);
        }
    }
    sessions.sort_by_key(|s| s.started.unwrap_or(s.updated));
    sessions
}

/// `claude agents --json`, asked at most every 3 s. The dashboard refreshes every second and
/// claude takes about half of one to answer, so a stale answer is served while a thread fetches.
fn agents_json() -> String {
    static CACHE: Mutex<Option<(Instant, String)>> = Mutex::new(None);
    static FETCHING: AtomicBool = AtomicBool::new(false);
    fn remember(json: String) -> String {
        *CACHE.lock().unwrap() = Some((Instant::now(), json.clone()));
        json
    }
    let cached = CACHE.lock().unwrap().clone();
    match cached {
        Some((at, json)) if at.elapsed() < Duration::from_secs(3) => json,
        Some((_, json)) => {
            if !FETCHING.swap(true, Ordering::SeqCst) {
                std::thread::spawn(|| {
                    remember(run_agents());
                    FETCHING.store(false, Ordering::SeqCst);
                });
            }
            json
        }
        None => remember(run_agents()),
    }
}

/// Fails soft: no claude on PATH, a non-zero exit or a hang past 2 s all read as no agents.
fn run_agents() -> String {
    let path = std::env::var("PATH").unwrap_or_else(|_| crate::harness::launch_path());
    let Some(claude) = crate::harness::executable("claude", &path) else {
        return String::new();
    };
    let Ok(mut child) = Command::new(claude)
        .args(["agents", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return String::new();
    };
    let Some(mut stdout) = child.stdout.take() else {
        return String::new();
    };
    // Drained on its own thread so a long list never blocks the child on a full pipe.
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        stdout
            .read_to_string(&mut out)
            .map(|_| out)
            .unwrap_or_default()
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return reader.join().unwrap_or_default(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return String::new();
            }
        }
    }
}

/// What the fleet view shows, so `logs`, `attach` and `stop` act on every visible row.
pub fn find(state: &Path, session_id: &str) -> Result<Option<Session>> {
    Ok(with_agents(sessions(state)?)
        .into_iter()
        .find(|s| s.session_id == session_id))
}

/// Pure: whether the array `claude agents --json` prints lists this session, so Claude's
/// daemon owns its process.
pub fn is_agent(agents: &str, session_id: &str) -> bool {
    serde_json::from_str::<Vec<Value>>(agents)
        .unwrap_or_default()
        .iter()
        .any(|a| a["sessionId"] == session_id)
}

/// Stop the harness behind a fleet session. Returns false when the process is already
/// gone. The pid came from a hook payload long ago, so the command is checked first: a
/// reused pid never gets signalled.
pub fn stop(state: &Path, session_id: &str) -> Result<bool> {
    let session = find(state, session_id)?.context("no such run or session")?;
    // A background session belongs to Claude's daemon, which respawns a worker whose process
    // dies (`attempt` in ~/.claude/daemon/roster.json). Only `claude stop` ends one for good.
    if ASK_CLAUDE.load(Ordering::Relaxed) && is_agent(&agents_json(), session_id) {
        let claude = crate::harness::executable("claude", &crate::harness::launch_path())
            .context("claude not found")?;
        let short = session_id.get(..8).context("invalid session id")?;
        let out = Command::new(claude)
            .args(["stop", short])
            .stdin(Stdio::null())
            .output()?;
        ensure!(
            out.status.success(),
            "claude stop: {}{}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return Ok(true);
    }
    let pid = session.pid.context("session has no harness pid")?;
    ensure!(pid > 1, "invalid harness pid");
    let output = std::process::Command::new("/bin/ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "command="])
        .output()?;
    let command = String::from_utf8_lossy(&output.stdout);
    let Some(program) = command.split_whitespace().next() else {
        return Ok(false);
    };
    ensure!(
        Path::new(program)
            .file_name()
            .is_some_and(|f| f == session.harness.as_str()),
        "pid {pid} is not a {} process; refusing to signal a reused pid",
        session.harness
    );
    ensure!(
        unsafe { libc::kill(pid as i32, libc::SIGTERM) } == 0,
        "unable to signal session {session_id}"
    );
    Ok(true)
}

pub fn age(updated: DateTime<Utc>) -> String {
    let s = (Utc::now() - updated).num_seconds().max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// Dollars for a table cell: cents, or four places when a run cost less than a cent.
pub fn cost(usd: f64) -> String {
    if usd < 0.01 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

/// "98k/200k 49%": how full the context window was at the session's last turn.
pub fn context(s: &Session) -> String {
    match (s.context_tokens, s.context_window) {
        (Some(t), Some(w)) if w > 0 => format!("{}/{} {}%", short(t), short(w), t * 100 / w),
        _ => "-".into(),
    }
}

pub fn tokens(s: &Session) -> String {
    match (s.tokens_in, s.tokens_out) {
        (None, None) => "-".into(),
        (i, o) => format!("{}/{}", short(i.unwrap_or(0)), short(o.unwrap_or(0))),
    }
}
fn short(n: u64) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => format!("{}k", n / 1000),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// `~/x` for paths under the home directory, so the column fits.
pub fn tilde(path: &Path) -> String {
    dirs::home_dir()
        .and_then(|h| path.strip_prefix(h).ok())
        .map_or_else(
            || path.display().to_string(),
            |rest| format!("~/{}", rest.display()),
        )
}

/// The command Claude Code runs for every fleet event. `$PPID` is the harness process: hooks
/// run under `sh -c`, and the shell's parent is Claude whether or not it execs the command.
pub fn hook_command(exe: &Path, state: &Path) -> String {
    format!("{} --state-dir {} hook $PPID", sh(exe), sh(state))
}

/// Merge the fleet hook into a Claude Code settings file, replacing any earlier cones entry so a
/// moved binary or state dir is picked up. Everything else in the file is preserved.
pub fn install(settings: &Path, command: &str) -> Result<()> {
    let mut root: Value = match fs::read(settings) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", settings.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e.into()),
    };
    let hooks = root
        .as_object_mut()
        .context("settings is not a JSON object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("settings.hooks is not an object")?;
    for event in EVENTS {
        let list = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("settings.hooks.{event} is not an array"))?;
        list.retain(|entry| !is_fleet_hook(entry));
        list.push(json!({"hooks": [{"type": "command", "command": command}]}));
    }
    if let Some(parent) = settings.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(settings, serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}

/// True when a settings file carries the fleet hook on every event.
pub fn installed(settings: &Path) -> bool {
    let Ok(Ok(root)) = fs::read(settings).map(|b| serde_json::from_slice::<Value>(&b)) else {
        return false;
    };
    EVENTS.iter().all(|event| {
        root["hooks"][event]
            .as_array()
            .is_some_and(|list| list.iter().any(is_fleet_hook))
    })
}

fn is_fleet_hook(entry: &Value) -> bool {
    entry["hooks"].as_array().is_some_and(|hooks| {
        hooks.iter().any(|h| {
            h["command"]
                .as_str()
                .is_some_and(|c| c.ends_with(" hook $PPID"))
        })
    })
}

fn sh(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "'\\''"))
}
