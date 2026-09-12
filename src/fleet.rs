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
        },
    )
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
