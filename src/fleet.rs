//! Fleet discovery from harness-owned registries and transcripts. Missing reports
//! remain absent; state and usage are never estimated. See docs/harness.md.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    /// Matched by pid and cwd against the coordinator skill's status file, never by title.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub coordinator: bool,
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
    spark: &crate::config::Sparkline,
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
    spark: &crate::config::Sparkline,
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
        match (self.harness.as_str(), self.kind.as_deref()) {
            ("claude", Some("interactive")) => true,
            ("claude", _) | ("codex", Some("daemon")) => false,
            _ => true,
        }
    }
}
fn claude() -> String {
    "claude".into()
}

pub fn claude_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Ok(dirs::home_dir()
        .context("missing home directory")?
        .join(".claude"))
}

/// Live registry sessions, oldest first. Skip malformed entries and mismatched pid/start pairs.
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

/// Batch process start times in Claude's UTC format to reject reused pids.
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
    let d = if read_transcript {
        transcript.as_deref().map(details).unwrap_or_default()
    } else {
        Details::default()
    };
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
        context_window: statusline_window(dir, id),
        cost_usd: None,
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

/// Read only the window saved by the user's statusLine command.
fn statusline_window(claude: &Path, id: &str) -> Option<u64> {
    let text = fs::read_to_string(claude.join("statusline").join(format!("{id}.json"))).ok()?;
    serde_json::from_str::<Value>(&text).ok()?["context_window"]["context_window_size"].as_u64()
}

#[derive(Default, Clone)]
struct Details {
    title: Option<String>,
    last: Option<String>,
    report: Report,
}

/// Cache by file length; a changed transcript requires a full usage recount.
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
        session.harness == "claude",
        "only Claude sessions can be renamed here"
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
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
    context: Option<u64>,
    model: Option<String>,
    started: Option<DateTime<Utc>>,
    last_activity: Option<DateTime<Utc>>,
    activity: Vec<Activity>,
}

/// Count streaming usage once per message id; skip `<synthetic>` placeholder messages.
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
            r.activity.push(Activity::at(t));
        }
        let message = &event["message"];
        // Tool calls are content blocks on assistant lines; streaming repeats a message's
        // usage per block but writes each block once.
        if event["type"] == "assistant"
            && let Some(blocks) = message["content"].as_array()
            && let Some(a) = r.activity.last_mut()
        {
            a.tools += blocks.iter().filter(|b| b["type"] == "tool_use").count() as u64;
        }
        if r.first_prompt.is_none() && event["type"] == "user" && event["isMeta"] != true {
            r.first_prompt = match &message["content"] {
                Value::String(text) => headline(text),
                Value::Array(blocks) => blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .find_map(headline),
                _ => None,
            };
        }
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
        if let Some(a) = r.activity.last_mut()
            && event["type"] == "assistant"
        {
            a.messages += 1;
            a.tokens_out += n("output_tokens");
        }
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

pub fn all(claude: &Path) -> Result<Vec<Session>> {
    let mut out = sessions(claude)?;
    out.extend(crate::codex::sessions(&crate::codex::home(claude)));
    out.extend(crate::pi::sessions(&crate::pi::home(claude)));
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
fn control_session(claude: &Path, session_id: &str) -> Result<Option<Session>> {
    match fs::read_dir(claude.join("sessions")) {
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
                    let starts = process_starts(value["pid"].as_u64().into_iter());
                    return Ok(session(claude, &value, &starts, false));
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(crate::codex::sessions(&crate::codex::home(claude))
        .into_iter()
        .chain(crate::pi::sessions(&crate::pi::home(claude)))
        .find(|s| s.session_id == session_id))
}

/// Verify process identity before signalling; return false when already gone.
pub fn stop(claude: &Path, session_id: &str) -> Result<bool> {
    let session = control_session(claude, session_id)?.context("no such run or session")?;
    // Use `claude rm`: the daemon respawns killed workers, and `stop` leaves a job record.
    // The transcript remains resumable.
    if session.kind.as_deref() == Some("bg") {
        let claude = crate::harness::executable("claude", &crate::harness::launch_path())
            .context("claude not found")?;
        let short = session_id.get(..8).context("invalid session id")?;
        let out = Command::new(claude)
            .args(["rm", short])
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
    match (s.context_tokens, s.context_window) {
        (Some(t), Some(w)) => format!("{}/{}", short(t), short(w)),
        (Some(t), None) => short(t),
        (None, _) => "-".into(),
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
        let spark = |metric: &str, bound: &str| crate::config::Sparkline {
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
            title: Some("Mine".into()),
            last: None,
            coordinator: false,
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
