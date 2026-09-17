//! Historical session discovery, separate from fleet polling and dashboard loading.
//! `Reader` owns a worker and cache; requesting or polling a page performs no file IO.
use crate::{
    codex,
    config::HarnessKind,
    fleet,
    harness::{self, spec::Native},
    pi,
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    time::SystemTime,
};

const WINDOW: u64 = 64 * 1024;
const MAX_WINDOW: u64 = 1024 * 1024;
const COLUMN_CACHE: usize = 128;
pub const MAX_PAGE: usize = 100;

/// Explicit roots keep tests isolated and preserve the home needed by native resume.
#[derive(Clone, Debug)]
pub struct Source {
    pub harness: HarnessKind,
    pub home: PathBuf,
}

/// A session id is unique only within its harness and native home.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Key {
    pub harness: String,
    pub home: PathBuf,
    pub session_id: String,
}

/// Scalars read on demand. There is no live state or activity inference in history.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Columns {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Entry {
    pub key: Key,
    pub cwd: PathBuf,
    pub transcript: PathBuf,
    /// Codex archives are discoverable, but opening one may require native unarchive.
    pub archived: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Columns>,
}

/// Cursors belong to an indexed snapshot. A changed refresh requires a new first page.
#[derive(Clone, Debug)]
pub struct Cursor {
    generation: u64,
    last_activity: Option<DateTime<Utc>>,
    key: Key,
}

#[derive(Clone, Debug)]
pub struct Query {
    pub after: Option<Cursor>,
    pub limit: usize,
    /// Match indexed title, cwd, harness and session id before slicing a page.
    pub filter: String,
    /// The caller supplies known live/visible identities; history never polls processes.
    pub excluded: HashSet<Key>,
    pub include_archived: bool,
    /// Initial indexing is automatic. Later filesystem scans are explicit.
    pub refresh: bool,
    /// Request separately for the visible page after its lightweight rows have arrived.
    pub hydrate: bool,
    /// When set, hydrate only these entries within the requested page.
    pub hydrate_keys: Option<HashSet<Key>>,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            after: None,
            limit: 50,
            filter: String::new(),
            excluded: HashSet::new(),
            include_archived: false,
            refresh: false,
            hydrate: false,
            hydrate_keys: None,
        }
    }
}

/// Work done for this request, useful for fixtures and local measurements.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub indexed_files: usize,
    pub metadata_reads: usize,
    pub metadata_bytes: u64,
    pub hydrated_files: usize,
}

#[derive(Debug)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub next: Option<Cursor>,
    pub total: usize,
    pub generation: u64,
    pub stats: Stats,
    /// Native home aliases resolved by the worker, for matching live rows without UI file IO.
    pub homes: HashMap<PathBuf, PathBuf>,
}

/// One outstanding request, with no queue of obsolete scroll positions or refreshes.
/// Dropping the reader disconnects it; the worker exits after its current request.
pub struct Reader {
    requests: Sender<Query>,
    results: Receiver<Result<Page>>,
    busy: bool,
}

impl Reader {
    pub fn new(sources: Vec<Source>) -> std::io::Result<Self> {
        Self::with_sources(move || sources)
    }

    /// Discover configured native homes on the worker, never on the dashboard input thread.
    pub fn discover(claude: PathBuf) -> std::io::Result<Self> {
        Self::with_sources(move || {
            harness::known()
                .iter()
                .flat_map(|&kind| {
                    harness::spec(kind)
                        .home
                        .all(&claude)
                        .into_iter()
                        .map(move |home| Source {
                            harness: kind,
                            home,
                        })
                })
                .collect()
        })
    }

    fn with_sources(
        sources: impl FnOnce() -> Vec<Source> + Send + 'static,
    ) -> std::io::Result<Self> {
        let (requests, rx) = mpsc::channel();
        let (tx, results) = mpsc::channel();
        std::thread::Builder::new()
            .name("cones-history".into())
            .spawn(move || {
                let sources = sources();
                let mut cache = Cache::default();
                while let Ok(query) = rx.recv() {
                    if tx.send(cache.page(&sources, query)).is_err() {
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

    /// False means a request is still running; callers can try their latest query later.
    pub fn request(&mut self, query: Query) -> Result<bool> {
        if self.busy {
            return Ok(false);
        }
        self.requests.send(query).context("history worker exited")?;
        self.busy = true;
        Ok(true)
    }

    pub fn poll(&mut self) -> Option<Result<Page>> {
        if !self.busy {
            return None;
        }
        match self.results.try_recv() {
            Ok(result) => {
                self.busy = false;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.busy = false;
                Some(Err(anyhow::anyhow!("history worker exited")))
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
}

fn stamp(path: &Path) -> std::io::Result<Stamp> {
    let m = fs::metadata(path)?;
    Ok(Stamp {
        modified: m.modified()?,
        len: m.len(),
        device: m.dev(),
        inode: m.ino(),
    })
}

#[derive(Clone)]
struct Cached {
    stamp: Stamp,
    entry: Option<Entry>,
}

struct Hydrated {
    stamp: Stamp,
    statusline: Option<Stamp>,
    columns: Columns,
    used: u64,
}

#[derive(Default)]
struct NativeIndex {
    files: Vec<(PathBuf, Stamp)>,
    titles: HashMap<String, String>,
}

#[derive(Default)]
struct Cache {
    initialized: bool,
    generation: u64,
    files: HashMap<PathBuf, Cached>,
    entries: Vec<Entry>,
    native: HashMap<PathBuf, NativeIndex>,
    columns: HashMap<PathBuf, Hydrated>,
    used: u64,
    homes: HashMap<PathBuf, PathBuf>,
}

impl Cache {
    fn page(&mut self, sources: &[Source], query: Query) -> Result<Page> {
        ensure!(
            (1..=MAX_PAGE).contains(&query.limit),
            "history page size must be 1..={MAX_PAGE}"
        );
        let mut stats = Stats::default();
        if !self.initialized || query.refresh {
            self.scan(sources, &mut stats)?;
        }
        if let Some(cursor) = &query.after {
            ensure!(
                cursor.generation == self.generation,
                "history changed; restart pagination"
            );
        }
        let excluded: HashSet<Key> = query
            .excluded
            .into_iter()
            .map(|mut key| {
                if let Some(home) = self.homes.get(&key.home) {
                    key.home = home.clone();
                }
                key
            })
            .collect();
        let needle = query.filter.to_lowercase();
        let matched: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| {
                (query.include_archived || !e.archived)
                    && !excluded.contains(&e.key)
                    && (needle.is_empty()
                        || e.title
                            .as_deref()
                            .unwrap_or("")
                            .to_lowercase()
                            .contains(&needle)
                        || e.cwd.to_string_lossy().to_lowercase().contains(&needle)
                        || e.key.harness.contains(&needle)
                        || e.key.session_id.to_lowercase().contains(&needle))
            })
            .collect();
        let total = matched.len();
        let mut remaining = matched.into_iter().filter(|e| {
            query
                .after
                .as_ref()
                .is_none_or(|c| order(e.last_activity, &e.key, c.last_activity, &c.key).is_gt())
        });
        let mut entries: Vec<Entry> = remaining.by_ref().take(query.limit).cloned().collect();
        let next = remaining
            .next()
            .and_then(|_| entries.last())
            .map(|e| Cursor {
                generation: self.generation,
                last_activity: e.last_activity,
                key: e.key.clone(),
            });
        if query.hydrate {
            for entry in &mut entries {
                if query
                    .hydrate_keys
                    .as_ref()
                    .is_some_and(|keys| !keys.contains(&entry.key))
                {
                    continue;
                }
                let columns = self.hydrate(entry, &mut stats)?;
                // Native Codex names outrank transcript prompts, including cached hydration.
                if entry.key.harness != "codex" || entry.title.is_none() {
                    entry.title = columns.title.clone().or(entry.title.take());
                }
                entry.columns = Some(columns);
            }
        }
        stats.indexed_files = self.files.len();
        Ok(Page {
            entries,
            next,
            total,
            generation: self.generation,
            stats,
            homes: self.homes.clone(),
        })
    }

    fn scan(&mut self, sources: &[Source], stats: &mut Stats) -> Result<()> {
        let mut files = HashMap::new();
        let mut native_titles = HashMap::new();
        let mut roots = HashSet::new();
        let mut homes = HashMap::new();
        for source in sources {
            let home = match fs::canonicalize(&source.home) {
                Ok(home) => home,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).context("reading history home"),
            };
            homes.insert(source.home.clone(), home.clone());
            if !roots.insert((source.harness.to_string(), home.clone())) {
                continue;
            }
            let source = Source {
                harness: source.harness,
                home,
            };
            let titles = if harness::spec(source.harness).transcript.handler == Native::Codex {
                self.native_titles(&source.home)?
            } else {
                HashMap::new()
            };
            for (path, archived) in discover(&source)? {
                let current = match stamp(&path) {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e).context("reading history metadata"),
                };
                let cached = match self.files.get(&path).filter(|c| c.stamp == current) {
                    Some(c) => c.clone(),
                    None => {
                        let entry = metadata(&source, &path, archived, current.len, stats)?;
                        ensure!(
                            stamp(&path)? == current,
                            "history changed while indexing; refresh again"
                        );
                        Cached {
                            stamp: current,
                            entry,
                        }
                    }
                };
                if let Some(entry) = &cached.entry
                    && let Some(title) = titles.get(&entry.key.session_id)
                {
                    native_titles.insert(entry.key.clone(), title.clone());
                }
                files.insert(path, cached);
            }
        }
        // A Claude conversation can have copies under its original cwd and a worktree.
        // Keep the copy with the latest reported activity; path breaks equal-time ties.
        let mut unique: HashMap<Key, Entry> = HashMap::new();
        for e in files.values().filter_map(|c| c.entry.as_ref()) {
            let replace = unique.get(&e.key).is_none_or(|old| {
                (e.last_activity, &e.transcript) > (old.last_activity, &old.transcript)
            });
            if replace {
                unique.insert(e.key.clone(), e.clone());
            }
        }
        let mut entries: Vec<Entry> = unique.into_values().collect();
        for entry in &mut entries {
            if let Some(title) = native_titles.remove(&entry.key) {
                entry.title = Some(title);
            }
        }
        entries.sort_by(|a, b| order(a.last_activity, &a.key, b.last_activity, &b.key));
        if !self.initialized || entries != self.entries {
            self.generation += 1;
        }
        self.entries = entries;
        self.files = files;
        self.columns
            .retain(|p, c| self.files.get(p).is_some_and(|f| f.stamp == c.stamp));
        self.initialized = true;
        self.homes = homes;
        Ok(())
    }

    fn native_titles(&mut self, home: &Path) -> Result<HashMap<String, String>> {
        let mut files = Vec::new();
        for e in fs::read_dir(home)? {
            let p = e?.path();
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            if name == "session_index.jsonl"
                || (name.starts_with("state_")
                    && (name.ends_with(".sqlite") || name.ends_with(".sqlite-wal")))
            {
                files.push((p.clone(), stamp(&p)?));
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let cached = self.native.entry(home.to_owned()).or_default();
        if cached.files != files {
            cached.titles = codex::index(home).titles;
            cached.files = files;
        }
        Ok(cached.titles.clone())
    }

    fn hydrate(&mut self, e: &Entry, stats: &mut Stats) -> Result<Columns> {
        let current = stamp(&e.transcript)?;
        ensure!(
            self.files
                .get(&e.transcript)
                .is_some_and(|f| f.stamp == current),
            "history changed; refresh before loading columns"
        );
        let spec = harness::by_name(&e.key.harness).context("unknown history harness")?;
        let statusline = spec.transcript.statusline.as_ref().map(|source| {
            e.key
                .home
                .join(&source.directory)
                .join(format!("{}.json", e.key.session_id))
        });
        let status_stamp = statusline.as_deref().and_then(|path| stamp(path).ok());
        self.used += 1;
        if let Some(c) = self.columns.get_mut(&e.transcript)
            && c.stamp == current
            && c.statusline == status_stamp
        {
            c.used = self.used;
            return Ok(c.columns.clone());
        }
        let mut columns = match spec.transcript.handler {
            Native::Claude => fleet::history_columns(&e.transcript)?,
            Native::Codex => codex_columns(&e.transcript)?,
            Native::Pi => pi_columns(&e.transcript)?,
        };
        if status_stamp.is_some() {
            let mut bytes = Vec::new();
            File::open(statusline.as_ref().expect("stamped statusline"))?
                .take(WINDOW)
                .read_to_end(&mut bytes)?;
            columns.context_window = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| {
                v.pointer(
                    &spec
                        .transcript
                        .statusline
                        .as_ref()
                        .expect("stamped source")
                        .window_pointer,
                )?
                .as_u64()
            });
        }
        ensure!(
            stamp(&e.transcript)? == current,
            "history changed while loading columns; refresh again"
        );
        stats.hydrated_files += 1;
        self.columns.insert(
            e.transcript.clone(),
            Hydrated {
                stamp: current,
                statusline: status_stamp,
                columns: columns.clone(),
                used: self.used,
            },
        );
        while self.columns.len() > COLUMN_CACHE {
            let oldest = self
                .columns
                .iter()
                .min_by_key(|(_, c)| c.used)
                .map(|(p, _)| p.clone())
                .unwrap();
            self.columns.remove(&oldest);
        }
        Ok(columns)
    }
}

fn order(
    a: Option<DateTime<Utc>>,
    ak: &Key,
    b: Option<DateTime<Utc>>,
    bk: &Key,
) -> std::cmp::Ordering {
    b.cmp(&a).then_with(|| ak.cmp(bk))
}

fn children(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("reading {}", dir.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", dir.display())),
    }
}

fn discover(source: &Source) -> Result<Vec<(PathBuf, bool)>> {
    let mut out = Vec::new();
    let mut stack: Vec<_> = harness::spec(source.harness)
        .transcript
        .roots
        .iter()
        .map(|root| (source.home.join(&root.path), 0, root.depth, root.archived))
        .collect();
    while let Some((dir, depth, max_depth, archived)) = stack.pop() {
        for e in children(&dir)? {
            let ty = e.file_type()?;
            let p = e.path();
            if ty.is_dir() && max_depth.is_none_or(|max| depth < max) {
                stack.push((p, depth + 1, max_depth, archived));
            } else if ty.is_file() && p.extension().is_some_and(|e| e == "jsonl") {
                out.push((p, archived));
            }
        }
    }
    Ok(out)
}

fn timestamp(v: &Value) -> Option<DateTime<Utc>> {
    v["timestamp"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(Into::into)
}

/// Ignore fragments at window boundaries. A valid final line without a newline is accepted.
fn events(bytes: &[u8], start: bool, end: bool) -> impl Iterator<Item = Value> + '_ {
    let from = if start {
        0
    } else {
        bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1)
    };
    let to = if end {
        bytes.len()
    } else {
        bytes.iter().rposition(|b| *b == b'\n').unwrap_or(0)
    };
    bytes
        .get(from..to)
        .unwrap_or_default()
        .split(|b| *b == b'\n')
        .filter_map(|l| serde_json::from_slice(l).ok())
}

fn window(file: &mut File, offset: u64, n: u64, stats: &mut Stats) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(n).read_to_end(&mut bytes)?;
    stats.metadata_bytes += bytes.len() as u64;
    Ok(bytes)
}

fn metadata(
    source: &Source,
    path: &Path,
    archived: bool,
    len: u64,
    stats: &mut Stats,
) -> Result<Option<Entry>> {
    stats.metadata_reads += 1;
    let mut file = File::open(path)?;
    let mut size = WINDOW.min(len);
    let (head, identity) = loop {
        let bytes = window(&mut file, 0, size, stats)?;
        let head: Vec<Value> = events(&bytes, true, size == len).collect();
        let identity = identity(source.harness, path, &head);
        if identity.is_some() || size >= len || size >= MAX_WINDOW {
            break (head, identity);
        }
        size = (size * 4).min(len).min(MAX_WINDOW);
    };
    let Some((id, cwd, started)) = identity else {
        return Ok(None);
    };
    if cwd.as_os_str().is_empty() {
        return Ok(None);
    }
    if head.iter().any(|v| {
        v["isSidechain"] == true
            || v["payload"]["source"].get("subagent").is_some()
            || v["payload"]["source"] == "subagent"
    }) {
        return Ok(None);
    }
    let mut size = WINDOW.min(len);
    let (tail, last_activity) = loop {
        let bytes = window(&mut file, len - size, size, stats)?;
        let tail: Vec<Value> = events(&bytes, size == len, true).collect();
        let last = tail.iter().rev().find_map(timestamp);
        if last.is_some() || size >= len || size >= MAX_WINDOW {
            break (tail, last);
        }
        size = (size * 4).min(len).min(MAX_WINDOW);
    };
    let title = match harness::spec(source.harness).transcript.handler {
        Native::Claude => claude_title(&tail, true)
            .or_else(|| claude_title(&head, true))
            .or_else(|| claude_title(&tail, false))
            .or_else(|| claude_title(&head, false))
            .or_else(|| head.iter().find_map(claude_prompt)),
        Native::Codex => head.iter().find_map(|v| codex::prompt(&v.to_string())),
        Native::Pi => tail
            .iter()
            .rev()
            .find(|v| v["type"] == "session_info")
            .and_then(|v| v["name"].as_str())
            .and_then(fleet::headline)
            .or_else(|| {
                head.iter()
                    .find(|v| v["type"] == "message" && v["message"]["role"] == "user")
                    .and_then(|v| text(&v["message"]["content"]))
            }),
    };
    Ok(Some(Entry {
        key: Key {
            harness: source.harness.to_string(),
            home: source.home.clone(),
            session_id: id,
        },
        cwd,
        transcript: path.to_owned(),
        archived,
        started,
        last_activity,
        title,
        columns: None,
    }))
}

type Identity = (String, PathBuf, Option<DateTime<Utc>>);

fn identity(harness: HarnessKind, path: &Path, events: &[Value]) -> Option<Identity> {
    match harness::spec(harness).transcript.handler {
        Native::Claude => {
            let id = path.file_stem()?.to_str()?;
            uuid::Uuid::parse_str(id).ok()?;
            let cwd = events.iter().find_map(|v| v["cwd"].as_str())?;
            if cwd.is_empty() {
                return None;
            }
            Some((id.into(), cwd.into(), events.iter().find_map(timestamp)))
        }
        Native::Codex => events
            .iter()
            .find_map(|v| codex::meta(&v.to_string()))
            .map(|m| (m.session_id, m.cwd, Some(m.started))),
        Native::Pi => events
            .iter()
            .find_map(|v| pi::meta(&v.to_string()))
            .map(|m| (m.session_id, m.cwd, Some(m.started))),
    }
}

fn text(value: &Value) -> Option<String> {
    value.as_str().and_then(fleet::headline).or_else(|| {
        value
            .as_array()?
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .find_map(fleet::headline)
    })
}

fn claude_prompt(v: &Value) -> Option<String> {
    (v["type"] == "user" && v["isMeta"] != true)
        .then(|| text(&v["message"]["content"]))
        .flatten()
}

fn claude_title(events: &[Value], named: bool) -> Option<String> {
    events.iter().rev().find_map(|v| match v["type"].as_str() {
        Some("custom-title") if named => v["customTitle"].as_str().and_then(fleet::headline),
        Some("agent-name") if named => v["agentName"].as_str().and_then(fleet::headline),
        Some("ai-title") if !named => v["aiTitle"].as_str().and_then(fleet::headline),
        _ => None,
    })
}

fn codex_columns(path: &Path) -> Result<Columns> {
    let mut tail = codex::Tail::default();
    let mut title = None;
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        if title.is_none() {
            title = codex::prompt(&line);
        }
        tail.fold(&line);
        // History keeps scalars, not one activity allocation per line ever written.
        tail.activity.clear();
    }
    Ok(Columns {
        title,
        model: tail.model,
        tokens_in: tail.tokens_in,
        tokens_out: tail.tokens_out,
        context_tokens: tail.context_tokens,
        context_window: tail.context_window,
        last: tail.last,
        ..Columns::default()
    })
}

fn pi_columns(path: &Path) -> Result<Columns> {
    let mut out = Columns::default();
    let mut name = None;
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let tail = pi::tail(&line);
        name = tail.name.or(name);
        out.title = out.title.or(tail.prompt);
        out.model = tail.model.or(out.model);
        out.last = tail.last.or(out.last);
        if v["type"] == "message"
            && v["message"]["role"] == "assistant"
            && v["message"]["usage"].is_object()
        {
            let usage = &v["message"]["usage"];
            if ["input", "cacheRead", "cacheWrite"]
                .iter()
                .any(|k| usage[k].is_u64())
            {
                out.tokens_in = Some(out.tokens_in.unwrap_or(0) + tail.tokens_in);
                out.context_tokens = tail.context_tokens;
            }
            if usage["output"].is_u64() {
                out.tokens_out = Some(out.tokens_out.unwrap_or(0) + tail.tokens_out);
            }
            if v["message"]["usage"]["cost"]["total"].is_number() {
                out.cost_usd = Some(out.cost_usd.unwrap_or(0.0) + tail.cost_usd);
            }
        }
    }
    out.title = name.or(out.title);
    Ok(out)
}
