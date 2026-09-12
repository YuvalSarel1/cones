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
    io::{BufRead, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub v: u32,
    pub session_id: String,
    #[serde(default = "claude")]
    pub harness: String,
    pub cwd: PathBuf,
    /// `active`, `idle` (Stop fired), `blocked` (a Notification such as a permission prompt)
    /// or `exited`.
    pub state: String,
    pub updated: DateTime<Utc>,
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
pub fn record(state: &Path, pid: u32, payload: &Value) -> Result<()> {
    let id = payload["session_id"]
        .as_str()
        .context("hook payload has no session_id")?;
    let event = payload["hook_event_name"].as_str().unwrap_or("");
    let transcript = payload["transcript_path"].as_str().map(PathBuf::from);
    let previous = find(state, id)?;
    // Counting tokens means reading the whole transcript, so do it once per turn, not per tool.
    let counted = match event {
        "Stop" | "SessionEnd" => transcript.as_deref().and_then(|t| usage(t).ok()),
        _ => None,
    };
    let (tokens_in, tokens_out) = counted.unwrap_or_else(|| {
        previous
            .as_ref()
            .map_or((None, None), |p| (p.tokens_in, p.tokens_out))
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
                "Stop" => "idle",
                "Notification" => "blocked",
                "SessionEnd" => "exited",
                _ => "active",
            }
            .into(),
            updated: Utc::now(),
            event: Some(event.into()),
            tool: payload["tool_name"].as_str().map(Into::into),
            pid: Some(pid),
            transcript_path: transcript,
            tokens_in,
            tokens_out,
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

/// Total input and output tokens in a Claude transcript. Streaming writes one line per content
/// block with the same message id and usage, so each message is counted once.
fn usage(transcript: &Path) -> Result<(Option<u64>, Option<u64>)> {
    let mut seen = HashSet::new();
    let (mut input, mut output) = (0, 0);
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
        input +=
            n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
        output += n("output_tokens");
    }
    Ok((Some(input), Some(output)))
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

/// Newest first. Unreadable files are skipped; a session the hook is mid-write on is
/// not a failure of the listing.
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
    out.sort_by_key(|s| std::cmp::Reverse(s.updated));
    Ok(out)
}

pub fn find(state: &Path, session_id: &str) -> Result<Option<Session>> {
    Ok(sessions(state)?
        .into_iter()
        .find(|s| s.session_id == session_id))
}

/// SIGTERM the harness behind a fleet session. Returns false when the process is already
/// gone. The pid came from a hook payload long ago, so the command is checked first: a
/// reused pid never gets signalled.
pub fn stop(state: &Path, session_id: &str) -> Result<bool> {
    let session = find(state, session_id)?.context("no such run or session")?;
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
