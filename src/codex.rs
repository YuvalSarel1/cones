//! Codex in the fleet. Codex keeps no session registry, so a Codex row is assembled from two
//! reports: the process table (pid and start time from `ps`, the working directory from `lsof`)
//! and the rollout file Codex writes under `~/.codex/sessions` once a session has its first
//! turn. A rollout is tied to a process only by the two facts it records, its cwd and its start
//! time; a process without one shows `-` for title, last reply and state. cones runs no Codex
//! job and holds no Codex budget: seeing a session is all this module does.
use crate::fleet::Session;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    io::BufRead,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

/// Codex's home: `$CODEX_HOME`, the override Codex honors, else `.codex` beside the Claude dir
/// (`~/.codex` next to `~/.claude`). Holds `sessions/` and `session_index.jsonl`.
// ponytail: deriving the home from the Claude dir keeps a test's temp dir hermetic; a layout
// where the two do not sit together sets CODEX_HOME.
pub fn home(claude: &Path) -> PathBuf {
    match std::env::var_os("CODEX_HOME").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => claude.with_file_name(".codex"),
    }
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

/// Every live Codex session, oldest first by process start. No Codex home means Codex is not
/// installed here: the process table is not read, so a machine or a test without one sees no
/// `codex` process, whatever else is running.
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

/// Thread names as Codex 0.154 keeps them: the `threads` table of the newest `state_*.sqlite`
/// under the Codex home, `name` (the name Codex or the user gave) else `title` (the first
/// prompt). `session_index.jsonl` stopped being written with the move to sqlite, so it is read
/// behind the database, for threads older than the move.
// ponytail: shells out to /usr/bin/sqlite3 (13 ms for a hundred threads) instead of adding a
// sqlite crate; the table is read in full every tick, cache by mtime if it ever shows.
pub fn names(codex: &Path) -> HashMap<String, String> {
    let mut out = fs::read_to_string(codex.join("session_index.jsonl"))
        .map(|t| titles(&t))
        .unwrap_or_default();
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
        .arg("select id, coalesce(name, title) as t from threads where coalesce(name, title) <> ''")
        .stderr(Stdio::null())
        .output()
    else {
        return out;
    };
    for v in serde_json::from_slice::<Vec<Value>>(&run.stdout).unwrap_or_default() {
        if let (Some(id), Some(t)) = (v["id"].as_str(), v["t"].as_str())
            && let Some(first) = crate::fleet::headline(t)
        {
            out.insert(id.to_owned(), first);
        }
    }
    out
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

/// Fleet rows for live processes: the rollout each wrote, its title from `names`,
/// last reply and state from its tail. Reads only under `codex`.
pub fn rows(codex: &Path, procs: &[Process]) -> Vec<Session> {
    let Some(since) = procs.iter().map(|p| p.started).min() else {
        return Vec::new();
    };
    let rollouts = rollouts(codex, since);
    let owned = attribute(procs, &rollouts);
    let titles = names(codex);
    let mut out: Vec<Session> = procs
        .iter()
        .map(|p| {
            let rollout = owned.get(&p.pid);
            let t = rollout.map(|(path, _)| tail_of(path)).unwrap_or_default();
            let id =
                rollout.map_or_else(|| format!("codex-{}", p.pid), |(_, m)| m.session_id.clone());
            Session {
                title: titles
                    .get(&id)
                    .cloned()
                    .or_else(|| rollout.and_then(|(path, _)| prompt_of(path))),
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

/// A Codex thread the dashboard launched behind the app-server daemon. The daemon's TUI is a
/// client: leaving it keeps the thread working, and `codex --remote ... resume ID` opens it
/// again. The process table shows nothing while no client is attached, and the app server has
/// no thread list yet, so cones keeps its own list under the state dir; `x x` on the row
/// forgets an entry.
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

/// The thread a launch in `dir` at `since` produced: the newest rollout in that directory
/// started since then, if it has had a turn. The daemon drops a thread that disconnects before
/// its first turn and `resume` on it exits at once, so such a launch is not recorded.
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

/// Fleet rows for recorded threads no live client shows: `kind` is `daemon`, which `enter`
/// resumes; state and last reply come from the rollout's tail, the title from the index. A
/// thread whose rollout is gone is not a row. `live` are the process-table rows, which carry
/// the same id while a client is attached.
pub fn thread_rows(codex: &Path, state: &Path, live: &[Session]) -> Vec<Session> {
    let titles = names(codex);
    threads(state)
        .into_iter()
        .filter(|t| t.rollout.is_file() && !live.iter().any(|s| s.session_id == t.id))
        .map(|t| {
            let tail = tail_of(&t.rollout);
            Session {
                title: titles.get(&t.id).cloned().or_else(|| prompt_of(&t.rollout)),
                session_id: t.id,
                harness: "codex".into(),
                kind: Some("daemon".into()),
                cwd: t.cwd,
                state: tail.state.unwrap_or("-").into(),
                last_activity: tail.last_activity,
                model: tail.model,
                started: Some(t.started),
                pid: None,
                transcript_path: Some(t.rollout),
                tokens_in: None,
                tokens_out: None,
                context_tokens: None,
                cost_usd: None,
                last: tail.last,
            }
        })
        .collect()
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

/// The first prompt of a rollout, cached: Codex names a thread in `session_index.jsonl` only
/// some time after the first turn (a daemon thread from VS Code had none an hour in), so a row
/// without a name shows what was asked instead of `-`. The prompt is the first `UserMessage`
/// item; the `role: user` messages before it carry AGENTS.md and skills, not the ask.
// ponytail: scans the head line by line and stops at the first prompt; a rollout with no prompt
// yet is not cached, so the next tick reads it again.
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

/// The headline of a `UserMessage` item_completed event, if `line` is one.
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

#[cfg(test)]
mod tests {
    use super::*;

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
{{"timestamp":"{at}","type":"event_msg","payload":{{"type":"task_complete"}}}}"##
            ));
            text.push('\n');
        }
        let path = dir.join(format!("{name}.jsonl"));
        fs::write(&path, text).unwrap();
        path
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
            let sql = "create table threads(id text, name text, title text); insert into threads values('dddd', null, 'fix the build'), ('eeee', 'Green CI', 'x');";
            assert!(
                Command::new("sqlite3")
                    .arg(&db)
                    .arg(sql)
                    .status()
                    .unwrap()
                    .success()
            );
            assert_eq!(
                names(&home).get("eeee").map(String::as_str),
                Some("Green CI")
            );
        }
        let rows = thread_rows(&home, &state, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind.as_deref(), Some("daemon"));
        assert_eq!(
            (rows[0].state.as_str(), rows[0].title.as_deref()),
            ("idle", Some("fix the build"))
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
