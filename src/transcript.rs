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
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    collections::{HashSet, VecDeque},
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
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
    Match {
        source: Box<Source>,
        anchor: crate::search::Anchor,
    },
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
    pub newer: Option<Cursor>,
    pub matched: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    before: u64,
    stamp: Stamp,
    forward: bool,
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
        self.matched = None;
        self.earlier = older.earlier;
        self.older = older.older;
        self.bytes_read += older.bytes_read;
    }

    pub fn append(&mut self, mut newer: Transcript) {
        self.messages.append(&mut newer.messages);
        self.newer = newer.newer;
        self.matched = None;
        self.bytes_read += newer.bytes_read;
    }
}

impl Cursor {
    pub fn forward(&self) -> bool {
        self.forward
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
            Self::Match { source, .. } => source.stamps(),
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
    before: Option<(u64, bool)>,
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
        let before = cursor.map(|c| (c.before, c.forward));
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
        let source = match &target.source {
            Source::Match { source, .. } => source.as_ref(),
            source => source,
        };
        let mut document = match source {
            Source::Conversation(path) => {
                let stamp = stamps[0].as_ref().unwrap();
                let anchor = match &target.source {
                    Source::Match { anchor, .. } if cursor.is_none() => Some(anchor),
                    _ => None,
                };
                let mut document = if cursor.is_some_and(|c| c.forward) {
                    read_forward(path, &target.harness, cursor.unwrap().before, stamp)?
                } else if let Some(anchor) = anchor {
                    read_match(path, &target.harness, anchor, stamp)?
                } else {
                    read_conversation(
                        path,
                        &target.harness,
                        cursor.map_or(stamp.len, |c| c.before),
                    )?
                };
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
                let anchor = match &target.source {
                    Source::Match { anchor, .. } if cursor.is_none() => Some(anchor),
                    _ => None,
                };
                let document = if anchor.is_some() || cursor.is_some() {
                    read_opencode(
                        database,
                        session_id,
                        anchor,
                        cursor,
                        stamps[0].as_ref().unwrap(),
                    )?
                } else {
                    crate::opencode::preview(database, session_id)?
                };
                ensure!(
                    target.source.stamps()? == stamps,
                    "OpenCode transcript changed while reading; reload history"
                );
                document
            }
            Source::Match { .. } => unreachable!("nested search target"),
        };
        if cursor.is_some_and(|c| !c.forward()) {
            // Prepending must preserve the current window's forward cursor.
            document.newer = None;
        }
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
                        forward: false,
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
        let before = messages.back().map_or(0, Message::bytes);
        if absorb(&mut messages, message) {
            size -= before;
        }
        let last = messages.back_mut().expect("absorb leaves a message behind");
        earlier |= trim_message(last);
        size += last.bytes();
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
        newer: None,
        matched: None,
    }
}

/// Append a message, merging an assistant turn a harness split across records into the one
/// it continues. Returns whether it merged, so a caller counting retained bytes can discount
/// what the previous message already contributed.
fn absorb(messages: &mut VecDeque<Message>, message: Message) -> bool {
    if let Some(previous) = messages.back_mut()
        && message.role == Role::Assistant
        && previous.role == Role::Assistant
        && message.id.is_some()
        && message.id == previous.id
    {
        if !message.text.is_empty() {
            if !previous.text.is_empty() {
                previous.text.push_str("\n\n");
            }
            previous.text.push_str(&message.text);
        }
        previous.tools.extend(message.tools);
        previous.at = message.at.or(previous.at);
        return true;
    }
    messages.push_back(message);
    false
}

/// Rows read per OpenCode page. Matches the text index: one bounded statement at a time,
/// so a long conversation never holds a snapshot open across the whole export.
const EXPORT_PAGE: usize = 64;

/// A conversation read for something other than a pane. No message is shortened and no
/// byte window bounds how far back the read goes, so what this returns is what the
/// harness recorded, less the roles its definition excludes from conversation text.
#[derive(Clone, Debug, Default)]
pub struct Export {
    pub messages: Vec<Message>,
    /// Messages dropped to honour the requested tail. Zero means nothing was left out.
    pub omitted: usize,
    /// The file ended mid-record, so a writer is still appending and this read is behind.
    pub incomplete: bool,
}

/// Export a conversation. `tail` keeps that many of the most recent messages; `None` keeps
/// every one. Reading opens files for reading only: no harness client, no native state.
pub fn export(source: &Source, harness: &str, tail: Option<usize>) -> Result<Export> {
    ensure!(
        harness::by_name(harness).is_some_and(|spec| spec.transcript.available),
        "{harness} keeps no transcript cones can read"
    );
    ensure!(
        tail != Some(0),
        "--tail takes a positive number of messages"
    );
    let mut out = Export::default();
    let mut kept: VecDeque<Message> = VecDeque::new();
    match source {
        Source::Conversation(path) => {
            let mut reader = BufReader::new(output::open_read(path)?);
            let mut seen = HashSet::new();
            let mut bytes = Vec::new();
            loop {
                bytes.clear();
                let count = reader
                    .read_until(b'\n', &mut bytes)
                    .with_context(|| format!("reading {}", path.display()))?;
                if count == 0 {
                    break;
                }
                // A record without its newline is still being written; it is not text yet.
                if !bytes.ends_with(b"\n") {
                    out.incomplete = true;
                    break;
                }
                let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
                    continue;
                };
                if let Some(id) = v["uuid"].as_str()
                    && !seen.insert(id.to_owned())
                {
                    continue;
                }
                keep(&mut kept, harness, &v, tail, &mut out.omitted);
            }
        }
        Source::Opencode {
            database,
            session_id,
        } => {
            let mut at = 0;
            loop {
                let events =
                    crate::opencode::conversation_page(database, session_id, at, EXPORT_PAGE)?;
                let count = events.len();
                for v in &events {
                    keep(&mut kept, harness, v, tail, &mut out.omitted);
                }
                at += count;
                if count < EXPORT_PAGE {
                    break;
                }
            }
        }
        Source::Run { .. } => bail!("a run's captured output is not a conversation"),
        Source::Match { .. } => bail!("a search result is not a conversation"),
    }
    out.messages = kept.into();
    Ok(out)
}

/// Add one native record's message to the export, dropping the oldest once the tail is full.
fn keep(
    kept: &mut VecDeque<Message>,
    harness: &str,
    v: &Value,
    tail: Option<usize>,
    omitted: &mut usize,
) {
    let Some(mut message) = message(harness, v) else {
        return;
    };
    message.text = plain(&message.text);
    if message.text.trim().is_empty() && message.tools.is_empty() {
        return;
    }
    // A merged continuation joins the message already counted, so the tail is unchanged.
    if !absorb(kept, message)
        && let Some(tail) = tail
    {
        while kept.len() > tail {
            kept.pop_front();
            *omitted += 1;
        }
    }
}

/// Stream complete native records for the text index. Prompt selectors are shared
/// with previews, so tool output, thinking and harness control records stay out.
pub(crate) fn search_passages(
    entry: &crate::history::Entry,
    mut visit: impl FnMut(&str, u64, u64) -> Result<()>,
) -> Result<()> {
    if entry.key.harness == "opencode" {
        let mut at = 0;
        loop {
            let events = crate::opencode::conversation_page(
                &entry.transcript,
                &entry.key.session_id,
                at,
                64,
            )?;
            let count = events.len();
            for (i, v) in events.into_iter().enumerate() {
                if let Some(m) = message("opencode", &v) {
                    visit(&plain(&m.text), (at + i) as u64, (at + i + 1) as u64)?;
                }
            }
            at += count;
            if count < 64 {
                break;
            }
        }
        return Ok(());
    }
    let mut reader = BufReader::new(File::open(&entry.transcript)?);
    let mut bytes = Vec::new();
    let mut offset = 0;
    let mut seen = HashSet::new();
    loop {
        bytes.clear();
        let len = reader.read_until(b'\n', &mut bytes)?;
        if len == 0 {
            break;
        }
        let start = offset;
        offset += len as u64;
        // A writer's incomplete last record is picked up by the next refresh.
        if !bytes.ends_with(b"\n") {
            break;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if let Some(id) = v["uuid"].as_str()
            && !seen.insert(id.to_owned())
        {
            continue;
        }
        if let Some(m) = message(&entry.key.harness, &v) {
            let text = plain(&m.text);
            if !text.trim().is_empty() {
                visit(&text, start, offset)?;
            }
        }
    }
    Ok(())
}

fn read_match(
    path: &Path,
    harness: &str,
    anchor: &crate::search::Anchor,
    stamp: &Stamp,
) -> Result<Transcript> {
    ensure!(
        anchor.offset < anchor.end && anchor.end <= stamp.len,
        "search passage changed; refresh history"
    );
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(anchor.offset))?;
    let mut bytes = Vec::new();
    file.take(anchor.end - anchor.offset)
        .read_to_end(&mut bytes)?;
    let value: Value = serde_json::from_slice(&bytes)?;
    let mut matched =
        message(harness, &value).context("search passage changed; refresh history")?;
    matched.text = plain(&matched.text);
    trim_match(&mut matched.text, anchor)?;
    matched.offset = anchor.offset;
    let mut document = read_conversation(path, harness, anchor.offset)?;
    // A short context leaves the initial selection visibly near the matching passage.
    if document.messages.len() > 4 {
        document.messages.drain(..document.messages.len() - 4);
    }
    document.matched = Some(document.messages.len());
    document.messages.push(matched);
    document.bytes_read += bytes.len() as u64;
    if let Some(first) = document.messages.first()
        && first.offset > 0
    {
        document.earlier = true;
        document.older = Some(Cursor {
            before: first.offset,
            stamp: stamp.clone(),
            forward: false,
        });
    }
    if anchor.end < stamp.len {
        document.newer = Some(Cursor {
            before: anchor.end,
            stamp: stamp.clone(),
            forward: true,
        });
    }
    Ok(document)
}

fn trim_match(text: &mut String, anchor: &crate::search::Anchor) -> Result<()> {
    let at = text
        .find(&anchor.text)
        .context("search passage changed; refresh history")?;
    // Keep the match visible even when the rest of an individual message is large.
    let mut from = at.saturating_sub(256);
    while !text.is_char_boundary(from) {
        from += 1;
    }
    let mut to = (from + MAX_TEXT / 2).min(text.len());
    while !text.is_char_boundary(to) {
        to -= 1;
    }
    *text = format!(
        "{}{}{}",
        if from > 0 { "…\n" } else { "" },
        &text[from..to],
        if to < text.len() { "\n…" } else { "" }
    );
    Ok(())
}

fn read_forward(path: &Path, harness: &str, start: u64, stamp: &Stamp) -> Result<Transcript> {
    let mut reader = BufReader::new(File::open(path)?);
    reader.seek(SeekFrom::Start(start))?;
    let mut offset = start;
    let mut document = Transcript::default();
    let mut bytes = Vec::new();
    let mut size = 0;
    while offset < stamp.len && document.messages.len() < MAX_MESSAGES && size < MAX_TEXT {
        bytes.clear();
        let count = (&mut reader)
            .take(stamp.len - offset)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            break;
        }
        let at = offset;
        offset += count as u64;
        document.bytes_read += count as u64;
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if let Some(mut m) = message(harness, &v) {
            m.text = plain(&m.text);
            m.offset = at;
            trim_message(&mut m);
            size += m.bytes();
            document.messages.push(m);
        }
        if document.bytes_read >= MAX_WINDOW {
            break;
        }
    }
    if offset < stamp.len {
        document.newer = Some(Cursor {
            before: offset,
            stamp: stamp.clone(),
            forward: true,
        });
    }
    Ok(document)
}

fn read_opencode(
    database: &Path,
    session: &str,
    anchor: Option<&crate::search::Anchor>,
    cursor: Option<&Cursor>,
    stamp: &Stamp,
) -> Result<Transcript> {
    let start = if let Some(anchor) = anchor {
        anchor.offset.saturating_sub(4)
    } else if let Some(cursor) = cursor {
        if cursor.forward {
            cursor.before
        } else {
            cursor.before.saturating_sub(MAX_MESSAGES as u64)
        }
    } else {
        0
    };
    let limit = cursor
        .filter(|c| !c.forward)
        .map_or(MAX_MESSAGES, |c| (c.before - start) as usize);
    let events = crate::opencode::conversation_page(database, session, start as usize, limit)?;
    let count = events.len();
    let mut document = Transcript::default();
    for (i, v) in events.into_iter().enumerate() {
        if let Some(mut m) = message("opencode", &v) {
            m.text = plain(&m.text);
            if let Some(anchor) = anchor.filter(|a| a.offset == start + i as u64) {
                trim_match(&mut m.text, anchor)?;
                document.matched = Some(document.messages.len());
            }
            trim_message(&mut m);
            document.messages.push(m);
        }
    }
    ensure!(
        anchor.is_none() || document.matched.is_some(),
        "search passage changed; refresh history"
    );
    if start > 0 {
        document.earlier = true;
        document.older = Some(Cursor {
            before: start,
            stamp: stamp.clone(),
            forward: false,
        });
    }
    if count == limit {
        document.newer = Some(Cursor {
            before: start + count as u64,
            stamp: stamp.clone(),
            forward: true,
        });
    }
    Ok(document)
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
