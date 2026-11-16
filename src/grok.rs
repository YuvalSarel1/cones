//! Grok Build: live rows from its processes and the session folder each one holds open;
//! history, `cones show` and the columns from that folder. Grok keeps one folder per session,
//! `sessions/<url-encoded cwd>/<id>/`, holding `summary.json` (id, folder, start, title, model,
//! effort), `updates.jsonl` (the conversation), `events.jsonl` (turns and phases, open for
//! writing while a client holds the session), `usage.json` (tokens and dollar ticks) and
//! `signals.json` (the context window). Nothing here writes to that folder.
use crate::{
    config::HarnessKind,
    cost::Info,
    fleet::{Activity, Session},
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

/// How much of the end of `events.jsonl` and `updates.jsonl` a refresh reads. The last turn
/// event and the last reply sit at the end; a streaming phase line is under a hundred bytes.
const TAIL: u64 = 256 * 1024;
/// The file a session's conversation is read from; history lists a folder by this file.
pub const TRANSCRIPT: &str = "updates.jsonl";
/// Grok counts dollars in ticks: 10^10 to the dollar.
const TICKS_PER_USD: f64 = 1e10;

fn spec() -> &'static crate::harness::spec::HarnessSpec {
    crate::harness::spec(HarnessKind::Grok)
}

fn json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn time(v: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v.as_str()?)
        .ok()
        .map(Into::into)
}

/// The whole lines in the last `TAIL` bytes of a file, or in all of a shorter one.
fn tail(path: &Path) -> Vec<Value> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let from = len.saturating_sub(TAIL);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(from)).is_err() || file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let mut lines = bytes.split(|b| *b == b'\n');
    if from > 0 {
        lines.next();
    }
    lines
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect()
}

/// A session folder's own record: its id, folder and start. `None` for anything that is not
/// a top-level session folder, including a child Grok keeps hidden, such as a subagent's.
fn summary(dir: &Path) -> Option<(String, PathBuf, Option<DateTime<Utc>>, Value)> {
    let v = json(&dir.join("summary.json"))?;
    let id = v["info"]["id"].as_str()?;
    uuid::Uuid::parse_str(id).ok()?;
    if dir.file_name()? != id || v["hidden"] == true || v["session_kind"] == "subagent" {
        return None;
    }
    let cwd = PathBuf::from(v["info"]["cwd"].as_str().filter(|c| !c.is_empty())?);
    Some((id.to_owned(), cwd, time(&v["created_at"]), v))
}

/// History's identity for a transcript: the folder's `updates.jsonl` and its summary.
pub(crate) fn meta(path: &Path) -> Option<(String, PathBuf, Option<DateTime<Utc>>)> {
    if path.file_name()? != TRANSCRIPT {
        return None;
    }
    let (id, cwd, started, _) = summary(path.parent()?)?;
    Some((id, cwd, started))
}

/// The title Grok generated for the session, or the one `/rename` gave it.
pub(crate) fn title(dir: &Path) -> Option<String> {
    let (.., summary) = summary(dir)?;
    crate::fleet::headline(summary["generated_title"].as_str()?)
}

/// What a session folder reports: the columns a row and a history entry show.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Report {
    pub id: String,
    pub cwd: PathBuf,
    pub started: Option<DateTime<Utc>>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub forked_from: Option<String>,
    pub state: Option<&'static str>,
    pub last_activity: Option<DateTime<Utc>>,
    pub last: Option<String>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub cost_usd: Option<f64>,
    pub cost_info: Option<Info>,
    pub activity: Vec<Activity>,
}

/// Read a session folder. A file Grok has not written yet, or one it is halfway through,
/// leaves its columns absent rather than failing the row.
pub(crate) fn report(dir: &Path) -> Option<Report> {
    let (id, cwd, started, summary) = summary(dir)?;
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let mut r = Report {
        id,
        cwd,
        started,
        title: text(&summary["generated_title"]).and_then(|t| crate::fleet::headline(&t)),
        model: text(&summary["current_model_id"]),
        effort: text(&summary["reasoning_effort"]),
        forked_from: text(&summary["parent_session_id"]),
        last_activity: time(&summary["last_active_at"]),
        ..Report::default()
    };
    let rules = &spec().state;
    for event in tail(&dir.join("events.jsonl")) {
        if let Some(state) = rules.read(&event) {
            r.state = Some(state);
        }
        if let Some(at) = time(&event["ts"]) {
            r.last_activity = r.last_activity.max(Some(at));
            let mut activity = Activity::at(at);
            activity.tools = u64::from(event["type"] == "tool_completed");
            activity.messages = u64::from(event["type"] == "turn_ended");
            r.activity.push(activity);
        }
    }
    let messages = &spec().transcript.messages;
    let updates = tail(&dir.join(TRANSCRIPT));
    r.last = updates
        .iter()
        .rev()
        .find_map(|v| messages.assistant.headline(v));
    if r.title.is_none() {
        r.title = first_prompt(&dir.join(TRANSCRIPT));
    }
    if let Some(usage) = json(&dir.join("usage.json")) {
        let session = &usage["session"];
        r.tokens_in = session["inputTokens"].as_u64();
        r.tokens_out = session["outputTokens"].as_u64();
        let native = session["costUsdTicks"]
            .as_u64()
            .map(|ticks| ticks as f64 / TICKS_PER_USD);
        (r.cost_usd, r.cost_info) = crate::cost::prefer_native(native, (None, None));
    }
    if let Some(signals) = json(&dir.join("signals.json")) {
        r.context_tokens = signals["contextTokensUsed"].as_u64();
        r.context_window = signals["contextWindowTokens"].as_u64().filter(|w| *w > 0);
    }
    Some(r)
}

/// The first instruction, for a session Grok has not titled yet.
fn first_prompt(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let user = &spec().transcript.messages.user;
    std::io::BufReader::new(File::open(path).ok()?)
        .lines()
        .take(200)
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .find_map(|v| user.headline_with_attachments(&v, true))
}

/// The top-level session folder a live client holds `events.jsonl` open in: Grok opens it
/// for writing when the client takes the session and closes it when the client lets it go,
/// including on `/new` and `/resume`. None when it holds no such file, or several.
#[cfg(target_os = "macos")]
fn held(pid: u32, root: &Path) -> Option<PathBuf> {
    let mut held = crate::process_info::open_files(pid)
        .ok()?
        .into_iter()
        .filter_map(|file| file.path)
        .filter(|path| path.file_name().is_some_and(|n| n == "events.jsonl"))
        .filter_map(|path| Some(path.parent()?.to_owned()))
        .filter(|dir| dir.starts_with(root) && summary(dir).is_some())
        .collect::<Vec<_>>();
    held.dedup();
    (held.len() == 1).then(|| held.remove(0))
}

#[cfg(target_os = "macos")]
pub fn sessions(home: &Path) -> Result<Vec<Session>> {
    if !home.is_dir() {
        return Ok(Vec::new());
    }
    let table = crate::fleet::pass_table("/bin/ps")?;
    let processes = spec().discovery.processes(&table);
    if processes.is_empty() {
        return Ok(Vec::new());
    }
    let own = crate::fleet::own_home_processes(
        "/bin/ps",
        HarnessKind::Grok,
        home,
        &processes.iter().map(|p| p.pid).collect::<Vec<_>>(),
    );
    // Open-file paths are real paths, so the root is compared as one.
    let root = spec().transcript.live_path(home);
    let root = root.canonicalize().unwrap_or(root);
    Ok(processes
        .into_iter()
        .filter(|p| own.contains(&p.pid))
        .filter_map(|p| {
            let cwd = crate::process_info::cwd(p.pid)?;
            let dir = held(p.pid, &root);
            let r = dir.as_deref().and_then(report).unwrap_or_default();
            let native = !r.id.is_empty();
            Some(Session {
                session_id: format!("grok-{}", p.pid),
                native_id: native.then(|| r.id.clone()),
                harness: "grok".into(),
                kind: None,
                cwd,
                moved_to: None,
                state: r.state.unwrap_or("-").into(),
                started: Some(p.started),
                last_activity: r.last_activity,
                model: r.model,
                next_model: None,
                pid: Some(p.pid),
                transcript_path: dir.map(|d| d.join(TRANSCRIPT)),
                tokens_in: r.tokens_in,
                tokens_out: r.tokens_out,
                context_tokens: r.context_tokens,
                context_window: r.context_window,
                cost_usd: r.cost_usd,
                cost_info: r.cost_info,
                effort: r.effort,
                bypass: None,
                usage: None,
                tasks: None,
                plan: None,
                title: r.title,
                last: r.last,
                coordinator: false,
                forked_from: r.forked_from,
                forked_from_harness: None,
                launched_by: None,
                activity: r.activity,
            })
        })
        .collect())
}

#[cfg(not(target_os = "macos"))]
pub fn sessions(_home: &Path) -> Result<Vec<Session>> {
    anyhow::bail!("native process discovery requires macOS")
}
