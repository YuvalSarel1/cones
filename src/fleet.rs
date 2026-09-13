//! Fleet: every Claude Code session on this Mac, read from the registry Claude itself keeps,
//! `~/.claude/sessions/<pid>.json`, plus each session's transcript for title, last reply, model,
//! timestamps and tokens. Codex sessions join through [`crate::codex`], from the process table
//! and Codex's rollout files. cones installs nothing into a session and runs nothing inside it.
//! Every value is something the harness wrote; a value it did not write is `None`, never a guess.
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
    /// The `timestamp` of the first transcript line that carries one. Rows sort by this so they
    /// hold still while the session works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<DateTime<Utc>>,
    /// The `timestamp` of the last transcript line that carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<DateTime<Utc>>,
    /// The bare API model id Claude wrote on the last message with usage, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    /// The prompt size Claude reported on the last message with usage: input plus cache creation
    /// and cache read. There is no window field; Claude states the window size only in its
    /// statusLine payload, which reaches nothing outside the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
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

/// Every live session in Claude's registry, oldest first by start time; a session whose
/// transcript reports no start sorts last, by id. A file whose process is gone, or whose pid now
/// belongs to another process, is a crashed session and is skipped; unparsable files are skipped
/// too, since Claude may be mid-write on one.
pub fn sessions(claude: &Path) -> Result<Vec<Session>> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(claude.join("sessions")) else {
        return Ok(out);
    };
    let values: Vec<Value> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| fs::read(p).ok())
        .filter_map(|b| serde_json::from_slice(&b).ok())
        .collect();
    let starts = process_starts(values.iter().filter_map(|v| v["pid"].as_u64()));
    out.extend(values.iter().filter_map(|v| session(claude, v, &starts)));
    out.sort_by(|a, b| {
        (a.started.is_none(), a.started, &a.session_id).cmp(&(
            b.started.is_none(),
            b.started,
            &b.session_id,
        ))
    });
    Ok(out)
}

/// Start time of each live pid as `ps` prints it under UTC. Claude writes that same text to the
/// registry as `procStart`, so the two compare as strings and a reused pid never passes for the
/// session that had it. One `ps` per refresh covers every entry.
fn process_starts(pids: impl Iterator<Item = u64>) -> HashMap<u32, String> {
    // ps rejects the whole list when one pid is above the kernel's maximum (99998 on macOS);
    // such a pid runs nothing anyway.
    let list = pids
        .filter(|p| *p <= 99_998)
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    if list.is_empty() {
        return HashMap::new();
    }
    Command::new("/bin/ps")
        .env("TZ", "UTC")
        .args(["-o", "pid=,lstart=", "-p", &list])
        .stdin(Stdio::null())
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| {
                    let (pid, start) = l.trim().split_once(' ')?;
                    Some((pid.parse().ok()?, start.trim().to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One registry entry as a fleet row. Pure apart from the transcript read; `starts` is the
/// process table from `process_starts`.
fn session(dir: &Path, v: &Value, starts: &HashMap<u32, String>) -> Option<Session> {
    let pid = v["pid"].as_u64()? as u32;
    let id = v["sessionId"].as_str()?;
    let start = starts.get(&pid)?;
    if v["procStart"].as_str().is_some_and(|s| s.trim() != start) {
        return None;
    }
    // The id names a transcript file, so it is checked before it becomes a path.
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    let cwd = PathBuf::from(v["cwd"].as_str().unwrap_or(""));
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
        // A status this version does not know renders as Claude's own word, never as a guess.
        state: match v["status"].as_str().unwrap_or("-") {
            "busy" | "shell" => "active",
            "blocked" | "waiting" | "needs_user" | "needs_trust" => "blocked",
            other => other,
        }
        .into(),
        // Start, last activity, model and context are the transcript's own words; the registry
        // `startedAt` and `updatedAt` and the file's mtime are not read for them.
        started: d.report.started,
        last_activity: d.report.last_activity,
        model: d.report.model,
        pid: Some(pid),
        transcript_path: transcript,
        tokens_in: d.report.tokens_in,
        tokens_out: d.report.tokens_out,
        context_tokens: d.report.context,
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
    report: Report,
}

/// Title, last reply, model, timestamps and token counts from a transcript, recomputed only
/// when the file grew. The dashboard reloads every second and the count reads the whole file.
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
        report: report(transcript).unwrap_or_default(),
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
    exchanges(transcript, 1)
}

/// The last `n` exchanges of a transcript, oldest first, each rendered as `exchange` renders
/// one and separated by a blank line. Only what the transcript records appears: user text and
/// assistant text; a turn that was all tool calls shows its prompt alone. Reads the tail of the
/// file, 1 MiB for one exchange, 4 MiB for more.
pub fn exchanges(transcript: &Path, n: usize) -> Vec<String> {
    // ponytail: an exchange older than the last few MiB is not what the pane is for.
    let bytes = if n <= 1 { 1 << 20 } else { 4 << 20 };
    let Ok(text) = crate::output::tail(transcript, bytes) else {
        return Vec::new();
    };
    let mut turns: Vec<(String, Vec<String>)> = Vec::new();
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
                turns.push((texts.join("\n"), Vec::new()));
            }
            Some("assistant") => {
                let texts = texts.into_iter().map(str::to_owned);
                // A reply whose prompt lies before the window still shows, under no prompt.
                match turns.last_mut() {
                    Some((_, reply)) => reply.extend(texts),
                    None => turns.push((String::new(), texts.collect())),
                }
            }
            // A Codex rollout: user turns are `input_text` blocks, replies `output_text`. Codex
            // also files its environment and instruction blocks as user messages; they are
            // tagged XML, not something the user typed.
            Some("response_item") if v["payload"]["type"] == "message" => {
                let p = &v["payload"];
                let inputs: Vec<&str> = p["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|b| b["type"] == "input_text")
                    .filter_map(|b| b["text"].as_str())
                    .filter(|t| !t.trim_start().starts_with('<'))
                    .collect();
                if p["role"] == "user" && !inputs.concat().trim().is_empty() {
                    turns.push((inputs.join("\n"), Vec::new()));
                }
                // Codex opens a reply with a newline; the pane already separates replies.
                let mut replies = crate::codex::assistant_texts(&v).map(|t| t.trim().to_owned());
                match turns.last_mut() {
                    Some((_, reply)) => reply.extend(replies),
                    None => {
                        if let Some(first) = replies.next() {
                            let mut reply = vec![first];
                            reply.extend(replies);
                            turns.push((String::new(), reply));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let keep = turns.len().saturating_sub(n);
    let mut out = Vec::new();
    for (prompt, reply) in turns.drain(keep..) {
        if !out.is_empty() {
            out.push(String::new());
        }
        out.extend(prompt.trim().lines().map(|l| format!("> {l}")));
        out.push(String::new());
        out.extend(reply.join("\n\n").lines().map(|l| l.replace("**", "")));
    }
    out
}

/// The first non-empty line of a reply, bold markers dropped: what a one-line cell shows.
pub fn headline(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.replace("**", ""))
}

/// Title and assistant headlines in transcript lines, Claude's or Codex's.
fn scan(lines: &str) -> (Option<String>, Vec<String>) {
    let mut out = (None, Vec::new());
    for line in lines.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v["type"].as_str() {
            Some("ai-title") => out.0 = v["aiTitle"].as_str().map(Into::into),
            Some("agent-name") => out.0 = v["agentName"].as_str().map(Into::into),
            Some("assistant") => out.1.extend(
                v["message"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|b| b["text"].as_str().and_then(headline)),
            ),
            Some("response_item") => out
                .1
                .extend(crate::codex::assistant_texts(&v).filter_map(headline)),
            _ => {}
        }
    }
    out
}

/// What one pass over a transcript reads out of Claude's own lines.
#[derive(Default, Clone)]
struct Report {
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
    /// The prompt size on the last message with usage.
    context: Option<u64>,
    /// `message.model` on that same message, verbatim.
    model: Option<String>,
    /// The `timestamp` on the first and the last line that carries one.
    started: Option<DateTime<Utc>>,
    last_activity: Option<DateTime<Utc>>,
}

/// Total input and output tokens in a Claude transcript, the last message's prompt size as the
/// context in use, the model id on that message, and the first and last line timestamps.
/// Streaming writes one line per content block with the same message id and usage, so each
/// message is counted once. A message whose model is `<synthetic>` is Claude's own placeholder
/// for a turn no model answered (all-zero usage); it is not a report and is skipped.
fn report(transcript: &Path) -> Result<Report> {
    let mut seen = HashSet::new();
    let (mut input, mut output) = (0, 0);
    let mut r = Report::default();
    let mut counted = false;
    for line in std::io::BufReader::new(fs::File::open(transcript)?).lines() {
        let Ok(event) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        if let Some(t) = event["timestamp"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(DateTime::<Utc>::from)
        {
            r.started.get_or_insert(t);
            r.last_activity = Some(t);
        }
        let message = &event["message"];
        let Some(u) = message.get("usage") else {
            continue;
        };
        let model = message["model"].as_str();
        if model == Some("<synthetic>") {
            continue;
        }
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
        counted = true;
        r.context = Some(prompt);
        r.model = model.map(Into::into);
    }
    if counted {
        r.tokens_in = Some(input);
        r.tokens_out = Some(output);
    }
    Ok(r)
}

pub fn alive(pid: u32) -> bool {
    // Signal 0 checks existence; EPERM means it exists under another user.
    unsafe { libc::kill(pid as i32, 0) == 0 || *libc::__error() == libc::EPERM }
}

/// Every live session of every harness, oldest first by start time: Claude's registry plus
/// Codex's process table and rollouts.
pub fn all(claude: &Path) -> Result<Vec<Session>> {
    let mut out = sessions(claude)?;
    out.extend(crate::codex::sessions(&crate::codex::home(claude)));
    out.sort_by(|a, b| {
        (a.started.is_none(), a.started, &a.session_id).cmp(&(
            b.started.is_none(),
            b.started,
            &b.session_id,
        ))
    });
    Ok(out)
}

/// What the fleet view shows, so `logs`, `attach` and `stop` act on every visible row.
pub fn find(claude: &Path, session_id: &str) -> Result<Option<Session>> {
    Ok(all(claude)?
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

/// True when a Claude settings file still carries entries from the removed `cones hook`. Each
/// would run a command the binary no longer has, on every event of every session.
pub fn stale_hook(settings: &Path) -> bool {
    let Ok(Ok(root)) = fs::read(settings).map(|b| serde_json::from_slice::<Value>(&b)) else {
        return false;
    };
    root["hooks"].as_object().is_some_and(|events| {
        events
            .values()
            .flat_map(|l| l.as_array().into_iter().flatten())
            .flat_map(|e| e["hooks"].as_array().into_iter().flatten())
            .any(|h| {
                h["command"]
                    .as_str()
                    .is_some_and(|c| c.ends_with(" hook $PPID"))
            })
    })
}

/// Time since a reported instant as a table cell: `4s`, `6m`, `2h`, `3d`.
pub fn age(since: DateTime<Utc>) -> String {
    let s = (Utc::now() - since).num_seconds().max(0);
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

/// The prompt size Claude reported on the session's last message: "98k", with no denominator and
/// no percentage. Claude Code states the window size only in the statusLine payload, which
/// reaches nothing outside the session; the transcript carries the bare model id, the registry
/// nothing. Guessing 200k, or 1M from a `[1m]` in settings.json, once rendered live sessions at
/// 194%. A missing denominator beats a wrong one.
pub fn context(s: &Session) -> String {
    s.context_tokens.map_or_else(|| "-".into(), short)
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
