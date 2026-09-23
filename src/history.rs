//! Historical session discovery, separate from fleet polling and dashboard loading.
//! `Reader` owns a worker and cache; requesting or polling a page performs no file IO.
//!
//! Submit a [`Query`] and poll for one [`Page`] at a time. The first request builds
//! the metadata index; later scans require `refresh`. Snapshot cursors must restart
//! when a refresh changes the rows. Filtering and exclusions precede pagination.
//! Native-home aliases return with the page so callers need no path resolution in
//! their input loop. File mtime, length and inode invalidate cached metadata but
//! never supply activity timestamps.
//!
//! Metadata reads use bounded head and tail windows. Hydration is a separate
//! request for visible entries: it streams one transcript at a time and caches
//! only scalar summaries, without per-session activity vectors. This keeps
//! browsing a large archive independent of full transcript size.
use crate::cost::Reader as _;
use crate::{
    codex,
    config::HarnessKind,
    fleet,
    harness::{self, spec::Native},
    pi, search,
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

/// Initial head and tail windows, grown independently when identity or activity is missing.
const WINDOW: u64 = 64 * 1024;
/// Widening this ceiling recovers metadata at the cost of more IO on each uncached file.
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
    pub cost_info: Option<crate::cost::Info>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit: Option<search::Hit>,
}

/// Cursors belong to an indexed snapshot. A changed refresh requires a new first page.
#[derive(Clone, Debug)]
pub struct Cursor {
    generation: u64,
    last_activity: Option<DateTime<Utc>>,
    key: Key,
    filter: String,
    search: search::Mode,
    search_revision: u64,
    score: Option<f32>,
}

#[derive(Clone, Debug)]
pub struct Query {
    pub after: Option<Cursor>,
    pub limit: usize,
    /// Search metadata and conversation passages before slicing a page.
    pub filter: String,
    /// Words the passages must contain, or the meaning they must be close to.
    pub search: search::Mode,
    /// The caller supplies known live/visible identities; history never polls processes.
    pub excluded: HashSet<Key>,
    pub include_archived: bool,
    /// Initial indexing is automatic. Later filesystem scans are explicit.
    pub refresh: bool,
    /// Finish the meaning index for every conversation, with no query to search for.
    pub index: bool,
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
            search: search::Mode::default(),
            excluded: HashSet::new(),
            include_archived: false,
            refresh: false,
            index: false,
            hydrate: false,
            hydrate_keys: None,
        }
    }
}

/// Work done for this request, useful for fixtures and local measurements.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Stats {
    pub indexed_files: usize,
    pub metadata_reads: usize,
    pub metadata_bytes: u64,
    pub hydrated_files: usize,
    pub metadata_cache_hits: usize,
    pub column_cache_hits: usize,
    pub index_ms: f64,
    pub hydrate_ms: f64,
    pub worker_ms: f64,
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
    pub search_pending: bool,
    pub search_status: Option<String>,
    pub search_error: Option<String>,
}

/// One outstanding request, with no queue of obsolete scroll positions or refreshes.
/// Dropping the reader disconnects it; the worker exits after its current request.
pub struct Reader {
    requests: Sender<Query>,
    results: Receiver<Result<Page>>,
    busy: bool,
}

impl Reader {
    /// Fixture-friendly text search, with no persistent cache or model downloads.
    pub fn new(sources: Vec<Source>) -> std::io::Result<Self> {
        Self::with_sources(move || sources, None)
    }

    /// Search explicit native homes with persistent text and local semantic indexing.
    pub fn with_search(sources: Vec<Source>, state: PathBuf) -> std::io::Result<Self> {
        Self::with_sources(move || sources, Some(state.join("search")))
    }

    /// Discover configured native homes on the worker, never on the dashboard input thread.
    pub fn discover(claude: PathBuf, state: PathBuf) -> std::io::Result<Self> {
        Self::with_sources(move || sources(&claude), Some(state.join("search")))
    }

    fn with_sources(
        sources: impl FnOnce() -> Vec<Source> + Send + 'static,
        search_directory: Option<PathBuf>,
    ) -> std::io::Result<Self> {
        let (requests, rx) = mpsc::channel();
        let (tx, results) = mpsc::channel();
        std::thread::Builder::new()
            .name("cones-history".into())
            .spawn(move || {
                let sources = sources();
                let mut cache = Cache {
                    search_directory,
                    ..Default::default()
                };
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

/// Every configured native home, including the siblings a harness keeps beside its own.
pub fn sources(claude: &Path) -> Vec<Source> {
    harness::known()
        .iter()
        .flat_map(|&kind| {
            harness::spec(kind)
                .home
                .all(claude)
                .into_iter()
                .map(move |home| Source {
                    harness: kind,
                    home,
                })
        })
        .collect()
}

/// One scan of every source, for a caller that wants the whole list instead of a page.
/// `Reader` is asynchronous because the dashboard cannot block; a command already has.
pub fn all(sources: &[Source]) -> Result<Vec<Entry>> {
    let mut cache = Cache::default();
    cache.scan(sources, &mut Stats::default())?;
    Ok(cache.entries)
}

/// The command and MCP surfaces use the same query engine without a dashboard worker.
/// Keep this for a client's lifetime so repeated searches reuse metadata and embeddings.
pub(crate) struct Catalog {
    sources: Vec<Source>,
    cache: Cache,
}

impl Catalog {
    pub(crate) fn new(sources: Vec<Source>, state: Option<PathBuf>) -> Self {
        Self {
            sources,
            cache: Cache {
                search_directory: state.map(|p| p.join("search")),
                ..Default::default()
            },
        }
    }

    pub(crate) fn page(
        &mut self,
        query: Query,
        offset: usize,
        include: impl Fn(&Entry) -> bool,
    ) -> Result<Page> {
        self.cache
            .page_filtered(&self.sources, query, offset, include)
    }

    /// The meaning index over every conversation. `sync` rereads changed transcripts;
    /// `embed` schedules the next batch, which is the only step that loads a model.
    pub(crate) fn index(
        &mut self,
        sync: bool,
        embed: bool,
    ) -> Result<(search::Progress, Option<search::Results>)> {
        let cache = &mut self.cache;
        if sync || !cache.initialized {
            cache.scan(&self.sources, &mut Stats::default())?;
            cache.search_results = None;
        }
        if cache.search.is_none() {
            cache.search = Some(search::Index::open(cache.search_directory.clone())?);
        }
        let index = cache.search.as_mut().unwrap();
        let _guard = index.lock()?;
        if sync {
            index.sync(&cache.entries)?;
        }
        let results = embed.then(|| index.fill(false)).transpose()?;
        Ok((index.progress()?, results))
    }
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
    pricing: Option<crate::cost::CatalogStamp>,
    columns: Columns,
    used: u64,
}

#[derive(Default)]
struct NativeIndex {
    files: Vec<(PathBuf, Stamp)>,
    titles: HashMap<String, String>,
}

struct DatabaseIndex {
    stamp: crate::opencode::Fingerprint,
    entries: Vec<Entry>,
}

struct DatabaseColumns {
    stamp: crate::opencode::Fingerprint,
    pricing: Option<crate::cost::CatalogStamp>,
    columns: Columns,
    used: u64,
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
    databases: HashMap<PathBuf, DatabaseIndex>,
    database_columns: HashMap<Key, DatabaseColumns>,
    search_directory: Option<PathBuf>,
    search: Option<search::Index>,
    search_results: Option<((String, search::Mode), search::Results)>,
    search_revision: u64,
}

impl Cache {
    fn page(&mut self, sources: &[Source], query: Query) -> Result<Page> {
        self.page_filtered(sources, query, 0, |_| true)
    }

    fn page_filtered(
        &mut self,
        sources: &[Source],
        query: Query,
        offset: usize,
        include: impl Fn(&Entry) -> bool,
    ) -> Result<Page> {
        let started = std::time::Instant::now();
        ensure!(
            (1..=MAX_PAGE).contains(&query.limit),
            "history page size must be 1..={MAX_PAGE}"
        );
        let mut stats = Stats::default();
        if !self.initialized || query.refresh {
            let indexing = std::time::Instant::now();
            self.scan(sources, &mut stats)?;
            self.search_results = None;
            stats.index_ms = indexing.elapsed().as_secs_f64() * 1000.0;
        }
        if let Some(cursor) = &query.after {
            ensure!(
                cursor.generation == self.generation,
                "history changed; restart pagination"
            );
            ensure!(
                cursor.filter == query.filter && cursor.search == query.search,
                "search changed; restart pagination"
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
        let mut matched: Vec<Entry> = self
            .entries
            .iter()
            .filter(|e| (query.include_archived || !e.archived) && !excluded.contains(&e.key))
            .cloned()
            .collect();
        let searching = !query.filter.trim().is_empty();
        let mut search_pending = false;
        let mut search_status = None;
        let mut search_error = None;
        if searching {
            if self
                .search_results
                .as_ref()
                .is_none_or(|((q, m), _)| q != &query.filter || *m != query.search)
                || (query.after.is_none() && !query.hydrate)
            {
                if self.search.is_none() {
                    self.search = Some(search::Index::open(self.search_directory.clone())?);
                }
                let index = self.search.as_mut().unwrap();
                let _guard = index.lock()?;
                index.sync(&matched)?;
                let results = index.search(&matched, &query.filter, query.search, query.refresh)?;
                self.search_results = Some(((query.filter.clone(), query.search), results));
                self.search_revision += 1;
            }
            if let Some(cursor) = &query.after {
                ensure!(
                    cursor.search_revision == self.search_revision,
                    "search changed; restart pagination"
                );
            }
            let results = &self.search_results.as_ref().unwrap().1;
            search_pending = results.pending;
            search_status = results.status.clone();
            search_error = results.error.clone();
            matched.retain_mut(|e| {
                e.hit = results.hits.get(&search::identity(e)).cloned();
                e.hit.is_some()
            });
            matched.sort_by(|a, b| {
                b.hit
                    .as_ref()
                    .unwrap()
                    .score
                    .total_cmp(&a.hit.as_ref().unwrap().score)
                    .then_with(|| order(a.last_activity, &a.key, b.last_activity, &b.key))
            });
        } else if query.index && !query.hydrate {
            // Nothing to match, so the page is the whole list; the work is the index itself.
            if self.search.is_none() {
                self.search = Some(search::Index::open(self.search_directory.clone())?);
            }
            let index = self.search.as_mut().unwrap();
            let _guard = index.lock()?;
            index.sync(&matched)?;
            let results = index.fill(query.refresh)?;
            search_pending = results.pending;
            search_status = results.status;
            search_error = results.error;
        }
        // Scope after searching, before pagination. A scoped command must not remove other
        // projects from the shared index, and its limit counts only eligible results.
        matched.retain(include);
        let total = matched.len();
        let mut remaining = matched
            .into_iter()
            .filter(|e| {
                query.after.as_ref().is_none_or(|c| {
                    let rank = e.hit.as_ref().map(|h| h.score);
                    c.score
                        .zip(rank)
                        .map_or(std::cmp::Ordering::Equal, |(old, new)| old.total_cmp(&new))
                        .then_with(|| order(e.last_activity, &e.key, c.last_activity, &c.key))
                        .is_gt()
                })
            })
            .skip(offset);
        let mut entries: Vec<Entry> = remaining.by_ref().take(query.limit).collect();
        let next = remaining
            .next()
            .and_then(|_| entries.last())
            .map(|e| Cursor {
                generation: self.generation,
                last_activity: e.last_activity,
                key: e.key.clone(),
                filter: query.filter.clone(),
                search: query.search,
                search_revision: self.search_revision,
                score: e.hit.as_ref().map(|h| h.score),
            });
        if query.hydrate {
            let hydrating = std::time::Instant::now();
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
            stats.hydrate_ms = hydrating.elapsed().as_secs_f64() * 1000.0;
        }
        stats.indexed_files = self.files.len() + self.databases.len();
        stats.worker_ms = started.elapsed().as_secs_f64() * 1000.0;
        Ok(Page {
            entries,
            next,
            total,
            generation: self.generation,
            stats,
            homes: self.homes.clone(),
            search_pending,
            search_status,
            search_error,
        })
    }

    fn scan(&mut self, sources: &[Source], stats: &mut Stats) -> Result<()> {
        let mut files = HashMap::new();
        let mut databases = HashMap::new();
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
            if source.harness == HarnessKind::Opencode {
                for db in crate::opencode::databases(&source.home)? {
                    let current = crate::opencode::fingerprint(&db)?;
                    let entries = match self.databases.get(&db).filter(|c| c.stamp == current) {
                        Some(c) => {
                            stats.metadata_cache_hits += 1;
                            c.entries.clone()
                        }
                        None => {
                            let entries = crate::opencode::history(&db, &source.home)?;
                            ensure!(
                                crate::opencode::fingerprint(&db)? == current,
                                "OpenCode history changed while indexing; refresh again"
                            );
                            stats.metadata_reads += 1;
                            entries
                        }
                    };
                    databases.insert(
                        db,
                        DatabaseIndex {
                            stamp: current,
                            entries,
                        },
                    );
                }
                continue;
            }
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
                    Some(c) => {
                        stats.metadata_cache_hits += 1;
                        c.clone()
                    }
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
        for e in files
            .values()
            .filter_map(|c| c.entry.as_ref())
            .chain(databases.values().flat_map(|db| &db.entries))
        {
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
        self.databases = databases;
        self.database_columns
            .retain(|key, _| self.entries.iter().any(|entry| &entry.key == key));
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
        if e.key.harness == "opencode" {
            return self.hydrate_database(e, stats);
        }
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
        let catalog = crate::cost::snapshot();
        let pricing = catalog.as_ref().map(|c| c.stamp.clone());
        self.used += 1;
        if let Some(c) = self.columns.get_mut(&e.transcript)
            && c.stamp == current
            && c.statusline == status_stamp
            && c.pricing == pricing
        {
            c.used = self.used;
            stats.column_cache_hits += 1;
            return Ok(c.columns.clone());
        }
        let mut columns = match spec.transcript.handler {
            Native::Claude => fleet::history_columns_priced(&e.transcript, catalog.as_deref())?,
            Native::Codex => codex_columns_priced(&e.transcript, catalog.as_deref())?,
            Native::Pi => pi_columns(&e.transcript, catalog.as_deref())?,
            Native::Opencode => unreachable!("database hydration handled above"),
            Native::External(_) => anyhow::bail!("native history unavailable for this harness"),
        };
        if status_stamp.is_some() {
            let mut bytes = Vec::new();
            File::open(statusline.as_ref().expect("stamped statusline"))?
                .take(WINDOW)
                .read_to_end(&mut bytes)?;
            let source = spec.transcript.statusline.as_ref().expect("stamped source");
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                columns.context_window = v.pointer(&source.window_pointer).and_then(Value::as_u64);
                (columns.cost_usd, columns.cost_info) = crate::cost::prefer_native(
                    fleet::statusline_cost(source, &v),
                    (columns.cost_usd, columns.cost_info),
                );
            }
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
                pricing,
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

    fn hydrate_database(&mut self, e: &Entry, stats: &mut Stats) -> Result<Columns> {
        let current = crate::opencode::fingerprint(&e.transcript)?;
        let catalog = crate::cost::snapshot();
        let pricing = catalog.as_ref().map(|c| c.stamp.clone());
        ensure!(
            self.databases
                .get(&e.transcript)
                .is_some_and(|db| db.stamp == current),
            "OpenCode history changed; refresh before loading columns"
        );
        self.used += 1;
        if let Some(cached) = self
            .database_columns
            .get_mut(&e.key)
            .filter(|c| c.stamp == current && c.pricing == pricing)
        {
            cached.used = self.used;
            stats.column_cache_hits += 1;
            return Ok(cached.columns.clone());
        }
        let columns =
            crate::opencode::columns_priced(&e.transcript, &e.key.session_id, catalog.as_deref())?;
        ensure!(
            crate::opencode::fingerprint(&e.transcript)? == current,
            "OpenCode history changed while loading columns; refresh again"
        );
        stats.hydrated_files += 1;
        self.database_columns.insert(
            e.key.clone(),
            DatabaseColumns {
                stamp: current,
                pricing,
                columns: columns.clone(),
                used: self.used,
            },
        );
        while self.database_columns.len() > COLUMN_CACHE {
            let oldest = self
                .database_columns
                .iter()
                .min_by_key(|(_, c)| c.used)
                .map(|(key, _)| key.clone())
                .unwrap();
            self.database_columns.remove(&oldest);
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
    if source.harness.terminal_only() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut stack: Vec<_> = harness::spec(source.harness)
        .transcript
        .roots
        .iter()
        .map(|root| (root.resolve(&source.home), 0, root.depth, root.archived))
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
    let definition = harness::spec(source.harness);
    let user = &definition.transcript.messages.user;
    let title = match definition.transcript.handler {
        Native::Claude => claude_title(&tail, true)
            .or_else(|| claude_title(&head, true))
            .or_else(|| claude_title(&tail, false))
            .or_else(|| claude_title(&head, false))
            .or_else(|| user_title(user, &head)),
        Native::Codex => head.iter().find_map(|v| codex::prompt(&v.to_string())),
        Native::Pi => tail
            .iter()
            .rev()
            .find(|v| v["type"] == "session_info")
            .and_then(|v| v["name"].as_str())
            .and_then(fleet::headline)
            .or_else(|| user_title(user, &head)),
        Native::Opencode => unreachable!("SQLite metadata uses its native reader"),
        Native::External(_) => None,
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
        hit: None,
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
        Native::Opencode | Native::External(_) => None,
    }
}

fn user_title(user: &harness::spec::MessageText, events: &[Value]) -> Option<String> {
    events
        .iter()
        .find_map(|event| user.headline_with_attachments(event, true))
}

fn claude_title(events: &[Value], named: bool) -> Option<String> {
    events.iter().rev().find_map(|v| match v["type"].as_str() {
        Some("custom-title") if named => v["customTitle"].as_str().and_then(fleet::headline),
        Some("agent-name") if named => v["agentName"].as_str().and_then(fleet::headline),
        Some("ai-title") if !named => v["aiTitle"].as_str().and_then(fleet::headline),
        _ => None,
    })
}

fn codex_columns_priced(path: &Path, catalog: Option<&crate::cost::Catalog>) -> Result<Columns> {
    let mut tail = codex::Tail::default();
    let mut title = None;
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        if title.is_none() {
            title = codex::prompt(&line);
        }
        tail.fold_priced(&line, catalog);
        // History keeps scalars, not one activity allocation per line ever written.
        tail.activity.clear();
    }
    let (cost_usd, cost_info) = tail.accounting.report(None);
    Ok(Columns {
        title,
        model: tail.model,
        tokens_in: tail.tokens_in,
        tokens_out: tail.tokens_out,
        context_tokens: tail.context_tokens,
        context_window: tail.context_window,
        cost_usd,
        cost_info,
        last: tail.last,
    })
}

fn pi_columns(path: &Path, catalog: Option<&crate::cost::Catalog>) -> Result<Columns> {
    let mut out = Columns::default();
    let mut name = None;
    let mut costs = harness::spec(HarnessKind::Pi)
        .transcript
        .handler
        .accounting();
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        costs.observe(&v, catalog);
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
        }
    }
    (out.cost_usd, out.cost_info) = costs.report(None);
    out.title = name.or(out.title);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cost_history_and_live_use_the_same_pricing_and_pi_unknowns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let text = [
            json!({"type":"session_meta","payload":{"model_provider":"provider"}}),
            json!({"type":"turn_context","payload":{"model":"model"}}),
            json!({"type":"event_msg","payload":{"type":"token_count","info":{
                "total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"cache_write_input_tokens":30,"output_tokens":10},
                "last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"cache_write_input_tokens":30,"output_tokens":10}
            }}}),
        ].iter().map(|v| format!("{v}\n")).collect::<String>();
        fs::write(&path, &text).unwrap();
        let catalog = crate::cost::tests::fixture();
        let history = codex_columns_priced(&path, Some(&catalog)).unwrap();
        let mut live = codex::Tail::default();
        live.fold_priced(&text, Some(&catalog));
        assert_eq!(
            (history.cost_usd, history.cost_info),
            live.accounting.report(None)
        );

        let message = |id: &str, cost: f64| {
            json!({"type":"message","id":id,"message":{
                "role":"assistant","usage":{"input":2,"output":1,"cost":{"total":cost}}
            }})
        };
        let priced = message("one", 0.2);
        let unpriced = message("two", 0.0);
        let text = format!("{priced}\n{priced}\n{unpriced}\n");
        fs::write(&path, &text).unwrap();
        let history = pi_columns(&path, None).unwrap();
        let live = pi::tail(&text);
        assert_eq!(history.cost_usd, Some(0.2));
        assert_eq!(
            history.cost_info.as_ref().unwrap().coverage,
            crate::cost::Coverage::Partial
        );
        assert_eq!(
            (history.cost_usd, history.cost_info),
            live.costs.report(None)
        );
        fs::write(&path, format!("{unpriced}\n")).unwrap();
        assert!(pi_columns(&path, None).unwrap().cost_usd.is_none());
    }

    #[test]
    fn pi_history_and_live_share_native_prices_fallbacks_and_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let catalog = crate::cost::tests::fixture();
        let estimated = pi::tests::unpriced_message("estimated");
        let mut native = pi::tests::unpriced_message("native");
        native["message"]["usage"]["cost"]["total"] = json!(0.2);
        let mut unknown = pi::tests::unpriced_message("unknown");
        unknown["message"]["model"] = json!("not-in-catalog");
        let text = format!("{estimated}\n{estimated}\n{native}\n{unknown}\n");
        fs::write(&path, &text).unwrap();
        for catalog in [Some(&catalog), None] {
            let history = pi_columns(&path, catalog).unwrap();
            let live = pi::tail_priced(&text, catalog);
            assert_eq!(
                (history.cost_usd, history.cost_info),
                live.costs.report(None)
            );
        }
        let columns = pi_columns(&path, Some(&catalog)).unwrap();
        assert!((columns.cost_usd.unwrap() - 0.20025).abs() < 1e-12);
        let info = columns.cost_info.unwrap();
        assert_eq!(info.source, crate::cost::Source::ModelsDev);
        assert_eq!(info.coverage, crate::cost::Coverage::Partial);
        assert_eq!((info.priced_records, info.unpriced_records), (2, 1));
    }

    fn history_entry(harness: HarnessKind, records: &[Value]) -> Entry {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.jsonl");
        let text: String = records.iter().map(|v| format!("{v}\n")).collect();
        fs::write(&path, &text).unwrap();
        metadata(
            &Source {
                harness,
                home: dir.path().to_owned(),
            },
            &path,
            false,
            text.len() as u64,
            &mut Stats::default(),
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn history_titles_use_declared_user_content() {
        for kind in [HarnessKind::Claude, HarnessKind::Pi] {
            let header = json!({
                "type": if kind == HarnessKind::Pi { "session" } else { "system" },
                "id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "cwd": "/fixture",
                "timestamp": "2026-09-17T10:00:00Z",
            });
            let user = |content: Value| {
                if kind == HarnessKind::Claude {
                    json!({"type": "user", "message": {"content": content}})
                } else {
                    json!({"type": "message", "message": {"role": "user", "content": content}})
                }
            };
            for (content, expected) in [
                (json!([{"type": "image"}]), "[image]"),
                (json!([{"type": "input_image"}]), "[image]"),
                (json!([{"type": "document"}]), "[document]"),
                (json!("String instruction"), "String instruction"),
                (
                    json!([{"type": "text", "text": "Block instruction\nMore detail"}]),
                    "Block instruction",
                ),
            ] {
                let mut records = vec![header.clone(), user(json!([]))];
                if kind == HarnessKind::Claude {
                    let mut hidden = user(json!("Injected instruction"));
                    hidden["isMeta"] = json!(true);
                    records.push(hidden);
                }
                records.push(user(content));
                records.push(user(json!("Later instruction")));
                assert_eq!(
                    history_entry(kind, &records).title.as_deref(),
                    Some(expected),
                    "{kind}: skip empty or excluded messages and preserve attachment labels"
                );
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("transcript.jsonl");
                fs::write(
                    &path,
                    records.iter().map(|v| format!("{v}\n")).collect::<String>(),
                )
                .unwrap();
                let columns = if kind == HarnessKind::Claude {
                    fleet::history_columns(&path).unwrap()
                } else {
                    pi_columns(&path, None).unwrap()
                };
                assert_eq!(
                    columns.title.as_deref(),
                    Some(expected),
                    "{kind}: hydration preserves the selected user source"
                );
                records.push(if kind == HarnessKind::Claude {
                    json!({"type": "custom-title", "customTitle": "Named session"})
                } else {
                    json!({"type": "session_info", "name": "Named session"})
                });
                assert_eq!(
                    history_entry(kind, &records).title.as_deref(),
                    Some("Named session"),
                    "{kind}: a native title precedes the user-message fallback"
                );
            }
        }
    }

    #[test]
    fn user_titles_honor_selector_overrides() {
        let mut user: harness::spec::MessageText = serde_yaml::from_str(
            r#"
headline: last
sources:
  - when: {"/kind": instruction}
    unless: [/hidden]
    path: /body
    shape: blocks
    types: [plain]
    labels: {picture: "[picture]"}
"#,
        )
        .unwrap();
        let events = [
            json!({"type":"user", "message":{"content":"Old path"}}),
            json!({"kind":"instruction", "hidden":true, "body":[{"type":"plain","text":"Excluded"}]}),
            json!({"kind":"instruction", "body":"Wrong shape"}),
            json!({"kind":"instruction", "body":[{"type":"text","text":"Wrong block type"}]}),
            json!({"kind":"instruction", "body":[
                {"type":"picture"},
                {"type":"plain","text":"First\nMore detail"},
                {"type":"plain","text":"Last\nMore detail"},
                {"type":"text","text":"Excluded block"}
            ]}),
        ];
        assert_eq!(user_title(&user, &events[..4]), None);
        assert_eq!(user_title(&user, &events).as_deref(), Some("Last"));
        user.headline = harness::spec::Headline::First;
        assert_eq!(user_title(&user, &events).as_deref(), Some("[picture]"));
    }
}
