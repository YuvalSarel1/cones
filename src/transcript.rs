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
use crate::harness;
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    collections::{HashSet, VecDeque},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::PathBuf,
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
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub at: Option<DateTime<Utc>>,
    id: Option<String>,
}

#[derive(Debug, Default)]
pub struct Transcript {
    pub messages: Vec<Message>,
    pub earlier: bool,
    /// Bytes read for this snapshot, including a retried larger tail window.
    pub bytes_read: u64,
}

pub struct Response {
    pub target: Target,
    pub result: Result<Arc<Transcript>>,
}

/// One outstanding request. The UI can replace its desired target while the worker finishes.
pub struct Reader {
    requests: mpsc::Sender<Target>,
    results: mpsc::Receiver<Response>,
    busy: bool,
}

impl Reader {
    pub fn new() -> std::io::Result<Self> {
        let (requests, input) = mpsc::channel::<Target>();
        let (output, results) = mpsc::channel();
        std::thread::Builder::new()
            .name("cones-transcript".into())
            .spawn(move || {
                let mut cache = Cache::default();
                while let Ok(target) = input.recv() {
                    let result = cache.read(&target);
                    if output.send(Response { target, result }).is_err() {
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
        if self.busy {
            return Ok(false);
        }
        self.requests
            .send(target)
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

#[derive(PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    len: u64,
    device: u64,
    inode: u64,
}

impl Stamp {
    fn of(target: &Target) -> Result<Self> {
        let m = fs::metadata(&target.path).context("reading transcript metadata")?;
        ensure!(m.is_file(), "transcript is not a regular file");
        Ok(Self {
            modified: m.modified()?,
            len: m.len(),
            device: m.dev(),
            inode: m.ino(),
        })
    }
}

#[derive(Default)]
struct Cache {
    entries: VecDeque<(Target, Stamp, Arc<Transcript>)>,
}

impl Cache {
    fn read(&mut self, target: &Target) -> Result<Arc<Transcript>> {
        let stamp = Stamp::of(target)?;
        if let Some(i) = self
            .entries
            .iter()
            .position(|(t, s, _)| t == target && *s == stamp)
        {
            let cached = self.entries.remove(i).unwrap();
            let result = Arc::clone(&cached.2);
            self.entries.push_back(cached);
            return Ok(result);
        }
        let mut file = File::open(&target.path).context("opening transcript")?;
        let mut size = WINDOW.min(stamp.len);
        let mut bytes_read = 0;
        let document = loop {
            let offset = stamp.len - size;
            let start = offset.saturating_sub(1);
            file.seek(SeekFrom::Start(start))?;
            let mut bytes = Vec::new();
            (&mut file)
                .take(stamp.len - start)
                .read_to_end(&mut bytes)?;
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
            let mut document = parse(&target.harness, &bytes[from..]);
            document.earlier |= offset > 0;
            if !document.messages.is_empty() || size >= stamp.len || size >= MAX_WINDOW {
                document.bytes_read = bytes_read;
                break document;
            }
            size = (size * 4).min(stamp.len).min(MAX_WINDOW);
        };
        ensure!(
            Stamp::of(target)? == stamp,
            "transcript changed while reading; reload history"
        );
        let document = Arc::new(document);
        self.entries.retain(|(t, _, _)| t != target);
        self.entries
            .push_back((target.clone(), stamp, Arc::clone(&document)));
        while self.entries.len() > CACHE_SIZE {
            self.entries.pop_front();
        }
        Ok(document)
    }
}

/// Retain recent user and assistant text, in file order. Tool results and thinking are excluded.
pub fn parse(harness: &str, bytes: &[u8]) -> Transcript {
    let mut messages: VecDeque<Message> = VecDeque::new();
    let mut size = 0;
    let mut earlier = false;
    let mut seen = HashSet::new();
    for line in bytes.split(|b| *b == b'\n') {
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(mut message) = message(harness, &v) else {
            continue;
        };
        if let Some(id) = v["uuid"].as_str()
            && !seen.insert(id.to_owned())
        {
            continue;
        }
        message.text = plain(&message.text);
        if message.text.trim().is_empty() {
            continue;
        }
        if let Some(previous) = messages.back_mut()
            && message.role == Role::Assistant
            && previous.role == Role::Assistant
            && message.id.is_some()
            && message.id == previous.id
        {
            size -= previous.text.len();
            previous.text.push_str("\n\n");
            previous.text.push_str(&message.text);
            previous.at = message.at.or(previous.at);
            earlier |= trim_text(&mut previous.text);
            size += previous.text.len();
        } else {
            earlier |= trim_text(&mut message.text);
            size += message.text.len();
            messages.push_back(message);
        }
        while messages.len() > MAX_MESSAGES || size > MAX_TEXT {
            if let Some(old) = messages.pop_front() {
                size -= old.text.len();
                earlier = true;
            }
        }
    }
    Transcript {
        messages: messages.into(),
        earlier,
        bytes_read: 0,
    }
}

fn trim_text(text: &mut String) -> bool {
    if text.len() <= MAX_TEXT {
        return false;
    }
    let mut from = text.len() - MAX_TEXT;
    while !text.is_char_boundary(from) {
        from += 1;
    }
    text.drain(..from);
    true
}

fn message(harness: &str, v: &Value) -> Option<Message> {
    let spec = harness::by_name(harness)?;
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
            id: source.id(v).map(str::to_owned),
            at: v["timestamp"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(Into::into),
        });
    }
    None
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
