//! OpenCode's native SQLite reports and terminal clients. Reads never run OpenCode,
//! migrate its database, change permissions, or infer a busy state from saved messages.
pub(crate) mod reporting;

use crate::{
    config::HarnessKind,
    cost::{Adapter, Reading, Response},
    fleet::{self, Session},
    history::{Columns, Entry, Key},
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    fs,
    io::Read,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

pub fn home(claude: &Path) -> PathBuf {
    crate::harness::spec(HarnessKind::Opencode)
        .home
        .resolve(claude)
}

/// One native home can contain stable and development-channel databases.
pub fn databases(home: &Path) -> Result<Vec<PathBuf>> {
    if let Some(value) = std::env::var_os("OPENCODE_DB").filter(|v| !v.is_empty()) {
        if value == ":memory:" {
            return Ok(Vec::new());
        }
        let path = home.join(value);
        return Ok(path.is_file().then_some(path).into_iter().collect());
    }
    let mut paths = Vec::new();
    if !home.is_dir() {
        return Ok(paths);
    }
    for entry in fs::read_dir(home)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if (name == "opencode.db" || (name.starts_with("opencode-") && name.ends_with(".db")))
            && entry.file_type()?.is_file()
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    file: (SystemTime, u64, u64),
    wal: Option<(SystemTime, u64, u64)>,
}

/// WAL writes need not change the main file's length or timestamp.
pub(crate) fn fingerprint(db: &Path) -> Result<Fingerprint> {
    // A database symlink's WAL lives beside its target, not beside the alias.
    let db = fs::canonicalize(db)?;
    let stamp = |path: &Path| -> std::io::Result<_> {
        let m = fs::metadata(path)?;
        Ok((m.modified()?, m.len(), m.ino()))
    };
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    let wal = match stamp(Path::new(&wal)) {
        Ok(stamp) => Some(stamp),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    Ok(Fingerprint {
        file: stamp(&db)?,
        wal,
    })
}

fn query(db: &Path, sql: &str) -> Result<Vec<Value>> {
    ensure!(db.is_file(), "OpenCode database no longer exists");
    let before = fingerprint(db)?;
    let db = fs::canonicalize(db)?;
    let mut header = [0; 20];
    fs::File::open(&db)?.read_exact(&mut header)?;
    // Apple's sqlite3 cannot initialize a missing WAL in readonly mode. A closed,
    // checkpointed WAL database can be read as an immutable snapshot. Never use
    // immutable with a WAL present, and reject a writer appearing during the read.
    let path = if before.wal.is_none() && header[18..20] == [2, 2] {
        let mut uri = String::from("file:");
        for &b in db.as_os_str().as_bytes() {
            if b.is_ascii_alphanumeric() || b"/-_.~:".contains(&b) {
                uri.push(b as char);
            } else {
                use std::fmt::Write;
                write!(&mut uri, "%{b:02X}")?;
            }
        }
        uri.push_str("?immutable=1");
        uri.into()
    } else {
        db.as_os_str().to_owned()
    };
    let rows = crate::sqlite::query(&path, sql).context("reading the OpenCode database")?;
    ensure!(
        fingerprint(&db)? == before,
        "OpenCode database changed while reading; retry"
    );
    Ok(rows)
}

fn valid_id(id: &str) -> bool {
    id.starts_with("ses_")
        && id.len() > 4
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn operand(id: &str) -> Result<String> {
    ensure!(valid_id(id), "invalid OpenCode session id");
    Ok(format!("'{id}'"))
}

fn millis(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(value.as_i64()?)
}

pub fn require_session(db: &Path, id: &str) -> Result<()> {
    ensure!(
        !query(
            db,
            &format!("SELECT id FROM session WHERE id = {}", operand(id)?)
        )?
        .is_empty(),
        "OpenCode session no longer exists; reload history"
    );
    Ok(())
}

/// A route report names a conversation, not a database. Resolve that exact id and
/// directory, retaining a known database when the viewer came from history.
pub fn session_database(
    home: &Path,
    id: &str,
    cwd: &Path,
    preferred: Option<&Path>,
) -> Result<PathBuf> {
    let paths = if let Some(path) = preferred {
        ensure!(path.is_file(), "the source database no longer exists");
        vec![path.to_owned()]
    } else {
        databases(home)?
    };
    let mut found = Vec::new();
    for path in paths {
        let rows = query(
            &path,
            &format!("SELECT directory FROM session WHERE id = {}", operand(id)?),
        )?;
        if rows.iter().any(|r| {
            r["directory"]
                .as_str()
                .is_some_and(|dir| Path::new(dir) == cwd)
        }) {
            found.push(path);
        }
    }
    ensure!(
        found.len() == 1,
        "OpenCode fork needs one database containing the reported conversation and directory"
    );
    Ok(found.remove(0))
}

/// The directory, title and timestamps are native fields, never filesystem fallbacks.
pub(crate) fn history(db: &Path, home: &Path) -> Result<Vec<Entry>> {
    entries(db, home, "")
}

fn entries(db: &Path, home: &Path, filter: &str) -> Result<Vec<Entry>> {
    Ok(query(
        db,
        &format!(
            "SELECT id, directory, title, time_created, time_updated, time_archived
         FROM session WHERE parent_id IS NULL {filter}"
        ),
    )?
    .into_iter()
    .filter_map(|v| {
        let id = v["id"].as_str().filter(|id| valid_id(id))?;
        let cwd = v["directory"].as_str().filter(|cwd| !cwd.is_empty())?;
        Some(Entry {
            key: Key {
                harness: "opencode".into(),
                home: home.to_owned(),
                session_id: id.into(),
            },
            cwd: cwd.into(),
            transcript: db.to_owned(),
            archived: !v["time_archived"].is_null(),
            started: millis(&v["time_created"]),
            last_activity: millis(&v["time_updated"]),
            title: v["title"].as_str().and_then(fleet::headline),
            columns: None,
            hit: None,
        })
    })
    .collect())
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CostAdapter;

impl Adapter for CostAdapter {
    fn read<'a>(&'a mut self, event: &'a Value) -> Reading<'a> {
        if event["role"] != "assistant" {
            return Reading::Ignore;
        }
        let tokens = &event["tokens"];
        Reading::Response(Response {
            id: event["id"].as_str(),
            reported_usd: event["cost"].as_f64(),
            empty: [
                "/input",
                "/output",
                "/reasoning",
                "/cache/read",
                "/cache/write",
            ]
            .iter()
            .all(|p| tokens.pointer(p).and_then(Value::as_u64) == Some(0)),
            usage: (|| {
                let n = |p| {
                    tokens
                        .pointer(p)
                        .and_then(Value::as_u64)
                        .ok_or("missing_counters")
                };
                Ok(crate::cost::Usage {
                    provider: event["providerID"]
                        .as_str()
                        .ok_or("missing_provider_or_model")?,
                    model: event["modelID"]
                        .as_str()
                        .ok_or("missing_provider_or_model")?,
                    input: n("/input")?,
                    // OpenCode separates reasoning tokens from ordinary output.
                    output: n("/output")?
                        .checked_add(n("/reasoning")?)
                        .ok_or("invalid_usage")?,
                    cache_read: n("/cache/read")?,
                    cache_write: n("/cache/write")?,
                })
            })(),
            gap: None,
        })
    }
}

/// Scalar usage reads exclude tool payloads, reasoning text and message text.
pub(crate) fn columns(db: &Path, id: &str) -> Result<Columns> {
    let catalog = crate::cost::snapshot();
    columns_priced(db, id, catalog.as_deref())
}

pub(crate) fn columns_priced(
    db: &Path,
    id: &str,
    catalog: Option<&crate::cost::Catalog>,
) -> Result<Columns> {
    let id = operand(id)?;
    let mut columns = Columns::default();
    let mut costs = crate::harness::spec(HarnessKind::Opencode)
        .transcript
        .handler
        .accounting();
    let mut input = 0_u64;
    let mut output = 0_u64;
    let mut usage = false;
    for mut v in query(
        db,
        &format!(
            "SELECT id, 'assistant' AS role,
                    json_extract(data, '$.modelID') AS modelID,
                    json_extract(data, '$.providerID') AS providerID,
                    json_extract(data, '$.tokens') AS tokens,
                    json_extract(data, '$.cost') AS cost
             FROM message WHERE session_id = {id} AND json_valid(data)
                 AND json_extract(data, '$.role') = 'assistant'
             ORDER BY time_created, id"
        ),
    )? {
        if let Some(model) = v["modelID"].as_str() {
            columns.model = Some(match v["providerID"].as_str() {
                Some(provider) => format!("{provider}/{model}"),
                None => model.to_owned(),
            });
        }
        v["tokens"] = v["tokens"]
            .as_str()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .unwrap_or(Value::Null);
        costs.observe(&v, catalog);
        if let Some(tokens) = v.get("tokens").filter(|v| v.is_object()) {
            let n = |pointer| tokens.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
            let prompt = n("/input")
                .saturating_add(n("/cache/read"))
                .saturating_add(n("/cache/write"));
            input = input.saturating_add(prompt);
            output = output.saturating_add(n("/output"));
            usage = true;
            // OpenCode's context sidebar uses its latest assistant with output.
            if n("/output") > 0 {
                columns.context_tokens = Some(
                    prompt
                        .saturating_add(n("/output"))
                        .saturating_add(n("/reasoning")),
                );
            }
        }
    }
    if usage {
        columns.tokens_in = Some(input);
        columns.tokens_out = Some(output);
    }
    // Current OpenCode reports a session total. Older schemas expose message costs only.
    let fields = query(db, "PRAGMA table_info(session)")?;
    let native_total = if fields.iter().any(|v| v["name"] == "cost") {
        query(db, &format!("SELECT cost FROM session WHERE id = {id}"))?
            .first()
            .and_then(|row| row["cost"].as_f64())
    } else {
        None
    };
    (columns.cost_usd, columns.cost_info) = costs.report(native_total);
    let text = text_events(db, &id, 1, Some("assistant"))?;
    columns.last = text
        .last()
        .and_then(|v| v["text"].as_str())
        .and_then(fleet::headline);
    Ok(columns)
}

/// Only selected text crosses the SQLite boundary. Both message count and text size
/// are bounded before output is collected; synthetic and ignored parts stay excluded.
fn text_events(db: &Path, id: &str, limit: usize, role: Option<&str>) -> Result<Vec<Value>> {
    // Columns need the first line; previews keep the newest tail of long messages.
    let clip = if role.is_some() { 1 } else { -32768 };
    let extended_clip = if role.is_some() { 1 } else { -32769 };
    let role = role.map_or(String::new(), |role| {
        format!("AND json_extract(m.data, '$.role') = '{role}'")
    });
    let mut events = query(
        db,
        &format!(
            "SELECT id, role, created, substr(raw_text, {clip}, 32768) AS text,
                    length(raw_text) > 32768 AS truncated FROM (
             SELECT m.id, json_extract(m.data, '$.role') AS role,
                    m.time_created AS created,
                    (SELECT substr(group_concat(text, char(10)), {extended_clip}, 32769) FROM
                        (SELECT substr(json_extract(p.data, '$.text'), {extended_clip}, 32769) AS text
                         FROM part p WHERE p.message_id = m.id AND p.session_id = m.session_id
                           AND json_valid(p.data)
                           AND json_extract(p.data, '$.type') = 'text'
                           AND coalesce(json_extract(p.data, '$.synthetic'), 0) = 0
                           AND coalesce(json_extract(p.data, '$.ignored'), 0) = 0
                         ORDER BY p.id)) AS raw_text
             FROM message m WHERE m.session_id = {id} AND json_valid(m.data)
               AND json_extract(m.data, '$.role') IN ('user', 'assistant') {role}
               AND raw_text IS NOT NULL AND trim(raw_text) != ''
             ORDER BY m.time_created DESC, m.id DESC LIMIT {limit})
             ORDER BY created DESC, id DESC"
        ),
    )?;
    events.reverse();
    for event in &mut events {
        if let Some(at) = millis(&event["created"]) {
            event["timestamp"] = at.to_rfc3339().into();
        }
    }
    Ok(events)
}

pub(crate) fn preview(db: &Path, id: &str) -> Result<crate::transcript::Transcript> {
    require_session(db, id)?;
    let events = text_events(db, &operand(id)?, 41, None)?;
    let truncated = events.iter().any(|v| v["truncated"] == 1);
    let mut bytes = Vec::new();
    for event in events {
        serde_json::to_writer(&mut bytes, &event)?;
        bytes.push(b'\n');
    }
    let mut document = crate::transcript::parse("opencode", &bytes);
    document.earlier |= truncated;
    document.bytes_read = bytes.len() as u64;
    Ok(document)
}

/// Search reads the same visible text roles as previews, in stable message order.
pub(crate) fn conversation_page(
    db: &Path,
    id: &str,
    offset: usize,
    limit: usize,
) -> Result<Vec<Value>> {
    let id = operand(id)?;
    query(
        db,
        &format!(
            "SELECT m.id, json_extract(m.data, '$.role') AS role,
            (SELECT group_concat(text, char(10)) FROM (
                SELECT json_extract(p.data, '$.text') AS text FROM part p
                WHERE p.message_id = m.id AND p.session_id = m.session_id
                    AND json_valid(p.data) AND json_extract(p.data, '$.type') = 'text'
                    AND coalesce(json_extract(p.data, '$.synthetic'), 0) = 0
                    AND coalesce(json_extract(p.data, '$.ignored'), 0) = 0
                ORDER BY p.id)) AS text
         FROM message m WHERE m.session_id = {id} AND json_valid(m.data)
            AND json_extract(m.data, '$.role') IN ('user', 'assistant')
            AND text IS NOT NULL AND trim(text) != ''
         ORDER BY m.time_created, m.id LIMIT {limit} OFFSET {offset}"
        ),
    )
}

#[derive(Clone, Debug)]
pub struct Process {
    pub pid: u32,
    pub started: DateTime<Utc>,
    pub cwd: Option<PathBuf>,
    /// Only an explicit native session argument identifies a saved conversation.
    pub session_id: Option<String>,
}

pub fn processes(ps: &str) -> Vec<Process> {
    crate::harness::spec(HarnessKind::Opencode)
        .discovery
        .processes(ps)
        .into_iter()
        .map(|line| {
            let words: Vec<_> = line.command.split_whitespace().skip(1).collect();
            // An attached backend may use another machine's database.
            let remote = words
                .first()
                .is_some_and(|word| matches!(*word, "attach" | "run"))
                || words
                    .iter()
                    .any(|word| *word == "--attach" || word.starts_with("--attach="))
                || words.contains(&"--fork");
            let mut session_id = None;
            if !remote {
                let mut words = words.iter();
                while let Some(word) = words.next() {
                    if *word == "--" || *word == "--prompt" || word.starts_with("--prompt=") {
                        break;
                    }
                    let value = if matches!(*word, "--session" | "-s") {
                        words.next().copied()
                    } else {
                        word.strip_prefix("--session=")
                    };
                    if let Some(id) = value.filter(|id| valid_id(id)) {
                        session_id = Some(id.to_owned());
                    } else if matches!(
                        *word,
                        "--model" | "-m" | "--agent" | "--port" | "--hostname"
                    ) {
                        words.next();
                    } else if !word.starts_with('-') {
                        break;
                    }
                }
            }
            Process {
                pid: line.pid,
                started: line.started,
                cwd: None,
                session_id,
            }
        })
        .collect()
}

pub fn sessions(home: &Path) -> Result<Vec<Session>> {
    if !home.is_dir() {
        return Ok(Vec::new());
    }
    let mut procs = processes(&fleet::pass_table("/bin/ps")?);
    let own = fleet::own_home_processes(
        "/bin/ps",
        HarnessKind::Opencode,
        &procs.iter().map(|p| p.pid).collect::<Vec<_>>(),
    );
    procs.retain(|p| own.contains(&p.pid));
    #[cfg(target_os = "macos")]
    for process in &mut procs {
        process.cwd = crate::process_info::cwd(process.pid);
    }
    let missing = procs
        .iter()
        .filter(|p| p.cwd.is_none())
        .map(|p| p.pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    if !missing.is_empty() {
        let output = crate::observe::spawn(
            crate::observe::op::OPEN_FILES,
            Command::new("/usr/sbin/lsof").args(["-nPw", "-a", "-p", &missing, "-d", "cwd", "-Fn"]),
        );
        if let Ok(output) = output {
            let cwds = crate::codex::cwds(&String::from_utf8_lossy(&output.stdout));
            for process in &mut procs {
                if process.cwd.is_none() {
                    process.cwd = cwds.get(&process.pid).cloned();
                }
            }
        }
    }
    rows(home, &procs)
}

pub fn rows(home: &Path, procs: &[Process]) -> Result<Vec<Session>> {
    let mut entries = Vec::new();
    let ids = procs
        .iter()
        .filter_map(|p| p.session_id.as_deref())
        .map(operand)
        .collect::<Result<Vec<_>>>()?;
    if !ids.is_empty() {
        for db in databases(home)? {
            entries.extend(self::entries(
                &db,
                home,
                &format!("AND id IN ({})", ids.join(",")),
            )?);
        }
    }
    procs
        .iter()
        .map(|p| {
            let mut candidates = entries.iter().filter(|e| {
                p.session_id.as_ref() == Some(&e.key.session_id)
                    && p.cwd.as_ref() == Some(&e.cwd)
                    && procs
                        .iter()
                        .filter(|other| other.session_id == p.session_id)
                        .count()
                        == 1
            });
            let first = candidates.next();
            let entry = first.filter(|_| candidates.next().is_none());
            let report = entry
                .map(|e| columns(&e.transcript, &e.key.session_id))
                .transpose()?
                .unwrap_or_default();
            Ok(Session {
                session_id: entry.map_or_else(
                    || format!("opencode-{}", p.pid),
                    |e| e.key.session_id.clone(),
                ),
                harness: "opencode".into(),
                kind: None,
                cwd: p.cwd.clone().unwrap_or_default(),
                state: "-".into(),
                started: Some(p.started),
                last_activity: entry.and_then(|e| e.last_activity),
                pid: Some(p.pid),
                transcript_path: entry.map(|e| e.transcript.clone()),
                title: entry.and_then(|e| e.title.clone()),
                next_model: None,
                model: report.model,
                tokens_in: report.tokens_in,
                tokens_out: report.tokens_out,
                context_tokens: report.context_tokens,
                context_window: None,
                cost_usd: report.cost_usd,
                cost_info: report.cost_info,
                last: report.last,
                effort: None,
                usage: None,
                coordinator: false,
                forked_from: None,
                activity: Vec::new(),
                moved_to: None,
            })
        })
        .collect()
}
