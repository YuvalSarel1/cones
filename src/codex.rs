//! Codex fleet discovery from processes, writer locks, the thread database and rollouts.
//! This module observes native sessions; it does not execute supervised jobs.
mod runtime;
use crate::{
    cost::{Adapter, Reader, Reading, Response},
    fleet::Session,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    io::{BufRead, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

/// Honor `CODEX_HOME`, otherwise use `.codex` beside the Claude directory.
/// The sibling default keeps fixture homes isolated.
pub fn home(claude: &Path) -> PathBuf {
    crate::harness::spec(crate::config::HarnessKind::Codex)
        .home
        .resolve(claude)
}

/// Every Codex home to read. `CODEX_HOME` pins one; otherwise the default home and
/// its `.codex-*` siblings, because a daemon serves one provider region and a model
/// in another region needs a home of its own to be seen and joined here.
pub fn homes(claude: &Path) -> Vec<PathBuf> {
    crate::harness::spec(crate::config::HarnessKind::Codex)
        .home
        .all(claude)
}

/// The daemon's pid file and lock directory, as the definition names them.
fn daemon_files() -> &'static crate::harness::spec::Daemon {
    crate::harness::spec(crate::config::HarnessKind::Codex)
        .discovery
        .daemon
        .as_ref()
        .expect("validated codex daemon paths")
}

/// The home a rollout lives in: the parent of the `sessions` directory holding it.
/// A row carries its rollout, so a join resumes against the daemon that owns it.
pub fn home_of(rollout: &Path) -> Option<&Path> {
    crate::harness::spec(crate::config::HarnessKind::Codex)
        .transcript
        .home_of(rollout)
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
    /// The first line of the prompt on the command line, or `None` without one.
    pub prompt: Option<String>,
}

/// The `session_meta` line Codex writes first in every rollout file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub session_id: String,
    pub forked_from: Option<String>,
    pub cwd: PathBuf,
    pub started: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tail {
    pub last: Option<String>,
    /// Latest turn: `active` after `task_started`, `done` after `task_complete`,
    /// `stopped` after `turn_aborted`. A new turn returns to `active`.
    pub state: Option<&'static str>,
    pub last_activity: Option<DateTime<Utc>>,
    /// `turn_context.model` on the last turn, verbatim.
    pub model: Option<String>,
    /// `thread_settings_applied` model differing from `model`, until the next `turn_context`.
    pub next_model: Option<String>,
    /// `turn_context.effort` on the last turn, verbatim.
    pub effort: Option<String>,
    /// Latest `total_token_usage`: input includes cache hits; context uses `last_token_usage.total_tokens`.
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub context_tokens: Option<u64>,
    /// `token_count.info.model_context_window` on that event.
    pub context_window: Option<u64>,
    pub(crate) accounting: crate::cost::Accounting<CostAdapter>,
    /// Timestamped rollout activity for sparklines; see docs/harness.md for event mappings.
    pub activity: Vec<crate::fleet::Activity>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CostAdapter {
    provider: Option<String>,
    model: Option<String>,
    unsupported_tier: bool,
    previous: Option<Counters>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counters {
    input: u64,
    cached: u64,
    written: u64,
    output: u64,
}

impl Counters {
    fn read(value: &Value, bedrock: bool) -> Option<Self> {
        Some(Self {
            input: value["input_tokens"].as_u64()?,
            cached: value["cached_input_tokens"].as_u64()?,
            // Older providers do not report cache writes. Bedrock requires this counter.
            written: if bedrock {
                value["cache_write_input_tokens"].as_u64()?
            } else {
                value["cache_write_input_tokens"].as_u64().unwrap_or(0)
            },
            output: value["output_tokens"].as_u64()?,
        })
    }

    fn since(self, previous: Self) -> Option<Self> {
        Some(Self {
            input: self.input.checked_sub(previous.input)?,
            cached: self.cached.checked_sub(previous.cached)?,
            written: self.written.checked_sub(previous.written)?,
            output: self.output.checked_sub(previous.output)?,
        })
    }
}

impl Adapter for CostAdapter {
    fn read<'a>(&'a mut self, event: &'a Value) -> Reading<'a> {
        let payload = &event["payload"];
        if event["type"] == "session_meta" {
            self.provider = payload["model_provider"].as_str().map(str::to_owned);
        }
        if event["type"] == "turn_context" {
            self.model = payload["model"].as_str().map(str::to_owned);
            self.unsupported_tier = !matches!(
                payload["service_tier"].as_str(),
                None | Some("default" | "auto")
            );
        }
        if event["type"] != "event_msg" {
            return Reading::Ignore;
        }
        if payload["type"] == "model_rerouted" {
            // A reroute must identify the actual model before it can be priced.
            self.model = payload["to_model"].as_str().map(str::to_owned);
        }
        if payload["type"] != "token_count" || !payload["info"].is_object() {
            return Reading::Ignore;
        }
        let info = &payload["info"];
        let bedrock = self.provider.as_deref() == Some("amazon-bedrock");
        let Some(current) = Counters::read(&info["total_token_usage"], bedrock) else {
            return Reading::Gap("missing_counters");
        };
        if self.previous == Some(current) {
            // Rate-limit/usage updates may repeat the last completed request.
            return Reading::Ignore;
        }
        let previous = self.previous.replace(current).unwrap_or_default();
        let Some(delta) = current.since(previous) else {
            return Reading::Gap("counter_reset");
        };
        let Some(last) = Counters::read(&info["last_token_usage"], bedrock) else {
            return Reading::Gap("missing_request_usage");
        };
        let gap = if delta != last {
            // A cumulative jump can cover requests whose model/tier is no longer recoverable.
            if delta.since(last).is_none() {
                return Reading::Gap("unobserved_usage");
            }
            Some("unobserved_usage")
        } else {
            None
        };
        Reading::Response(Response {
            id: None, // Repeated cumulative updates were eliminated above.
            reported_usd: None,
            empty: last == Counters::default(),
            usage: (|| {
                if self.unsupported_tier {
                    return Err("unsupported_service_tier");
                }
                Ok(crate::cost::Usage {
                    provider: self
                        .provider
                        .as_deref()
                        .ok_or("missing_provider_or_model")?,
                    model: self.model.as_deref().ok_or("missing_provider_or_model")?,
                    input: last
                        .input
                        .checked_sub(last.cached)
                        .and_then(|n| n.checked_sub(last.written))
                        .ok_or("invalid_usage")?,
                    cache_read: last.cached,
                    cache_write: last.written,
                    // Reasoning is already included in output_tokens.
                    output: last.output,
                })
            })(),
            gap,
        })
    }
}

/// Skip discovery when the Codex home is absent, including in isolated tests.
pub fn sessions(codex: &Path) -> anyhow::Result<Vec<Session>> {
    sessions_from("/bin/ps", codex)
}

/// `sessions` against a named `ps`, so a test can point it at one that cannot run.
pub fn sessions_from(ps: &str, codex: &Path) -> anyhow::Result<Vec<Session>> {
    if !codex.is_dir() {
        return Ok(Vec::new());
    }
    let table = crate::fleet::pass_table(ps)?;
    let mut procs = processes(&table);
    if procs.is_empty() {
        return Ok(Vec::new());
    }
    let own = crate::fleet::own_home_processes(
        ps,
        crate::config::HarnessKind::Codex,
        &procs.iter().map(|p| p.pid).collect::<Vec<_>>(),
    );
    procs.retain(|p| own.contains(&p.pid));
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
        crate::observe::spawn(
            crate::observe::op::OPEN_FILES,
            Command::new("/usr/sbin/lsof")
                .args(["-nPw", "-a", "-p", &list, "-d", "cwd", "-Fn"])
                .stdin(Stdio::null()),
        )
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
    };
    let cwds = cwds(&lsof);
    for p in &mut procs {
        if p.cwd.is_none() {
            p.cwd = cwds.get(&p.pid).cloned();
        }
    }
    Ok(rows(codex, &procs))
}

/// Parse `TZ=UTC ps -axww -o pid=,lstart=,command=`; `cwd` is filled from lsof.
pub fn processes(ps: &str) -> Vec<Process> {
    crate::harness::spec(crate::config::HarnessKind::Codex)
        .discovery
        .processes(ps)
        .into_iter()
        .map(|line| {
            let (remote, thread, prompt) = client_options(line.command.split_whitespace().skip(1));
            Process {
                pid: line.pid,
                started: line.started,
                cwd: None,
                thread,
                remote,
                prompt,
            }
        })
        .collect()
}

/// Stop parsing options at the prompt so its text cannot identify a client or thread, and
/// return the prompt itself: until the daemon makes the thread, it is all the row can say.
fn client_options<'a>(
    mut args: impl Iterator<Item = &'a str>,
) -> (bool, Option<String>, Option<String>) {
    /// `ps` keeps a multi-line prompt on one line and spells its breaks out: a newline prints as
    /// the four characters `\012` and a carriage return as `^M`. Words either side of a break are
    /// not neighbours, so take the first line and say that it was cut rather than glue the halves.
    fn rest<'a>(words: impl Iterator<Item = &'a str>) -> Option<String> {
        let text = words.collect::<Vec<_>>().join(" ");
        let cut = ["\\012", "^M"].iter().filter_map(|b| text.find(b)).min();
        let first = cut.map_or(text.as_str(), |i| text[..i].trim_end());
        (!first.is_empty()).then(|| match cut {
            Some(_) => format!("{first}…"),
            None => first.to_owned(),
        })
    }
    /// A thread id as Codex prints it: 36 characters of hex and dashes.
    fn is_thread(arg: &str) -> bool {
        arg.len() == 36 && arg.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit())
    }
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
            // A separator does not hide the thread: `resume -- <id>` is how this dashboard joins one.
            "--" => {
                let next = args.next();
                return match next.filter(|a| resume && is_thread(a)) {
                    Some(id) => (remote, Some(id.to_owned()), rest(args)),
                    None => (remote, None, rest(next.into_iter().chain(args))),
                };
            }
            "resume" if !resume => resume = true,
            _ if arg.starts_with('-') => {}
            _ => {
                let thread = (resume && is_thread(arg)).then(|| arg.to_owned());
                // A resumed thread names itself first; anything after it is the prompt.
                let prompt = if thread.is_some() {
                    rest(args)
                } else {
                    rest(std::iter::once(arg).chain(args))
                };
                return (remote, thread, prompt);
            }
        }
    }
    (remote, None, None)
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
        forked_from: p["forked_from_id"].as_str().map(str::to_owned),
        cwd: PathBuf::from(p["cwd"].as_str()?),
        started: DateTime::parse_from_rfc3339(p["timestamp"].as_str()?)
            .ok()?
            .into(),
    })
}

pub fn assistant_texts(v: &Value) -> impl Iterator<Item = &str> {
    crate::harness::spec(crate::config::HarnessKind::Codex)
        .transcript
        .messages
        .assistant
        .parts(v, false)
        .into_iter()
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
        self.fold_priced(lines, None);
    }

    pub(crate) fn fold_priced(&mut self, lines: &str, catalog: Option<&crate::cost::Catalog>) {
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
            t.accounting.observe(&v, catalog);
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
            if let Some(first) = crate::harness::spec(crate::config::HarnessKind::Codex)
                .transcript
                .messages
                .assistant
                .headline(&v)
            {
                t.last = Some(first);
            }
            if v["type"] == "turn_context" {
                if let Some(model) = v["payload"]["model"].as_str() {
                    t.model = Some(model.to_owned());
                }
                t.next_model = None;
                if let Some(effort) = v["payload"]["effort"].as_str() {
                    t.effort = Some(effort.to_owned());
                }
            }
            // `/model` mid-turn applies from the next turn; the running turn keeps its model.
            if payload["type"] == "thread_settings_applied" {
                t.next_model = payload["thread_settings"]["model"]
                    .as_str()
                    .filter(|m| Some(*m) != t.model.as_deref())
                    .map(str::to_owned);
            }
            if let Some(state) = crate::harness::spec(crate::config::HarnessKind::Codex)
                .state
                .read(&v)
            {
                t.state = Some(state);
            }
            if v["type"] == "event_msg" && payload["type"] == "token_count" {
                let info = &payload["info"];
                let total = &info["total_token_usage"];
                t.tokens_in = total["input_tokens"].as_u64().or(t.tokens_in);
                t.tokens_out = total["output_tokens"].as_u64().or(t.tokens_out);
                t.context_tokens = info["last_token_usage"]["total_tokens"]
                    .as_u64()
                    .or(t.context_tokens);
                t.context_window = info["model_context_window"].as_u64().or(t.context_window);
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

/// What the index was read from, so an unchanged home is not read again: the index file, the
/// state database, and the database's write-ahead log, which is where a running daemon's
/// newest threads sit until it checkpoints. A file that is absent is part of the answer too.
type Sources = Vec<Option<(std::time::SystemTime, u64)>>;

fn sources(codex: &Path) -> Sources {
    let stat = |p: PathBuf| {
        let m = fs::metadata(p).ok()?;
        Some((m.modified().ok()?, m.len()))
    };
    let db = state_db(codex);
    vec![
        stat(codex.join("session_index.jsonl")),
        db.clone().and_then(stat),
        db.map(|p| p.with_extension("sqlite-wal")).and_then(stat),
    ]
}

/// The newest state database in a home, which is the one its daemon writes.
fn state_db(codex: &Path) -> Option<PathBuf> {
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
    dbs.pop()
}

/// The index for a home, read again only when the home's own sources changed.
///
/// Live rows and saved-thread rows both need it, and several homes are read in one refresh,
/// so a pass would otherwise open the same database two or three times. The rule is the
/// sources, not elapsed time: an appending daemon moves its database or its log, and a home
/// nobody has touched is not reparsed at the refresh cadence.
pub(crate) fn shared_index(codex: &Path) -> Arc<Index> {
    type Held = HashMap<PathBuf, (Sources, Arc<Index>)>;
    static HELD: Mutex<Option<Held>> = Mutex::new(None);
    let now = sources(codex);
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    let held = held.get_or_insert_with(HashMap::new);
    if let Some((was, index)) = held.get(codex)
        && *was == now
    {
        crate::observe::shared(crate::observe::op::SQLITE);
        return index.clone();
    }
    let index = Arc::new(index(codex));
    // Homes come from configuration and a fixture's home is gone once its test ends; the map
    // is bounded by how many a machine has, and a run of fixtures cannot grow it without end.
    if held.len() >= 64 {
        held.clear();
    }
    held.insert(codex.to_owned(), (now, index.clone()));
    index
}

pub fn index(codex: &Path) -> Index {
    let mut out = Index {
        titles: fs::read_to_string(codex.join("session_index.jsonl"))
            .map(|t| titles(&t))
            .unwrap_or_default(),
        threads: HashMap::new(),
    };
    let Some(db) = state_db(codex) else {
        return out;
    };
    let Ok(rows) = crate::sqlite::query(
        db.as_os_str(),
        "select id, coalesce(name, title) as t, rollout_path, cwd from threads",
    ) else {
        return out;
    };
    for v in rows {
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
    let files: Vec<PathBuf> = fs::read_dir(codex.join(&daemon_files().locks))
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
    let lsof = crate::observe::spawn(
        crate::observe::op::OPEN_FILES,
        Command::new("/usr/sbin/lsof")
            .args(["-nPw", "-a", "-p", &pids, "-Fpn"])
            .args(files)
            .stdin(Stdio::null()),
    )
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
    let pid = codex.join(&daemon_files().pid);
    // A standalone install names it app-server.pid; Codex's managed daemon package, which
    // other installs download on `daemon start`, names it daemon.pid in the same folder.
    let text = fs::read_to_string(&pid)
        .or_else(|_| fs::read_to_string(pid.with_file_name("daemon.pid")))
        .ok()?;
    let pid = serde_json::from_str::<Value>(&text).ok()?["pid"].as_u64()? as u32;
    // Signal 0 through the kernel, not `/bin/kill`: liveness is read once per pass per home.
    crate::observe::read(
        crate::observe::op::LIVENESS,
        || crate::fleet::alive(pid),
        |_| true,
    )
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
    let mut stack = vec![
        crate::harness::spec(crate::config::HarnessKind::Codex)
            .transcript
            .live_path(codex),
    ];
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
    let index = shared_index(codex);
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
    // A remote client is a viewer, so `thread_rows` supplies the row of the thread it opened and the
    // viewer gets none: its own start time is not the thread's. A named thread with a rollout is that
    // row whether the daemon still holds the writer lock or released it, because the daemon drops a
    // lock minutes after a thread goes quiet while the viewer stays open for hours: reading a released
    // lock as "no thread yet" turns that viewer into a session of its own long after the work ended.
    // Until the daemon makes the thread there is nothing to defer to, so a client that named none, or
    // named one with no rollout, keeps a row rather than leave the fleet a gap. Only a thread this
    // client could have opened counts, by folder and by starting no earlier than the client; a client
    // that resumes an older thread without naming it is rare enough to show twice.
    let names_thread = |p: &Process| {
        p.thread
            .as_deref()
            .is_some_and(|id| rollout_for(codex, &index, id).is_some())
    };
    let viewable: Vec<Meta> = locks
        .iter()
        .filter(|(_, pid)| Some(**pid) == daemon)
        .filter_map(|(id, _)| meta_of(&rollout_for(codex, &index, id)?))
        .collect();
    let mut out: Vec<Session> = procs
        .iter()
        .filter(|p| {
            !(p.remote
                && (names_thread(p)
                    || viewable.iter().any(|m| {
                        Some(m.cwd.as_path()) == p.cwd.as_deref() && m.started >= p.started
                    })))
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
            let (cost_usd, cost_info) = t.accounting.report(None);
            Session {
                title: index
                    .titles
                    .get(&id)
                    .cloned()
                    .or_else(|| rollout.and_then(|(path, _)| prompt_of(path)))
                    .or_else(|| p.prompt.clone()),
                session_id: id,
                harness: "codex".into(),
                kind,
                cwd: p.cwd.clone().unwrap_or_default(),
                state: t.state.unwrap_or("-").into(),
                last_activity: t.last_activity,
                next_model: t.next_model,
                model: t.model,
                effort: t.effort,
                usage: None,
                started: Some(p.started),
                pid: Some(p.pid),
                transcript_path: rollout.map(|(path, _)| path.clone()),
                tokens_in: t.tokens_in,
                tokens_out: t.tokens_out,
                context_tokens: t.context_tokens,
                context_window: t.context_window,
                cost_usd,
                cost_info,
                last: t.last,
                coordinator: false,
                forked_from: rollout.and_then(|(_, meta)| meta.forked_from.clone()),
                activity: t.activity,
                moved_to: None,
                native_id: None,
            }
        })
        .collect();
    out.sort_by_key(|s| s.started);
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert(s.session_id.clone()));
    runtime::apply(codex, &mut out);
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

/// List daemon-held threads and saved launches without live client rows. Omit threads whose
/// rollout is missing, and threads in `removed`, which the dashboard forgot. Two sources produce a
/// row and forgetting drops only the record, so without `removed` the daemon's lock, which it keeps
/// for minutes after the client goes, hands the row back on the next start. A saved record is the
/// un-forget: `remember` writes one after a reported turn, so a resumed thread returns by itself.
pub fn thread_rows(
    codex: &Path,
    state: &Path,
    live: &[Session],
    removed: &BTreeSet<String>,
) -> Vec<Session> {
    thread_rows_observed(codex, state, live, removed, |_, _| {})
}

pub(crate) fn thread_rows_observed(
    codex: &Path,
    state: &Path,
    live: &[Session],
    removed: &BTreeSet<String>,
    mut source: impl FnMut(&str, &str),
) -> Vec<Session> {
    let index = shared_index(codex);
    let daemon = daemon_pid(codex);
    let mut ids: Vec<(String, Option<Thread>)> =
        locks(codex, &daemon.into_iter().collect::<Vec<_>>())
            .into_iter()
            .filter(|(_, pid)| Some(*pid) == daemon)
            .filter(|(id, _)| !removed.contains(id))
            .map(|(id, _)| (id, None))
            .collect();
    for t in threads(state) {
        if !ids.iter().any(|(id, _)| *id == t.id) {
            ids.push((t.id.clone(), Some(t)));
        }
    }
    ids.sort_by(|a, b| a.0.cmp(&b.0));
    // A daemon-held thread has no process of its own: the daemon runs it, so the kernel charges
    // its work there. Threads the same daemon holds all report that one process.
    let held = daemon
        .filter(|_| ids.iter().any(|(_, record)| record.is_none()))
        .and_then(|pid| crate::fleet::usage(std::iter::once(pid)).remove(&pid));
    let mut rows: Vec<_> = ids
        .into_iter()
        .filter(|(id, _)| !live.iter().any(|s| s.session_id == *id))
        .filter_map(|(id, record)| {
            let rollout = record
                .as_ref()
                .map(|t| t.rollout.clone())
                .filter(|p| p.is_file())
                .or_else(|| rollout_for(codex, &index, &id))?;
            let meta = meta_of(&rollout);
            let tail = tail_of(&rollout);
            source(
                &id,
                if record.is_some() {
                    "saved_launch"
                } else {
                    "daemon_lock"
                },
            );
            let (cost_usd, cost_info) = tail.accounting.report(None);
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
                next_model: tail.next_model,
                model: tail.model,
                effort: tail.effort,
                // A saved launch the daemon no longer holds is detached, and reports no process.
                usage: record.is_none().then_some(held).flatten(),
                started: meta
                    .as_ref()
                    .map(|m| m.started)
                    .or_else(|| record.as_ref().map(|t| t.started)),
                pid: None,
                transcript_path: Some(rollout),
                tokens_in: tail.tokens_in,
                tokens_out: tail.tokens_out,
                context_tokens: tail.context_tokens,
                context_window: tail.context_window,
                cost_usd,
                cost_info,
                last: tail.last,
                coordinator: false,
                forked_from: meta.and_then(|m| m.forked_from),
                activity: tail.activity,
                moved_to: None,
                native_id: None,
                session_id: id,
            })
        })
        .collect();
    runtime::apply(codex, &mut rows);
    rows
}

/// Read rollout headers only from files modified since the earliest process start.
fn rollouts(codex: &Path, since: DateTime<Utc>) -> Vec<(PathBuf, Meta)> {
    let mut out = Vec::new();
    let mut stack = vec![
        crate::harness::spec(crate::config::HarnessKind::Codex)
            .transcript
            .live_path(codex),
    ];
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
pub(crate) fn prompt_of(path: &Path) -> Option<String> {
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
    crate::harness::spec(crate::config::HarnessKind::Codex)
        .transcript
        .messages
        .user
        .headline(&v)
}

/// The UI's actual user message, excluding model-history instructions and skills.
pub fn user_texts(v: &Value) -> impl Iterator<Item = &str> {
    crate::harness::spec(crate::config::HarnessKind::Codex)
        .transcript
        .messages
        .user
        .parts(v, false)
        .into_iter()
}

/// Fold appended bytes after the first full read, retaining earlier state.
/// Leave a partial final line for the next read.
fn tail_of(path: &Path) -> Tail {
    let catalog = crate::cost::snapshot();
    tail_of_priced(path, catalog.as_deref())
}

fn tail_of_priced(path: &Path, catalog: Option<&crate::cost::Catalog>) -> Tail {
    type CachedTail = (u64, Tail, Option<crate::cost::CatalogStamp>);
    static CACHE: Mutex<Option<HashMap<PathBuf, CachedTail>>> = Mutex::new(None);
    let stamp = catalog.map(|c| c.stamp.clone());
    let len = fs::metadata(path).map_or(0, |m| m.len());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    let (seen, t, priced_with) = cache.entry(path.to_owned()).or_default();
    if *seen > len || *priced_with != stamp {
        // Truncated or replaced: start over.
        (*seen, *t) = (0, Tail::default());
        *priced_with = stamp;
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
        t.fold_priced(&String::from_utf8_lossy(&data[..=end]), catalog);
        *seen += end as u64 + 1;
    }
    t.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cost_header(model: &str) -> String {
        format!(
            "{}\n{}\n",
            serde_json::json!({"type":"session_meta","payload":{"model_provider":"provider"}}),
            serde_json::json!({"type":"turn_context","payload":{"model":model}})
        )
    }

    fn cost_event(total: [u64; 4], last: [u64; 4]) -> String {
        let usage = |n: [u64; 4]| {
            serde_json::json!({
                "input_tokens":n[0], "cached_input_tokens":n[1],
                "cache_write_input_tokens":n[2], "output_tokens":n[3],
                "reasoning_output_tokens": n[3].saturating_sub(1),
            })
        };
        format!(
            "{}\n",
            serde_json::json!({
                "type":"event_msg", "payload":{"type":"token_count","info":{
                    "total_token_usage":usage(total), "last_token_usage":usage(last)
                }}
            })
        )
    }

    #[test]
    fn cost_counts_each_response_once_at_its_reported_model_and_cache_rates() {
        let catalog = crate::cost::tests::fixture();
        let first = cost_event([100, 40, 30, 10], [100, 40, 30, 10]);
        let mut tail = Tail::default();
        tail.fold_priced(&(cost_header("model") + &first + &first), Some(&catalog));
        let (usd, info) = tail.accounting.report(None);
        assert!((usd.unwrap() - 0.00025).abs() < 1e-12);
        assert_eq!(
            info.unwrap().priced_records,
            1,
            "duplicate updates do not spend twice"
        );
        let second = cost_event([120, 50, 30, 12], [20, 10, 0, 2]);
        tail.fold_priced(&(cost_header("other_model") + &second), Some(&catalog));
        let (usd, info) = tail.accounting.report(None);
        assert!(
            (usd.unwrap() - 0.0002665).abs() < 1e-12,
            "price the first request at its own model"
        );
        let info = info.unwrap();
        assert_eq!(info.coverage, crate::cost::Coverage::Complete);
        assert_eq!(info.priced_records, 2);
        assert_eq!(info.catalog, Some(catalog.stamp.clone()));
        assert!(crate::cost::display(usd, Some(&info)).starts_with("~$"));
    }

    #[test]
    fn cost_missing_requests_models_tiers_and_counters_never_look_complete() {
        let catalog = crate::cost::tests::fixture();
        let good = cost_event([100, 40, 30, 10], [100, 40, 30, 10]);
        let mut tail = Tail::default();
        tail.fold_priced(&(cost_header("model") + &good), Some(&catalog));
        tail.fold_priced(
            &(cost_header("not-in-catalog") + &cost_event([200, 80, 60, 20], [100, 40, 30, 10])),
            Some(&catalog),
        );
        let (usd, info) = tail.accounting.report(None);
        assert_eq!(
            info.as_ref().unwrap().coverage,
            crate::cost::Coverage::Partial
        );
        assert_eq!(crate::cost::display(usd, info.as_ref()), "~$0.0003");

        for (header, event, reason) in [
            (
                cost_header("model"),
                cost_event([300, 120, 90, 30], [100, 40, 30, 10]),
                "unobserved_usage",
            ),
            (
                cost_header("model"),
                cost_event([10, 11, 0, 1], [10, 11, 0, 1]),
                "invalid_usage",
            ),
            (
                format!(
                    "{}{}\n",
                    cost_header("model"),
                    serde_json::json!({"type":"turn_context","payload":{"model":"model","service_tier":"priority"}})
                ),
                good.clone(),
                "unsupported_service_tier",
            ),
            (
                cost_header("model").replace("\"provider\"", "\"amazon-bedrock\""),
                good.replace("\"cache_write_input_tokens\":30,", ""),
                "missing_counters",
            ),
        ] {
            let mut tail = Tail::default();
            tail.fold_priced(&(header + &event), Some(&catalog));
            let (_, info) = tail.accounting.report(None);
            assert!(
                info.as_ref().unwrap().unpriced_reasons.contains_key(reason),
                "{reason}: {info:?}"
            );
            assert_ne!(info.unwrap().coverage, crate::cost::Coverage::Complete);
        }
    }

    #[test]
    fn cost_replays_a_cached_rollout_when_a_catalog_becomes_available() {
        let catalog = crate::cost::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let text = cost_header("model") + &cost_event([101, 40, 30, 10], [101, 40, 30, 10]);
        fs::write(&path, text).unwrap();
        let absent = tail_of_priced(&path, None).accounting.report(None);
        assert!(absent.0.is_none());
        let priced = tail_of_priced(&path, Some(&catalog))
            .accounting
            .report(None);
        assert!((priced.0.unwrap() - 0.000464).abs() < 1e-12);
        assert_eq!(
            priced,
            tail_of_priced(&path, Some(&catalog))
                .accounting
                .report(None)
        );
        assert!(
            tail_of_priced(&path, None)
                .accounting
                .report(None)
                .0
                .is_none()
        );
    }

    #[test]
    fn a_version_or_help_probe_is_not_a_session() {
        // `codex app-server daemon start` runs `codex --version` on the binary it is about to use.
        let ps = "  1 Wed Sep 23 05:31:47 2026 /Users/u/.codex/packages/standalone/current/bin/codex --version\n  2 Wed Sep 23 05:31:47 2026 codex -V\n  3 Wed Sep 23 05:31:47 2026 codex --help\n  4 Wed Sep 23 05:31:47 2026 codex -h\n  5 Wed Sep 23 05:31:47 2026 codex help\n  6 Wed Sep 23 05:31:47 2026 codex fix the parser\n";
        let pids: Vec<u32> = processes(ps).iter().map(|p| p.pid).collect();
        assert_eq!(pids, [6]);
    }

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
        assert_eq!(
            client_options(prompt.split_whitespace()),
            (
                true,
                None,
                Some("explain resume 01a0a430-b8d1-7682-a7ed-51904a118c65".into())
            ),
            "a prompt that says resume is still the prompt"
        );
        for args in [
            "--remote unix:///s.sock -C /repo -- fix this",
            "--remote unix:///s.sock -C /repo fix this",
        ] {
            assert_eq!(
                client_options(args.split_whitespace()).2,
                Some("fix this".into()),
                "{args}"
            );
        }
        assert_eq!(
            client_options(
                "--remote unix:///s.sock resume 01a0a430-b8d1-7682-a7ed-51904a118c65 fix this"
                    .split_whitespace()
            ),
            (
                true,
                Some("01a0a430-b8d1-7682-a7ed-51904a118c65".into()),
                Some("fix this".into())
            ),
            "a resumed thread names itself before its prompt"
        );
        for args in ["--remote unix:///s.sock", "--remote unix:///s.sock --"] {
            assert_eq!(client_options(args.split_whitespace()).2, None, "{args}");
        }
        // `ps` prints a newline inside the prompt as `\012` and a carriage return as `^M`.
        for args in [
            "--remote unix:///s.sock -- fix this\\012then explain it",
            "--remote unix:///s.sock -- fix this^Mthen explain it",
            "--remote unix:///s.sock -- fix this \\012 then explain it",
        ] {
            assert_eq!(
                client_options(args.split_whitespace()).2,
                Some("fix this…".into()),
                "a line break is not a space: {args}"
            );
        }
        assert_eq!(
            client_options("--remote unix:///s.sock -- \\012fix this".split_whitespace()).2,
            None,
            "a prompt whose first line is empty says nothing"
        );
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
    fn a_thread_reports_the_effort_of_its_latest_turn_context() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("rollout.jsonl");
        let context = |effort: &str| {
            format!(
                r#"{{"timestamp":"2026-09-15T08:31:22.334Z","type":"turn_context","payload":{{"model":"openai.gpt-6-astra","effort":"{effort}"}}}}"#
            )
        };
        fs::write(&path, format!("{}\n", context("medium"))).unwrap();
        assert_eq!(tail_of(&path).effort.as_deref(), Some("medium"));
        fs::write(
            &path,
            format!("{}\n{}\n", context("medium"), context("xhigh")),
        )
        .unwrap();
        let t = tail_of(&path);
        assert_eq!(t.effort.as_deref(), Some("xhigh"), "the latest turn wins");
        assert_eq!(t.model.as_deref(), Some("openai.gpt-6-astra"));
        fs::write(
            &path,
            "{\"timestamp\":\"2026-09-15T08:31:22.334Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"openai.gpt-6-astra\"}}\n",
        )
        .unwrap();
        assert_eq!(
            tail_of(&path).effort,
            None,
            "a turn that reports no effort reports none"
        );
    }

    #[test]
    fn a_mid_turn_model_change_shows_pending_until_the_next_turn() {
        let context = |model: &str| {
            format!(
                "{}\n",
                serde_json::json!({"type":"turn_context","payload":{"model":model}})
            )
        };
        let settings = |model: &str| {
            format!(
                "{}\n",
                serde_json::json!({"type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":{"model":model}}})
            )
        };
        // The row label, and the JSON a `cones ls --json` consumer reads.
        let shown = |t: &Tail| {
            let s: crate::fleet::Session = serde_json::from_value(serde_json::json!({
                "session_id": "x", "cwd": "/x", "state": "active",
                "model": t.model, "next_model": t.next_model,
            }))
            .unwrap();
            let json = serde_json::to_value(&s).unwrap();
            (
                crate::fleet::row_model(&s),
                json["model"].clone(),
                json.get("next_model").cloned(),
            )
        };
        let mut t = Tail::default();
        t.fold(&(context("openai.gpt-6-astra") + &settings("openai.gpt-6-astra")));
        assert_eq!(
            shown(&t),
            ("GPT-6 Astra".into(), "openai.gpt-6-astra".into(), None),
            "settings naming the running model are not a change"
        );
        t.fold(&settings("openai.gpt-5.6-sol"));
        assert_eq!(
            shown(&t),
            (
                "GPT-6 Astra → 5.6 Sol".into(),
                "openai.gpt-6-astra".into(),
                Some("openai.gpt-5.6-sol".into())
            ),
            "the running turn keeps its model id; the change is pending"
        );
        t.fold(&settings("openai.gpt-6-astra"));
        assert_eq!(
            shown(&t),
            ("GPT-6 Astra".into(), "openai.gpt-6-astra".into(), None),
            "switching back cancels the change"
        );
        t.fold(&(settings("openai.gpt-5.6-sol") + &context("openai.gpt-5.6-sol")));
        assert_eq!(
            shown(&t),
            ("GPT-5.6 Sol".into(), "openai.gpt-5.6-sol".into(), None),
            "the next turn runs the new model"
        );
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
    fn a_managed_daemon_install_names_its_pid_file_daemon_pid() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("app-server-daemon")).unwrap();
        assert_eq!(daemon_pid(d.path()), None);
        fs::write(
            d.path().join("app-server-daemon/daemon.pid"),
            serde_json::json!({"pid": std::process::id()}).to_string(),
        )
        .unwrap();
        assert_eq!(daemon_pid(d.path()), Some(std::process::id()));
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
            live.extend(thread_rows(&home, &state, &live, &BTreeSet::new()));
            live.sort_by(|a, b| a.session_id.cmp(&b.session_id));
            live
        };
        assert_eq!(
            fleet(&procs)
                .iter()
                .map(|s| (s.session_id.as_str(), s.title.as_deref()))
                .collect::<Vec<_>>(),
            [
                ("codex-7", Some("first prompt")),
                ("codex-8", Some("second prompt"))
            ],
            "a client whose thread the daemon has not created yet is still a session, and says              what it was asked"
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
        assert!(
            live[0].usage.is_some_and(|u| u.rss > 0) && live[0].usage == live[1].usage,
            "threads one daemon holds report that daemon's process, the one the kernel charges"
        );
        assert_eq!(
            live.iter().map(|s| s.started).collect::<Vec<_>>(),
            fleet(&[]).iter().map(|s| s.started).collect::<Vec<_>>(),
            "closing the viewer leaves the thread's row and start time unchanged"
        );
        let mut viewers = processes(&format!(
            "11 Sun Sep 13 10:05:00 2026 codex --remote unix:///s.sock resume -- {A}\n\
             12 Sun Sep 13 10:05:01 2026 codex --remote unix:///s.sock resume -- {B}\n"
        ));
        assert_eq!(
            viewers
                .iter()
                .map(|p| p.thread.as_deref())
                .collect::<Vec<_>>(),
            [Some(A), Some(B)],
            "a separator does not hide the thread a peek names"
        );
        for p in &mut viewers {
            p.cwd = Some(work.clone());
        }
        let ids = |list: &[Session]| {
            list.iter()
                .map(|s| (s.session_id.clone(), s.started))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&fleet(&viewers)),
            ids(&fleet(&[])),
            "peeking threads adds no viewer rows and leaves their start times alone"
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
        assert!(
            fleet(&viewers).is_empty(),
            "a released writer lock leaves the viewers viewers: a peek left open outliving the                 daemon's hold on its thread is not a session of its own"
        );
        assert!(fleet(&[]).is_empty(), "the daemon released both threads");
        let unmade = Process {
            pid: 13,
            thread: Some("01a0a432-0000-7000-8000-000000000000".into()),
            ..viewers[0].clone()
        };
        assert_eq!(
            rows(&home, &[unmade])
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["codex-13"],
            "a thread with no rollout has no row to defer to, so its client keeps one"
        );
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
            thread_rows(&home, &state, &[], &BTreeSet::new())[0]
                .title
                .as_deref(),
            Some("fix the flaky test"),
            "unnamed thread shows its first prompt, not AGENTS.md"
        );
        fs::write(
            home.join("session_index.jsonl"),
            r#"{"id":"dddd","thread_name":"fix the build","updated_at":"x"}"#,
        )
        .unwrap();
        {
            let db = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
            db.execute_batch(
                "create table threads(id text, name text, title text, rollout_path text, cwd text);
                 insert into threads values('dddd', null, 'fix the build', '', ''),
                                           ('eeee', 'Green CI', 'x', '', '');",
            )
            .unwrap();
            assert_eq!(
                index(&home).titles.get("eeee").map(String::as_str),
                Some("Green CI"),
                "the state database names a thread the index file does not"
            );
        }
        let rows = thread_rows(&home, &state, &[], &BTreeSet::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind.as_deref(), Some("daemon"));
        assert_eq!(
            rows[0].usage, None,
            "a saved launch no daemon holds has no process to report"
        );
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
            thread_rows(&home, &state, &live, &BTreeSet::new()).is_empty(),
            "an attached client's row wins"
        );
        forget(&state, "dddd").unwrap();
        assert!(threads(&state).is_empty());
        assert!(thread_rows(&home, &state, &[], &BTreeSet::new()).is_empty());
    }
}
