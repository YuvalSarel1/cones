//! Codex in the fleet. Codex keeps no session registry, so a Codex row is assembled from two
//! reports: the process table (pid and start time from `ps`, the working directory from `lsof`)
//! and the rollout file Codex writes under `~/.codex/sessions` once a session has its first
//! turn. A rollout is tied to a process only by the two facts it records, its cwd and its start
//! time; a process without one shows `-` for title, last reply and state. cones runs no Codex
//! job and holds no Codex budget: seeing a session is all this module does.
use crate::fleet::Session;
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    io::BufRead,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

/// Codex's home: `$CODEX_HOME`, the override Codex honors, or `~/.codex`. Holds `sessions/`
/// and `session_index.jsonl`.
pub fn home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Ok(dirs::home_dir()
        .context("missing home directory")?
        .join(".codex"))
}

/// A live `codex` process: what the process table states about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// Start time as `ps -o lstart` prints it under UTC.
    pub started: DateTime<Utc>,
    /// Working directory as `lsof -d cwd` prints it; `None` when lsof could not read it.
    pub cwd: Option<PathBuf>,
}

/// The `session_meta` line Codex writes first in every rollout file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub session_id: String,
    pub cwd: PathBuf,
    pub started: DateTime<Utc>,
}

/// What the tail of a rollout file records.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tail {
    /// First line of the assistant's most recent message.
    pub last: Option<String>,
    /// `active` after `task_started`, `idle` after `task_complete` or `turn_aborted`.
    pub state: Option<&'static str>,
    /// Timestamp of the last line.
    pub last_activity: Option<DateTime<Utc>>,
    /// `turn_context.model` on the last turn, verbatim.
    pub model: Option<String>,
}

/// Every live Codex session, oldest first by process start.
pub fn sessions(codex: &Path) -> Vec<Session> {
    let ps = Command::new("/bin/ps")
        .env("TZ", "UTC")
        .args(["-axww", "-o", "pid=,lstart=,command="])
        .stdin(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let mut procs = processes(&ps);
    if procs.is_empty() {
        return Vec::new();
    }
    let list = procs
        .iter()
        .map(|p| p.pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    // lsof exits non-zero when one pid has gone; the others are still printed.
    let lsof = Command::new("/usr/sbin/lsof")
        .args(["-nPw", "-a", "-p", &list, "-d", "cwd", "-Fn"])
        .stdin(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let cwds = cwds(&lsof);
    for p in &mut procs {
        p.cwd = cwds.get(&p.pid).cloned();
    }
    rows(codex, &procs)
}

/// Subcommands that run no session: servers, account and package management, one-shot tools.
/// Everything else (`codex`, `codex exec`, `codex review`, `codex resume`, `codex fork`, a prompt
/// or flags) is a session.
const NOT_SESSIONS: [&str; 24] = [
    "agents",
    "login",
    "logout",
    "mcp",
    "mcp-server",
    "plugin",
    "app-server",
    "remote-control",
    "app",
    "completion",
    "update",
    "doctor",
    "sandbox",
    "debug",
    "apply",
    "a",
    "queue",
    "archive",
    "delete",
    "migrate-rollouts",
    "unarchive",
    "cloud",
    "exec-server",
    "features",
];

/// Live Codex session processes in the output of `TZ=UTC ps -axww -o pid=,lstart=,command=`. A
/// process counts when its program is named `codex` and its first argument is not a subcommand
/// from `NOT_SESSIONS`; the word `codex` inside another command's text does not count. `cwd`
/// is left for lsof.
pub fn processes(ps: &str) -> Vec<Process> {
    ps.lines()
        .filter_map(|line| {
            let (pid, rest) = line.trim_start().split_once(' ')?;
            let rest = rest.trim_start();
            // `lstart` is fixed width: `Sun Sep 13 15:19:19 2026`, the day padded with a space.
            let (start, command) = rest.split_at_checked(24)?;
            let mut words = command.split_whitespace();
            let program = Path::new(words.next()?).file_name()?;
            if program != "codex" || words.next().is_some_and(|a| NOT_SESSIONS.contains(&a)) {
                return None;
            }
            Some(Process {
                pid: pid.parse().ok()?,
                started: NaiveDateTime::parse_from_str(start, "%a %b %e %H:%M:%S %Y")
                    .ok()?
                    .and_utc(),
                cwd: None,
            })
        })
        .collect()
}

/// Working directory per pid from `lsof -a -p <pids> -d cwd -Fn`: a `p<pid>` line, then
/// `fcwd` and `n<path>`.
pub fn cwds(lsof: &str) -> HashMap<u32, PathBuf> {
    let mut out = HashMap::new();
    let mut pid = None;
    for line in lsof.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.trim().parse().ok();
        } else if let (Some(p), Some(path)) = (pid, line.strip_prefix('n')) {
            out.insert(p, PathBuf::from(path));
        }
    }
    out
}

/// The `session_meta` line: session id, cwd and the session's own start time.
pub fn meta(line: &str) -> Option<Meta> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v["type"] != "session_meta" {
        return None;
    }
    let p = &v["payload"];
    let id = p["session_id"].as_str().or(p["id"].as_str())?;
    // The id names a file and a row, so it is checked before either.
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(Meta {
        session_id: id.into(),
        cwd: PathBuf::from(p["cwd"].as_str()?),
        started: DateTime::parse_from_rfc3339(p["timestamp"].as_str()?)
            .ok()?
            .into(),
    })
}

/// Assistant texts in one rollout line: a `response_item` message with role `assistant`, each
/// `output_text` block's text.
pub fn assistant_texts(v: &Value) -> impl Iterator<Item = &str> {
    let p = &v["payload"];
    let message =
        v["type"] == "response_item" && p["type"] == "message" && p["role"] == "assistant";
    p["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |_| message)
        .filter(|b| b["type"] == "output_text")
        .filter_map(|b| b["text"].as_str())
}

/// Last reply, turn state and last timestamp from rollout lines. Lines that are not JSON are
/// skipped; Codex may be mid-write on the last one.
pub fn tail(lines: &str) -> Tail {
    let mut t = Tail::default();
    for line in lines.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(ts) = v["timestamp"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        {
            t.last_activity = Some(ts.into());
        }
        if let Some(first) = assistant_texts(&v).find_map(crate::fleet::headline) {
            t.last = Some(first);
        }
        if v["type"] == "turn_context"
            && let Some(model) = v["payload"]["model"].as_str()
        {
            t.model = Some(model.to_owned());
        }
        if v["type"] == "event_msg" {
            match v["payload"]["type"].as_str() {
                Some("task_started") => t.state = Some("active"),
                Some("task_complete" | "turn_aborted") => t.state = Some("idle"),
                _ => {}
            }
        }
    }
    t
}

/// Thread names from `session_index.jsonl`, one `{"id","thread_name","updated_at"}` per line;
/// the last line for an id wins.
pub fn titles(index: &str) -> HashMap<String, String> {
    index
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            Some((
                v["id"].as_str()?.to_owned(),
                v["thread_name"]
                    .as_str()
                    .filter(|t| !t.is_empty())?
                    .to_owned(),
            ))
        })
        .collect()
}

/// Which rollout each process wrote, by the two facts a rollout records: its cwd and its start.
/// A rollout belongs to a process when that process is the only live Codex in the rollout's
/// directory that started at or before it; the newest such rollout is the live thread, since
/// `/new` opens another file. With two Codex processes in one directory the file could be
/// either's, so neither gets it.
pub fn attribute<'a>(
    procs: &[Process],
    rollouts: &'a [(PathBuf, Meta)],
) -> HashMap<u32, &'a (PathBuf, Meta)> {
    let mut out: HashMap<u32, &(PathBuf, Meta)> = HashMap::new();
    for r in rollouts {
        let mut owners = procs
            .iter()
            .filter(|p| p.cwd.as_deref() == Some(r.1.cwd.as_path()) && p.started <= r.1.started);
        let (Some(owner), None) = (owners.next(), owners.next()) else {
            continue;
        };
        let newer = out
            .get(&owner.pid)
            .is_none_or(|have| have.1.started < r.1.started);
        if newer {
            out.insert(owner.pid, r);
        }
    }
    out
}

/// Fleet rows for live processes: the rollout each wrote, its title from the session index,
/// last reply and state from its tail. Reads only under `codex`.
pub fn rows(codex: &Path, procs: &[Process]) -> Vec<Session> {
    let Some(since) = procs.iter().map(|p| p.started).min() else {
        return Vec::new();
    };
    let rollouts = rollouts(codex, since);
    let owned = attribute(procs, &rollouts);
    let titles = fs::read_to_string(codex.join("session_index.jsonl"))
        .map(|t| titles(&t))
        .unwrap_or_default();
    let mut out: Vec<Session> = procs
        .iter()
        .map(|p| {
            let rollout = owned.get(&p.pid);
            let t = rollout.map(|(path, _)| tail_of(path)).unwrap_or_default();
            let id =
                rollout.map_or_else(|| format!("codex-{}", p.pid), |(_, m)| m.session_id.clone());
            Session {
                title: titles.get(&id).cloned(),
                session_id: id,
                harness: "codex".into(),
                kind: None,
                cwd: p.cwd.clone().unwrap_or_default(),
                // Codex writes task_started and task_complete; nothing else about the turn.
                state: t.state.unwrap_or("-").into(),
                // The rollout's last timestamp; the process start is not substituted for it.
                last_activity: t.last_activity,
                model: t.model,
                started: Some(p.started),
                pid: Some(p.pid),
                transcript_path: rollout.map(|(path, _)| path.clone()),
                tokens_in: None,
                tokens_out: None,
                context_tokens: None,
                cost_usd: None,
                last: t.last,
            }
        })
        .collect();
    out.sort_by_key(|s| s.started);
    out
}

/// Rollout files under `sessions/YYYY/MM/DD/` modified since `since`, with their first line. A
/// live session's file is written after its process started, so an older file is never read.
fn rollouts(codex: &Path, since: DateTime<Utc>) -> Vec<(PathBuf, Meta)> {
    let mut out = Vec::new();
    let mut stack = vec![codex.join("sessions")];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(md) = entry.metadata() else {
                continue;
            };
            if md.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl")
                && md
                    .modified()
                    .is_ok_and(|m| DateTime::<Utc>::from(m) >= since)
                && let Some(m) = meta_of(&path)
            {
                out.push((path, m));
            }
        }
    }
    out
}

/// The first line of a rollout, cached: it never changes once written.
fn meta_of(path: &Path) -> Option<Meta> {
    static CACHE: Mutex<Option<HashMap<PathBuf, Meta>>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some(m) = cache.get(path) {
        return Some(m.clone());
    }
    let mut first = String::new();
    std::io::BufReader::new(fs::File::open(path).ok()?)
        .read_line(&mut first)
        .ok()?;
    let m = meta(&first)?;
    cache.insert(path.to_owned(), m.clone());
    Some(m)
}

/// The rollout's tail, recomputed only when the file grew; the dashboard reloads every second.
fn tail_of(path: &Path) -> Tail {
    static CACHE: Mutex<Option<HashMap<PathBuf, (u64, Tail)>>> = Mutex::new(None);
    let len = fs::metadata(path).map_or(0, |m| m.len());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some((seen, t)) = cache.get(path)
        && *seen == len
    {
        return t.clone();
    }
    // ponytail: the last MiB; a reply older than that is not what the row is for.
    let t = crate::output::tail(path, 1 << 20)
        .map(|text| tail(&text))
        .unwrap_or_default();
    cache.insert(path.to_owned(), (len, t.clone()));
    t
}
