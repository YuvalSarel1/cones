//! Fleet: every Claude Code session on this Mac, read from the registry Claude itself keeps,
//! `~/.claude/sessions/<pid>.json`, plus each session's transcript for title, last reply, model,
//! timestamps and tokens. Codex sessions join through [`crate::codex`], from the process table
//! and Codex's rollout files. cones installs nothing into a session and runs nothing inside it.
//! Every value is something the harness wrote; a value it did not write is `None`, never a guess.
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
    /// Claude's own kind: `bg` for a daemon-owned background session, `interactive` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub cwd: PathBuf,
    /// `active`, `idle` or `blocked` (waiting on a permission, trust or user prompt).
    pub state: String,
    /// The `timestamp` of the first transcript line that carries one. Rows sort by this so they
    /// hold still while the session works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<DateTime<Utc>>,
    /// The `timestamp` of the last transcript line that carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<DateTime<Utc>>,
    /// The bare API model id Claude wrote on the last message with usage, verbatim.
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
    /// The prompt size Claude reported on the last message with usage: input plus cache creation
    /// and cache read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// The window the harness states: Codex's `model_context_window` in the rollout; for Claude,
    /// `context_window.context_window_size` from the statusLine payload, which only a statusLine
    /// command sees, so it is read from `<claude dir>/statusline/<session id>.json` when that
    /// command saved it there (see docs/harness.md). None when nothing reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Claude's own session title (`ai-title`, or a user-set `agent-name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// First line of the assistant's most recent text: what the session is doing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    /// The folder's orchestrator. The start-orchestrator skill writes
    /// `<claude dir>/orchestrator/<sha1 of the folder>.json` naming its session pid and cwd
    /// every tick; this session's pid and cwd match it. A title is never the evidence.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub coordinator: bool,
    /// One entry per transcript line that carries a timestamp, oldest first: what the
    /// `sparkline` column counts. Not in `cones ls --json`.
    #[serde(skip)]
    pub activity: Vec<Activity>,
}

/// One transcript line the harness wrote, as the sparkline counts it: the line itself, the
/// assistant messages, tool calls and output tokens it carried.
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

/// The sparkline's buckets for one session, oldest first: `metric` summed over its activity in
/// each `bucket` of seconds. The edges sit on the clock, a 1m bucket running from :00 to :59,
/// not back from the instant of the reload: measured from `now` itself the edges slid a
/// second per reload and lines near one hopped between bars, so the row danced. Pinned, the
/// bars step left once per bucket and only the newest one grows. A line after `now` counts in
/// the newest bucket.
pub fn buckets(
    activity: &[Activity],
    spark: &crate::config::Sparkline,
    now: DateTime<Utc>,
) -> Vec<u64> {
    let secs = spark.bucket_seconds().unwrap_or(60) as i64;
    // The last second of the current bucket.
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

/// Every session's sparkline cell by session id, drawn against one bound: the busiest bucket
/// on screen for `fleet` and `log`, the row's own for `row`, the number given otherwise. A
/// bucket with nothing in it is the lowest bar; one over a fixed bound is the highest.
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

/// One bar per value, the lowest for nothing and the highest at or over `bound`. The highest
/// is `▇`, not `█`: the full block touches the row above and the chart bleeds into it.
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
    /// A harness that runs in someone else's terminal cannot be joined from here: a Codex TUI,
    /// or an interactive Claude, which `claude attach` does not know (it takes background jobs
    /// only). Background Claude and Codex daemon threads open fine.
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
pub const STATES: [&str; 6] = ["active", "idle", "blocked", "done", "failed", "stopped"];

/// Claude's config directory: `$CLAUDE_CONFIG_DIR`, the same override Claude Code honors, or
/// `~/.claude`. Holds `sessions/`, `projects/` and `jobs/`.
pub fn claude_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Ok(dirs::home_dir()
        .context("missing home directory")?
        .join(".claude"))
}

/// Every live session in Claude's registry, oldest first by start time; a session whose
/// transcript reports no start sorts last, by id. A file whose process is gone, or whose pid now
/// belongs to another process, is a crashed session and is skipped; unparsable files are skipped
/// too, since Claude may be mid-write on one.
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

/// The live orchestrators as (pid, folder), from the status files the start-orchestrator skill
/// rewrites every tick under `<claude dir>/orchestrator/`. Both must match a session: a stale
/// file whose pid was reused names some other process, but not one in the same folder.
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

/// Start time of each live pid as `ps` prints it under UTC. Claude writes that same text to the
/// registry as `procStart`, so the two compare as strings and a reused pid never passes for the
/// session that had it. One `ps` per refresh covers every entry.
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

/// One registry entry as a fleet row. Pure apart from the transcript read; `starts` is the
/// process table from `process_starts`.
fn session(
    dir: &Path,
    v: &Value,
    starts: &HashMap<u32, String>,
    read_transcript: bool,
) -> Option<Session> {
    let pid = v["pid"].as_u64()? as u32;
    let id = v["sessionId"].as_str()?;
    // A warm spare the daemon keeps ready for the next `claude --bg` has an entry too; it is
    // no one's session until claimed, and `claude agents` hides it as well.
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
        // The folder `claude agents` files the row under: a background job's launch directory
        // from its own state, since EnterWorktree rewrites the registry cwd to the worktree.
        cwd: job["cwd"].as_str().map(PathBuf::from).unwrap_or(cwd),
        // A new prompt flips the registry to busy at once; Claude rewrites a finished job's
        // state.json only with its first progress note, tens of seconds later, so busy is read
        // before the job's own done. Then the job state, the registry status, and a job whose
        // tempo is blocked. A status this version does not know renders as Claude's own word,
        // never as a guess.
        state: match (job["state"].as_str(), v["status"].as_str().unwrap_or("-")) {
            (_, "busy" | "shell") => "active",
            (Some(done @ ("done" | "failed" | "stopped")), _) => done,
            (_, "blocked" | "waiting" | "needs_user" | "needs_trust") => "blocked",
            _ if job["tempo"].as_str() == Some("blocked") => "blocked",
            (_, other) => other,
        }
        .into(),
        // Start, last activity, model and context are the transcript's own words; the registry
        // `startedAt` and `updatedAt` and the file's mtime are not read for them.
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
        // Then the job's own name, the one `claude agents` shows: a claimed spare keeps its
        // 8-hex id as the registry name until Claude renames it, and a short job never gets an
        // ai-title.
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
        // A background job's one-line status from Claude beats the transcript's last text.
        last: job["detail"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(Into::into)
            .or(d.last),
        coordinator: false,
        activity: d.report.activity,
    })
}
/// `context_window.context_window_size` from the statusLine payload the user's statusLine command
/// saved as `<claude dir>/statusline/<session id>.json`; None when it saved nothing.
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

/// Title, last reply, model, timestamps and token counts from a transcript, recomputed only
/// when the file grew. The dashboard reloads every second and the count reads the whole file.
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

/// Print the last assistant lines of a session transcript and, with `follow`, each new one as it
/// lands. Ctrl+C returns; the session in its own terminal is untouched.
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

/// The last user prompt and the assistant's full reply to it, as pane lines: prompt lines
/// quoted with `> `, a blank, then every assistant text since. Tool results are user messages
/// too; only text counts as a prompt.
pub fn exchange(transcript: &Path) -> Vec<String> {
    exchanges(transcript, 1)
}

/// The last `n` exchanges of a transcript, oldest first, each rendered as `exchange` renders
/// one and separated by a blank line. Only what the transcript records appears: user text and
/// assistant text; a turn that was all tool calls shows its prompt alone. Reads the tail of the
/// file, 1 MiB for one exchange, 4 MiB for more.
pub fn exchanges(transcript: &Path, n: usize) -> Vec<String> {
    // ponytail: an exchange older than the last few MiB is not what the pane is for.
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
            // A Codex rollout: user turns are `input_text` blocks, replies `output_text`. Codex
            // also files its environment and instruction blocks as user messages; they are
            // tagged XML, not something the user typed.
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
                // Codex opens a reply with a newline; the pane already separates replies.
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

/// The first non-empty line of a reply, bold markers dropped: what a one-line cell shows.
pub fn headline(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.replace("**", ""))
}

/// Title and assistant headlines in transcript lines, Claude's or Codex's.
fn scan(lines: &str) -> (Option<String>, Vec<String>) {
    let mut out = (None, Vec::new());
    for line in lines.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v["type"].as_str() {
            Some("ai-title") => out.0 = v["aiTitle"].as_str().map(Into::into),
            Some("agent-name") => out.0 = v["agentName"].as_str().map(Into::into),
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
    out
}

/// What one pass over a transcript reads out of Claude's own lines.
#[derive(Default, Clone)]
struct Report {
    /// The user's first instruction, used while Claude has not supplied a descriptive name.
    first_prompt: Option<String>,
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
    /// The prompt size on the last message with usage.
    context: Option<u64>,
    /// `message.model` on that same message, verbatim.
    model: Option<String>,
    /// The `timestamp` on the first and the last line that carries one.
    started: Option<DateTime<Utc>>,
    last_activity: Option<DateTime<Utc>>,
    /// Every line with a timestamp, with the messages, tool calls and output tokens on it.
    activity: Vec<Activity>,
}

/// Total input and output tokens in a Claude transcript, the last message's prompt size as the
/// context in use, the model id on that message, and the first and last line timestamps.
/// Streaming writes one line per content block with the same message id and usage, so each
/// message is counted once. A message whose model is `<synthetic>` is Claude's own placeholder
/// for a turn no model answered (all-zero usage); it is not a report and is skipped.
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

/// Every live session of every harness, oldest first by start time: Claude's registry plus
/// Codex's process table and rollouts.
pub fn all(claude: &Path) -> Result<Vec<Session>> {
    let mut out = sessions(claude)?;
    out.extend(crate::codex::sessions(&crate::codex::home(claude)));
    sort(&mut out);
    Ok(out)
}

/// Fleet order: oldest start first, unknown starts last, the id breaking ties. Every list of
/// rows goes through here, so rows joined from another source fall into place by age.
pub fn sort(out: &mut [Session]) {
    out.sort_by(|a, b| {
        (a.started.is_none(), a.started, &a.session_id).cmp(&(
            b.started.is_none(),
            b.started,
            &b.session_id,
        ))
    });
}

/// What the fleet view shows, so `logs`, `attach` and `stop` act on every visible row.
pub fn find(claude: &Path, session_id: &str) -> Result<Option<Session>> {
    Ok(all(claude)?
        .into_iter()
        .find(|s| s.session_id == session_id))
}

/// Control needs the target's current identity and owner, not every session's transcript.
/// Keep the registry's pid/start check, including spare and reused-pid rejection.
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
        .find(|s| s.session_id == session_id))
}

/// Stop the harness behind a fleet session. Returns false when the process is already
/// gone. The pid came from the registry, so the command is checked first: a reused pid never
/// gets signalled.
pub fn stop(claude: &Path, session_id: &str) -> Result<bool> {
    let session = control_session(claude, session_id)?.context("no such run or session")?;
    // A background session belongs to Claude's daemon, which respawns a worker whose process
    // dies (`attempt` in ~/.claude/daemon/roster.json). `claude stop` ends it but leaves the
    // job record, so `claude agents` keeps listing it as stopped; `claude rm` ends it and
    // drops the record, what ctrl+x does in `claude agents`. The transcript stays, so
    // `claude --resume <session>` still has the conversation.
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

/// True when a Claude settings file still carries entries from the removed `cones hook`. Each
/// would run a command the binary no longer has, on every event of every session.
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

/// Time since a reported instant as a table cell: `4s`, `6m`, `2h`, `3d`.
pub fn age(since: DateTime<Utc>) -> String {
    let s = (Utc::now() - since).num_seconds().max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// Dollars for a table cell: cents, or four places when a run cost less than a cent.
pub fn cost(usd: f64) -> String {
    if usd < 0.01 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

/// The prompt size the harness reported on the session's last message over the window it
/// stated: "98k/200k", or "98k" alone when nothing stated a window. The denominator is never
/// guessed: 200k, or 1M from a `[1m]` in settings.json, once rendered live sessions at 194%.
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

/// `~/x` for paths under the home directory, so the column fits.
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
        // Three timestamped lines: a user prompt, a streamed reply in two blocks (one usage,
        // one tool call) and a tool result four minutes later.
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
        // Buckets sit on the clock: 10:00 to 10:05 inclusive at 10:05:00. Oldest left: the
        // prompt and the reply's two lines in the 10:00 minute, quiet, the tool result in the
        // 10:04 minute, nothing yet in 10:05.
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
        // Any second inside the same minute sees the same bars; the next minute steps left.
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
}
