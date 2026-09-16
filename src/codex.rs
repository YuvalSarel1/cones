//! Codex fleet discovery from processes, writer locks, the thread database and rollouts.
//! This module observes native sessions; it does not execute supervised jobs.
use crate::fleet::Session;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

/// Honor `CODEX_HOME`, otherwise use `.codex` beside the Claude directory.
/// The sibling default keeps fixture homes isolated.
pub fn home(claude: &Path) -> PathBuf {
    match std::env::var_os("CODEX_HOME").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => claude.with_file_name(".codex"),
    }
}

/// Every Codex home to read. `CODEX_HOME` pins one; otherwise the default home and
/// its `.codex-*` siblings, because a daemon serves one provider region and a model
/// in another region needs a home of its own to be seen and joined here.
pub fn homes(claude: &Path) -> Vec<PathBuf> {
    let base = home(claude);
    if std::env::var_os("CODEX_HOME").is_some_and(|d| !d.is_empty()) {
        return vec![base];
    }
    // ponytail: siblings by name; a configured list if homes ever live elsewhere.
    let mut extra: Vec<PathBuf> = base
        .parent()
        .and_then(|p| fs::read_dir(p).ok())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(".codex-"))
                && p.join("config.toml").is_file()
        })
        .collect();
    extra.sort();
    let mut out = vec![base];
    out.extend(extra);
    out
}

/// The home a rollout lives in: the parent of the `sessions` directory holding it.
/// A row carries its rollout, so a join resumes against the daemon that owns it.
pub fn home_of(rollout: &Path) -> Option<&Path> {
    rollout
        .ancestors()
        .find(|a| a.file_name() == Some(std::ffi::OsStr::new("sessions")))?
        .parent()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// Start time as `ps -o lstart` prints it under UTC.
    pub started: DateTime<Utc>,
    /// Working directory as `lsof -d cwd` prints it; `None` when lsof could not read it.
    pub cwd: Option<PathBuf>,
    /// An explicit resume id takes precedence over cwd and start-time attribution.
    pub thread: Option<String>,
    /// `--remote` makes this process a viewer of an app-server thread, not its writer.
    pub remote: bool,
}

/// The `session_meta` line Codex writes first in every rollout file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub session_id: String,
    pub cwd: PathBuf,
    pub started: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tail {
    pub last: Option<String>,
    /// Latest turn: `active` after `task_started`, `done` after `task_complete`,
    /// `stopped` after `turn_aborted`. A new turn returns to `active`.
    pub state: Option<&'static str>,
    pub last_activity: Option<DateTime<Utc>>,
    /// `turn_context.model` on the last turn, verbatim.
    pub model: Option<String>,
    /// Latest `total_token_usage`: input includes cache hits; context uses `last_token_usage.total_tokens`.
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub context_tokens: Option<u64>,
    /// `token_count.info.model_context_window` on that event.
    pub context_window: Option<u64>,
    /// Timestamped rollout activity for sparklines; see docs/harness.md for event mappings.
    pub activity: Vec<crate::fleet::Activity>,
}

/// Skip discovery when the Codex home is absent, including in isolated tests.
pub fn sessions(codex: &Path) -> Vec<Session> {
    if !codex.is_dir() {
        return Vec::new();
    }
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
    #[cfg(target_os = "macos")]
    for p in &mut procs {
        p.cwd = crate::process_info::cwd(p.pid);
    }
    let list = procs
        .iter()
        .filter(|p| p.cwd.is_none())
        .map(|p| p.pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    // lsof exits non-zero when one pid has gone; the others are still printed.
    let lsof = if list.is_empty() {
        String::new()
    } else {
        Command::new("/usr/sbin/lsof")
            .args(["-nPw", "-a", "-p", &list, "-d", "cwd", "-Fn"])
            .stdin(Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    let cwds = cwds(&lsof);
    for p in &mut procs {
        if p.cwd.is_none() {
            p.cwd = cwds.get(&p.pid).cloned();
        }
    }
    rows(codex, &procs)
}

/// Subcommands that do not create sessions.
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

/// Parse `TZ=UTC ps -axww -o pid=,lstart=,command=`; `cwd` is filled from lsof.
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
            let (remote, thread) = client_options(command.split_whitespace().skip(1));
            Some(Process {
                pid: pid.parse().ok()?,
                started: NaiveDateTime::parse_from_str(start, "%a %b %e %H:%M:%S %Y")
                    .ok()?
                    .and_utc(),
                cwd: None,
                thread,
                remote,
            })
        })
        .collect()
}

/// Stop parsing options at the prompt so its text cannot identify a client or thread.
fn client_options<'a>(mut args: impl Iterator<Item = &'a str>) -> (bool, Option<String>) {
    let mut remote = false;
    let mut resume = false;
    while let Some(arg) = args.next() {
        match arg {
            "--remote" => remote = args.next().is_some(),
            _ if arg.starts_with("--remote=") => remote = arg.len() > "--remote=".len(),
            "-c"
            | "--config"
            | "--enable"
            | "--disable"
            | "--remote-auth-token-env"
            | "-i"
            | "--image"
            | "-m"
            | "--model"
            | "--local-provider"
            | "-p"
            | "--profile"
            | "-s"
            | "--sandbox"
            | "-C"
            | "--cd"
            | "--add-dir"
            | "-a"
            | "--ask-for-approval" => {
                args.next();
            }
            "--" => break,
            "resume" if !resume => resume = true,
            _ if arg.starts_with('-') => {}
            _ => {
                let thread = (resume
                    && arg.len() == 36
                    && arg.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit()))
                .then(|| arg.to_owned());
                return (remote, thread);
            }
        }
    }
    (remote, None)
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

/// Ignore malformed JSON, including a partially written last line.
pub fn tail(lines: &str) -> Tail {
    let mut t = Tail::default();
    t.fold(lines);
    t
}

impl Tail {
    /// Fold new events into prior state so long tool output does not hide an earlier turn start.
    pub fn fold(&mut self, lines: &str) {
        let t = self;
        for line in lines.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let Some(ts) = v["timestamp"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            {
                t.last_activity = Some(ts.into());
                t.activity.push(crate::fleet::Activity::at(ts.into()));
            }
            let payload = &v["payload"];
            if let Some(a) = t.activity.last_mut() {
                if v["type"] == "response_item" {
                    match payload["type"].as_str() {
                        Some("message") if payload["role"] == "assistant" => a.messages += 1,
                        Some("function_call" | "custom_tool_call" | "local_shell_call") => {
                            a.tools += 1
                        }
                        _ => {}
                    }
                }
                if v["type"] == "event_msg" && payload["type"] == "token_count" {
                    a.tokens_out += payload["info"]["last_token_usage"]["output_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                }
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
                    Some("task_complete") => t.state = Some("done"),
                    Some("turn_aborted") => t.state = Some("stopped"),
                    Some("token_count") => {
                        let info = &v["payload"]["info"];
                        let total = &info["total_token_usage"];
                        t.tokens_in = total["input_tokens"].as_u64().or(t.tokens_in);
                        t.tokens_out = total["output_tokens"].as_u64().or(t.tokens_out);
                        t.context_tokens = info["last_token_usage"]["total_tokens"]
                            .as_u64()
                            .or(t.context_tokens);
                        t.context_window =
                            info["model_context_window"].as_u64().or(t.context_window);
                    }
                    _ => {}
                }
            }
        }
    }
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

/// Thread names, rollout paths and cwd from the latest `state_*.sqlite`.
/// Prefer database names; fall back to `session_index.jsonl` for older threads.
#[derive(Debug, Default)]
pub struct Index {
    pub titles: HashMap<String, String>,
    pub threads: HashMap<String, (PathBuf, PathBuf)>,
}

pub fn index(codex: &Path) -> Index {
    let mut out = Index {
        titles: fs::read_to_string(codex.join("session_index.jsonl"))
            .map(|t| titles(&t))
            .unwrap_or_default(),
        threads: HashMap::new(),
    };
    let mut dbs: Vec<PathBuf> = fs::read_dir(codex)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("state_") && n.ends_with(".sqlite"))
        })
        .collect();
    dbs.sort();
    let Some(db) = dbs.pop() else { return out };
    let Ok(run) = Command::new("sqlite3")
        .args(["-readonly", "-json"])
        .arg(&db)
        .arg("select id, coalesce(name, title) as t, rollout_path, cwd from threads")
        .stderr(Stdio::null())
        .output()
    else {
        return out;
    };
    for v in serde_json::from_slice::<Vec<Value>>(&run.stdout).unwrap_or_default() {
        let Some(id) = v["id"].as_str() else { continue };
        if let Some(first) = v["t"].as_str().and_then(crate::fleet::headline) {
            out.titles.insert(id.to_owned(), first);
        }
        if let (Some(r), Some(c)) = (v["rollout_path"].as_str(), v["cwd"].as_str()) {
            out.threads
                .insert(id.to_owned(), (PathBuf::from(r), PathBuf::from(c)));
        }
    }
    out
}

/// Map held thread writer locks to their process ids. Unheld lock files do not count.
pub fn locks(codex: &Path, pids: &[u32]) -> HashMap<String, u32> {
    if pids.is_empty() {
        return HashMap::new();
    }
    let files: Vec<PathBuf> = fs::read_dir(codex.join("thread-writer-locks"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "lock")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.'))
        })
        .collect();
    if files.is_empty() {
        return HashMap::new();
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt;
        let allowed: HashMap<(u64, u64), String> = files
            .iter()
            .filter_map(|p| {
                let metadata = fs::metadata(p).ok()?;
                Some((
                    (metadata.dev(), metadata.ino()),
                    p.file_stem()?.to_str()?.to_owned(),
                ))
            })
            .collect();
        let mut out = HashMap::new();
        let mut unreadable = Vec::new();
        for &pid in pids {
            match crate::process_info::open_files(pid) {
                Ok(paths) => {
                    for file in paths {
                        if let Some(id) = allowed.get(&(file.device, file.inode)) {
                            out.insert(id.clone(), pid);
                        }
                    }
                }
                Err(_) => unreadable.push(pid),
            }
        }
        if !unreadable.is_empty() {
            out.extend(locks_with_lsof(&files, &unreadable));
        }
        out
    }
    #[cfg(not(target_os = "macos"))]
    locks_with_lsof(&files, pids)
}

fn locks_with_lsof(files: &[PathBuf], pids: &[u32]) -> HashMap<String, u32> {
    let pids = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let lsof = Command::new("/usr/sbin/lsof")
        .args(["-nPw", "-a", "-p", &pids, "-Fpn"])
        .args(files)
        .stdin(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    parse_locks(&lsof)
}

/// Thread id to pid from `lsof -Fpn` output: a `p<pid>` line, then `n<path>` lines.
pub fn parse_locks(lsof: &str) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    let mut pid = None;
    for line in lsof.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.trim().parse().ok();
        } else if let (Some(p), Some(path)) = (pid, line.strip_prefix('n'))
            && let Some(id) = Path::new(path).file_stem().and_then(|s| s.to_str())
        {
            out.insert(id.to_owned(), p);
        }
    }
    out
}

pub fn daemon_pid(codex: &Path) -> Option<u32> {
    let text = fs::read_to_string(codex.join("app-server-daemon/app-server.pid")).ok()?;
    let pid = serde_json::from_str::<Value>(&text).ok()?["pid"].as_u64()? as u32;
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .ok()?
        .success()
        .then_some(pid)
}

/// Prefer the database rollout path; fall back to `sessions/**/*-<id>.jsonl`.
fn rollout_for(codex: &Path, index: &Index, id: &str) -> Option<PathBuf> {
    if let Some((path, _)) = index.threads.get(id)
        && path.is_file()
    {
        return Some(path.clone());
    }
    let suffix = format!("-{id}.jsonl");
    let mut stack = vec![codex.join("sessions")];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&suffix))
            {
                return Some(path);
            }
        }
    }
    None
}

/// Attribute only when one eligible process shares the rollout's cwd and predates it.
/// Choose its newest rollout; ambiguous matches and remote clients get none.
pub fn attribute<'a>(
    procs: &[Process],
    rollouts: &'a [(PathBuf, Meta)],
) -> HashMap<u32, &'a (PathBuf, Meta)> {
    let mut out: HashMap<u32, &(PathBuf, Meta)> = HashMap::new();
    for r in rollouts {
        let mut owners = procs.iter().filter(|p| {
            !p.remote && p.cwd.as_deref() == Some(r.1.cwd.as_path()) && p.started <= r.1.started
        });
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

/// Build fleet rows from live processes; only daemon-held threads are joinable.
pub fn rows(codex: &Path, procs: &[Process]) -> Vec<Session> {
    let Some(since) = procs.iter().map(|p| p.started).min() else {
        return Vec::new();
    };
    let index = index(codex);
    let daemon = daemon_pid(codex);
    let pids: Vec<u32> = procs.iter().map(|p| p.pid).chain(daemon).collect();
    let locks = locks(codex, &pids);
    let held: HashMap<u32, String> = locks.iter().map(|(id, pid)| (*pid, id.clone())).collect();
    // Writer locks identify owners and take precedence over cwd/start-time attribution.
    let rollouts: Vec<_> = rollouts(codex, since)
        .into_iter()
        .filter(|(_, meta)| !locks.contains_key(&meta.session_id))
        .collect();
    let guessed = attribute(procs, &rollouts);
    // A remote client is a viewer, so `thread_rows` supplies its row. The daemon creates the
    // thread after the client asks for one, and until it does that thread is in no lock, database
    // or rollout, so a client with nothing yet to view keeps a row of its own. Only a thread this
    // client could have opened counts, by folder and by starting no earlier than the client; a
    // client that resumes an older thread without naming it is rare enough to show twice.
    let viewable: Vec<Meta> = locks
        .iter()
        .filter(|(_, pid)| Some(**pid) == daemon)
        .filter_map(|(id, _)| meta_of(&rollout_for(codex, &index, id)?))
        .collect();
    let mut out: Vec<Session> = procs
        .iter()
        .filter(|p| {
            !(p.remote
                && p.thread.is_none()
                && viewable
                    .iter()
                    .any(|m| Some(m.cwd.as_path()) == p.cwd.as_deref() && m.started >= p.started))
        })
        .map(|p| {
            // Explicit resume ids and held locks beat cwd/start-time attribution.
            let stated = p
                .thread
                .as_deref()
                .or_else(|| held.get(&p.pid).map(String::as_str))
                .and_then(|id| rollout_for(codex, &index, id))
                .and_then(|path| meta_of(&path).map(|m| (path, m)));
            let rollout = stated.as_ref().or_else(|| guessed.get(&p.pid).copied());
            let t = rollout.map(|(path, _)| tail_of(path)).unwrap_or_default();
            let id =
                rollout.map_or_else(|| format!("codex-{}", p.pid), |(_, m)| m.session_id.clone());
            let kind = (daemon.is_some() && locks.get(&id) == daemon.as_ref())
                .then(|| "daemon".to_owned());
            Session {
                title: index
                    .titles
                    .get(&id)
                    .cloned()
                    .or_else(|| rollout.and_then(|(path, _)| prompt_of(path))),
                session_id: id,
                harness: "codex".into(),
                kind,
                cwd: p.cwd.clone().unwrap_or_default(),
                state: t.state.unwrap_or("-").into(),
                last_activity: t.last_activity,
                model: t.model,
                started: Some(p.started),
                pid: Some(p.pid),
                transcript_path: rollout.map(|(path, _)| path.clone()),
                tokens_in: t.tokens_in,
                tokens_out: t.tokens_out,
                context_tokens: t.context_tokens,
                context_window: t.context_window,
                cost_usd: None,
                last: t.last,
                coordinator: false,
                activity: t.activity,
            }
        })
        .collect();
    out.sort_by_key(|s| s.started);
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert(s.session_id.clone()));
    out
}

/// Saved daemon launch for discovery while no client is attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub id: String,
    pub cwd: PathBuf,
    pub started: DateTime<Utc>,
    pub rollout: PathBuf,
}

fn threads_path(state: &Path) -> PathBuf {
    state.join("codex-threads.json")
}

pub fn threads(state: &Path) -> Vec<Thread> {
    fs::read_to_string(threads_path(state))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn remember(state: &Path, t: Thread) -> std::io::Result<()> {
    let mut all = threads(state);
    all.retain(|x| x.id != t.id);
    all.push(t);
    fs::create_dir_all(state)?;
    fs::write(threads_path(state), serde_json::to_string_pretty(&all)?)
}

pub fn forget(state: &Path, id: &str) -> std::io::Result<()> {
    let mut all = threads(state);
    all.retain(|x| x.id != id);
    fs::write(threads_path(state), serde_json::to_string_pretty(&all)?)
}

/// Record the newest launch in `dir` only after its first turn; the daemon drops
/// threads disconnected before that turn, so they cannot be resumed.
pub fn launched(codex: &Path, dir: &Path, since: DateTime<Utc>) -> Option<Thread> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
    rollouts(codex, since)
        .into_iter()
        .filter(|(_, m)| m.started >= since)
        .filter(|(_, m)| m.cwd.canonicalize().unwrap_or_else(|_| m.cwd.clone()) == dir)
        .filter(|(path, _)| tail_of(path).state.is_some())
        .max_by_key(|(_, m)| m.started)
        .map(|(rollout, m)| Thread {
            id: m.session_id,
            cwd: m.cwd,
            started: m.started,
            rollout,
        })
}

/// List daemon-held threads and saved launches without live client rows.
/// Omit threads whose rollout is missing.
pub fn thread_rows(codex: &Path, state: &Path, live: &[Session]) -> Vec<Session> {
    let index = index(codex);
    let daemon = daemon_pid(codex);
    let mut ids: Vec<(String, Option<Thread>)> =
        locks(codex, &daemon.into_iter().collect::<Vec<_>>())
            .into_iter()
            .filter(|(_, pid)| Some(*pid) == daemon)
            .map(|(id, _)| (id, None))
            .collect();
    for t in threads(state) {
        if !ids.iter().any(|(id, _)| *id == t.id) {
            ids.push((t.id.clone(), Some(t)));
        }
    }
    ids.sort_by(|a, b| a.0.cmp(&b.0));
    ids.into_iter()
        .filter(|(id, _)| !live.iter().any(|s| s.session_id == *id))
        .filter_map(|(id, record)| {
            let rollout = record
                .as_ref()
                .map(|t| t.rollout.clone())
                .filter(|p| p.is_file())
                .or_else(|| rollout_for(codex, &index, &id))?;
            let meta = meta_of(&rollout);
            let tail = tail_of(&rollout);
            Some(Session {
                title: index
                    .titles
                    .get(&id)
                    .cloned()
                    .or_else(|| prompt_of(&rollout)),
                harness: "codex".into(),
                kind: Some("daemon".into()),
                cwd: index
                    .threads
                    .get(&id)
                    .map(|(_, cwd)| cwd.clone())
                    .or_else(|| meta.as_ref().map(|m| m.cwd.clone()))
                    .or_else(|| record.as_ref().map(|t| t.cwd.clone()))
                    .unwrap_or_default(),
                state: tail.state.unwrap_or("-").into(),
                last_activity: tail.last_activity,
                model: tail.model,
                started: meta
                    .map(|m| m.started)
                    .or_else(|| record.as_ref().map(|t| t.started)),
                pid: None,
                transcript_path: Some(rollout),
                tokens_in: tail.tokens_in,
                tokens_out: tail.tokens_out,
                context_tokens: tail.context_tokens,
                context_window: tail.context_window,
                cost_usd: None,
                last: tail.last,
                coordinator: false,
                activity: tail.activity,
                session_id: id,
            })
        })
        .collect()
}

/// Read rollout headers only from files modified since the earliest process start.
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

/// Cache the first actual prompt. Earlier user-role messages contain instructions
/// and skills; retry uncached files until a `UserMessage` arrives.
fn prompt_of(path: &Path) -> Option<String> {
    static CACHE: Mutex<Option<HashMap<PathBuf, String>>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some(p) = cache.get(path) {
        return Some(p.clone());
    }
    let p = std::io::BufReader::new(fs::File::open(path).ok()?)
        .lines()
        .map_while(Result::ok)
        .find_map(|l| prompt(&l))?;
    cache.insert(path.to_owned(), p.clone());
    Some(p)
}

pub fn prompt(line: &str) -> Option<String> {
    let v = serde_json::from_str::<Value>(line).ok()?;
    let item = &v["payload"]["item"];
    if v["type"] != "event_msg" || item["type"] != "UserMessage" {
        return None;
    }
    item["content"]
        .as_array()?
        .iter()
        .filter_map(|c| c["text"].as_str())
        .find_map(crate::fleet::headline)
}

/// Fold appended bytes after the first full read, retaining earlier state.
/// Leave a partial final line for the next read.
fn tail_of(path: &Path) -> Tail {
    static CACHE: Mutex<Option<HashMap<PathBuf, (u64, Tail)>>> = Mutex::new(None);
    let len = fs::metadata(path).map_or(0, |m| m.len());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    let (seen, t) = cache.entry(path.to_owned()).or_default();
    if *seen > len {
        // Truncated or replaced: start over.
        (*seen, *t) = (0, Tail::default());
    }
    if *seen == len {
        return t.clone();
    }
    let Ok(mut file) = fs::File::open(path) else {
        return t.clone();
    };
    let mut data = Vec::new();
    if file.seek(SeekFrom::Start(*seen)).is_ok()
        && file.take(len - *seen).read_to_end(&mut data).is_ok()
        && let Some(end) = data.iter().rposition(|b| *b == b'\n')
    {
        t.fold(&String::from_utf8_lossy(&data[..=end]));
        *seen += end as u64 + 1;
    }
    t.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_states_its_thread_by_lock_or_resume_argument() {
        let ps = "  7 Sun Sep 13 15:19:19 2026 /usr/local/bin/codex --remote unix:///s.sock resume 01a09fed-867a-7a42-b7b9-22fbcbd280d7\n  8 Sun Sep 13 15:19:19 2026 codex resume\n  9 Sun Sep 13 15:19:19 2026 codex app-server --listen unix://\n";
        let procs = processes(ps);
        assert_eq!(procs.len(), 2, "app-server runs no session");
        assert_eq!(
            procs[0].thread.as_deref(),
            Some("01a09fed-867a-7a42-b7b9-22fbcbd280d7")
        );
        assert_eq!(procs[1].thread, None, "a bare resume picks later");
        assert!(procs[0].remote);
        assert!(!procs[1].remote);
        let held = parse_locks(
            "p22416\nf12\nn/Users/u/.codex/thread-writer-locks/01a09fed-867a-7a42-b7b9-22fbcbd280d7.lock\n",
        );
        assert_eq!(
            held.get("01a09fed-867a-7a42-b7b9-22fbcbd280d7").copied(),
            Some(22416)
        );
    }

    #[test]
    fn remote_options_are_read_before_the_prompt() {
        for args in [
            "--remote unix:///s.sock -C /repo fix this",
            "--remote=unix:///s.sock fix this",
            "-C /repo --remote unix:///s.sock fix this",
            "--model example --remote=unix:///s.sock resume --all",
            "resume --remote unix:///s.sock --last",
        ] {
            assert!(client_options(args.split_whitespace()).0, "{args}");
        }
        for args in [
            "explain --remote unix:///s.sock",
            "-C /repo explain --remote unix:///s.sock",
            "-- explain --remote unix:///s.sock",
            "--config --remote",
            "--remote",
            "--remote=",
        ] {
            assert!(!client_options(args.split_whitespace()).0, "{args}");
        }
        let prompt = "--remote unix:///s.sock explain resume 01a0a430-b8d1-7682-a7ed-51904a118c65";
        assert_eq!(client_options(prompt.split_whitespace()), (true, None));
    }

    fn rollout(home: &Path, name: &str, id: &str, cwd: &Path, at: &str, turn: bool) -> PathBuf {
        let dir = home.join("sessions/2026/09/13");
        fs::create_dir_all(&dir).unwrap();
        let mut text = format!(
            r#"{{"timestamp":"{at}","type":"session_meta","payload":{{"id":"{id}","timestamp":"{at}","cwd":{}}}}}"#,
            serde_json::to_string(cwd).unwrap()
        );
        text.push('\n');
        if turn {
            text.push_str(&format!(
                r##"{{"timestamp":"{at}","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"# AGENTS.md instructions"}}]}}}}
{{"timestamp":"{at}","type":"event_msg","payload":{{"type":"item_completed","item":{{"type":"UserMessage","content":[{{"type":"text","text":"\n  **fix** the flaky test\nplease"}}]}}}}}}
{{"timestamp":"{at}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":900,"output_tokens":40}},"last_token_usage":{{"total_tokens":120}},"model_context_window":272000}}}}}}
{{"timestamp":"{at}","type":"event_msg","payload":{{"type":"task_complete"}}}}"##
            ));
            text.push('\n');
        }
        let path = dir.join(format!("{name}.jsonl"));
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn a_turn_state_survives_a_megabyte_of_tool_output_and_a_half_written_line() {
        use std::io::Write;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("rollout.jsonl");
        let started = r#"{"timestamp":"2026-09-15T08:31:22.334Z","type":"event_msg","payload":{"type":"task_started"}}"#;
        let noise = format!(
            r#"{{"timestamp":"2026-09-15T08:31:23.000Z","type":"event_msg","payload":{{"type":"item_completed","item":{{"type":"CommandExecution","output":"{}"}}}}}}"#,
            "x".repeat(4096)
        );
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{started}").unwrap();
        for _ in 0..300 {
            writeln!(f, "{noise}").unwrap();
        }
        f.flush().unwrap();
        assert!(fs::metadata(&path).unwrap().len() > 1 << 20);
        assert_eq!(tail_of(&path).state, Some("active"));
        let done = r#"{"timestamp":"2026-09-15T08:40:00.000Z","type":"event_msg","payload":{"type":"task_complete"}}"#;
        write!(f, "{}", &done[..40]).unwrap();
        f.flush().unwrap();
        assert_eq!(
            tail_of(&path).state,
            Some("active"),
            "a half-written line waits"
        );
        writeln!(f, "{}", &done[40..]).unwrap();
        f.flush().unwrap();
        assert_eq!(tail_of(&path).state, Some("done"));
        writeln!(f, "{started}").unwrap();
        f.flush().unwrap();
        assert_eq!(
            tail_of(&path).state,
            Some("active"),
            "a new turn replaces the completed turn's state"
        );
        let aborted = r#"{"timestamp":"2026-09-15T08:41:00.000Z","type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#;
        writeln!(f, "{aborted}").unwrap();
        f.flush().unwrap();
        assert_eq!(tail_of(&path).state, Some("stopped"));
        writeln!(f, "{started}").unwrap();
        f.flush().unwrap();
        assert_eq!(
            tail_of(&path).state,
            Some("active"),
            "a new turn replaces the aborted turn's state"
        );
        fs::write(&path, format!("{started}\n")).unwrap();
        assert_eq!(
            tail_of(&path).state,
            Some("active"),
            "a shorter file is read anew"
        );
    }

    #[test]
    fn remote_viewers_and_their_daemon_threads_each_produce_one_session() {
        use fs2::FileExt;

        const A: &str = "01a0a430-b8d1-7682-a7ed-51904a118c65";
        const B: &str = "01a0a431-2ee0-76d2-88ae-71ce9826d86c";
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("codex");
        let state = d.path().join("state");
        let work = d.path().join("repo");
        fs::create_dir_all(home.join("app-server-daemon")).unwrap();
        fs::create_dir_all(home.join("thread-writer-locks")).unwrap();
        fs::write(
            home.join("app-server-daemon/app-server.pid"),
            serde_json::json!({"pid": std::process::id()}).to_string(),
        )
        .unwrap();
        let mut procs = processes(
            "7 Sun Sep 13 10:00:00 2026 codex --remote unix:///s.sock -C /repo first prompt\n\
             8 Sun Sep 13 10:01:00 2026 codex --remote unix:///s.sock -C /repo second prompt\n",
        );
        for p in &mut procs {
            p.cwd = Some(work.clone());
        }
        let fleet = |procs: &[Process]| {
            let mut live = rows(&home, procs);
            live.extend(thread_rows(&home, &state, &live));
            live.sort_by(|a, b| a.session_id.cmp(&b.session_id));
            live
        };
        assert_eq!(
            fleet(&procs)
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["codex-7", "codex-8"],
            "a client whose thread the daemon has not created yet is still a session"
        );
        let held: Vec<_> = [(A, "2026-09-13T10:00:01Z"), (B, "2026-09-13T10:01:01Z")]
            .into_iter()
            .map(|(id, at)| {
                rollout(&home, &format!("rollout-{id}"), id, &work, at, true);
                let file =
                    fs::File::create(home.join("thread-writer-locks").join(format!("{id}.lock")))
                        .unwrap();
                file.lock_exclusive().unwrap();
                file
            })
            .collect();
        let live = fleet(&procs);
        assert_eq!(
            live.iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            [A, B],
            "two launches in the same folder stay two sessions, without codex-PID rows"
        );
        assert!(live.iter().all(|s| s.kind.as_deref() == Some("daemon")));
        assert_eq!(
            live.iter().map(|s| s.started).collect::<Vec<_>>(),
            fleet(&[]).iter().map(|s| s.started).collect::<Vec<_>>(),
            "closing the viewer leaves the thread's row and start time unchanged"
        );
        for p in &mut procs {
            p.thread = Some(A.into());
        }
        assert_eq!(
            fleet(&procs)
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            [A, B],
            "two resume clients on one thread still give it just one row"
        );
        let latecomer = Process {
            pid: 10,
            started: "2026-09-13T10:02:00Z".parse().unwrap(),
            thread: None,
            ..procs[0].clone()
        };
        assert_eq!(
            rows(&home, &[latecomer])
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["codex-10"],
            "older threads in the folder do not cover a client waiting for its own"
        );
        let standalone = Process {
            pid: 9,
            remote: false,
            thread: None,
            ..procs[0].clone()
        };
        let bare = rows(&home, &[standalone]);
        assert_eq!(
            bare[0].session_id, "codex-9",
            "a standalone TUI cannot claim a rollout whose writer is the daemon"
        );
        drop(held);
        assert!(fleet(&[]).is_empty(), "the daemon released both threads");
    }

    #[test]
    fn a_launch_is_the_newest_rollout_in_its_dir_with_a_turn_and_the_list_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let (home, state, work) = (
            d.path().join("codex"),
            d.path().join("state"),
            d.path().join("w"),
        );
        let other = d.path().join("elsewhere");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&other).unwrap();
        let since = DateTime::parse_from_rfc3339("2026-09-13T10:00:00Z")
            .unwrap()
            .into();
        rollout(&home, "old", "aaaa", &work, "2026-09-13T09:59:00Z", true);
        rollout(&home, "away", "bbbb", &other, "2026-09-13T10:00:30Z", true);
        rollout(&home, "fresh", "cccc", &work, "2026-09-13T10:00:40Z", false);
        assert_eq!(
            launched(&home, &work, since),
            None,
            "no turn yet, nothing to come back to"
        );
        let path = rollout(&home, "turned", "dddd", &work, "2026-09-13T10:00:20Z", true);
        let t = launched(&home, &work, since).expect("the turned thread in this dir");
        assert_eq!((t.id.as_str(), &t.rollout), ("dddd", &path));
        remember(&state, t.clone()).unwrap();
        assert_eq!(threads(&state), vec![t.clone()]);
        remember(&state, t.clone()).unwrap();
        assert_eq!(
            threads(&state).len(),
            1,
            "remembering twice keeps one entry"
        );
        assert_eq!(
            thread_rows(&home, &state, &[])[0].title.as_deref(),
            Some("fix the flaky test"),
            "unnamed thread shows its first prompt, not AGENTS.md"
        );
        fs::write(
            home.join("session_index.jsonl"),
            r#"{"id":"dddd","thread_name":"fix the build","updated_at":"x"}"#,
        )
        .unwrap();
        if Command::new("sqlite3")
            .arg("-version")
            .stdout(Stdio::null())
            .status()
            .is_ok()
        {
            let db = home.join("state_5.sqlite");
            let sql = "create table threads(id text, name text, title text, rollout_path text, cwd text); insert into threads values('dddd', null, 'fix the build', '', ''), ('eeee', 'Green CI', 'x', '', '');";
            assert!(
                Command::new("sqlite3")
                    .arg(&db)
                    .arg(sql)
                    .status()
                    .unwrap()
                    .success()
            );
            assert_eq!(
                index(&home).titles.get("eeee").map(String::as_str),
                Some("Green CI")
            );
        }
        let rows = thread_rows(&home, &state, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind.as_deref(), Some("daemon"));
        assert_eq!(
            (
                rows[0].tokens_in,
                rows[0].tokens_out,
                rows[0].context_tokens,
                rows[0].context_window
            ),
            (Some(900), Some(40), Some(120), Some(272_000)),
            "tokens and window from the rollout's last token_count"
        );
        assert_eq!(
            (rows[0].state.as_str(), rows[0].title.as_deref()),
            ("done", Some("fix the build"))
        );
        let live = rows.clone();
        assert!(
            thread_rows(&home, &state, &live).is_empty(),
            "an attached client's row wins"
        );
        forget(&state, "dddd").unwrap();
        assert!(threads(&state).is_empty());
        assert!(thread_rows(&home, &state, &[]).is_empty());
    }
}
