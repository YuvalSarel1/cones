//! Bounded, read-only conversation previews. No harness clients or shared fleet caches.
//!
//! [`Reader`] owns a separate worker and a small snapshot cache invalidated by file
//! mtime, length and inode. Selection reads a tail window and widens it only when
//! no conversation text was found. The bounded retries and retained text keep
//! selection independent of transcript size. Earlier omitted text is marked in
//! [`Transcript::earlier`].
//!
//! Native message selection comes from harness definitions. Codex shares its
//! user-message extractor without taking the live prompt cache lock. Control
//! sequences are stripped before text is returned for drawing.
use crate::{harness, output};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    collections::{HashSet, VecDeque},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::SystemTime,
};

const WINDOW: u64 = 256 * 1024;
const MAX_WINDOW: u64 = 4 * 1024 * 1024;
const MAX_TEXT: usize = 128 * 1024;
const MAX_MESSAGES: usize = 40;
const CACHE_SIZE: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub key: String,
    pub harness: String,
    pub source: Source,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Conversation(PathBuf),
    Opencode {
        database: PathBuf,
        session_id: String,
    },
    Run {
        events: Option<PathBuf>,
        stderr: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Output,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub tools: Vec<Tool>,
    pub at: Option<DateTime<Utc>>,
    id: Option<String>,
    offset: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool {
    pub name: String,
    pub input: String,
}

impl Message {
    fn bytes(&self) -> usize {
        self.text.len()
            + self
                .tools
                .iter()
                .map(|t| t.name.len() + t.input.len())
                .sum::<usize>()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Transcript {
    pub messages: Vec<Message>,
    pub earlier: bool,
    /// Bytes read for this snapshot, including a retried larger tail window.
    pub bytes_read: u64,
    pub older: Option<Cursor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    before: u64,
    stamp: Stamp,
}

impl Transcript {
    pub fn prepend(&mut self, mut older: Transcript) {
        if let (Some(last), Some(first)) = (older.messages.last_mut(), self.messages.first())
            && last.role == Role::Assistant
            && first.role == Role::Assistant
            && last.id.is_some()
            && last.id == first.id
        {
            last.text.push_str("\n\n");
            last.text.push_str(&first.text);
            last.tools.extend(first.tools.iter().cloned());
            last.at = first.at.or(last.at);
            self.messages.remove(0);
        }
        older.messages.append(&mut self.messages);
        self.messages = older.messages;
        self.earlier = older.earlier;
        self.older = older.older;
        self.bytes_read += older.bytes_read;
    }
}

pub struct Response {
    pub target: Target,
    pub result: Result<Arc<Transcript>>,
    pub elapsed_ms: f64,
    pub cache_hit: bool,
    pub bytes_read: u64,
    pub cursor: Option<Cursor>,
}

/// One outstanding request. The UI can replace its desired target while the worker finishes.
pub struct Reader {
    requests: mpsc::Sender<(Target, Option<Cursor>)>,
    results: mpsc::Receiver<Response>,
    busy: bool,
}

impl Reader {
    pub fn new() -> std::io::Result<Self> {
        let (requests, input) = mpsc::channel::<(Target, Option<Cursor>)>();
        let (output, results) = mpsc::channel();
        std::thread::Builder::new()
            .name("cones-transcript".into())
            .spawn(move || {
                let mut cache = Cache::default();
                while let Ok((target, cursor)) = input.recv() {
                    let started = std::time::Instant::now();
                    let mut cache_hit = false;
                    let result = cache.read(&target, cursor.as_ref(), &mut cache_hit);
                    let bytes_read = if cache_hit {
                        0
                    } else {
                        result.as_ref().map_or(0, |document| document.bytes_read)
                    };
                    if output
                        .send(Response {
                            target,
                            result,
                            cache_hit,
                            bytes_read,
                            cursor,
                            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests,
            results,
            busy: false,
        })
    }

    pub fn request(&mut self, target: Target) -> Result<bool> {
        self.request_page(target, None)
    }

    pub fn request_page(&mut self, target: Target, cursor: Option<Cursor>) -> Result<bool> {
        if self.busy {
            return Ok(false);
        }
        self.requests
            .send((target, cursor))
            .context("transcript worker exited")?;
        self.busy = true;
        Ok(true)
    }

    pub fn poll(&mut self) -> Option<Result<Response>> {
        if !self.busy {
            return None;
        }
        match self.results.try_recv() {
            Ok(result) => {
                self.busy = false;
                Some(Ok(result))
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.busy = false;
                Some(Err(anyhow::anyhow!("transcript worker exited")))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    len: u64,
    device: u64,
    inode: u64,
    database: Option<crate::opencode::Fingerprint>,
}

impl Stamp {
    fn of(path: &Path) -> Result<Self> {
        let m = fs::metadata(path).context("reading preview metadata")?;
        ensure!(m.is_file(), "transcript is not a regular file");
        Ok(Self {
            modified: m.modified()?,
            len: m.len(),
            device: m.dev(),
            inode: m.ino(),
            database: None,
        })
    }
}

impl Source {
    fn stamps(&self) -> Result<Vec<Option<Stamp>>> {
        match self {
            Self::Conversation(path) => Ok(vec![Some(Stamp::of(path)?)]),
            Self::Opencode { database, .. } => {
                let mut stamp = Stamp::of(database)?;
                stamp.database = Some(crate::opencode::fingerprint(database)?);
                Ok(vec![Some(stamp)])
            }
            Self::Run { events, stderr } => [events, stderr]
                .into_iter()
                .map(|path| {
                    let Some(path) = path else { return Ok(None) };
                    match Stamp::of(path) {
                        Ok(stamp) => Ok(Some(stamp)),
                        Err(error)
                            if error
                                .downcast_ref::<std::io::Error>()
                                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                        {
                            Ok(None)
                        }
                        Err(error) => Err(error),
                    }
                })
                .collect(),
        }
    }
}

#[derive(Default)]
struct Cache {
    entries: VecDeque<Cached>,
}

struct Cached {
    target: Target,
    stamps: Vec<Option<Stamp>>,
    before: Option<u64>,
    document: Arc<Transcript>,
}

impl Cache {
    fn read(
        &mut self,
        target: &Target,
        cursor: Option<&Cursor>,
        cache_hit: &mut bool,
    ) -> Result<Arc<Transcript>> {
        let stamps = target.source.stamps()?;
        if let Some(cursor) = cursor {
            ensure!(
                stamps.first().and_then(Option::as_ref) == Some(&cursor.stamp),
                "transcript changed; refresh before loading earlier messages"
            );
        }
        let before = cursor.map(|c| c.before);
        if let Some(i) = self
            .entries
            .iter()
            .position(|c| c.target == *target && c.stamps == stamps && c.before == before)
        {
            *cache_hit = true;
            let cached = self.entries.remove(i).unwrap();
            let result = Arc::clone(&cached.document);
            self.entries.push_back(cached);
            return Ok(result);
        }
        let document = match &target.source {
            Source::Conversation(path) => {
                let stamp = stamps[0].as_ref().unwrap();
                let mut document =
                    read_conversation(path, &target.harness, before.unwrap_or(stamp.len))?;
                if let Some(cursor) = &mut document.older {
                    cursor.stamp = stamp.clone();
                }
                ensure!(
                    target.source.stamps()? == stamps,
                    "transcript changed while reading; reload history"
                );
                document
            }
            Source::Run { events, stderr } => read_run(events, stderr, &stamps)?,
            Source::Opencode {
                database,
                session_id,
            } => {
                let document = crate::opencode::preview(database, session_id)?;
                ensure!(
                    target.source.stamps()? == stamps,
                    "OpenCode transcript changed while reading; reload history"
                );
                document
            }
        };
        let document = Arc::new(document);
        self.entries
            .retain(|c| c.target != *target || c.before != before);
        self.entries.push_back(Cached {
            target: target.clone(),
            stamps,
            before,
            document: Arc::clone(&document),
        });
        while self.entries.len() > CACHE_SIZE {
            self.entries.pop_front();
        }
        Ok(document)
    }
}

/// Read a complete-line tail bounded by the length observed before opening the file.
fn read_tail(path: &Path, len: u64, size: u64) -> Result<(Vec<u8>, u64)> {
    let mut file = output::open_read(path)?;
    let offset = len.saturating_sub(size);
    let start = offset.saturating_sub(1);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(len - start).read_to_end(&mut bytes)?;
    let bytes_read = bytes.len() as u64;
    let from = if offset == 0 {
        0
    } else if bytes.first() == Some(&b'\n') {
        1
    } else {
        bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1)
    };
    bytes.drain(..from);
    Ok((bytes, bytes_read))
}

fn read_run(
    events: &Option<PathBuf>,
    stderr: &Option<PathBuf>,
    stamps: &[Option<Stamp>],
) -> Result<Transcript> {
    let mut document = Transcript::default();
    for (i, path) in [events, stderr].into_iter().enumerate() {
        let (Some(path), Some(stamp)) = (path, &stamps[i]) else {
            continue;
        };
        let limit = if i == 0 { WINDOW } else { 16 * 1024 };
        let (bytes, count) = read_tail(path, stamp.len, limit)?;
        document.bytes_read += count;
        document.earlier |= stamp.len > limit;
        let mut text = if i == 0 {
            bytes
                .split_inclusive(|b| *b == b'\n')
                .filter(|line| line.ends_with(b"\n"))
                .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
                .flat_map(|event| output::describe(&event))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            plain(&String::from_utf8_lossy(&bytes))
        };
        text = plain(&text);
        if text.trim().is_empty() {
            continue;
        }
        document.earlier |= trim_text(&mut text);
        if i == 1 {
            text.insert_str(0, "Harness stderr:\n");
        }
        document.messages.push(Message {
            role: Role::Output,
            text,
            tools: Vec::new(),
            at: None,
            id: None,
            offset: 0,
        });
    }
    Ok(document)
}

fn read_conversation(path: &Path, harness: &str, len: u64) -> Result<Transcript> {
    let mut file = File::open(path).context("opening transcript")?;
    let mut size = WINDOW.min(len);
    let mut bytes_read = 0;
    let document = loop {
        let offset = len - size;
        let start = offset.saturating_sub(1);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        (&mut file).take(len - start).read_to_end(&mut bytes)?;
        bytes_read += bytes.len() as u64;
        // Inspect the byte before the window so a whole first line is not discarded.
        let from = if offset == 0 {
            0
        } else if bytes.first() == Some(&b'\n') {
            1
        } else {
            bytes
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |i| i + 1)
        };
        let mut document = parse_at(harness, &bytes[from..], start + from as u64);
        document.earlier |= offset > 0;
        if !document.messages.is_empty() || size >= len || size >= MAX_WINDOW {
            document.bytes_read = bytes_read;
            if document.earlier {
                let before = document
                    .messages
                    .first()
                    .map_or(start + from as u64, |m| m.offset);
                if before > 0 && before < len {
                    document.older = Some(Cursor {
                        before,
                        stamp: Stamp::of(path)?,
                    });
                }
            }
            break document;
        }
        size = (size * 4).min(len).min(MAX_WINDOW);
    };
    Ok(document)
}

/// Retain conversation text and compact tool calls. Tool results and thinking are excluded.
pub fn parse(harness: &str, bytes: &[u8]) -> Transcript {
    parse_at(harness, bytes, 0)
}

fn parse_at(harness: &str, bytes: &[u8], mut offset: u64) -> Transcript {
    let mut messages: VecDeque<Message> = VecDeque::new();
    let mut size = 0;
    let mut earlier = false;
    let mut seen = HashSet::new();
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        let at = offset;
        offset += line.len() as u64;
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(mut message) = message(harness, &v) else {
            continue;
        };
        message.offset = at;
        if let Some(id) = v["uuid"].as_str()
            && !seen.insert(id.to_owned())
        {
            continue;
        }
        message.text = plain(&message.text);
        if message.text.trim().is_empty() && message.tools.is_empty() {
            continue;
        }
        if let Some(previous) = messages.back_mut()
            && message.role == Role::Assistant
            && previous.role == Role::Assistant
            && message.id.is_some()
            && message.id == previous.id
        {
            size -= previous.bytes();
            if !message.text.is_empty() {
                if !previous.text.is_empty() {
                    previous.text.push_str("\n\n");
                }
                previous.text.push_str(&message.text);
            }
            previous.tools.extend(message.tools);
            previous.at = message.at.or(previous.at);
            earlier |= trim_message(previous);
            size += previous.bytes();
        } else {
            earlier |= trim_message(&mut message);
            size += message.bytes();
            messages.push_back(message);
        }
        while messages.len() > MAX_MESSAGES || size > MAX_TEXT {
            if let Some(old) = messages.pop_front() {
                size -= old.bytes();
                earlier = true;
            }
        }
    }
    Transcript {
        messages: messages.into(),
        earlier,
        bytes_read: 0,
        older: None,
    }
}

fn trim_text(text: &mut String) -> bool {
    trim_text_to(text, MAX_TEXT)
}

fn trim_message(message: &mut Message) -> bool {
    let mut trimmed = false;
    if message.tools.len() > MAX_MESSAGES {
        message.tools.truncate(MAX_MESSAGES - 1);
        message.tools.push(Tool {
            name: "Additional tool calls omitted".into(),
            input: String::new(),
        });
        trimmed = true;
    }
    let tools = message.bytes() - message.text.len();
    trim_text_to(&mut message.text, MAX_TEXT.saturating_sub(tools)) || trimmed
}

fn trim_text_to(text: &mut String, limit: usize) -> bool {
    if text.len() <= limit {
        return false;
    }
    let mut from = text.len() - limit;
    while !text.is_char_boundary(from) {
        from += 1;
    }
    text.drain(..from);
    true
}

fn message(harness: &str, v: &Value) -> Option<Message> {
    let spec = harness::by_name(harness)?;
    let tools = tools(harness, v);
    for (role, source) in [
        (Role::User, &spec.transcript.messages.user),
        (Role::Assistant, &spec.transcript.messages.assistant),
    ] {
        let parts = source.parts(v, true);
        if parts.is_empty() {
            continue;
        }
        return Some(Message {
            role,
            text: parts.join("\n"),
            tools: if role == Role::Assistant {
                tools
            } else {
                Vec::new()
            },
            id: source.id(v).map(str::to_owned),
            offset: 0,
            at: v["timestamp"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(Into::into),
        });
    }
    (!tools.is_empty()).then(|| Message {
        role: Role::Assistant,
        text: String::new(),
        tools,
        id: spec.transcript.messages.assistant.id(v).map(str::to_owned),
        offset: 0,
        at: v["timestamp"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(Into::into),
    })
}

/// Display only the native call records. This never executes a tool or infers its outcome.
fn tools(harness: &str, v: &Value) -> Vec<Tool> {
    fn summary(name: &Value, input: &Value) -> Option<Tool> {
        let name = name.as_str()?;
        let parsed = input
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok());
        let input = parsed.as_ref().unwrap_or(input);
        let detail = [
            "command",
            "cmd",
            "file_path",
            "path",
            "pattern",
            "query",
            "url",
        ]
        .iter()
        .find_map(|key| input.get(key).and_then(Value::as_str))
        .or_else(|| input.as_str())
        .unwrap_or("");
        let short = |text: &str, max: usize| {
            let clean = plain(text);
            let text = clean.lines().next().unwrap_or("").trim();
            let mut out: String = text.chars().take(max).collect();
            if text.chars().count() > max {
                out.push('…');
            }
            out
        };
        Some(Tool {
            name: short(name, 80),
            input: short(detail, 160),
        })
    }
    match harness {
        "claude" if v["type"] == "assistant" && v["isMeta"] != true && v["isSidechain"] != true => {
            v["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|part| part["type"] == "tool_use")
                .filter_map(|part| summary(&part["name"], &part["input"]))
                .collect()
        }
        "pi" if v["type"] == "message" && v["message"]["role"] == "assistant" => {
            v["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|part| part["type"] == "toolCall")
                .filter_map(|part| summary(&part["name"], &part["arguments"]))
                .collect()
        }
        "codex" if v["type"] == "response_item" => {
            let p = &v["payload"];
            match p["type"].as_str() {
                Some("function_call") => summary(&p["name"], &p["arguments"]).into_iter().collect(),
                Some("custom_tool_call") => summary(&p["name"], &p["input"]).into_iter().collect(),
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// Transcripts are data, never terminal commands. Strip CSI/OSC/DCS and other controls.
pub fn plain(text: &str) -> String {
    enum Escape {
        None,
        Start,
        Csi,
        String,
        StringEnd,
    }
    let mut escape = Escape::None;
    let mut out = String::new();
    for c in text.chars() {
        escape = match escape {
            Escape::None => match c {
                '\x1b' => Escape::Start,
                '\u{009b}' => Escape::Csi,
                '\u{0090}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => Escape::String,
                '\n' => {
                    out.push('\n');
                    Escape::None
                }
                '\t' => {
                    out.push_str("    ");
                    Escape::None
                }
                c if c.is_control() => Escape::None,
                c => {
                    out.push(c);
                    Escape::None
                }
            },
            Escape::Start => match c {
                '[' => Escape::Csi,
                ']' | 'P' | '_' | '^' => Escape::String,
                '\x20'..='\x2f' => Escape::Start,
                _ => Escape::None,
            },
            Escape::Csi => {
                if ('\x40'..='\x7e').contains(&c) {
                    Escape::None
                } else {
                    Escape::Csi
                }
            }
            Escape::String => match c {
                '\x07' | '\u{009c}' => Escape::None,
                '\x1b' => Escape::StringEnd,
                _ => Escape::String,
            },
            Escape::StringEnd => {
                if c == '\\' {
                    Escape::None
                } else {
                    Escape::String
                }
            }
        };
    }
    out
}
