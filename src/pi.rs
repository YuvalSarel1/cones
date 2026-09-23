//! pi fleet discovery from processes and session files. pi overwrites argv, so
//! files are matched by cwd and write time; multiple processes in one cwd are ambiguous.
//! pi supports neither attach nor supervised cones jobs.
pub(crate) mod reporting;
use crate::{
    cost::{Adapter, Reader, Reading, Response},
    fleet::{Activity, Session},
};
use chrono::{DateTime, Utc};
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
    crate::harness::spec(crate::config::HarnessKind::Pi)
        .home
        .resolve(claude)
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
    /// Native costs summed, with catalog estimates for unpriced responses when available.
    pub cost_usd: f64,
    pub(crate) costs: crate::cost::Accounting<CostAdapter>,
    /// The name `--name` or `/name` set, from the last `session_info` entry.
    pub name: Option<String>,
    pub prompt: Option<String>,
    pub last_activity: Option<DateTime<Utc>>,
    pub activity: Vec<Activity>,
}

/// Skip process discovery when the pi home is absent.
pub fn sessions(pi: &Path) -> anyhow::Result<Vec<Session>> {
    sessions_from("/bin/ps", pi)
}

/// `sessions` against a named `ps`, so a test can point it at one that cannot run.
pub fn sessions_from(ps: &str, pi: &Path) -> anyhow::Result<Vec<Session>> {
    if !pi.is_dir() {
        return Ok(Vec::new());
    }
    let table = crate::fleet::pass_table(ps)?;
    let mut procs = processes(&table);
    if procs.is_empty() {
        return Ok(Vec::new());
    }
    let own = crate::fleet::own_home_processes(
        ps,
        crate::config::HarnessKind::Pi,
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
    let cwds = crate::codex::cwds(&lsof);
    for p in &mut procs {
        if p.cwd.is_none() {
            p.cwd = cwds.get(&p.pid).cloned();
        }
    }
    Ok(rows(pi, &procs))
}

/// Parse pi's process title. It erases subcommands, so sessions and updates look alike;
/// `pi-rpc` is excluded.
pub fn processes(ps: &str) -> Vec<Process> {
    crate::harness::spec(crate::config::HarnessKind::Pi)
        .discovery
        .processes(ps)
        .into_iter()
        .map(|line| Process {
            pid: line.pid,
            started: line.started,
            cwd: None,
        })
        .collect()
}

/// Folder names flatten `/` and `:` alike; verify cwd from the file header to resolve collisions.
pub fn session_dir(pi: &Path, cwd: &Path) -> PathBuf {
    let name = cwd
        .to_string_lossy()
        .trim_start_matches('/')
        .replace(['/', ':'], "-");
    let transcript = &crate::harness::spec(crate::config::HarnessKind::Pi).transcript;
    if let Some(directory) = transcript.live_scan_root().override_dir() {
        return directory;
    }
    transcript.live_path(pi).join(format!("--{name}--"))
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
    tail_priced(lines, None)
}

pub(crate) fn tail_priced(lines: &str, catalog: Option<&crate::cost::Catalog>) -> Tail {
    let mut t = Tail::default();
    for line in lines.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let spec = crate::harness::spec(crate::config::HarnessKind::Pi);
        if let Some(state) = spec.state.read(&v) {
            t.state = Some(state);
        }
        if t.prompt.is_none() {
            t.prompt = spec
                .transcript
                .messages
                .user
                .headline_with_attachments(&v, true);
        }
        if let Some(last) = spec.transcript.messages.assistant.headline(&v) {
            t.last = Some(last);
        }
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
        if m["role"] == "assistant" {
            t.costs.observe(&v, catalog);
            if let Some(model) = m["model"].as_str() {
                t.model = Some(model.to_owned());
            }
            let u = &m["usage"];
            let n = |k: &str| u[k].as_u64().unwrap_or(0);
            let prompt = n("input") + n("cacheRead") + n("cacheWrite");
            t.tokens_in += prompt;
            t.tokens_out += n("output");
            t.context_tokens = Some(prompt);
            t.cost_usd = t.costs.report(None).0.unwrap_or(0.0);
            if let Some(a) = t.activity.last_mut() {
                a.messages += 1;
                a.tokens_out += n("output");
                a.tools += m["content"]
                    .as_array()
                    .map_or(0, |c| c.iter().filter(|b| b["type"] == "toolCall").count())
                    as u64;
            }
        }
    }
    t
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CostAdapter;

impl Adapter for CostAdapter {
    fn read<'a>(&'a mut self, event: &'a Value) -> Reading<'a> {
        if event["type"] != "message" || event["message"]["role"] != "assistant" {
            return Reading::Ignore;
        }
        let message = &event["message"];
        let usage = &message["usage"];
        Reading::Response(Response {
            id: event["id"].as_str(),
            reported_usd: usage["cost"]["total"].as_f64(),
            empty: ["input", "output", "cacheRead", "cacheWrite"]
                .iter()
                .all(|k| usage[k].as_u64() == Some(0)),
            usage: (|| {
                // The shared catalog has no separate long-retention cache-write rate.
                if usage["cacheWrite1h"].as_u64().is_some_and(|n| n > 0) {
                    return Err("unsupported_cache_retention");
                }
                Ok(crate::cost::Usage {
                    provider: message["provider"]
                        .as_str()
                        .ok_or("missing_provider_or_model")?,
                    model: message["model"]
                        .as_str()
                        .ok_or("missing_provider_or_model")?,
                    input: usage["input"].as_u64().ok_or("missing_counters")?,
                    output: usage["output"].as_u64().ok_or("missing_counters")?,
                    cache_read: usage["cacheRead"].as_u64().ok_or("missing_counters")?,
                    cache_write: usage["cacheWrite"].as_u64().ok_or("missing_counters")?,
                })
            })(),
            gap: None,
        })
    }
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
            row(p, file)
        })
        .collect();
    out.sort_by_key(|s| s.started);
    out
}

/// Read only the id supplied to this owned CLI through --session-id.
pub fn session_for_id(home: &Path, process: &Process, id: &str) -> Option<Session> {
    use std::io::{BufRead, BufReader, Read};
    let cwd = process.cwd.as_deref()?;
    let matches: Vec<_> = fs::read_dir(session_dir(home, cwd))
        .ok()?
        .flatten()
        .filter_map(|e| {
            if !e.file_type().ok()?.is_file() || e.path().extension()? != "jsonl" {
                return None;
            }
            let file = fs::File::open(e.path()).ok()?;
            let mut line = String::new();
            BufReader::new(file.take(64 * 1024))
                .read_line(&mut line)
                .ok()?;
            let meta = meta(&line)?;
            (meta.session_id == id && meta.cwd == cwd).then(|| (e.path(), meta))
        })
        .collect();
    let [file] = matches.as_slice() else {
        return None;
    };
    Some(row(process, Some(file.clone())))
}

fn row(p: &Process, file: Option<(PathBuf, Meta)>) -> Session {
    let t = file
        .as_ref()
        .map(|(path, _)| tail_of(path))
        .unwrap_or_default();
    let (cost_usd, cost_info) = t.costs.report(None);
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
        cost_usd,
        cost_info,
        last: t.last,
        effort: None,
        usage: None,
        coordinator: false,
        forked_from: None,
        activity: t.activity,
        moved_to: None,
    }
}

/// Choose the latest file written since process start and verify its header cwd and start.
/// `--continue` reopens old files, so their creation time cannot identify the process, but a
/// header stamped after this process booted opened a later session, whose own process may
/// already be gone: its file keeps the newest write time in the folder and would be stolen.
fn session_file(pi: &Path, cwd: &Path, p: &Process) -> Option<(PathBuf, Meta)> {
    // `ps` prints whole seconds and pi writes the header a moment after it starts.
    let booted = p.started + chrono::Duration::seconds(5);
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
        if let Some(m) = meta_of(&path).filter(|m| m.cwd == cwd && m.started <= booted) {
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

/// Recount when the file or pricing snapshot changes, including catalog arrival and expiry.
fn tail_of(path: &Path) -> Tail {
    let catalog = crate::cost::snapshot();
    tail_of_priced(path, catalog.as_deref())
}

fn tail_of_priced(path: &Path, catalog: Option<&crate::cost::Catalog>) -> Tail {
    use std::os::unix::fs::MetadataExt;
    type FileStamp = (u64, u64, i64, i64);
    type CachedTail = (Option<FileStamp>, Tail, Option<crate::cost::CatalogStamp>);
    static CACHE: Mutex<Option<HashMap<PathBuf, CachedTail>>> = Mutex::new(None);
    let stamp = catalog.map(|c| c.stamp.clone());
    let file_stamp = fs::metadata(path)
        .ok()
        .map(|m| (m.ino(), m.len(), m.mtime(), m.mtime_nsec()));
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some((_, t, _)) = cache
        .get(path)
        .filter(|(seen, _, priced_with)| *seen == file_stamp && *priced_with == stamp)
    {
        return t.clone();
    }
    let t = fs::read_to_string(path)
        .map(|s| tail_priced(&s, catalog))
        .unwrap_or_default();
    cache.insert(path.to_owned(), (file_stamp, t.clone(), stamp));
    t
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn unpriced_message(id: &str) -> Value {
        json!({"type":"message","id":id,"message":{
            "role":"assistant","provider":"provider","model":"model",
            "usage":{"input":30,"output":10,"cacheRead":40,"cacheWrite":30,
                "cost":{"total":0}}
        }})
    }

    #[test]
    fn unpriced_responses_use_their_own_model_caches_and_context_tier_once() {
        let catalog = crate::cost::tests::fixture();
        let first = unpriced_message("one");
        let mut second = unpriced_message("two");
        second["message"]["usage"]["input"] = json!(31);
        let mut third = unpriced_message("three");
        third["message"]["model"] = json!("other_model");
        third["message"]["usage"]
            .as_object_mut()
            .unwrap()
            .remove("cost");
        let t = tail_priced(
            &format!("{first}\n{first}\n{second}\n{third}\n"),
            Some(&catalog),
        );
        let (usd, info) = t.costs.report(None);
        // Ordinary input excludes both caches; the second response crosses the fixture's tier.
        assert!((usd.unwrap() - (0.00025 + 0.000464 + 0.000105)).abs() < 1e-12);
        let info = info.unwrap();
        assert_eq!(info.source, crate::cost::Source::ModelsDev);
        assert_eq!(info.coverage, crate::cost::Coverage::Complete);
        assert_eq!(info.priced_records, 3);
        assert_eq!(info.catalog, Some(catalog.stamp));
        assert!(crate::cost::display(usd, Some(&info)).starts_with("~$"));
    }

    #[test]
    fn native_prices_and_explicit_empty_zero_win_over_catalog_estimates() {
        let catalog = crate::cost::tests::fixture();
        let mut reported = unpriced_message("reported");
        reported["message"]["usage"]["cost"]["total"] = json!(0.2);
        let mut empty = unpriced_message("empty");
        for key in ["input", "output", "cacheRead", "cacheWrite"] {
            empty["message"]["usage"][key] = json!(0);
        }
        let native = tail_priced(&format!("{reported}\n{empty}\n"), Some(&catalog));
        let (usd, info) = native.costs.report(None);
        assert_eq!(usd, Some(0.2));
        let info = info.unwrap();
        assert_eq!(info.source, crate::cost::Source::Harness);
        assert_eq!(info.priced_records, 2);
        assert_eq!(info.catalog, None);
        assert_eq!(crate::cost::display(usd, Some(&info)), "$0.20");

        let estimate = unpriced_message("estimated");
        let mixed = tail_priced(
            &format!("{reported}\n{empty}\n{estimate}\n"),
            Some(&catalog),
        );
        let (usd, info) = mixed.costs.report(None);
        assert!((usd.unwrap() - 0.20025).abs() < 1e-12);
        assert_eq!(info.unwrap().source, crate::cost::Source::ModelsDev);
    }

    #[test]
    fn incomplete_or_unknown_pi_usage_keeps_the_subtotal_partial() {
        let catalog = crate::cost::tests::fixture();
        let priced = unpriced_message("priced");
        let missing = unpriced_message("missing");
        let mut cases = Vec::new();
        for key in ["provider", "model"] {
            let mut event = missing.clone();
            event["message"].as_object_mut().unwrap().remove(key);
            cases.push((event, "missing_provider_or_model"));
        }
        for key in ["input", "output", "cacheRead", "cacheWrite"] {
            let mut event = missing.clone();
            event["message"]["usage"]
                .as_object_mut()
                .unwrap()
                .remove(key);
            cases.push((event, "missing_counters"));
        }
        for (model, reason) in [
            ("unknown", "unknown_provider_or_model"),
            ("missing-cache", "missing_rate"),
            ("free-or-unknown", "unpriced_model"),
        ] {
            let mut event = missing.clone();
            event["message"]["model"] = json!(model);
            cases.push((event, reason));
        }
        for (event, reason) in cases {
            let t = tail_priced(&format!("{priced}\n{event}\n"), Some(&catalog));
            let (usd, info) = t.costs.report(None);
            assert!((usd.unwrap() - 0.00025).abs() < 1e-12);
            let info = info.unwrap();
            assert_eq!(info.coverage, crate::cost::Coverage::Partial);
            assert_eq!(info.unpriced_reasons.get(reason), Some(&1));
            assert_eq!(crate::cost::display(usd, Some(&info)), "~$0.0003");
            assert!(
                tail_priced(&event.to_string(), Some(&catalog))
                    .costs
                    .report(None)
                    .0
                    .is_none()
            );
        }
    }

    #[test]
    fn cached_pi_cost_replays_on_catalog_arrival_refresh_and_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        fs::write(&path, format!("{}\n", unpriced_message("one"))).unwrap();
        let mut catalog = crate::cost::tests::fixture();
        let report = |catalog: Option<&crate::cost::Catalog>| {
            tail_of_priced(&path, catalog).costs.report(None)
        };
        let (usd, info) = report(None);
        assert!(usd.is_none());
        assert_eq!(info.unwrap().unpriced_reasons["catalog_unavailable"], 1);
        let (usd, info) = report(Some(&catalog));
        assert!((usd.unwrap() - 0.00025).abs() < 1e-12);
        assert_eq!(info.unwrap().catalog, Some(catalog.stamp.clone()));
        catalog.stamp.fetched_at += chrono::Duration::seconds(1);
        assert_eq!(
            report(Some(&catalog)).1.unwrap().catalog,
            Some(catalog.stamp.clone())
        );
        assert!(report(None).0.is_none());
    }

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
            // `ps` truncates to the second, so the header lands just after the process start.
            started: "2026-08-25T14:06:44Z".parse().unwrap(),
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

    #[test]
    fn a_session_opened_after_the_process_is_never_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let pi = dir.path();
        let cwd = Path::new("/src/one");
        let folder = session_dir(pi, cwd);
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("2026-08-25T14-06-44-035Z_01a0393e-ad43.jsonl"),
            SESSION,
        )
        .unwrap();
        // A second pi in the same folder, closed again: its file keeps the newest write time.
        fs::write(
            folder.join("2026-08-25T14-20-00-000Z_01a0393f-be54.jsonl"),
            SESSION
                .replace("01a0393e-ad43", "01a0393f-be54")
                .replace("14:06:4", "14:20:0")
                .replace("fix the flaky test", "rename the module"),
        )
        .unwrap();
        let p = Process {
            pid: 7,
            started: "2026-08-25T14:06:44Z".parse().unwrap(),
            cwd: Some(cwd.into()),
        };
        let row = &rows(pi, &[p])[0];
        assert_eq!(row.session_id, "01a0393e-ad43");
        assert_eq!(row.title.as_deref(), Some("fix the flaky test"));
    }
}
