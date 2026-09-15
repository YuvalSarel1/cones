//! pi in the fleet. pi keeps no session registry, so a pi row is assembled from two reports:
//! the process table (pid and start time from `ps`, the working directory from the kernel) and
//! the session file pi writes under its agent directory once a session has its first turn. pi
//! sets its process title, so a live `pi` states nothing else about itself: no subcommand, no
//! session id, no flags. Its session is therefore the file written in that process's own
//! directory since it started, and a directory running two pi processes shows `-` for both.
//! pi has no background mode and no attach, so a pi row is seen, never joined; it has no dollar
//! budget flag either, so cones runs no pi job and holds no pi budget.
use crate::fleet::{Activity, Session};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

/// pi's agent directory: `$PI_CODING_AGENT_DIR`, the override pi honors, else `.pi/agent` beside
/// the Claude dir (`~/.pi/agent` next to `~/.claude`). Holds `sessions/`.
// ponytail: deriving the home from the Claude dir keeps a test's temp dir hermetic, as the Codex
// home does; a layout where the two do not sit together sets PI_CODING_AGENT_DIR.
pub fn home(claude: &Path) -> PathBuf {
    match std::env::var_os("PI_CODING_AGENT_DIR").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => claude.with_file_name(".pi").join("agent"),
    }
}

/// A live `pi` process: everything the process table states about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// Start time as `ps -o lstart` prints it under UTC.
    pub started: DateTime<Utc>,
    /// Working directory, which names the session directory pi writes into.
    pub cwd: Option<PathBuf>,
}

/// The `session` line pi writes first in every session file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub session_id: String,
    pub cwd: PathBuf,
    pub started: DateTime<Utc>,
}

/// What one pass over a session file reads out of pi's own entries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tail {
    /// First line of the assistant's most recent text.
    pub last: Option<String>,
    /// The latest turn, from the last message entry: `active` while the user's ask or a tool
    /// result is the last word, else the assistant's own `stopReason`.
    pub state: Option<&'static str>,
    /// `model` on the last assistant message, verbatim.
    pub model: Option<String>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// The prompt pi's own status line counts on the last assistant message: `usage.input`,
    /// `cacheRead` and `cacheWrite`, which pi keeps apart.
    pub context_tokens: Option<u64>,
    /// `usage.cost.total` summed. pi writes 0 for a model it has no price for.
    pub cost_usd: f64,
    /// The name `--name` or `/name` set, from the last `session_info` entry.
    pub name: Option<String>,
    /// The first line of the user's first ask, the title until a name is set.
    pub prompt: Option<String>,
    pub last_activity: Option<DateTime<Utc>>,
    pub activity: Vec<Activity>,
}

/// Every live pi session, oldest first by process start. No pi home means pi is not installed
/// here: the process table is not read at all.
pub fn sessions(pi: &Path) -> Vec<Session> {
    if !pi.is_dir() {
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
    let cwds = crate::codex::cwds(&lsof);
    for p in &mut procs {
        if p.cwd.is_none() {
            p.cwd = cwds.get(&p.pid).cloned();
        }
    }
    rows(pi, &procs)
}

/// Live pi processes in the output of `TZ=UTC ps -axww -o pid=,lstart=,command=`. pi overwrites
/// its argv with its process title, `pi`, so the whole command is that one word: a session and a
/// `pi update` look alike here, and only the program name tells pi from another program. The rpc
/// server titles itself `pi-rpc` and is not a session. `cwd` is read afterwards.
pub fn processes(ps: &str) -> Vec<Process> {
    ps.lines()
        .filter_map(|line| {
            let (pid, rest) = line.trim_start().split_once(' ')?;
            let rest = rest.trim_start();
            // `lstart` is fixed width: `Sun Sep 13 15:19:19 2026`, the day padded with a space.
            let (start, command) = rest.split_at_checked(24)?;
            let program = Path::new(command.split_whitespace().next()?).file_name()?;
            if program != "pi" {
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

/// The folder pi keeps a directory's sessions in: the path without its leading separator, every
/// `/` and `:` as `-`, wrapped in `--`. Two directories can share one name (`/a/b:c` and
/// `/a/b/c`), so the `cwd` in a file's own first line decides which one it belongs to.
pub fn session_dir(pi: &Path, cwd: &Path) -> PathBuf {
    let name = cwd
        .to_string_lossy()
        .trim_start_matches('/')
        .replace(['/', ':'], "-");
    pi.join("sessions").join(format!("--{name}--"))
}

/// The `session` line: session id, the directory pi ran in, and the file's own start.
pub fn meta(line: &str) -> Option<Meta> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v["type"] != "session" {
        return None;
    }
    let id = v["id"].as_str()?;
    // The id names a row and a log line, so it is checked before it becomes either.
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(Meta {
        session_id: id.into(),
        cwd: PathBuf::from(v["cwd"].as_str()?),
        started: DateTime::parse_from_rfc3339(v["timestamp"].as_str()?)
            .ok()?
            .into(),
    })
}

/// Last reply, turn state, usage and title from a session file's entries. Lines that are not
/// JSON are skipped; pi may be mid-write on the last one.
pub fn tail(lines: &str) -> Tail {
    let mut t = Tail::default();
    for line in lines.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let at = v["timestamp"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(DateTime::<Utc>::from);
        if let Some(at) = at {
            t.last_activity = Some(at);
            t.activity.push(Activity::at(at));
        }
        if v["type"] == "session_info" {
            t.name = v["name"]
                .as_str()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_owned);
            continue;
        }
        if v["type"] != "message" {
            continue;
        }
        let m = &v["message"];
        let texts = |kind: &'static str| {
            m["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(move |b| b["type"] == kind)
                .filter_map(|b| b["text"].as_str())
        };
        match m["role"].as_str() {
            // The user's ask, or a tool's answer: the model has the turn either way.
            Some("user") => {
                t.state = Some("active");
                if t.prompt.is_none() {
                    t.prompt = texts("text").find_map(crate::fleet::headline);
                }
            }
            Some("toolResult") => t.state = Some("active"),
            Some("assistant") => {
                // pi appends an assistant message once the model has answered, and its stop
                // reason is that turn's: a tool call means the turn goes on.
                t.state = Some(match m["stopReason"].as_str() {
                    Some("toolUse") => "active",
                    Some("stop") => "idle",
                    Some("aborted") => "stopped",
                    Some("error") => "failed",
                    _ => "-",
                });
                if let Some(last) = texts("text").filter_map(crate::fleet::headline).next_back() {
                    t.last = Some(last);
                }
                if let Some(model) = m["model"].as_str() {
                    t.model = Some(model.to_owned());
                }
                let u = &m["usage"];
                let n = |k: &str| u[k].as_u64().unwrap_or(0);
                // pi counts the prompt in three parts, as its own window figure sums them.
                let prompt = n("input") + n("cacheRead") + n("cacheWrite");
                t.tokens_in += prompt;
                t.tokens_out += n("output");
                t.context_tokens = Some(prompt);
                t.cost_usd += u["cost"]["total"].as_f64().unwrap_or(0.0);
                if let Some(a) = t.activity.last_mut() {
                    a.messages += 1;
                    a.tokens_out += n("output");
                    a.tools += m["content"]
                        .as_array()
                        .map_or(0, |c| c.iter().filter(|b| b["type"] == "toolCall").count())
                        as u64;
                }
            }
            _ => {}
        }
    }
    t
}

/// Fleet rows for live pi processes: the session file each wrote, and what that file records.
/// A process with no file of its own shows `-` everywhere but keeps a row, so `stop` can name
/// it. Reads only under `pi`.
pub fn rows(pi: &Path, procs: &[Process]) -> Vec<Session> {
    let mut out: Vec<Session> = procs
        .iter()
        .map(|p| {
            // A directory running two pi processes could hand either one the same file, so
            // neither takes it, exactly as two Codex processes in one directory take none.
            let alone = procs
                .iter()
                .filter(|o| o.cwd.is_some() && o.cwd == p.cwd)
                .count()
                == 1;
            let file = alone
                .then(|| p.cwd.as_deref().and_then(|cwd| session_file(pi, cwd, p)))
                .flatten();
            let t = file
                .as_ref()
                .map(|(path, _)| tail_of(path))
                .unwrap_or_default();
            Session {
                title: t.name.or(t.prompt),
                session_id: file
                    .as_ref()
                    .map_or_else(|| format!("pi-{}", p.pid), |(_, m)| m.session_id.clone()),
                harness: "pi".into(),
                // pi has no daemon: every session owns the terminal it was typed in.
                kind: None,
                cwd: p.cwd.clone().unwrap_or_default(),
                state: t.state.unwrap_or("-").into(),
                last_activity: t.last_activity,
                model: t.model,
                started: Some(p.started),
                pid: Some(p.pid),
                transcript_path: file.as_ref().map(|(path, _)| path.clone()),
                tokens_in: (t.tokens_in > 0).then_some(t.tokens_in),
                tokens_out: (t.tokens_out > 0).then_some(t.tokens_out),
                context_tokens: t.context_tokens,
                // pi prices a turn itself, and writes 0 for a model it has no price for.
                context_window: None,
                cost_usd: (t.cost_usd > 0.0).then_some(t.cost_usd),
                last: t.last,
                coordinator: false,
                activity: t.activity,
            }
        })
        .collect();
    out.sort_by_key(|s| s.started);
    out
}

/// The session file a process wrote: in its own directory, written since it started, the most
/// recently written one when it has opened several. `--continue` appends to a file older than
/// the process, so the file's own start is not the match; what it records is its directory,
/// which is checked against the process's, since two directories can share one folder name.
fn session_file(pi: &Path, cwd: &Path, p: &Process) -> Option<(PathBuf, Meta)> {
    let mut best: Option<(PathBuf, Meta, std::time::SystemTime)> = None;
    for entry in fs::read_dir(session_dir(pi, cwd))
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let Some(written) = entry.metadata().ok().and_then(|m| m.modified().ok()) else {
            continue;
        };
        if DateTime::<Utc>::from(written) < p.started {
            continue;
        }
        if best.as_ref().is_some_and(|(_, _, seen)| *seen >= written) {
            continue;
        }
        if let Some(m) = meta_of(&path).filter(|m| m.cwd == cwd) {
            best = Some((path, m, written));
        }
    }
    best.map(|(path, m, _)| (path, m))
}

/// The first line of a session file, cached: it never changes once written.
fn meta_of(path: &Path) -> Option<Meta> {
    static CACHE: Mutex<Option<HashMap<PathBuf, Meta>>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some(m) = cache.get(path) {
        return Some(m.clone());
    }
    let mut first = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(fs::File::open(path).ok()?),
        &mut first,
    )
    .ok()?;
    let m = meta(&first)?;
    cache.insert(path.to_owned(), m.clone());
    Some(m)
}

/// The file's entries, cached by length: totals are summed over the whole file, so a file that
/// has not grown since the last refresh is not read again.
// ponytail: rereads the whole file when it grows, where codex.rs folds each rollout's tail
// incrementally; do the same here if a long pi session slows the refresh.
fn tail_of(path: &Path) -> Tail {
    static CACHE: Mutex<Option<HashMap<PathBuf, (u64, Tail)>>> = Mutex::new(None);
    let len = fs::metadata(path).map_or(0, |m| m.len());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some((_, t)) = cache.get(path).filter(|(seen, _)| *seen == len) {
        return t.clone();
    }
    let t = fs::read_to_string(path)
        .map(|s| tail(&s))
        .unwrap_or_default();
    cache.insert(path.to_owned(), (len, t.clone()));
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = r#"{"type":"session","version":3,"id":"01a0393e-ad43","timestamp":"2026-08-25T14:06:44.035Z","cwd":"/src/one"}
{"type":"model_change","timestamp":"2026-08-25T14:06:44.050Z","provider":"amazon-bedrock","modelId":"us.openai.gpt-5.6-sol"}
{"type":"message","timestamp":"2026-08-25T14:06:44.053Z","message":{"role":"user","content":[{"type":"text","text":"fix the flaky test"}]}}
{"type":"message","timestamp":"2026-08-25T14:06:46.238Z","message":{"role":"assistant","content":[{"type":"text","text":"Reading it"},{"type":"toolCall","name":"read"}],"model":"us.openai.gpt-5.6-sol","usage":{"input":1117,"output":5,"cacheRead":1115,"cacheWrite":0,"cost":{"total":0.002}},"stopReason":"toolUse"}}
"#;

    #[test]
    fn a_process_line_is_pi_only_when_the_program_is() {
        let ps = "\
  101 Sun Sep 13 15:19:19 2026 pi
  102 Sun Sep 13 15:19:20 2026 pi-rpc
  103 Sun Sep 13 15:19:21 2026 /opt/homebrew/bin/pi
  104 Sun Sep 13 15:19:22 2026 vim notes-about-pi.md
";
        let procs = processes(ps);
        assert_eq!(
            procs.iter().map(|p| p.pid).collect::<Vec<_>>(),
            [101, 103],
            "the rpc server and a file named for pi are not sessions"
        );
        assert_eq!(
            procs[0].started.to_rfc3339(),
            "2026-09-13T15:19:19+00:00",
            "ps prints the start under UTC"
        );
    }

    #[test]
    fn the_session_folder_is_the_directory_flattened() {
        assert_eq!(
            session_dir(Path::new("/home/.pi/agent"), Path::new("/Users/y/work")),
            Path::new("/home/.pi/agent/sessions/--Users-y-work--")
        );
    }

    #[test]
    fn the_first_line_names_the_session_its_folder_and_its_start() {
        let m = meta(SESSION.lines().next().unwrap()).unwrap();
        assert_eq!(m.session_id, "01a0393e-ad43");
        assert_eq!(m.cwd, Path::new("/src/one"));
        assert_eq!(m.started.to_rfc3339(), "2026-08-25T14:06:44.035+00:00");
        assert_eq!(meta(r#"{"type":"message"}"#), None);
        assert_eq!(
            meta(
                r#"{"type":"session","id":"../escape","cwd":"/x","timestamp":"2026-08-25T14:06:44.035Z"}"#
            ),
            None
        );
    }

    #[test]
    fn the_entries_give_the_turn_the_usage_and_the_title() {
        let t = tail(SESSION);
        assert_eq!(
            t.state,
            Some("active"),
            "the turn goes on after a tool call"
        );
        assert_eq!(t.last.as_deref(), Some("Reading it"));
        assert_eq!(t.prompt.as_deref(), Some("fix the flaky test"));
        assert_eq!((t.tokens_in, t.tokens_out), (2232, 5));
        assert_eq!(
            t.context_tokens,
            Some(2232),
            "the prompt is pi's input, cache reads and cache writes"
        );
        assert_eq!(t.cost_usd, 0.002, "pi prices each turn itself");
        assert_eq!(t.model.as_deref(), Some("us.openai.gpt-5.6-sol"));
        assert_eq!(t.activity.len(), 4);
        assert_eq!((t.activity[3].messages, t.activity[3].tools), (1, 1));
    }

    #[test]
    fn a_finished_turn_is_idle_and_a_name_beats_the_first_ask() {
        let done = format!(
            "{SESSION}{}\n{}\n",
            r#"{"type":"message","timestamp":"2026-08-25T14:07:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Fixed it"}],"usage":{"input":2000,"output":9,"cost":{"total":0.001}},"stopReason":"stop"}}"#,
            r#"{"type":"session_info","timestamp":"2026-08-25T14:07:01.000Z","name":"flaky test"}"#
        );
        let t = tail(&done);
        assert_eq!(t.state, Some("idle"));
        assert_eq!(t.last.as_deref(), Some("Fixed it"));
        assert_eq!(t.name.as_deref(), Some("flaky test"));
        assert_eq!((t.tokens_in, t.context_tokens), (4232, Some(2000)));
        assert!((t.cost_usd - 0.003).abs() < 1e-9);
    }

    #[test]
    fn a_process_takes_the_file_written_in_its_own_directory_since_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let pi = dir.path();
        let cwd = Path::new("/src/one");
        fs::create_dir_all(session_dir(pi, cwd)).unwrap();
        let path = session_dir(pi, cwd).join("2026-08-25T14-06-44-035Z_01a0393e-ad43.jsonl");
        fs::write(&path, SESSION).unwrap();
        let p = Process {
            pid: 7,
            started: "2026-08-25T14:00:00Z".parse().unwrap(),
            cwd: Some(cwd.into()),
        };
        let one = rows(pi, std::slice::from_ref(&p));
        assert_eq!(one[0].session_id, "01a0393e-ad43");
        assert_eq!(one[0].state, "active");
        assert_eq!(one[0].title.as_deref(), Some("fix the flaky test"));
        assert_eq!(one[0].cost_usd, Some(0.002));
        assert!(one[0].own_terminal(), "pi has no way to join a session");

        // A second pi in the same directory: the file could be either's, so neither takes it.
        let two = [
            p.clone(),
            Process {
                pid: 8,
                ..p.clone()
            },
        ];
        let both = rows(pi, &two);
        assert_eq!(both[0].session_id, "pi-7");
        assert_eq!(both[1].state, "-");

        // A pi that started after the file was last written wrote none of it.
        let later = Process {
            started: Utc::now() + chrono::Duration::hours(1),
            ..p
        };
        assert_eq!(rows(pi, &[later])[0].session_id, "pi-7");
    }
}
