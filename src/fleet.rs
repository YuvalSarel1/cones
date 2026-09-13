//! Fleet: every Claude Code session on this Mac, read from the registry Claude itself keeps,
//! `~/.claude/sessions/<pid>.json`, plus each session's transcript for title, last reply and
//! tokens. cones installs nothing into the session and runs nothing inside it.
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub session_id: String,
    #[serde(default = "claude")]
    pub harness: String,
    /// Claude's own kind: `bg` for a daemon-owned background session, `interactive` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub cwd: PathBuf,
    /// `active`, `idle` or `blocked` (waiting on a permission, trust or user prompt).
    pub state: String,
    pub updated: DateTime<Utc>,
    /// When the session started. Rows sort by this so they hold still while `updated` ticks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<DateTime<Utc>>,
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
pub const STATES: [&str; 3] = ["active", "idle", "blocked"];

/// Claude's config directory: `$CLAUDE_CONFIG_DIR`, the same override Claude Code honors, or
/// `~/.claude`. Holds `sessions/`, `projects/` and `jobs/`.
pub fn claude_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Ok(dirs::home_dir()
        .context("missing home directory")?
        .join(".claude"))
}

/// Every live session in Claude's registry, oldest first by start time. A file whose pid is
/// gone is a crashed session and is skipped; unparsable files are skipped too, since Claude may
/// be mid-write on one.
pub fn sessions(claude: &Path) -> Result<Vec<Session>> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(claude.join("sessions")) else {
        return Ok(out);
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Some(v) = fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            && let Some(s) = session(claude, &v)
        {
            out.push(s);
        }
    }
    out.sort_by_key(|s| s.started.unwrap_or(s.updated));
    Ok(out)
}

/// One registry entry as a fleet row. Pure apart from the pid check and the transcript read.
fn session(dir: &Path, v: &Value) -> Option<Session> {
    let pid = v["pid"].as_u64()? as u32;
    let id = v["sessionId"].as_str()?;
    // The id names a transcript file, so it is checked before it becomes a path.
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        || !alive(pid)
    {
        return None;
    }
    let cwd = PathBuf::from(v["cwd"].as_str().unwrap_or(""));
    let millis = |k: &str| v[k].as_i64().and_then(DateTime::from_timestamp_millis);
    // The short job id becomes a directory name, so it is checked first.
    let job: Value = v["jobId"]
        .as_str()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        .and_then(|s| fs::read(dir.join("jobs").join(s).join("state.json")).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    // Claude keeps transcripts under projects/<cwd with every non-alphanumeric byte as '-'>.
    let transcript = dir
        .join("projects")
        .join(
            cwd.to_string_lossy()
                .replace(|c: char| !c.is_ascii_alphanumeric(), "-"),
        )
        .join(format!("{id}.jsonl"));
    let transcript = if transcript.is_file() {
        Some(transcript)
    } else {
        job["linkScanPath"].as_str().map(PathBuf::from)
    };
    let d = transcript.as_deref().map(details).unwrap_or_default();
    Some(Session {
        session_id: id.into(),
        harness: claude(),
        kind: v["kind"].as_str().map(Into::into),
        cwd,
        state: match v["status"].as_str().unwrap_or("") {
            "idle" => "idle",
            "blocked" | "waiting" | "needs_user" | "needs_trust" => "blocked",
            _ => "active",
        }
        .into(),
        updated: job["updatedAt"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(Into::into)
            .or_else(|| millis("updatedAt"))
            .or_else(|| millis("startedAt"))
            .unwrap_or_else(Utc::now),
        started: millis("startedAt"),
        pid: Some(pid),
        transcript_path: transcript,
        tokens_in: d.usage.tokens_in,
        tokens_out: d.usage.tokens_out,
        context_tokens: d.usage.context,
        // Nothing Claude writes outside a session states the window; see `context`.
        context_window: None,
        cost_usd: None,
        title: d
            .title
            .or_else(|| v["name"].as_str().filter(|n| !n.is_empty()).map(Into::into)),
        // A background job's one-line status from Claude beats the transcript's last text.
        last: job["detail"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(Into::into)
            .or(d.last),
    })
}
#[derive(Default, Clone)]
struct Details {
    title: Option<String>,
    last: Option<String>,
    usage: Usage,
}

/// Title, last reply and token counts from a transcript, recomputed only when the file grew.
/// The dashboard reloads every second and counting tokens reads the whole file.
fn details(transcript: &Path) -> Details {
    static CACHE: Mutex<Option<HashMap<PathBuf, (u64, Details)>>> = Mutex::new(None);
    let len = fs::metadata(transcript).map_or(0, |m| m.len());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some((seen, d)) = cache.get(transcript)
        && *seen == len
    {
        return d.clone();
    }
    let (title, mut last) = tail(transcript, 1);
    let d = Details {
        title,
        last: last.pop(),
        usage: usage(transcript).unwrap_or_default(),
    };
    cache.insert(transcript.to_owned(), (len, d.clone()));
    d
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
        last = Some(prompt);
    }
    Ok(Usage {
        tokens_in: Some(input),
        tokens_out: Some(output),
        context: last,
    })
}

pub fn alive(pid: u32) -> bool {
    // Signal 0 checks existence; EPERM means it exists under another user.
    unsafe { libc::kill(pid as i32, 0) == 0 || *libc::__error() == libc::EPERM }
}

/// What the fleet view shows, so `logs`, `attach` and `stop` act on every visible row.
pub fn find(claude: &Path, session_id: &str) -> Result<Option<Session>> {
    Ok(sessions(claude)?
        .into_iter()
        .find(|s| s.session_id == session_id))
}

/// Stop the harness behind a fleet session. Returns false when the process is already
/// gone. The pid came from the registry, so the command is checked first: a reused pid never
/// gets signalled.
pub fn stop(claude: &Path, session_id: &str) -> Result<bool> {
    let session = find(claude, session_id)?.context("no such run or session")?;
    // A background session belongs to Claude's daemon, which respawns a worker whose process
    // dies (`attempt` in ~/.claude/daemon/roster.json). Only `claude stop` ends one for good.
    if session.kind.as_deref() == Some("bg") {
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

/// Tokens in the context window at the session's last turn: "98k", or "98k/200k 49%" when the
/// harness reported the window size. The window is never inferred. Claude Code states it only
/// in the statusLine payload, which reaches nothing outside the session; the transcript carries
/// the bare model id, the registry nothing. Guessing 200k, or 1M from a `[1m]` in settings.json,
/// rendered live sessions at 194%. A missing denominator beats a wrong one.
pub fn context(s: &Session) -> String {
    match (s.context_tokens, s.context_window) {
        (Some(t), Some(w)) if w > 0 => format!("{}/{} {}%", short(t), short(w), t * 100 / w),
        (Some(t), _) => short(t),
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
