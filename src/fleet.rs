//! Fleet discovery from harness-owned registries and transcripts. Missing reports
//! remain absent; state and usage are never estimated. See docs/harness.md.
use crate::cost::{Adapter, Reader, Reading, Response};
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
    /// Native kind: Claude `bg`/`interactive`, or Codex `daemon`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub cwd: PathBuf,
    /// Normalized harness state; see `state` and docs/harness.md.
    pub state: String,
    /// First reported timestamp; rows sort by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<DateTime<Utc>>,
    /// The `timestamp` of the last transcript line that carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<DateTime<Utc>>,
    /// Model id reported by the harness, verbatim.
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
    /// Latest reported prompt size, including cache reads and creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// Reported window size. Claude requires a saved statusLine payload; see docs/harness.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_info: Option<crate::cost::Info>,
    /// Reasoning effort as the harness reports it, verbatim; see docs/harness.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Kernel-reported process usage, read once per refresh; excluded from JSON output.
    #[serde(skip)]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    /// Matched by pid and cwd against the coordinator skill's status file, never by title.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub coordinator: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// Timestamped activity for sparklines; excluded from JSON output.
    #[serde(skip)]
    pub activity: Vec<Activity>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Activity {
    pub at: DateTime<Utc>,
    pub messages: u64,
    pub tools: u64,
    pub tokens_out: u64,
}

impl Activity {
    pub fn at(at: DateTime<Utc>) -> Self {
        Self {
            at,
            ..Self::default()
        }
    }
}

/// Clock-aligned buckets, oldest first, so reloads do not shift their boundaries.
/// Future timestamps count in the newest bucket.
pub fn buckets(
    activity: &[Activity],
    spark: &crate::config::Activity,
    now: DateTime<Utc>,
) -> Vec<u64> {
    let secs = spark.bucket_seconds().unwrap_or(60) as i64;
    let edge = now.timestamp() - now.timestamp().rem_euclid(secs) + secs - 1;
    let mut out = vec![0; spark.bars];
    for a in activity {
        let ago = (edge - a.at.timestamp()).max(0);
        let i = (ago / secs) as usize;
        if i >= spark.bars {
            continue;
        }
        out[spark.bars - 1 - i] += match spark.metric.as_str() {
            "messages" => a.messages,
            "tools" => a.tools,
            "tokens" => a.tokens_out,
            _ => 1,
        };
    }
    out
}

pub fn sparklines(
    sessions: &[Session],
    spark: &crate::config::Activity,
    now: DateTime<Utc>,
) -> HashMap<String, String> {
    let all: Vec<(&Session, Vec<u64>)> = sessions
        .iter()
        .map(|s| (s, buckets(&s.activity, spark, now)))
        .collect();
    let fleet = all
        .iter()
        .flat_map(|(_, b)| b.iter().copied())
        .max()
        .unwrap_or(0);
    all.iter()
        .map(|(s, b)| {
            let (values, bound): (Vec<f64>, f64) = match spark.bound.as_str() {
                "row" => (
                    b.iter().map(|v| *v as f64).collect(),
                    b.iter().copied().max().unwrap_or(0) as f64,
                ),
                "log" => (
                    b.iter().map(|v| (*v as f64).ln_1p()).collect(),
                    (fleet as f64).ln_1p(),
                ),
                "fleet" => (b.iter().map(|v| *v as f64).collect(), fleet as f64),
                _ => (
                    b.iter().map(|v| *v as f64).collect(),
                    spark.fixed_bound().unwrap_or(1.0),
                ),
            };
            (s.session_id.clone(), bars(&values, bound))
        })
        .collect()
}

/// Use ▇ for full bars because █ touches the row above.
pub fn bars(values: &[f64], bound: f64) -> String {
    const BARS: [char; 7] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇'];
    values
        .iter()
        .map(|v| {
            if *v <= 0.0 || bound <= 0.0 {
                BARS[0]
            } else {
                BARS[((v / bound).min(1.0) * 6.999) as usize]
            }
        })
        .collect()
}

impl Session {
    /// Only Claude background sessions and Codex daemon threads are joinable.
    pub fn own_terminal(&self) -> bool {
        crate::harness::by_name(&self.harness).is_none_or(|spec| {
            spec.session(self.kind.as_deref()).join == crate::harness::spec::Join::Unavailable
        })
    }
}
fn claude() -> String {
    "claude".into()
}

/// Claude's home directory name under the user's home, which also says where a process with
/// no override of its own keeps every harness home beside it.
pub const CLAUDE_DIR: &str = ".claude";

pub fn claude_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(
        &crate::harness::spec(crate::config::HarnessKind::Claude)
            .home
            .env,
    )
    .filter(|d| !d.is_empty())
    {
        return Ok(PathBuf::from(dir));
    }
    Ok(dirs::home_dir()
        .context("missing home directory")?
        .join(CLAUDE_DIR))
}

/// Live registry sessions, oldest first. Skip malformed entries and mismatched pid/start pairs.
pub fn sessions(claude: &Path) -> Result<Vec<Session>> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(
        claude.join(
            crate::harness::spec(crate::config::HarnessKind::Claude)
                .discovery
                .registry
                .as_ref()
                .expect("Claude registry"),
        ),
    ) else {
        return Ok(out);
    };
    let values: Vec<Value> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| fs::read(p).ok())
        .filter_map(|b| serde_json::from_slice(&b).ok())
        .collect();
    let starts = process_starts(values.iter().filter_map(|v| v["pid"].as_u64()))?;
    let coordinators = coordinators(claude);
    out.extend(
        values
            .iter()
            .filter_map(|v| session(claude, v, &starts, true))
            .map(|mut s| {
                s.coordinator = s
                    .pid
                    .is_some_and(|p| coordinators.contains(&(p, s.cwd.clone())));
                s
            }),
    );
    sort(&mut out);
    Ok(out)
}

/// Match coordinator records by both pid and folder.
fn coordinators(claude: &Path) -> HashSet<(u32, PathBuf)> {
    fs::read_dir(claude.join("orchestrator"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let v: Value = serde_json::from_slice(&fs::read(e.path()).ok()?).ok()?;
            Some((v["pid"].as_u64()? as u32, PathBuf::from(v["cwd"].as_str()?)))
        })
        .collect()
}

/// The whole process table, which is how Codex and pi clients are discovered at all.
///
/// An unreadable table is an error, never an empty one: an empty table means the harness has
/// no client running, so swallowing the failure drops every native row for that read and the
/// rows flicker back on the next one. `ps` is a parameter so a test can point it at one that
/// cannot run.
pub fn process_table(ps: &str) -> Result<String> {
    let out = Command::new(ps)
        .env("TZ", "UTC")
        // lstart is locale text: under a non-English LANG ps prints its own month names,
        // which never match the start a harness recorded, so every live session is dropped.
        .env("LC_ALL", "C")
        .args(["-axww", "-o", "pid=,lstart=,command="])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("reading the process table with {ps}"))?;
    // Listing every process has no empty case, so a nonzero exit is a failure like any other.
    ensure!(
        out.status.success(),
        "{ps} could not list the process table: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The pids among `pids` running against a home this fleet reads. Process discovery names a
/// harness by its program alone, so a client started against another home, such as a capture
/// fixture's, otherwise arrives as a row with a command line and nothing else: its state, model
/// and activity live in records under a home cones never opens.
///
/// Two `ps` reads cover the listed pids alone, because the whole table with environments is
/// several times larger and every refresh would read it. macOS prints no environment for a
/// platform binary or another user's process, and a pid can go between the reads: an unreadable
/// environment keeps the pid, because a hidden environment must never empty the fleet.
///
/// This still finds the machine's real clients, so a test that asserts a whole row set must
/// exclude ids it did not create: a live harness process fails such a test for reasons that have
/// nothing to do with the change under test, and no environment override hides it.
pub fn own_home_processes(
    ps: &str,
    kind: crate::config::HarnessKind,
    pids: &[u32],
) -> HashSet<u32> {
    let home = &crate::harness::spec(kind).home;
    let Ok(read) = claude_dir().map(|dir| home.all(&dir)) else {
        return pids.iter().copied().collect();
    };
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let argv = process_column(ps, &list, "-wwp");
    let with_environment = process_column(ps, &list, "-wwEp");
    pids.iter()
        .copied()
        .filter(|pid| {
            with_environment
                .get(pid)
                .zip(argv.get(pid))
                .and_then(|(full, argv)| environment(full, argv))
                .and_then(|env| home.of_process(env))
                .is_none_or(|used| read.contains(&used))
        })
        .collect()
}

/// `ps` prints the environment after the command line with no separator of its own, so the
/// command line read without `-E` is what says where it ends. Reading the environment from the
/// right of the whole line would let a prompt that names a home variable claim another home.
fn environment<'a>(with_environment: &'a str, argv: &str) -> Option<&'a str> {
    with_environment
        .strip_prefix(argv)
        .filter(|e| !e.is_empty())
}

/// pid to the printed column for the listed pids. `ps` exits nonzero when one pid has gone;
/// the others are still printed, and a missing pid decides nothing.
fn process_column(ps: &str, list: &str, flags: &str) -> HashMap<u32, String> {
    if list.is_empty() {
        return HashMap::new();
    }
    let out = Command::new(ps)
        .args([flags, list, "-o", "pid=,command="])
        .stdin(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    out.lines()
        .filter_map(|line| {
            let (pid, rest) = line.trim_start().split_once(' ')?;
            Some((pid.parse().ok()?, rest.to_owned()))
        })
        .collect()
}

/// Shared parser for the OS command above: pid, UTC lstart, then the complete argv.
/// Column order and timestamp syntax belong to this OS adapter, not harness definitions.
pub struct ProcessLine<'a> {
    pub pid: u32,
    pub started: DateTime<Utc>,
    pub command: &'a str,
}

pub fn process_lines(ps: &str) -> Vec<ProcessLine<'_>> {
    ps.lines()
        .filter_map(|line| {
            let (pid, rest) = line.trim_start().split_once(' ')?;
            let (start, command) = rest.trim_start().split_at_checked(24)?;
            Some(ProcessLine {
                pid: pid.parse().ok()?,
                started: chrono::NaiveDateTime::parse_from_str(start, "%a %b %e %H:%M:%S %Y")
                    .ok()?
                    .and_utc(),
                command,
            })
        })
        .collect()
}

/// Batch process start times in Claude's UTC format to reject reused pids.
///
/// An unreadable process table is an error, never an empty map: every liveness check reads
/// this, so swallowing the failure would report a machine full of sessions as an empty fleet.
/// `ps` exiting nonzero because no listed pid is alive is a real empty table, not a failure.
fn process_starts(pids: impl Iterator<Item = u64>) -> Result<HashMap<u32, String>> {
    starts_from("/bin/ps", pids)
}

/// `process_starts` against a named `ps`, so a test can point it at one that cannot run.
fn starts_from(ps: &str, pids: impl Iterator<Item = u64>) -> Result<HashMap<u32, String>> {
    // ps rejects the whole list when one pid is above the kernel's maximum (99998 on macOS);
    // such a pid runs nothing anyway.
    let list = pids
        .filter(|p| *p <= 99_998)
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    if list.is_empty() {
        return Ok(HashMap::new());
    }
    let out = Command::new(ps)
        .env("TZ", "UTC")
        // lstart is locale text: under a non-English LANG ps prints its own month names,
        // which never match the start a harness recorded, so every live session is dropped.
        .env("LC_ALL", "C")
        .args(["-o", "pid=,lstart=", "-p", &list])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("reading the process table with {ps}"))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (pid, start) = l.trim().split_once(' ')?;
            Some((pid.parse().ok()?, start.trim().to_owned()))
        })
        .collect())
}

fn session(
    dir: &Path,
    v: &Value,
    starts: &HashMap<u32, String>,
    read_transcript: bool,
) -> Option<Session> {
    let pid = v["pid"].as_u64()? as u32;
    let id = v["sessionId"].as_str()?;
    // Unclaimed spare workers are registry entries but not user sessions.
    if v["spare"].as_bool() == Some(true) {
        return None;
    }
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
    let transcript = crate::harness::spec(crate::config::HarnessKind::Claude)
        .transcript
        .live_path(dir)
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
    let d = if read_transcript {
        transcript.as_deref().map(details).unwrap_or_default()
    } else {
        Details::default()
    };
    let (window, cost, effort) = statusline_values(dir, id);
    let (cost_usd, cost_info) = d.report.costs.report(cost);
    Some(Session {
        session_id: id.into(),
        harness: claude(),
        kind: v["kind"].as_str().map(Into::into),
        // Keep background jobs grouped by launch cwd when their registry cwd moves into a worktree.
        cwd: job["cwd"].as_str().map(PathBuf::from).unwrap_or(cwd),
        state: state(&job, v["status"].as_str().unwrap_or("-")),
        started: d.report.started,
        last_activity: d.report.last_activity,
        model: d.report.model,
        pid: Some(pid),
        transcript_path: transcript,
        tokens_in: d.report.tokens_in,
        tokens_out: d.report.tokens_out,
        context_tokens: d.report.context,
        context_window: window,
        cost_usd,
        cost_info,
        effort,
        usage: None,
        title: d
            .title
            .or_else(|| {
                // Claude names a job after its short id until it has a title; that is not a name.
                let named = |n: &&str| !n.is_empty() && Some(*n) != v["jobId"].as_str() && *n != id;
                job["name"]
                    .as_str()
                    .filter(named)
                    .or_else(|| v["name"].as_str().filter(named))
                    .map(Into::into)
            })
            .or(d.report.first_prompt),
        last: job["detail"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(Into::into)
            .or(d.last),
        coordinator: false,
        forked_from: None,
        activity: d.report.activity,
    })
}

/// Mirror Claude Code 2.1.272 state precedence; see docs/harness.md.
/// Registry busy wins because job state can lag a new turn. Background jobs never idle.
fn state(job: &Value, status: &str) -> String {
    let job_state = job["state"].as_str();
    let tempo = job["tempo"].as_str();
    let waking = !job["routine"].is_null()
        || job["selfWake"].as_bool() == Some(true)
        || job["inFlight"]["kinds"]
            .as_array()
            .is_some_and(|k| k.iter().any(|k| k.as_str() == Some("session_cron")));
    let finished = tempo != Some("active")
        && match job_state {
            Some("done") => !waking,
            Some("failed" | "stopped") => true,
            _ => false,
        };
    match job_state {
        _ if status == "busy" || status == "shell" => "active",
        Some(done) if finished => done,
        _ if status == "waiting" || tempo == Some("blocked") => "blocked",
        Some(_) => "active",
        None => status,
    }
    .into()
}

/// Read reported values saved by the user's statusLine command.
fn statusline_values(claude: &Path, id: &str) -> (Option<u64>, Option<f64>, Option<String>) {
    let Some(source) = crate::harness::spec(crate::config::HarnessKind::Claude)
        .transcript
        .statusline
        .as_ref()
    else {
        return (None, None, None);
    };
    let values = fs::read(claude.join(&source.directory).join(format!("{id}.json")))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let window = values
        .as_ref()
        .and_then(|v| v.pointer(&source.window_pointer)?.as_u64());
    let cost = values.as_ref().and_then(|v| statusline_cost(source, v));
    let effort = values.as_ref().and_then(|v| {
        let pointer = source.effort_pointer.as_deref()?;
        Some(v.pointer(pointer)?.as_str()?.to_owned())
    });
    (window, cost, effort)
}

/// Read reported dollars only from the declared statusline source.
pub(crate) fn statusline_cost(
    source: &crate::harness::spec::Statusline,
    value: &Value,
) -> Option<f64> {
    value
        .pointer(source.cost_pointer.as_deref()?)?
        .as_f64()
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}

#[derive(Default, Clone)]
struct Details {
    title: Option<String>,
    last: Option<String>,
    report: Report,
}

/// Recount when the transcript or its pricing snapshot changes.
fn details(transcript: &Path) -> Details {
    type Cached = (u64, Details, Option<crate::cost::CatalogStamp>);
    static CACHE: Mutex<Option<HashMap<PathBuf, Cached>>> = Mutex::new(None);
    let catalog = crate::cost::snapshot();
    let pricing = catalog.as_ref().map(|c| c.stamp.clone());
    let len = fs::metadata(transcript).map_or(0, |m| m.len());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some((seen, d, priced_with)) = cache.get(transcript)
        && *seen == len
        && *priced_with == pricing
    {
        return d.clone();
    }
    let (title, mut last) = tail(transcript, 1);
    let d = Details {
        title,
        last: last.pop(),
        report: report_with_activity(transcript, true, catalog.as_deref()).unwrap_or_default(),
    };
    cache.insert(transcript.to_owned(), (len, d.clone(), pricing));
    d
}

/// Read the title and last `n` assistant headlines from growing tail windows.
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
        // Large tool results may hide the title or reply; grow the window up to 16 MiB.
        if (out.0.is_some() && out.1.len() >= n) || window >= len || window >= 16 << 20 {
            break;
        }
        window *= 4;
    }
    let keep = out.1.len().saturating_sub(n);
    out.1.drain(..keep);
    out
}

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

/// Render the latest user prompt and reply, excluding tool-result user messages.
pub fn exchange(transcript: &Path) -> Vec<String> {
    exchanges(transcript, 1)
}

/// Render the last `n` exchanges, oldest first, from a bounded tail window.
pub fn exchanges(transcript: &Path, n: usize) -> Vec<String> {
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
            // Codex also stores instructions as user messages; exclude those XML blocks.
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

pub fn headline(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.replace("**", ""))
}

/// User-set titles override generated titles regardless of their order in the transcript.
fn scan(lines: &str) -> (Option<String>, Vec<String>) {
    let mut out = (None, Vec::new());
    let mut ai = None;
    for line in lines.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v["type"].as_str() {
            Some("ai-title") => ai = v["aiTitle"].as_str().map(Into::into),
            Some("agent-name") => out.0 = v["agentName"].as_str().map(Into::into),
            Some("custom-title") => out.0 = v["customTitle"].as_str().map(Into::into),
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
    out.0 = out.0.or(ai);
    out
}

/// Append Claude's native `custom-title` record. A live session may overwrite it
/// from memory; its registry entry is left alone.
pub fn rename(session: &Session, name: &str) -> Result<()> {
    use std::io::Write;
    let name = name.trim();
    ensure!(!name.is_empty(), "a title is needed");
    ensure!(
        crate::harness::by_name(&session.harness).is_some_and(|spec| spec.operations.rename),
        "this harness has no rename operation"
    );
    let path = session
        .transcript_path
        .as_deref()
        .context("this session has no transcript yet")?;
    let line = serde_json::json!({
        "type": "custom-title", "customTitle": name, "sessionId": session.session_id
    });
    let mut file = fs::OpenOptions::new().append(true).open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

#[derive(Default, Clone)]
struct Report {
    /// The user's first instruction, used while Claude has not supplied a descriptive name.
    first_prompt: Option<String>,
    title: Option<String>,
    ai_title: Option<String>,
    last: Option<String>,
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
    context: Option<u64>,
    model: Option<String>,
    started: Option<DateTime<Utc>>,
    last_activity: Option<DateTime<Utc>>,
    activity: Vec<Activity>,
    costs: crate::cost::Accounting<CostAdapter>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CostAdapter;

impl Adapter for CostAdapter {
    fn read<'a>(&'a mut self, event: &'a Value) -> Reading<'a> {
        let message = &event["message"];
        if event["type"] != "assistant" || message["model"] == "<synthetic>" {
            return Reading::Ignore;
        }
        let usage = &message["usage"];
        Reading::Response(Response {
            id: message["id"].as_str(),
            reported_usd: None, // Claude's dollar report is the saved session total.
            empty: [
                "input_tokens",
                "output_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
            ]
            .iter()
            .all(|k| usage[k].as_u64() == Some(0)),
            usage: (|| {
                // Never infer the endpoint from the model or local settings.
                let provider = message["provider"]
                    .as_str()
                    .ok_or("missing_provider_or_model")?;
                let model = message["model"]
                    .as_str()
                    .ok_or("missing_provider_or_model")?;
                if !matches!(usage["service_tier"].as_str(), None | Some("standard")) {
                    return Err("unsupported_service_tier");
                }
                if usage["cache_creation"]["ephemeral_1h_input_tokens"]
                    .as_u64()
                    .is_some_and(|n| n > 0)
                {
                    return Err("unsupported_cache_retention");
                }
                Ok(crate::cost::Usage {
                    provider,
                    model,
                    input: usage["input_tokens"].as_u64().ok_or("missing_counters")?,
                    output: usage["output_tokens"].as_u64().ok_or("missing_counters")?,
                    cache_read: usage["cache_read_input_tokens"]
                        .as_u64()
                        .ok_or("missing_counters")?,
                    cache_write: usage["cache_creation_input_tokens"]
                        .as_u64()
                        .ok_or("missing_counters")?,
                })
            })(),
            gap: None,
        })
    }
}

/// History hydrates only requested rows and never takes the live fleet's cache mutex.
#[cfg(test)]
pub(crate) fn history_columns(transcript: &Path) -> Result<crate::history::Columns> {
    let catalog = crate::cost::snapshot();
    history_columns_priced(transcript, catalog.as_deref())
}

pub(crate) fn history_columns_priced(
    transcript: &Path,
    catalog: Option<&crate::cost::Catalog>,
) -> Result<crate::history::Columns> {
    let report = report_with_activity(transcript, false, catalog)?;
    let (cost_usd, cost_info) = report.costs.report(None);
    Ok(crate::history::Columns {
        title: report.title.or(report.ai_title).or(report.first_prompt),
        model: report.model,
        tokens_in: report.tokens_in,
        tokens_out: report.tokens_out,
        context_tokens: report.context,
        last: report.last,
        cost_usd,
        cost_info,
        ..crate::history::Columns::default()
    })
}

/// Run output uses Claude's native JSON messages. Cache scalar summaries separately
/// from live activity, and read the context window only from the saved status line.
pub(crate) fn run_columns(
    transcript: Option<&Path>,
    claude: &Path,
    session_id: Option<&str>,
) -> crate::history::Columns {
    use std::os::unix::fs::MetadataExt;
    type Cached = (
        (u64, u64, i64, i64),
        crate::history::Columns,
        Option<crate::cost::CatalogStamp>,
    );
    static CACHE: Mutex<Option<HashMap<PathBuf, Cached>>> = Mutex::new(None);
    let mut columns = crate::history::Columns::default();
    let catalog = crate::cost::snapshot();
    let pricing = catalog.as_ref().map(|c| c.stamp.clone());
    if let Some(path) = transcript
        && let Ok(meta) = fs::metadata(path)
    {
        let stamp = (meta.ino(), meta.len(), meta.mtime(), meta.mtime_nsec());
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let cache = cache.get_or_insert_with(HashMap::new);
        if let Some((seen, saved, priced_with)) = cache.get(path)
            && *seen == stamp
            && *priced_with == pricing
        {
            columns = saved.clone();
        } else if let Ok(saved) = history_columns_priced(path, catalog.as_deref()) {
            if cache.len() >= 200 {
                cache.clear();
            }
            cache.insert(path.to_owned(), (stamp, saved.clone(), pricing));
            columns = saved;
        }
    }
    if let Some(id) = session_id {
        let (window, cost, _) = statusline_values(claude, id);
        columns.context_window = window;
        (columns.cost_usd, columns.cost_info) =
            crate::cost::prefer_native(cost, (columns.cost_usd, columns.cost_info));
    }
    columns
}

/// Count streaming usage once per message id; skip `<synthetic>` placeholder messages.
#[cfg(test)]
fn report(transcript: &Path) -> Result<Report> {
    report_with_activity(transcript, true, None)
}

fn report_with_activity(
    transcript: &Path,
    activity: bool,
    catalog: Option<&crate::cost::Catalog>,
) -> Result<Report> {
    let mut seen = HashSet::new();
    let (mut input, mut output) = (0, 0);
    let mut r = Report::default();
    let (mut input_counted, mut output_counted) = (false, false);
    for line in std::io::BufReader::new(fs::File::open(transcript)?).lines() {
        let Ok(event) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        r.costs.observe(&event, catalog);
        if let Some(t) = event["timestamp"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(DateTime::<Utc>::from)
        {
            r.started.get_or_insert(t);
            r.last_activity = Some(t);
            if activity {
                r.activity.push(Activity::at(t));
            }
        }
        let message = &event["message"];
        if event["type"] == "system" && event["subtype"] == "init" {
            r.model = event["model"].as_str().map(Into::into).or(r.model);
        }
        // Live discovery already has its tail scan; only history needs these
        // strings from the full pass, so it can find titles outside the tail window.
        if !activity {
            match event["type"].as_str() {
                Some("custom-title") => {
                    r.title = event["customTitle"].as_str().and_then(headline);
                }
                Some("agent-name") => {
                    r.title = event["agentName"].as_str().and_then(headline);
                }
                Some("ai-title") => {
                    r.ai_title = event["aiTitle"].as_str().and_then(headline);
                }
                _ => {}
            }
            if event["type"] == "assistant"
                && let Some(last) = message["content"].as_array().and_then(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|b| b["text"].as_str().and_then(headline))
                        .next_back()
                })
            {
                r.last = Some(last);
            }
        }
        // Tool calls are content blocks on assistant lines; streaming repeats a message's
        // usage per block but writes each block once.
        if event["type"] == "assistant"
            && let Some(blocks) = message["content"].as_array()
            && let Some(a) = r.activity.last_mut()
        {
            a.tools += blocks.iter().filter(|b| b["type"] == "tool_use").count() as u64;
        }
        if r.first_prompt.is_none() {
            r.first_prompt = crate::harness::spec(crate::config::HarnessKind::Claude)
                .transcript
                .messages
                .user
                .headline_with_attachments(&event, true);
        }
        let Some(u) = message.get("usage").filter(|u| u.is_object()) else {
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
        let has_input = [
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ]
        .iter()
        .any(|k| u[k].is_u64());
        input_counted |= has_input;
        output_counted |= u["output_tokens"].is_u64();
        if has_input {
            r.context = Some(prompt);
        }
        r.model = model.map(Into::into);
        if let Some(a) = r.activity.last_mut()
            && event["type"] == "assistant"
        {
            a.messages += 1;
            a.tokens_out += n("output_tokens");
        }
    }
    if input_counted {
        r.tokens_in = Some(input);
    }
    if output_counted {
        r.tokens_out = Some(output);
    }
    Ok(r)
}

/// What the kernel charges a session's own process. Commands it spawns are not counted,
/// so the number answers how hard the agent itself is working, not its whole process tree.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    /// Percent of one core, as `ps` reports it.
    pub cpu: f32,
    /// Resident set size in bytes.
    pub rss: u64,
}

/// Usage for the listed pids in one `ps` read, so a fleet costs one process, not one per row.
pub fn usage(pids: impl Iterator<Item = u32>) -> HashMap<u32, Usage> {
    usage_from("/bin/ps", pids)
}

/// `usage` against a named `ps`, so a test can point it at one that cannot run.
fn usage_from(ps: &str, pids: impl Iterator<Item = u32>) -> HashMap<u32, Usage> {
    // ps rejects the whole list when one pid is above the kernel's maximum, as in `starts_from`.
    let list = pids
        .filter(|p| *p <= 99_998)
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    if list.is_empty() {
        return HashMap::new();
    }
    let out = Command::new(ps)
        .args(["-o", "pid=,%cpu=,rss=", "-p", &list])
        .stdin(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    out.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let cpu = fields.next()?.parse().ok()?;
            // ps prints resident size in kibibytes.
            let rss = fields.next()?.parse::<u64>().ok()? * 1024;
            Some((pid, Usage { cpu, rss }))
        })
        .collect()
}

pub fn alive(pid: u32) -> bool {
    // Signal 0 checks existence; EPERM means it exists under another user.
    unsafe { libc::kill(pid as i32, 0) == 0 || *libc::__error() == libc::EPERM }
}

/// Every harness, for callers with no configuration to consult.
pub fn all(claude: &Path) -> Result<Vec<Session>> {
    all_observed(claude, &crate::config::Policy::default(), |_, _, _, _| {})
}

/// A harness config does not offer is not scanned at all, so its native home is never read.
pub(crate) fn all_observed(
    claude: &Path,
    offered: &crate::config::Policy,
    mut observe: impl FnMut(&str, &Path, std::time::Duration, &Result<Vec<Session>>),
) -> Result<Vec<Session>> {
    let mut out = Vec::new();
    for &kind in crate::harness::known() {
        if !offered.enabled_for(kind) {
            continue;
        }
        let spec = crate::harness::spec(kind);
        // Live process discovery keeps its existing default-home scope. Saved daemon threads
        // from additional homes are supplied separately by the dashboard.
        let home = spec.home.resolve(claude);
        let started = std::time::Instant::now();
        let result = spec.discovery.handler.sessions(&home);
        observe(&spec.name, &home, started.elapsed(), &result);
        out.extend(result?);
    }
    sort(&mut out);
    Ok(out)
}

/// Sort by oldest start, unknown starts last, then id.
pub fn sort(out: &mut [Session]) {
    out.sort_by(|a, b| {
        (a.started.is_none(), a.started, &a.session_id).cmp(&(
            b.started.is_none(),
            b.started,
            &b.session_id,
        ))
    });
}

pub fn find(claude: &Path, session_id: &str) -> Result<Option<Session>> {
    Ok(all(claude)?
        .into_iter()
        .find(|s| s.session_id == session_id))
}

/// Validate current pid/start identity without loading every transcript.
pub(crate) fn control_session(claude: &Path, session_id: &str) -> Result<Option<Session>> {
    match fs::read_dir(
        claude.join(
            crate::harness::spec(crate::config::HarnessKind::Claude)
                .discovery
                .registry
                .as_ref()
                .expect("Claude registry"),
        ),
    ) {
        Ok(entries) => {
            for path in entries.filter_map(|e| e.ok().map(|e| e.path())) {
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                let Some(value) = fs::read(path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                else {
                    continue;
                };
                if value["sessionId"].as_str() == Some(session_id) {
                    let starts = process_starts(value["pid"].as_u64().into_iter())?;
                    return Ok(session(claude, &value, &starts, false));
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    for &kind in crate::harness::known() {
        let spec = crate::harness::spec(kind);
        if spec.discovery.registry.is_some() {
            continue;
        }
        if let Some(session) = spec
            .discovery
            .handler
            .sessions(&spec.home.resolve(claude))?
            .into_iter()
            .find(|s| s.session_id == session_id)
        {
            return Ok(Some(session));
        }
    }
    Ok(None)
}

/// Verify process identity before signalling; return false when already gone.
pub fn stop(claude: &Path, session_id: &str) -> Result<bool> {
    let session = control_session(claude, session_id)?.context("no such run or session")?;
    // Use `claude rm`: the daemon respawns killed workers, and `stop` leaves a job record.
    // The transcript remains resumable.
    let spec = crate::harness::by_name(&session.harness).context("unknown session harness")?;
    if spec.session(session.kind.as_deref()).stop == crate::harness::spec::Stop::Remove {
        crate::harness::check_operation(spec, &spec.operations.remove, "remove")?;
        let claude = crate::harness::executable(&spec.name, &crate::harness::launch_path())
            .with_context(|| format!("{} not found", spec.name))?;
        let short = session_id.get(..8).context("invalid session id")?;
        let out = Command::new(claude)
            .args(crate::harness::spec::args(
                &spec.commands.remove,
                &[("id", session_id.as_ref()), ("short_id", short.as_ref())],
            )?)
            .stdin(Stdio::null())
            .output()?;
        ensure!(
            out.status.success(),
            "claude rm: {}{}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return Ok(true);
    }
    let pid = session.pid.context("session has no harness pid")?;
    terminate(pid, &session.harness)
}

/// SIGTERM a harness client by pid, for sessions no registry lists.
pub fn terminate(pid: u32, harness: &str) -> Result<bool> {
    ensure!(pid > 1, "invalid harness pid");
    let output = std::process::Command::new("/bin/ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "command="])
        .output()?;
    let command = String::from_utf8_lossy(&output.stdout);
    let Some(program) = command.split_whitespace().next() else {
        return Ok(false);
    };
    ensure!(
        Path::new(program).file_name().is_some_and(|f| f == harness),
        "pid {pid} is not a {harness} process; refusing to signal a reused pid"
    );
    ensure!(
        unsafe { libc::kill(pid as i32, libc::SIGTERM) } == 0,
        "unable to signal pid {pid}"
    );
    Ok(true)
}

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

pub fn age(since: DateTime<Utc>) -> String {
    let s = (Utc::now() - since).num_seconds().max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

pub fn cost(usd: f64) -> String {
    if usd < 0.01 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

/// Show reported prompt/window sizes; omit the denominator when no window was reported.
pub fn context(s: &Session) -> String {
    context_values(s.context_tokens, s.context_window)
}

pub(crate) fn context_values(tokens: Option<u64>, window: Option<u64>) -> String {
    match (tokens, window) {
        (Some(t), Some(w)) => format!("{}/{}", short(t), short(w)),
        (Some(t), None) => short(t),
        (None, _) => "-".into(),
    }
}

pub fn tokens(s: &Session) -> String {
    token_values(s.tokens_in, s.tokens_out)
}

pub(crate) fn token_values(input: Option<u64>, output: Option<u64>) -> String {
    match (input, output) {
        (None, None) => "-".into(),
        (i, o) => format!("{}/{}", short(i.unwrap_or(0)), short(o.unwrap_or(0))),
    }
}
/// Resident size in the same compact style as token counts.
pub fn bytes(n: u64) -> String {
    match n {
        0..1_048_576 => format!("{}K", n / 1024),
        1_048_576..1_073_741_824 => format!("{}M", n / 1_048_576),
        _ => format!("{:.1}G", n as f64 / 1_073_741_824.0),
    }
}

fn short(n: u64) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => format!("{}k", n / 1000),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// Every model Bedrock serves, under the name it presents it by, from
/// `aws bedrock list-foundation-models` on 2026-09-16. Keyed by [`stem`], so one
/// row answers for the id in every region and every `-v1:0` revision of it.
/// Regenerate when a provider ships a model cones should name.
const NAMES: [(&str, &str); 70] = [
    ("claude-fable-5", "Claude Fable 5"),
    ("claude-fable-5-1", "Claude Fable 5.1"),
    ("claude-haiku-4-5-20251001", "Claude Haiku 4.5"),
    ("claude-opus-4-1-20250805", "Claude Opus 4.1"),
    ("claude-opus-4-5-20251101", "Claude Opus 4.5"),
    ("claude-opus-4-6", "Claude Opus 4.6"),
    ("claude-opus-4-7", "Claude Opus 4.7"),
    ("claude-opus-4-8", "Claude Opus 4.8"),
    ("claude-opus-5", "Claude Opus 5"),
    ("claude-sonnet-4-20250514", "Claude Sonnet 4"),
    ("claude-sonnet-4-5-20250929", "Claude Sonnet 4.5"),
    ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
    ("claude-sonnet-5", "Claude Sonnet 5"),
    ("devstral-2-123b", "Devstral 2 123B"),
    ("gemma-3-12b-it", "Gemma 3 12B IT"),
    ("gemma-3-4b-it", "Gemma 3 4B IT"),
    ("glm-4.7", "GLM 4.7"),
    ("glm-4.7-flash", "GLM 4.7 Flash"),
    ("glm-5", "GLM 5"),
    ("gpt-5.6-luna", "GPT-5.6 Luna"),
    ("gpt-5.6-sol", "GPT-5.6 Sol"),
    ("gpt-5.6-terra", "GPT-5.6 Terra"),
    ("gpt-6-astra", "GPT-6 Astra"),
    ("gpt-oss-120b-1", "gpt-oss-120b"),
    ("gpt-oss-20b-1", "gpt-oss-20b"),
    ("gpt-oss-safeguard-120b", "GPT OSS Safeguard 120B"),
    ("gpt-oss-safeguard-20b", "GPT OSS Safeguard 20B"),
    ("grok-4.6", "Grok 4.6"),
    ("kimi-k2-thinking", "Kimi K2 Thinking"),
    ("kimi-k2.5", "Kimi K2.5"),
    ("llama3-1-70b-instruct", "Llama 3.1 70B Instruct"),
    ("llama3-1-8b-instruct", "Llama 3.1 8B Instruct"),
    ("llama3-3-70b-instruct", "Llama 3.3 70B Instruct"),
    ("llama3-70b-instruct", "Llama 3 70B Instruct"),
    ("llama3-8b-instruct", "Llama 3 8B Instruct"),
    (
        "llama4-maverick-17b-instruct",
        "Llama 4 Maverick 17B Instruct",
    ),
    ("llama4-scout-17b-instruct", "Llama 4 Scout 17B Instruct"),
    ("magistral-small-2509", "Magistral Small 2509"),
    ("minimax-m2", "MiniMax M2"),
    ("minimax-m2.1", "MiniMax M2.1"),
    ("minimax-m2.5", "MiniMax M2.5"),
    ("ministral-3-14b-instruct", "Ministral 14B 3.0"),
    ("ministral-3-3b-instruct", "Ministral 3B"),
    ("ministral-3-8b-instruct", "Ministral 3 8B"),
    ("mistral-7b-instruct", "Mistral 7B Instruct"),
    ("mistral-large-2402", "Mistral Large (24.02)"),
    ("mistral-large-2407", "Mistral Large (24.07)"),
    ("mistral-large-3-675b-instruct", "Mistral Large 3"),
    ("mixtral-8x7b-instruct", "Mixtral 8x7B Instruct"),
    ("nemotron-nano-3-30b", "Nemotron Nano 3 30B"),
    ("nova-2-lite", "Nova 2 Lite"),
    ("nova-2-sonic", "Nova 2 Sonic"),
    ("nova-lite", "Nova Lite"),
    ("nova-micro", "Nova Micro"),
    ("nova-pro", "Nova Pro"),
    ("palmyra-x4", "Palmyra X4"),
    ("palmyra-x5", "Palmyra X5"),
    ("pegasus-1-2", "Pegasus v1.2"),
    ("pixtral-large-2502", "Pixtral Large (25.02)"),
    ("qwen3-235b-a22b-2507", "Qwen3 235B A22B 2507"),
    ("qwen3-32b", "Qwen3 32B (dense)"),
    ("qwen3-coder-30b-a3b", "Qwen3-Coder-30B-A3B-Instruct"),
    ("qwen3-coder-480b-a35b", "Qwen3 Coder 480B A35B Instruct"),
    ("qwen3-next-80b-a3b", "Qwen3 Next 80B A3B"),
    ("qwen3-vl-235b-a22b", "Qwen3 VL 235B A22B"),
    ("r1", "DeepSeek-R1"),
    ("v3", "DeepSeek-V3.1"),
    ("v3.2", "DeepSeek V3.2"),
    ("voxtral-mini-3b-2507", "Voxtral Mini 3B 2507"),
    ("voxtral-small-24b-2507", "Voxtral Small 24B 2507"),
];

/// The part of a model id that names the model: no region, no provider, no
/// revision. `us.anthropic.claude-sonnet-4-5-20250929-v1:0` is
/// `claude-sonnet-4-5-20250929`.
fn stem(id: &str) -> &str {
    let mut id = id;
    // Region and provider lead in dotted words; a version carries digits.
    while let Some((word, rest)) = id.split_once('.')
        && word.chars().all(|c| c.is_ascii_alphabetic())
    {
        id = rest;
    }
    let id = id.split(':').next().unwrap_or(id);
    id.rsplit_once("-v")
        .filter(|(_, rev)| !rev.is_empty() && rev.bytes().all(|b| b.is_ascii_digit()))
        .map_or(id, |(head, _)| head)
}

/// Name a model the way it is presented: `claude-fable-5-1` is Fable 5.1 and
/// `us.openai.gpt-5.6-sol` is GPT-5.6 Sol. [`NAMES`] answers first. An id it does
/// not carry is spelled from its own words, so a model newer than the table is
/// named too, and the vendor word leads only when the id does not say the family.
/// An id of no family cones knows, and a bare alias a job writes by hand, are
/// shown verbatim.
pub fn model(id: &str) -> String {
    let (base, window) = id
        .split_once("[1m]")
        .map_or((id, ""), |(b, _)| (b, " (1M)"));
    let base = stem(base);
    if let Some((_, name)) = NAMES.iter().find(|(k, _)| *k == base) {
        // The row already shows the harness, so the column need not say Claude twice.
        return name.strip_prefix("Claude ").unwrap_or(name).to_owned() + window;
    }
    let capitalize = |w: &str| match w.chars().next() {
        Some(c) => c.to_uppercase().to_string() + &w[c.len_utf8()..],
        None => String::new(),
    };
    let (mut family, mut version, mut rest) = (None, Vec::new(), Vec::new());
    for word in base.split('-').filter(|w| !w.is_empty()) {
        let digits = |w: &str| w.bytes().all(|b| b.is_ascii_digit());
        match word {
            "claude" => {}
            "fable" | "opus" | "sonnet" | "haiku" | "gpt" => family = Some(word),
            // A release date is not a version.
            _ if word.len() == 8 && digits(word) => {}
            // `5`, `4o` and `5.6` are versions; `0613` and `16k` are names, and a
            // version leads, so the `1` of `gpt-oss-120b-1` is a revision.
            _ if rest.is_empty()
                && word.starts_with(|c: char| c.is_ascii_digit())
                && (word.len() <= 2 || word.contains('.')) =>
            {
                version.push(word);
            }
            _ => rest.push(word),
        }
    }
    // A family word on its own is an alias, not a model: `sonnet` stays `sonnet`.
    let Some(family) = family.filter(|_| !version.is_empty() || !rest.is_empty()) else {
        return id.into();
    };
    let version = version.join(".");
    // Claude spaces its version off the family, GPT carries it in the name.
    let head = match (family, version.is_empty()) {
        ("gpt", true) => "GPT".to_owned(),
        ("gpt", false) => format!("GPT-{version}"),
        (_, true) => capitalize(family),
        (_, false) => format!("{} {version}", capitalize(family)),
    };
    [head]
        .into_iter()
        .chain(rest.into_iter().map(capitalize))
        .collect::<Vec<_>>()
        .join(" ")
        + window
}

pub fn tilde(path: &Path) -> String {
    dirs::home_dir()
        .and_then(|h| path.strip_prefix(h).ok())
        .map_or_else(
            || path.display().to_string(),
            |rest| format!("~/{}", rest.display()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_reads_this_process_and_survives_a_dead_pid_and_an_unreadable_ps() {
        let me = std::process::id();
        let read = usage(vec![me, 99_999, u32::MAX].into_iter());
        let mine = read.get(&me).expect("this process is in the table");
        assert!(mine.cpu >= 0.0 && mine.cpu.is_finite(), "{mine:?}");
        assert!(
            mine.rss > 1024 * 1024,
            "a running test holds megabytes: {mine:?}"
        );
        assert!(
            !read.contains_key(&99_999) && !read.contains_key(&u32::MAX),
            "pids above the kernel maximum never reach ps"
        );
        assert!(usage(std::iter::empty()).is_empty(), "no pids, no ps");
        assert!(
            usage_from("/nonexistent/ps", std::iter::once(me)).is_empty(),
            "an unreadable process table reports nothing, not zero usage"
        );
    }

    #[test]
    fn resident_size_reads_in_kilobytes_megabytes_and_gigabytes() {
        assert_eq!(bytes(0), "0K");
        assert_eq!(bytes(4096), "4K");
        assert_eq!(bytes(1_048_575), "1023K");
        assert_eq!(bytes(1_048_576), "1M");
        assert_eq!(bytes(700 * 1_048_576), "700M");
        assert_eq!(bytes(1_073_741_824), "1.0G");
        assert_eq!(bytes(3 * 1_073_741_824 + 1_073_741_824 / 2), "3.5G");
    }

    #[test]
    fn statusline_effort_follows_the_declared_pointer_and_is_absent_without_one() {
        let source = crate::harness::spec(crate::config::HarnessKind::Claude)
            .transcript
            .statusline
            .as_ref()
            .expect("claude declares a statusline source");
        assert_eq!(source.effort_pointer.as_deref(), Some("/effort/level"));
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        fs::create_dir(home.join(&source.directory)).unwrap();
        let write = |id: &str, body: &str| {
            fs::write(
                home.join(&source.directory).join(format!("{id}.json")),
                body,
            )
            .unwrap();
        };
        write("reported", r#"{"effort":{"level":"high"}}"#);
        write(
            "silent",
            r#"{"context_window":{"context_window_size":200000}}"#,
        );
        write("wrong-shape", r#"{"effort":{"level":7}}"#);
        assert_eq!(
            statusline_values(home, "reported").2.as_deref(),
            Some("high")
        );
        assert_eq!(statusline_values(home, "silent").2, None);
        assert_eq!(
            statusline_values(home, "wrong-shape").2,
            None,
            "a level that is not a string is not an effort"
        );
        assert_eq!(statusline_values(home, "never-written").2, None);
    }

    #[test]
    fn statusline_cost_follows_the_declared_pointer_and_stays_absent_without_one() {
        let mut source = crate::harness::spec::Statusline {
            directory: PathBuf::from("statusline"),
            window_pointer: "/window".into(),
            cost_pointer: Some("/billing/dollars".into()),
            effort_pointer: None,
        };
        let payload = serde_json::json!({
            "cost": {"total_cost_usd": 99},
            "billing": {"dollars": 0.25},
        });
        assert_eq!(statusline_cost(&source, &payload), Some(0.25));
        for (value, expected) in [
            (serde_json::json!(0), Some(0.0)),
            (serde_json::json!(-1), None),
            (serde_json::json!("0.25"), None),
            (Value::Null, None),
        ] {
            assert_eq!(
                statusline_cost(&source, &serde_json::json!({"billing":{"dollars":value}})),
                expected
            );
        }
        source.cost_pointer = Some("/missing".into());
        assert_eq!(statusline_cost(&source, &payload), None);
        source.cost_pointer = None;
        assert_eq!(statusline_cost(&source, &payload), None);
    }

    #[test]
    fn a_model_id_is_shown_under_the_name_it_is_presented_by() {
        for (id, shown) in [
            // Bedrock's own name for it, in any region and any revision.
            ("claude-fable-5-1", "Fable 5.1"),
            ("claude-opus-5", "Opus 5"),
            ("claude-opus-5[1m]", "Opus 5 (1M)"),
            ("claude-haiku-4-5-20251001", "Haiku 4.5"),
            ("us.anthropic.claude-sonnet-4-5-20250929-v1:0", "Sonnet 4.5"),
            ("openai.gpt-6-astra", "GPT-6 Astra"),
            ("us.openai.gpt-5.6-sol", "GPT-5.6 Sol"),
            (
                "meta.llama3-3-70b-instruct-v1:0:128k",
                "Llama 3.3 70B Instruct",
            ),
            ("amazon.nova-pro-v1:0", "Nova Pro"),
            ("deepseek.r1-v1:0", "DeepSeek-R1"),
            ("openai.gpt-oss-120b-1:0", "gpt-oss-120b"),
            // Bedrock's own name loses to the id here, so the id stands.
            ("google.gemma-3-27b-it", "google.gemma-3-27b-it"),
            ("nvidia.nemotron-nano-9b-v2", "nvidia.nemotron-nano-9b-v2"),
            // Not a model Bedrock serves: spelled from the id's own words.
            ("gpt-5-codex", "GPT-5 Codex"),
            ("claude-opus-6", "Opus 6"),
            ("claude-3-5-sonnet-20241022", "Sonnet 3.5"),
            ("claude-3-7-sonnet-latest", "Sonnet 3.7 Latest"),
            ("gpt-4o", "GPT-4o"),
            // A snapshot and a window are names, not versions, and never join one.
            ("gpt-4-0613", "GPT-4 0613"),
            ("gpt-3.5-turbo-16k", "GPT-3.5 Turbo 16k"),
            // Nothing to name: the harness's own word for it stands.
            ("o3", "o3"),
            ("sonnet", "sonnet"),
            ("gpt", "gpt"),
        ] {
            assert_eq!(model(id), shown, "{id}");
        }
    }

    #[test]
    fn the_sparkline_counts_what_the_transcript_wrote_and_scales_to_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        fs::write(&transcript, concat!(
            "{\"type\":\"user\",\"timestamp\":\"2026-09-15T10:00:00Z\",\"message\":{\"content\":\"go\"}}\n",
            "{\"type\":\"assistant\",\"timestamp\":\"2026-09-15T10:00:05Z\",\"message\":{\"id\":\"m1\",\"model\":\"claude-fable-5-1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":40},\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\n",
            "{\"type\":\"assistant\",\"timestamp\":\"2026-09-15T10:00:06Z\",\"message\":{\"id\":\"m1\",\"model\":\"claude-fable-5-1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":40},\"content\":[{\"type\":\"tool_use\",\"name\":\"Read\"},{\"type\":\"tool_use\",\"name\":\"Grep\"}]}}\n",
            "{\"type\":\"user\",\"timestamp\":\"2026-09-15T10:04:00Z\",\"message\":{\"content\":[{\"type\":\"tool_result\"}]}}\n",
        )).unwrap();
        let r = report(&transcript).unwrap();
        let sum = |f: fn(&Activity) -> u64| r.activity.iter().map(f).sum::<u64>();
        assert_eq!(r.activity.len(), 4, "one entry per timestamped line");
        assert_eq!(sum(|a| a.messages), 1, "a streamed message counts once");
        assert_eq!(sum(|a| a.tools), 2, "each tool_use block counts");
        assert_eq!(sum(|a| a.tokens_out), 40);

        let now = "2026-09-15T10:05:00Z".parse::<DateTime<Utc>>().unwrap();
        let spark = |metric: &str, bound: &str| crate::config::Activity {
            bars: 6,
            bucket: "1m".into(),
            metric: metric.into(),
            bound: bound.into(),
        };
        assert_eq!(
            buckets(&r.activity, &spark("lines", "fleet"), now),
            [3, 0, 0, 0, 1, 0]
        );
        assert_eq!(
            buckets(&r.activity, &spark("tools", "fleet"), now),
            [2, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            buckets(&r.activity, &spark("tokens", "fleet"), now),
            [40, 0, 0, 0, 0, 0]
        );
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            buckets(
                &r.activity,
                &spark("lines", "fleet"),
                at("2026-09-15T10:05:59Z")
            ),
            [3, 0, 0, 0, 1, 0],
            "the edges do not slide with the reload"
        );
        assert_eq!(
            buckets(
                &r.activity,
                &spark("lines", "fleet"),
                at("2026-09-15T10:06:00Z")
            ),
            [0, 0, 0, 1, 0, 0]
        );

        assert_eq!(bars(&[0.0, 1.0, 2.0, 4.0, 8.0], 8.0), "▁▁▂▄▇");
        assert_eq!(
            bars(&[3.0, 30.0], 10.0),
            "▃▇",
            "over a fixed bound draws full, and full stops short of the row above"
        );

        let session = |id: &str| -> Session {
            serde_json::from_value(serde_json::json!({
                "session_id": id, "cwd": "/x", "state": "idle"
            }))
            .unwrap()
        };
        let mut a = session("a");
        a.activity = r.activity.clone();
        let mut b = session("b");
        b.activity = vec![Activity::at("2026-09-15T10:04:30Z".parse().unwrap()); 30];
        let rows = |bound: &str| {
            let s = sparklines(&[a.clone(), b.clone()], &spark("lines", bound), now);
            (s["a"].clone(), s["b"].clone())
        };
        assert_eq!(
            rows("fleet"),
            ("▁▁▁▁▁▁".into(), "▁▁▁▁▇▁".into()),
            "one scale: a's few lines are a sliver of b's 30"
        );
        assert_eq!(
            rows("row"),
            ("▇▁▁▁▃▁".into(), "▁▁▁▁▇▁".into()),
            "each row to its own peak"
        );
        assert_eq!(
            rows("4"),
            ("▆▁▁▁▂▁".into(), "▁▁▁▁▇▁".into()),
            "a fixed count fills a bar"
        );
        assert_eq!(
            rows("log").0,
            "▃▁▁▁▂▁",
            "log lifts the quiet row above the sliver fleet gave it"
        );
    }

    #[test]
    fn an_unnamed_claude_session_uses_its_first_instruction_until_named() {
        let dir = tempfile::tempdir().unwrap();
        let registry = dir.path().join("sessions");
        let project = dir.path().join("projects/-src-example");
        fs::create_dir_all(&registry).unwrap();
        fs::create_dir_all(&project).unwrap();
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let path = registry.join("session.json");
        let mut entry = serde_json::json!({
            "pid": std::process::id(), "sessionId": id, "cwd": "/src/example",
            "kind": "bg", "name": "aaaaaaaa", "jobId": "aaaaaaaa", "status": "idle"
        });
        fs::write(&path, entry.to_string()).unwrap();
        let transcript = project.join(format!("{id}.jsonl"));
        fs::write(&transcript, concat!(
            "{\"type\":\"user\",\"isMeta\":true,\"message\":{\"content\":\"environment instructions\"}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"text\":\"tool output\"}]}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":\"\\ninstall push clear worktrees\\nextra detail\"}}\n"
        )).unwrap();
        assert_eq!(
            sessions(dir.path()).unwrap()[0].title.as_deref(),
            Some("install push clear worktrees")
        );
        entry["name"] = Value::String("Publish the dashboard".into());
        fs::write(&path, entry.to_string()).unwrap();
        assert_eq!(
            sessions(dir.path()).unwrap()[0].title.as_deref(),
            Some("Publish the dashboard")
        );
    }

    #[test]
    fn the_orchestrator_status_file_marks_its_session_by_pid_and_folder() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("sessions")).unwrap();
        fs::create_dir_all(dir.path().join("orchestrator")).unwrap();
        // Two live entries share this process's pid; only the one in the status file's folder
        // is the orchestrator, so a reused pid in another folder is not.
        for (name, cwd) in [("aaaaaaaa", "/src/example"), ("bbbbbbbb", "/src/other")] {
            fs::write(
                dir.path().join(format!("sessions/{name}.json")),
                serde_json::json!({
                    "pid": std::process::id(), "sessionId": format!("{name}-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
                    "cwd": cwd, "kind": "bg", "jobId": name, "status": "idle"
                })
                .to_string(),
            )
            .unwrap();
        }
        fs::write(
            dir.path().join("orchestrator/status.json"),
            serde_json::json!({"cwd": "/src/example", "pid": std::process::id(), "peers": []})
                .to_string(),
        )
        .unwrap();
        let marks: Vec<(String, bool)> = sessions(dir.path())
            .unwrap()
            .into_iter()
            .map(|s| (s.cwd.display().to_string(), s.coordinator))
            .collect();
        assert_eq!(
            marks,
            [
                ("/src/example".to_owned(), true),
                ("/src/other".to_owned(), false)
            ]
        );
    }

    #[test]
    fn control_lookup_does_not_read_a_blocked_transcript() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt, sync::mpsc, time::Duration};
        let dir = tempfile::tempdir().unwrap();
        let registry = dir.path().join("sessions");
        let job = dir.path().join("jobs/aaaaaaaa");
        fs::create_dir_all(&registry).unwrap();
        fs::create_dir_all(&job).unwrap();
        let pipe = dir.path().join("transcript.fifo");
        let raw = CString::new(pipe.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(raw.as_ptr(), 0o600) }, 0);
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        fs::write(
            registry.join("entry.json"),
            serde_json::json!({
                "pid": std::process::id(), "sessionId": id, "cwd": "/src/example",
                "kind": "bg", "jobId": "aaaaaaaa", "status": "idle"
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            job.join("state.json"),
            serde_json::json!({"linkScanPath": pipe}).to_string(),
        )
        .unwrap();
        let root = dir.path().to_owned();
        let (tx, rx) = mpsc::channel();
        let task = std::thread::spawn(move || {
            tx.send(control_session(&root, id).unwrap().unwrap().session_id)
                .unwrap();
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), id);
        task.join().unwrap();
    }

    #[test]
    fn a_busy_registry_beats_a_finished_jobs_stale_state() {
        let dir = tempfile::tempdir().unwrap();
        let registry = dir.path().join("sessions");
        let job = dir.path().join("jobs/aaaaaaaa");
        fs::create_dir_all(&registry).unwrap();
        fs::create_dir_all(&job).unwrap();
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let path = registry.join("entry.json");
        let mut entry = serde_json::json!({
            "pid": std::process::id(), "sessionId": id, "cwd": "/src/example",
            "kind": "bg", "jobId": "aaaaaaaa", "status": "idle"
        });
        fs::write(&path, entry.to_string()).unwrap();
        fs::write(
            job.join("state.json"),
            serde_json::json!({"state": "done", "tempo": "idle"}).to_string(),
        )
        .unwrap();
        let state = |dir| sessions(dir).unwrap().remove(0).state;
        assert_eq!(state(dir.path()), "done");
        entry["status"] = "busy".into();
        fs::write(&path, entry.to_string()).unwrap();
        assert_eq!(state(dir.path()), "active");
    }

    #[test]
    fn a_job_still_taking_turns_is_working_however_the_registry_rests() {
        let word = |job: Value, status| super::state(&job, status);
        let job = |state, tempo| serde_json::json!({"state": state, "tempo": tempo});
        assert_eq!(word(job("working", "idle"), "idle"), "active");
        assert_eq!(
            word(job("blocked", "active"), "idle"),
            "active",
            "a job's own blocked is not needs input; the tempo and the registry say that"
        );
        assert_eq!(word(job("blocked", "blocked"), "idle"), "blocked");
        assert_eq!(word(job("working", "active"), "waiting"), "blocked");
        assert_eq!(word(job("done", "idle"), "idle"), "done");
        assert_eq!(word(job("failed", "idle"), "idle"), "failed");
        assert_eq!(
            word(job("done", "active"), "idle"),
            "active",
            "a tempo still active means the turn goes on, whatever the state says"
        );
        let mut waking = job("done", "idle");
        waking["selfWake"] = true.into();
        assert_eq!(
            word(waking, "idle"),
            "active",
            "a job that wakes itself has another turn coming, so it is not done"
        );
        let mut routine = job("done", "idle");
        routine["routine"] = serde_json::json!({"id": "nightly"});
        assert_eq!(word(routine, "idle"), "active");
        let mut stopped = job("stopped", "idle");
        stopped["selfWake"] = true.into();
        assert_eq!(
            word(stopped, "idle"),
            "stopped",
            "the wake only spares a job that finished well"
        );
        assert_eq!(
            word(Value::Null, "idle"),
            "idle",
            "a session with no job of its own is the registry's word"
        );
        assert_eq!(word(Value::Null, "waiting"), "blocked");
        assert_eq!(word(Value::Null, "surprising"), "surprising");
    }

    #[test]
    fn a_title_the_user_set_beats_the_generated_one_and_rename_writes_it() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        fs::write(
            &transcript,
            concat!(
                "{\"type\":\"custom-title\",\"customTitle\":\"Mine\"}\n",
                "{\"type\":\"ai-title\",\"aiTitle\":\"Generated later\"}\n"
            ),
        )
        .unwrap();
        assert_eq!(tail(&transcript, 1).0.as_deref(), Some("Mine"));
        let session = Session {
            session_id: "s1".into(),
            harness: "claude".into(),
            kind: None,
            cwd: dir.path().to_owned(),
            state: "idle".into(),
            started: None,
            last_activity: None,
            model: None,
            pid: None,
            transcript_path: Some(transcript.clone()),
            tokens_in: None,
            tokens_out: None,
            context_tokens: None,
            context_window: None,
            cost_usd: None,
            cost_info: None,
            title: Some("Mine".into()),
            last: None,
            effort: None,
            usage: None,
            coordinator: false,
            forked_from: None,
            activity: Vec::new(),
        };
        assert!(rename(&session, "  ").is_err(), "a blank title is refused");
        rename(&session, " Ours ").unwrap();
        assert_eq!(tail(&transcript, 1).0.as_deref(), Some("Ours"));
        let codex = Session {
            harness: "codex".into(),
            ..session
        };
        assert!(
            rename(&codex, "x").is_err(),
            "codex threads are named in codex"
        );
    }

    /// A client started against another home is another fleet's. The README capture starts
    /// twelve of them, and without this they arrive as rows with a command line and nothing else:
    /// no state, model or activity, because those records live in a home cones never opens.
    #[test]
    fn a_client_in_a_foreign_home_is_not_this_fleet() {
        let kind = crate::config::HarnessKind::Codex;
        let home = &crate::harness::spec(kind).home;
        assert_eq!(
            home.of_process(
                "codex -C /tmp/capture/projects/api Deduplicate events \
                 HOME=/tmp/capture CODEX_HOME=/tmp/capture/.codex TERM=xterm"
            )
            .as_deref(),
            Some(Path::new("/tmp/capture/.codex")),
            "the home the process names, not this machine's"
        );
        // With no override the home sits beside the Claude directory of that process's own home.
        assert_eq!(
            home.of_process("codex HOME=/tmp/capture TERM=xterm")
                .as_deref(),
            Some(Path::new("/tmp/capture/.codex"))
        );
        // A prompt that names the variable cannot claim a home: the command line read without
        // `-E` says where the environment begins.
        let argv = "codex -C /tmp/capture CODEX_HOME=/spoofed";
        assert_eq!(
            environment(&format!("{argv} HOME=/tmp/capture"), argv)
                .and_then(|env| home.of_process(env))
                .as_deref(),
            Some(Path::new("/tmp/capture/.codex"))
        );
        assert_eq!(
            home.of_process("codex --remote unix:///run/s prompt"),
            None,
            "an environment ps did not print names no home"
        );
        // Each definition states its own home, so the rule needs no per-harness code.
        assert_eq!(
            crate::harness::spec(crate::config::HarnessKind::Opencode)
                .home
                .of_process("opencode HOME=/tmp/capture XDG_DATA_HOME=/tmp/capture/xdg")
                .as_deref(),
            Some(Path::new("/tmp/capture/xdg/opencode"))
        );
        // Against the live table: this process runs against the home this fleet reads, and
        // launchd is a platform binary whose environment macOS hides. A hidden environment
        // keeps the process, because it must never empty the fleet.
        let me = std::process::id();
        assert!(own_home_processes("/bin/ps", kind, &[me]).contains(&me));
        assert!(own_home_processes("/bin/ps", kind, &[1]).contains(&1));
    }

    /// `lstart` is locale text. Under a non-English `LANG` ps prints its own month names, so
    /// `Fri Sep 18 17:07:16 2026` arrives as something the harness's recorded start can never
    /// equal, `session` drops every row and the whole fleet reads empty. The suite itself runs
    /// under C, so only a ps that reports the locale it was handed can catch this.
    #[test]
    fn the_process_table_is_read_in_one_locale_whatever_the_machine_speaks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ps = dir.path().join("ps");
        fs::write(&ps, "#!/bin/sh\necho \"1 $LC_ALL $TZ\"\n").unwrap();
        fs::set_permissions(&ps, fs::Permissions::from_mode(0o755)).unwrap();
        let ps = ps.to_str().unwrap();
        assert_eq!(
            process_table(ps).unwrap().trim(),
            "1 C UTC",
            "the whole table is read in one locale and one timezone"
        );
        assert_eq!(
            starts_from(ps, [1u64].into_iter())
                .unwrap()
                .get(&1)
                .map(String::as_str),
            Some("C UTC"),
            "and so are the starts of named pids"
        );
    }

    // An empty table is how a pid is reported dead, so a table that could not be read must not
    // become one: it would report every session on the machine as gone, and harness.md's rule is
    // that an unreported value is absent rather than guessed.
    #[test]
    fn an_unreadable_process_table_is_an_error_not_an_empty_fleet() {
        let me = u64::from(std::process::id());
        assert!(
            starts_from("/nonexistent/ps", [me].into_iter()).is_err(),
            "a ps that cannot run is an error, not an empty table"
        );
        assert!(
            starts_from("/bin/ps", [me].into_iter())
                .unwrap()
                .contains_key(&std::process::id()),
            "the real ps still reports this live process"
        );
        // The other half of the invariant: an empty table is what marks a session dead, which is
        // why the error above must never arrive as one.
        let dir = tempfile::tempdir().unwrap();
        let value = serde_json::json!({
            "pid": std::process::id(), "sessionId": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "cwd": "/src/example", "kind": "bg", "status": "idle"
        });
        assert!(
            session(dir.path(), &value, &HashMap::new(), false).is_none(),
            "a pid absent from the table is dropped as dead"
        );
        assert!(
            session(
                dir.path(),
                &value,
                &starts_from("/bin/ps", [me].into_iter()).unwrap(),
                false
            )
            .is_some(),
            "the same entry is live when the table reports its pid"
        );
    }

    // The signal itself is covered end to end by the runner suite, which spawns a
    // harness-named process. Keep this one process-free so it cannot flake.
    #[test]
    fn terminate_refuses_a_pid_that_runs_something_else() {
        assert!(
            terminate(std::process::id(), "codex")
                .unwrap_err()
                .to_string()
                .contains("refusing to signal a reused pid"),
            "a reused pid is left alone"
        );
        assert!(terminate(1, "codex").is_err(), "launchd is never a client");
    }
}
