//! pi fleet discovery from processes and session files. pi overwrites argv, so
//! files are matched by cwd and write time; multiple processes in one cwd are ambiguous.
//! pi supports neither attach nor supervised cones jobs.
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

/// Honor `PI_CODING_AGENT_DIR`, otherwise use `.pi/agent` beside the Claude directory.
pub fn home(claude: &Path) -> PathBuf {
    match std::env::var_os("PI_CODING_AGENT_DIR").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => claude.with_file_name(".pi").join("agent"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// Start time as `ps -o lstart` prints it under UTC.
    pub started: DateTime<Utc>,
    pub cwd: Option<PathBuf>,
}

/// The `session` line pi writes first in every session file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub session_id: String,
    pub cwd: PathBuf,
    pub started: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tail {
    pub last: Option<String>,
    /// State from the latest message role and assistant `stopReason`.
    pub state: Option<&'static str>,
    pub model: Option<String>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Latest prompt size: input plus cacheRead and cacheWrite.
    pub context_tokens: Option<u64>,
    /// `usage.cost.total` summed. pi writes 0 for a model it has no price for.
    pub cost_usd: f64,
    /// The name `--name` or `/name` set, from the last `session_info` entry.
    pub name: Option<String>,
    pub prompt: Option<String>,
    pub last_activity: Option<DateTime<Utc>>,
    pub activity: Vec<Activity>,
}

/// Skip process discovery when the pi home is absent.
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

/// Parse pi's process title. It erases subcommands, so sessions and updates look alike;
/// `pi-rpc` is excluded.
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

/// Folder names flatten `/` and `:` alike; verify cwd from the file header to resolve collisions.
pub fn session_dir(pi: &Path, cwd: &Path) -> PathBuf {
    let name = cwd
        .to_string_lossy()
        .trim_start_matches('/')
        .replace(['/', ':'], "-");
    pi.join("sessions").join(format!("--{name}--"))
}

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

/// Ignore malformed JSON, including a partially written last line.
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
            Some("user") => {
                t.state = Some("active");
                if t.prompt.is_none() {
                    t.prompt = texts("text").find_map(crate::fleet::headline);
                }
            }
            Some("toolResult") => t.state = Some("active"),
            Some("assistant") => {
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

pub fn rows(pi: &Path, procs: &[Process]) -> Vec<Session> {
    let mut out: Vec<Session> = procs
        .iter()
        .map(|p| {
            // Multiple pi processes in one cwd make attribution ambiguous.
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

/// Choose the latest file written since process start and verify its header cwd.
/// `--continue` reopens old files, so their creation time cannot identify the process.
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

/// Cache by length; recount the whole file when it changes.
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

        let later = Process {
            started: Utc::now() + chrono::Duration::hours(1),
            ..p
        };
        assert_eq!(rows(pi, &[later])[0].session_id, "pi-7");
    }
}
