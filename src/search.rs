//! Local history search. SQLite holds text and cached passage embeddings; inference
//! runs on a separate worker. Neither a dashboard draw nor a test starts a model.
use crate::{history::Entry, transcript};
use anyhow::{Context, Result, ensure};
use candle_core::{DType, Device, Tensor};
use candle_transformers::models::bert::{BertModel, Config};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

// Change the namespace when the model, chunking or extraction changes.
const VERSION: &str = "minilm-l6-v2-passages-v1";
const BATCH: usize = 32;
const DIMENSIONS: usize = 384;
/// MiniLM puts unrelated English prose around 0.3, so anything lower is noise, not a result.
const SEMANTIC_FLOOR: f32 = 0.5;
const MODEL_REVISION: &str = "1110a243fdf4706b3f48f1d95db1a4f5529b4d41";
const MODEL_FILES: &[(&str, &str)] = &[
    (
        "config.json",
        "953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41",
    ),
    (
        "tokenizer.json",
        "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037",
    ),
    (
        "model.safetensors",
        "53aa51172d142c89d9012cce15ae4d6cc0ca6895895114379cacb4fab128d9db",
    ),
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// Start and end of a JSONL record, or the ordinal of an OpenCode message.
    pub offset: u64,
    pub end: u64,
    /// The actual matched passage, also used to position long individual messages.
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub snippet: String,
    pub anchor: Option<Anchor>,
    pub semantic: bool,
    pub score: f32,
}

/// The search the reader was asked for. Meaning is opt-in: words alone never load a model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    #[default]
    Words,
    Meaning,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Words => "words",
            Mode::Meaning => "meaning",
        }
    }

    pub fn other(self) -> Self {
        match self {
            Mode::Words => Mode::Meaning,
            Mode::Meaning => Mode::Words,
        }
    }
}

#[derive(Default)]
pub struct Results {
    pub hits: HashMap<String, Hit>,
    pub pending: bool,
    pub status: Option<String>,
    pub error: Option<String>,
}

pub(crate) fn identity(entry: &Entry) -> String {
    serde_json::to_string(&entry.key).expect("history key is serializable")
}

pub(crate) struct Index {
    db: Connection,
    worker: Option<Embeddings>,
    directory: Option<PathBuf>,
    query_vector: Option<(String, Vec<f32>)>,
    failure: Option<String>,
}

impl Index {
    /// An in-memory text index is deliberately model-free, including in fixtures.
    pub(crate) fn open(directory: Option<PathBuf>) -> Result<Self> {
        let db = if let Some(directory) = &directory {
            crate::private_dir(directory)?;
            let path = directory.join(format!("{VERSION}.sqlite"));
            drop(crate::private_file(&path)?);
            Connection::open(path)?
        } else {
            Connection::open_in_memory()?
        };
        db.busy_timeout(Duration::from_secs(2))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, stamp TEXT NOT NULL);
             CREATE VIRTUAL TABLE IF NOT EXISTS passages USING fts5(
                 text, session UNINDEXED, anchor UNINDEXED, hash UNINDEXED,
                 tokenize = 'porter unicode61');
             CREATE TABLE IF NOT EXISTS vectors (hash TEXT PRIMARY KEY, vector BLOB NOT NULL);",
        )?;
        Ok(Self {
            db,
            worker: None,
            directory,
            query_vector: None,
            failure: None,
        })
    }

    /// A file is streamed only when its fingerprint or native title changes.
    pub(crate) fn sync(&mut self, entries: &[Entry]) -> Result<()> {
        let present: HashSet<_> = entries.iter().map(identity).collect();
        let old: Vec<String> = self
            .db
            .prepare("SELECT id FROM sessions")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in old.into_iter().filter(|id| !present.contains(id)) {
            let tx = self.db.transaction()?;
            tx.execute("DELETE FROM passages WHERE session = ?1", [&id])?;
            tx.execute("DELETE FROM sessions WHERE id = ?1", [&id])?;
            tx.commit()?;
        }
        for entry in entries {
            let id = identity(entry);
            let stamp = fingerprint(entry)?;
            let cached: Option<String> = self
                .db
                .query_row("SELECT stamp FROM sessions WHERE id = ?1", [&id], |r| {
                    r.get(0)
                })
                .optional()?;
            if cached.as_deref() == Some(&stamp) {
                continue;
            }
            let tx = self.db.transaction()?;
            tx.execute("DELETE FROM passages WHERE session = ?1", [&id])?;
            let mut insert = tx.prepare(
                "INSERT INTO passages (text, session, anchor, hash) VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut put = |text: &str, anchor: Option<Anchor>| -> Result<()> {
                for text in chunks(text) {
                    let anchor = anchor.clone().map(|mut a| {
                        a.text = text.to_owned();
                        a
                    });
                    let encoded = anchor.as_ref().map(serde_json::to_string).transpose()?;
                    insert.execute(params![text, id, encoded, digest(text)])?;
                }
                Ok(())
            };
            if let Some(title) = &entry.title {
                put(title, None)?;
            }
            transcript::search_passages(entry, |text, offset, end| {
                put(
                    text,
                    Some(Anchor {
                        offset,
                        end,
                        text: String::new(),
                    }),
                )
            })?;
            drop(insert);
            ensure!(
                fingerprint(entry)? == stamp,
                "history changed while indexing search; refresh"
            );
            tx.execute(
                "INSERT OR REPLACE INTO sessions (id, stamp) VALUES (?1, ?2)",
                params![id, stamp],
            )?;
            tx.commit()?;
        }
        self.db.execute(
            "DELETE FROM vectors WHERE hash NOT IN (SELECT hash FROM passages)",
            [],
        )?;
        Ok(())
    }

    pub(crate) fn search(
        &mut self,
        entries: &[Entry],
        query: &str,
        mode: Mode,
        retry: bool,
    ) -> Result<Results> {
        if retry && self.failure.is_some() {
            self.worker = None;
            self.failure = None;
        }
        self.poll()?;
        let mut results = Results::default();
        let eligible: HashSet<_> = entries.iter().map(identity).collect();
        let needle = query.trim().to_lowercase();
        for entry in entries {
            let metadata = format!(
                "{}\n{}\n{}\n{}",
                entry.title.as_deref().unwrap_or(""),
                entry.cwd.display(),
                entry.key.harness,
                entry.key.session_id
            );
            if metadata.to_lowercase().contains(&needle) {
                results.hits.insert(
                    identity(entry),
                    Hit {
                        snippet: String::new(),
                        anchor: None,
                        semantic: false,
                        score: 3.0,
                    },
                );
            }
        }
        let terms = terms(query);
        // Quote each token. User input is always data, never an FTS expression.
        let expression = terms
            .iter()
            .map(|s| format!("\"{}\"*", s.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");
        if mode == Mode::Words && !expression.is_empty() {
            let mut statement = self.db.prepare(
                "SELECT session, text, anchor, rank FROM passages
                 WHERE passages MATCH ?1 ORDER BY rank, rowid",
            )?;
            let mut rows = statement.query([expression])?;
            let mut rank = 0;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                if !eligible.contains(&id) {
                    continue;
                }
                let text: String = row.get(1)?;
                let anchor: Option<String> = row.get(2)?;
                let hit = Hit {
                    snippet: excerpt(&text, &terms),
                    anchor: anchor.map(|a| serde_json::from_str(&a)).transpose()?,
                    semantic: false,
                    score: 2.0 + 1.0 / (60.0 + rank as f32),
                };
                rank += 1;
                results
                    .hits
                    .entry(id)
                    .and_modify(|old| {
                        // Preserve exact metadata rank, but give it a passage to preview.
                        if old.anchor.is_none() && hit.anchor.is_some() {
                            old.anchor = hit.anchor.clone();
                            old.snippet = hit.snippet.clone();
                        }
                    })
                    .or_insert(hit);
            }
        }
        // The embedded query drops filler as well: "something about X" otherwise sits halfway
        // between X and every other conversational sentence in the history.
        let cleaned = terms.join(" ");
        if mode == Mode::Meaning
            && let Some((_, vector)) = self.query_vector.as_ref().filter(|(q, _)| *q == cleaned)
        {
            let mut statement = self.db.prepare(
                "SELECT p.session, p.text, p.anchor, v.vector FROM passages p
                 JOIN vectors v ON v.hash = p.hash ORDER BY p.rowid",
            )?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                if !eligible.contains(&id) {
                    continue;
                }
                let bytes: Vec<u8> = row.get(3)?;
                let score = cosine(vector, &decode(&bytes));
                if score < SEMANTIC_FLOOR {
                    continue;
                }
                let text: String = row.get(1)?;
                let anchor: Option<String> = row.get(2)?;
                let hit = Hit {
                    snippet: excerpt(&text, &terms),
                    anchor: anchor.map(|a| serde_json::from_str(&a)).transpose()?,
                    semantic: true,
                    score,
                };
                results
                    .hits
                    .entry(id)
                    .and_modify(|old| {
                        if old.semantic && score > old.score {
                            *old = hit.clone();
                        } else if !old.semantic {
                            // Semantic agreement breaks ties between lexical matches.
                            old.score = old.score.max(old.score.floor() + score / 10.0);
                        }
                    })
                    .or_insert(hit);
            }
        }
        if mode == Mode::Meaning && self.directory.is_some() && self.failure.is_none() {
            let missing: i64 = self.db.query_row(
                "SELECT count(DISTINCT p.hash) FROM passages p
                 LEFT JOIN vectors v ON v.hash = p.hash WHERE v.hash IS NULL",
                [],
                |r| r.get(0),
            )?;
            let query_missing = self
                .query_vector
                .as_ref()
                .is_none_or(|(q, _)| *q != cleaned);
            results.pending = missing > 0 || query_missing;
            if results.pending {
                results.status = Some(format!(
                    "Searching by meaning · {missing} passages remaining"
                ));
                if let Err(error) = self.schedule(&cleaned) {
                    self.failure = Some(format!("{error:#}"));
                }
            }
        }
        if mode == Mode::Meaning && self.failure.is_some() {
            results.pending = false;
            results.status = Some(
                "Search by meaning unavailable · shift+tab searches words · ctrl+r retries".into(),
            );
            results.error = self.failure.clone();
        }
        Ok(results)
    }

    fn poll(&mut self) -> Result<()> {
        let Some(worker) = &mut self.worker else {
            return Ok(());
        };
        match worker.output.try_recv() {
            Ok(Ok(response)) => {
                worker.busy = false;
                self.query_vector = Some((response.query, response.query_vector));
                let tx = self.db.transaction()?;
                for (hash, vector) in response.vectors {
                    ensure!(
                        vector.len() == DIMENSIONS && vector.iter().all(|x| x.is_finite()),
                        "invalid passage vector"
                    );
                    let bytes: Vec<_> = vector.iter().flat_map(|x| x.to_le_bytes()).collect();
                    tx.execute(
                        "INSERT OR REPLACE INTO vectors VALUES (?1, ?2)",
                        params![hash, bytes],
                    )?;
                }
                tx.commit()?;
            }
            Ok(Err(error)) => {
                worker.busy = false;
                self.failure = Some(error);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.failure = Some("embedding worker exited".into());
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        Ok(())
    }

    fn schedule(&mut self, query: &str) -> Result<()> {
        if self.worker.is_none() {
            self.worker = Some(Embeddings::new(
                self.directory.as_ref().unwrap().join("models"),
            )?);
        }
        let worker = self.worker.as_mut().unwrap();
        if worker.busy {
            return Ok(());
        }
        let chunks = self
            .db
            .prepare(
                "SELECT p.hash, p.text FROM passages p
             LEFT JOIN vectors v ON v.hash = p.hash WHERE v.hash IS NULL
             GROUP BY p.hash ORDER BY min(p.rowid) LIMIT ?1",
            )?
            .query_map([BATCH as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        worker
            .input
            .send(EmbeddingRequest {
                query: query.into(),
                chunks,
            })
            .context("embedding worker exited")?;
        worker.busy = true;
        Ok(())
    }
}

fn fingerprint(entry: &Entry) -> Result<String> {
    let m = fs::metadata(&entry.transcript)?;
    let db = if entry.key.harness == "opencode" {
        format!("{:?}", crate::opencode::fingerprint(&entry.transcript)?)
    } else {
        String::new()
    };
    Ok(format!(
        "{}:{}:{}:{}:{:?}:{:?}:{db}",
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime_nsec(),
        m.modified()?,
        entry.title
    ))
}

fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Short overlapping passages keep MiniLM's input window useful for long messages.
fn chunks(text: &str) -> Vec<&str> {
    let boundaries: Vec<_> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain([text.len()])
        .collect();
    let mut chunks = Vec::new();
    let mut at = 0;
    while at + 1 < boundaries.len() {
        let end = (at + 600).min(boundaries.len() - 1);
        let chunk = text[boundaries[at]..boundaries[end]].trim();
        if !chunk.is_empty() {
            chunks.push(chunk);
        }
        if end + 1 == boundaries.len() {
            break;
        }
        at = end.saturating_sub(100);
    }
    chunks
}

/// English filler is dropped: every term has to match, so "about" only hides real hits.
const FILLER: &[&str] = &[
    "a",
    "about",
    "all",
    "an",
    "and",
    "any",
    "anything",
    "are",
    "as",
    "at",
    "be",
    "but",
    "by",
    "can",
    "did",
    "do",
    "does",
    "find",
    "for",
    "from",
    "had",
    "has",
    "have",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "me",
    "my",
    "of",
    "on",
    "or",
    "our",
    "search",
    "show",
    "so",
    "some",
    "something",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "to",
    "up",
    "was",
    "we",
    "were",
    "what",
    "when",
    "where",
    "which",
    "who",
    "why",
    "with",
    "would",
    "you",
    "your",
];

/// Keep the filler when a query is nothing else, so a search for "how" still searches.
pub(crate) fn terms(query: &str) -> Vec<&str> {
    let words: Vec<_> = query
        .split_whitespace()
        .filter(|s| s.chars().any(char::is_alphanumeric))
        .collect();
    let kept: Vec<_> = words
        .iter()
        .copied()
        .filter(|w| {
            let bare: String = w
                .to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect();
            !FILLER.contains(&bare.as_str())
        })
        .collect();
    if kept.is_empty() { words } else { kept }
}

/// One scannable line: the match near a predictable column, whole words, no markup.
pub(crate) fn excerpt(text: &str, terms: &[&str]) -> String {
    const LEAD: usize = 24;
    const WINDOW: usize = 110;
    // Table rules, fences and emphasis read as noise here; the passage itself is in the anchor.
    let text = text
        .replace(['`', '*', '#', '|', '>'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let chars: Vec<_> = text.chars().collect();
    // Work in character positions; Unicode case folding can change byte lengths.
    let at = terms
        .iter()
        .find_map(|word| {
            let word = word.to_lowercase();
            chars
                .windows(word.chars().count().max(1))
                .position(|part| part.iter().collect::<String>().to_lowercase() == word)
        })
        .unwrap_or(0);
    let mut start = at.saturating_sub(LEAD);
    if start > 0
        && let Some(space) = chars[start..at].iter().position(|c| *c == ' ')
    {
        start += space + 1;
    }
    let mut end = (start + WINDOW).min(chars.len());
    if end < chars.len()
        && let Some(space) = chars[start..end]
            .iter()
            .rposition(|c| *c == ' ')
            .filter(|space| *space > WINDOW / 2)
    {
        end = start + space;
    }
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        chars[start..end].iter().collect::<String>(),
        if end < chars.len() { "…" } else { "" }
    )
}

fn decode(bytes: &[u8]) -> Vec<f32> {
    if bytes.len() != DIMENSIONS * 4 {
        return Vec::new();
    }
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != DIMENSIONS || b.len() != DIMENSIONS {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm =
        a.iter().map(|x| x * x).sum::<f32>().sqrt() * b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 && dot.is_finite() && norm.is_finite() {
        dot / norm
    } else {
        0.0
    }
}

struct EmbeddingRequest {
    query: String,
    chunks: Vec<(String, String)>,
}

struct EmbeddingResponse {
    query: String,
    query_vector: Vec<f32>,
    vectors: Vec<(String, Vec<f32>)>,
}

struct Embeddings {
    input: mpsc::Sender<EmbeddingRequest>,
    output: mpsc::Receiver<std::result::Result<EmbeddingResponse, String>>,
    busy: bool,
}

struct Model {
    bert: BertModel,
    tokenizer: tokenizers::Tokenizer,
}

impl Model {
    fn load(directory: &Path) -> Result<Self> {
        let directory = directory.join(MODEL_REVISION);
        crate::private_dir(&directory)?;
        for &(name, hash) in MODEL_FILES {
            model_file(&directory, name, hash)?;
        }
        let config: Config = serde_json::from_slice(&fs::read(directory.join("config.json"))?)?;
        let mut tokenizer = tokenizers::Tokenizer::from_file(directory.join("tokenizer.json"))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        tokenizer.with_padding(None);
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: 256,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let weights =
            candle_core::safetensors::load(directory.join("model.safetensors"), &Device::Cpu)?;
        let bert = BertModel::load(
            candle_nn::VarBuilder::from_tensors(weights, DType::F32, &Device::Cpu),
            &config,
        )?;
        Ok(Self { bert, tokenizer })
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let ids = Tensor::new(encoding.get_ids(), &Device::Cpu)?.unsqueeze(0)?;
        // One unpadded sequence: every token participates in mean pooling.
        let vector = self
            .bert
            .forward(&ids, &ids.zeros_like()?, None)?
            .mean(1)?
            .squeeze(0)?
            .to_vec1::<f32>()?;
        ensure!(
            vector.len() == DIMENSIONS && vector.iter().all(|x| x.is_finite()),
            "invalid embedding"
        );
        Ok(vector)
    }
}

fn file_digest(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        hasher.update(&bytes[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Use the same system HTTPS client as price downloads, with pinned content and
/// atomic replacement. Cached files require no network request.
fn model_file(directory: &Path, name: &str, hash: &str) -> Result<()> {
    let path = directory.join(name);
    if file_digest(&path).is_ok_and(|got| got == hash) {
        return Ok(());
    }
    let temporary = tempfile::NamedTempFile::new_in(directory)?;
    let url = format!(
        "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/{MODEL_REVISION}/{name}"
    );
    let output = Command::new("/usr/bin/curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "10",
            "--max-time",
            "180",
            "--max-filesize",
            "100000000",
            "--output",
        ])
        .arg(temporary.path())
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .context("downloading local search model")?;
    ensure!(
        output.status.success(),
        "could not download search model file {name}"
    );
    ensure!(
        file_digest(temporary.path())? == hash,
        "search model checksum mismatch: {name}"
    );
    temporary.persist(path)?;
    Ok(())
}

impl Embeddings {
    fn new(directory: PathBuf) -> Result<Self> {
        let (input, requests) = mpsc::channel::<EmbeddingRequest>();
        let (replies, output) = mpsc::channel();
        std::thread::Builder::new()
            .name("cones-embeddings".into())
            .spawn(move || {
                // Initialization is lazy: opening history alone never loads or downloads a model.
                let mut model = None;
                let pool = rayon::ThreadPoolBuilder::new().num_threads(2).build();
                let mut query_cache: Option<(String, Vec<f32>)> = None;
                while let Ok(request) = requests.recv() {
                    let result = (|| -> Result<EmbeddingResponse> {
                        if model.is_none() {
                            model = Some(Model::load(&directory)?);
                        }
                        let model = model.as_ref().unwrap();
                        let pool = pool.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?;
                        if query_cache
                            .as_ref()
                            .is_none_or(|(q, _)| *q != request.query)
                        {
                            let vector = pool.install(|| model.embed(&request.query))?;
                            query_cache = Some((request.query.clone(), vector));
                        }
                        let mut vectors = Vec::new();
                        for (hash, text) in request.chunks {
                            vectors.push((hash, pool.install(|| model.embed(&text))?));
                        }
                        Ok(EmbeddingResponse {
                            query: request.query,
                            query_vector: query_cache.as_ref().unwrap().1.clone(),
                            vectors,
                        })
                    })()
                    .map_err(|e| format!("{e:#}"));
                    let failed = result.is_err();
                    if replies.send(result).is_err() || failed {
                        break;
                    }
                }
            })?;
        Ok(Self {
            input,
            output,
            busy: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Key;
    use serde_json::json;

    fn entry(dir: &Path, id: &str, text: &str) -> Entry {
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(
            &path,
            format!(
                "{}\n",
                json!({
                    "type": "user", "message": {"content": text}
                })
            ),
        )
        .unwrap();
        Entry {
            key: Key {
                harness: "claude".into(),
                home: dir.into(),
                session_id: id.into(),
            },
            cwd: "/project".into(),
            transcript: path,
            title: Some(format!("Conversation {id}")),
            archived: false,
            started: None,
            last_activity: None,
            columns: None,
            hit: None,
        }
    }

    fn vector(n: usize) -> Vec<f32> {
        let mut v = vec![0.0; DIMENSIONS];
        v[n] = 1.0;
        v
    }

    fn cache_vector(index: &Index, text: &str, vector: &[f32]) {
        let bytes: Vec<_> = vector.iter().flat_map(|x| x.to_le_bytes()).collect();
        index
            .db
            .execute(
                "INSERT OR REPLACE INTO vectors VALUES (?1, ?2)",
                params![digest(text), bytes],
            )
            .unwrap();
    }

    #[test]
    fn words_search_matches_the_terms_typed_and_loads_no_model() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            entry(dir.path(), "literal", "login failures"),
            entry(
                dir.path(),
                "semantic",
                "People cannot sign into their accounts",
            ),
            entry(dir.path(), "unrelated", "Move the sidebar to the left"),
        ];
        let mut index = Index::open(None).unwrap();
        index.sync(&entries).unwrap();
        // A cached neighbour stays out of a words search even when it is a close one.
        cache_vector(&index, "People cannot sign into their accounts", &vector(0));
        index.query_vector = Some(("login".into(), vector(0)));
        let found = index.search(&entries, "login", Mode::Words, false).unwrap();
        assert_eq!(found.hits.len(), 1);
        let hit = &found.hits[&identity(&entries[0])];
        assert!(!hit.semantic);
        assert_eq!(hit.score, 2.0 + 1.0 / 60.0);
        assert!(hit.snippet.contains("login failures"));
        assert!(hit.anchor.is_some());
        assert!(!found.pending);
        assert_eq!(found.status, None);
        assert_eq!(found.error, None);
        assert!(index.worker.is_none());
    }

    #[test]
    fn meaning_search_returns_the_related_conversation_and_not_the_words() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            entry(dir.path(), "literal", "login failures"),
            entry(
                dir.path(),
                "semantic",
                "People cannot sign into their accounts",
            ),
            entry(dir.path(), "unrelated", "Move the sidebar to the left"),
        ];
        let mut index = Index::open(None).unwrap();
        index.sync(&entries).unwrap();
        let mut close = vector(1);
        close[0] = 0.8;
        close[1] = (1.0 - 0.8_f32.powi(2)).sqrt();
        cache_vector(&index, "People cannot sign into their accounts", &close);
        cache_vector(&index, "login failures", &vector(1));
        cache_vector(&index, "Move the sidebar to the left", &vector(1));
        index.query_vector = Some(("login".into(), vector(0)));
        let found = index
            .search(&entries, "login", Mode::Meaning, false)
            .unwrap();
        assert_eq!(found.hits.len(), 1);
        let hit = &found.hits[&identity(&entries[1])];
        assert!(hit.semantic);
        assert!((hit.score - 0.8).abs() < 1e-5, "{}", hit.score);
        assert!(hit.snippet.contains("sign into their accounts"));
        assert!(hit.anchor.is_some());
        assert!(!found.pending);
        assert_eq!(found.error, None);
        // A vector cached for the previous query must not answer the next one.
        let found = index
            .search(&entries, "sidebar", Mode::Meaning, false)
            .unwrap();
        assert!(found.hits.is_empty());
        let found = index
            .search(&entries, "sidebar", Mode::Words, false)
            .unwrap();
        assert_eq!(found.hits.len(), 1);
        assert!(found.hits.contains_key(&identity(&entries[2])));
    }

    #[test]
    fn filler_words_do_not_hide_the_conversation_they_describe() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            entry(
                dir.path(),
                "wanted",
                "We shipped the copilot studio connector",
            ),
            entry(dir.path(), "other", "Something about the release notes"),
        ];
        let mut index = Index::open(None).unwrap();
        index.sync(&entries).unwrap();
        let found = index
            .search(
                &entries,
                "something about copilot studio",
                Mode::Words,
                false,
            )
            .unwrap();
        assert_eq!(found.hits.len(), 1);
        assert!(
            found.hits[&identity(&entries[0])]
                .snippet
                .contains("copilot studio connector")
        );
        // Dropping filler must not loosen the rest into an any-of match.
        assert!(
            index
                .search(&entries, "copilot studio release", Mode::Words, false)
                .unwrap()
                .hits
                .is_empty()
        );
        assert!(
            index
                .search(&entries, "copilot zebra", Mode::Words, false)
                .unwrap()
                .hits
                .is_empty()
        );
    }

    #[test]
    fn a_query_of_nothing_but_filler_still_searches_for_it() {
        assert_eq!(
            terms("something about copilot studio"),
            ["copilot", "studio"]
        );
        assert_eq!(terms("How about?"), ["How", "about?"]);
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            entry(dir.path(), "asked", "How about that for a plan"),
            entry(dir.path(), "other", "Move the sidebar to the left"),
        ];
        let mut index = Index::open(None).unwrap();
        index.sync(&entries).unwrap();
        let found = index
            .search(&entries, "how about", Mode::Words, false)
            .unwrap();
        assert_eq!(found.hits.len(), 1);
        assert!(found.hits.contains_key(&identity(&entries[0])));
    }

    #[test]
    fn a_faint_resemblance_is_not_a_result() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            entry(dir.path(), "faint", "Move the sidebar to the left"),
            entry(
                dir.path(),
                "close",
                "People cannot sign into their accounts",
            ),
        ];
        let mut index = Index::open(None).unwrap();
        index.sync(&entries).unwrap();
        for (text, near) in [
            ("Move the sidebar to the left", 0.45_f32),
            ("People cannot sign into their accounts", 0.55),
        ] {
            let mut v = vector(1);
            v[0] = near;
            v[1] = (1.0 - near.powi(2)).sqrt();
            cache_vector(&index, text, &v);
        }
        index.query_vector = Some(("login".into(), vector(0)));
        let found = index
            .search(&entries, "login", Mode::Meaning, false)
            .unwrap();
        assert_eq!(found.hits.len(), 1);
        assert!(found.hits.contains_key(&identity(&entries[1])));
    }

    #[test]
    fn index_survives_reopen_and_reuses_vectors_only_for_unchanged_text() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let mut e = entry(dir.path(), "one", "First discussion");
        {
            let mut index = Index::open(Some(cache.clone())).unwrap();
            index.sync(std::slice::from_ref(&e)).unwrap();
            cache_vector(&index, "First discussion", &vector(0));
        }
        let mut index = Index::open(Some(cache)).unwrap();
        // This fixture checks the persistent index, with inference disabled.
        index.directory = None;
        index.sync(std::slice::from_ref(&e)).unwrap();
        assert_eq!(
            index
                .db
                .query_row("SELECT count(*) FROM vectors", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        e = entry(dir.path(), "one", "Second discussion");
        index.sync(std::slice::from_ref(&e)).unwrap();
        assert!(
            index
                .search(std::slice::from_ref(&e), "First", Mode::Words, false)
                .unwrap()
                .hits
                .is_empty()
        );
        assert_eq!(
            index
                .search(std::slice::from_ref(&e), "Second", Mode::Words, false)
                .unwrap()
                .hits
                .len(),
            1
        );
        assert_eq!(
            index
                .db
                .query_row("SELECT count(*) FROM vectors", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        index.sync(&[]).unwrap();
        assert!(
            index
                .search(&[e], "Second", Mode::Words, false)
                .unwrap()
                .hits
                .is_empty()
        );
    }

    #[test]
    fn a_broken_model_stops_meaning_search_and_leaves_words_working() {
        let dir = tempfile::tempdir().unwrap();
        let e = entry(dir.path(), "one", "Broken authentication");
        let mut index = Index::open(None).unwrap();
        index.sync(std::slice::from_ref(&e)).unwrap();
        index.failure = Some("offline".into());
        let found = index
            .search(
                std::slice::from_ref(&e),
                "authentication",
                Mode::Meaning,
                false,
            )
            .unwrap();
        assert!(found.hits.is_empty());
        assert!(!found.pending);
        let status = found.status.unwrap();
        assert!(
            status.contains("unavailable") && status.contains("shift+tab"),
            "{status}"
        );
        assert_eq!(found.error.as_deref(), Some("offline"));
        let found = index
            .search(&[e], "authentication", Mode::Words, false)
            .unwrap();
        assert_eq!(found.hits.len(), 1);
        assert_eq!(found.status, None);
        assert_eq!(found.error, None);
        assert!(index.worker.is_none());
    }

    #[test]
    fn punctuation_unicode_and_long_messages_remain_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let text = format!(
            "{} שלום retry_token {}",
            "opening ".repeat(2000),
            "closing ".repeat(2000)
        );
        let e = entry(dir.path(), "one", &text);
        let mut index = Index::open(None).unwrap();
        index.sync(std::slice::from_ref(&e)).unwrap();
        for query in ["שלום", "retry_token", "\"retry_token\"", "closing"] {
            let found = index
                .search(std::slice::from_ref(&e), query, Mode::Words, false)
                .unwrap();
            assert_eq!(found.hits.len(), 1, "{query}");
            assert!(found.hits[&identity(&e)].anchor.is_some());
        }
        for query in ["\"", "***", "OR NOT ()"] {
            index
                .search(std::slice::from_ref(&e), query, Mode::Words, false)
                .unwrap();
        }
        let excerpt = excerpt("İstanbul שלום café 🐱 retry_token", &["שלום"]);
        assert!(excerpt.contains("שלום"));
        assert_eq!(cosine(&vector(0), &vec![f32::NAN; DIMENSIONS]), 0.0);
    }

    #[test]
    fn an_excerpt_is_one_short_line_of_whole_words_around_the_match() {
        let table = format!(
            "| Copilot Studio (API) | 11,881 | `copilot_studio` | Microsoft 365 |\n{}",
            "filler ".repeat(40)
        );
        let e = excerpt(&table, &["copilot", "studio"]);
        assert!(!e.contains('|') && !e.contains('`'), "{e}");
        assert!(
            e.starts_with("Copilot Studio (API) 11,881 copilot_studio"),
            "{e}"
        );
        assert!(e.chars().count() <= 111, "{e}");
        assert!(e.ends_with('…'), "{e}");
        // A window that opened mid-word is what made a list of results unreadable.
        let late = excerpt(
            &format!("{}needle in the haystack", "head ".repeat(40)),
            &["needle"],
        );
        assert!(late.starts_with("…head head"), "{late}");
        assert!(late.ends_with("needle in the haystack"), "{late}");
    }
}
