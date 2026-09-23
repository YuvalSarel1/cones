//! Agent-facing history queries. CLI and MCP share the dashboard's index and native readers.
use crate::{harness, history, search, show, transcript};
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Words,
    Meaning,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Search {
    pub query: String,
    pub mode: Mode,
    pub dir: Option<PathBuf>,
    pub harness: Option<String>,
    pub home: Option<PathBuf>,
    pub since: Option<DateTime<Utc>>,
    pub limit: usize,
    pub offset: usize,
    pub wait_seconds: u64,
}

impl Default for Search {
    fn default() -> Self {
        Self {
            query: String::new(),
            mode: Mode::Words,
            dir: None,
            harness: None,
            home: None,
            since: None,
            limit: 20,
            offset: 0,
            wait_seconds: 30,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Show {
    pub id: String,
    pub harness: Option<String>,
    pub home: Option<PathBuf>,
    pub tail: Option<usize>,
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Serialize)]
pub struct Results {
    pub query: String,
    pub mode: Mode,
    pub entries: Vec<history::Entry>,
    pub total: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
    /// False when semantic indexing is still running or the model failed to load.
    pub complete: bool,
    pub pending: bool,
    pub status: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Syncing,
    /// Another cones process is embedding a batch; this one resumes when it commits.
    Waiting,
    DownloadingModel,
    Embedding,
    Done,
    Failed,
}

/// The meaning index, reported the same way before, during and after `cones index`.
#[derive(Clone, Debug, Serialize)]
pub struct IndexStatus {
    pub phase: Phase,
    pub conversations: i64,
    pub passages: i64,
    pub embedded: i64,
    pub remaining: i64,
    /// Passages per second since this command started embedding.
    pub rate: Option<f64>,
    pub eta_seconds: Option<u64>,
    pub model_downloaded: bool,
    pub cache: Option<PathBuf>,
    pub cache_bytes: u64,
    pub status: Option<String>,
    pub error: Option<String>,
}

pub struct Service {
    catalog: history::Catalog,
    sources: Vec<history::Source>,
    base: PathBuf,
    search: Option<PathBuf>,
}

impl Service {
    pub fn discover(claude: &Path, state: PathBuf, base: PathBuf) -> Self {
        Self::new(history::sources(claude), Some(state), base)
    }

    /// Explicit sources and an absent state directory keep fixtures model-free.
    pub fn new(sources: Vec<history::Source>, state: Option<PathBuf>, base: PathBuf) -> Self {
        Self {
            search: state.as_ref().map(|p| p.join("search")),
            catalog: history::Catalog::new(sources.clone(), state),
            sources,
            base,
        }
    }

    fn status(&self, phase: Phase, progress: search::Progress) -> IndexStatus {
        IndexStatus {
            phase,
            conversations: progress.conversations,
            passages: progress.passages,
            embedded: progress.embedded,
            remaining: progress.passages - progress.embedded,
            rate: None,
            eta_seconds: None,
            model_downloaded: self
                .search
                .as_ref()
                .is_some_and(|d| search::model_present(&d.join("models"))),
            cache_bytes: self.search.as_ref().map_or(0, |d| disk_usage(d)),
            cache: self.search.clone(),
            status: None,
            error: None,
        }
    }

    /// Rereads changed transcripts but never loads a model or embeds anything.
    pub fn index_status(&mut self) -> Result<IndexStatus> {
        let (progress, _) = self.catalog.index(true, false)?;
        let phase = if progress.passages == progress.embedded {
            Phase::Done
        } else {
            Phase::Embedding
        };
        Ok(self.status(phase, progress))
    }

    /// Embed until every passage has a vector, the timeout passes or the model fails.
    /// `report` sees every change of phase or count; the last status is returned.
    pub fn index(
        &mut self,
        timeout: Option<Duration>,
        mut report: impl FnMut(&IndexStatus),
    ) -> Result<IndexStatus> {
        report(&self.status(Phase::Syncing, search::Progress::default()));
        let started = Instant::now();
        let deadline = timeout.map(|t| started + t);
        let mut sync = true;
        let mut first: Option<(Instant, i64)> = None;
        let mut last: Option<(Phase, search::Progress)> = None;
        loop {
            let (progress, results) = self.catalog.index(sync, true)?;
            sync = false;
            let results = results.expect("embedding was requested");
            let mut status = self.status(Phase::Embedding, progress);
            if results.error.is_some() {
                status.phase = Phase::Failed;
            } else if !results.pending {
                status.phase = Phase::Done;
            } else if results.waiting {
                status.phase = Phase::Waiting;
            } else if !status.model_downloaded && progress.embedded == 0 {
                status.phase = Phase::DownloadingModel;
            }
            let (since, from) = *first.get_or_insert((Instant::now(), progress.embedded));
            let elapsed = since.elapsed().as_secs_f64();
            if progress.embedded > from && elapsed > 0.0 {
                let rate = (progress.embedded - from) as f64 / elapsed;
                status.rate = Some(rate);
                status.eta_seconds = Some((status.remaining as f64 / rate).ceil() as u64);
            }
            status.status = results.status;
            status.error = results.error;
            let finished = matches!(status.phase, Phase::Done | Phase::Failed)
                || deadline.is_some_and(|d| Instant::now() >= d);
            if finished || last != Some((status.phase, progress)) {
                report(&status);
                last = Some((status.phase, progress));
            }
            if finished {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn path(&self, path: Option<&Path>) -> Result<Option<PathBuf>> {
        path.map(|p| {
            let p = crate::expand_path(p, &self.base)?;
            Ok(p.canonicalize().unwrap_or(p))
        })
        .transpose()
    }

    pub fn search(&mut self, request: Search) -> Result<Results> {
        ensure!(!request.query.trim().is_empty(), "query cannot be empty");
        ensure!(request.query.len() <= 8192, "query exceeds 8192 bytes");
        ensure!(
            (1..=history::MAX_PAGE).contains(&request.limit),
            "limit must be 1..={}",
            history::MAX_PAGE
        );
        ensure!(request.wait_seconds <= 60, "wait_seconds must be 0..=60");
        validate_harness(request.harness.as_deref())?;
        let dir = self.path(request.dir.as_deref())?;
        let home = self.path(request.home.as_deref())?;
        let deadline = Instant::now() + Duration::from_secs(request.wait_seconds);
        let mut refresh = true;
        loop {
            let page = self.catalog.page(
                history::Query {
                    filter: request.query.clone(),
                    search: match request.mode {
                        Mode::Words => search::Mode::Words,
                        Mode::Meaning => search::Mode::Meaning,
                    },
                    limit: request.limit,
                    include_archived: true,
                    refresh,
                    ..Default::default()
                },
                request.offset,
                |e| {
                    request.harness.as_ref().is_none_or(|h| &e.key.harness == h)
                        && home.as_ref().is_none_or(|h| &e.key.home == h)
                        && dir.as_ref().is_none_or(|d| {
                            e.cwd
                                .canonicalize()
                                .unwrap_or_else(|_| e.cwd.clone())
                                .starts_with(d)
                        })
                        && request
                            .since
                            .is_none_or(|s| e.last_activity.is_some_and(|a| a >= s))
                },
            )?;
            if !page.search_pending || Instant::now() >= deadline {
                let next = request.offset.saturating_add(page.entries.len());
                return Ok(Results {
                    query: request.query,
                    mode: request.mode,
                    entries: page.entries,
                    total: page.total,
                    offset: request.offset,
                    next_offset: (next < page.total).then_some(next),
                    complete: !page.search_pending && page.search_error.is_none(),
                    pending: page.search_pending,
                    status: if page.search_error.is_some() {
                        Some("Meaning search unavailable; word search is still available.".into())
                    } else {
                        page.search_status
                    },
                    error: page.search_error,
                });
            }
            refresh = false;
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    pub fn show(&self, request: Show) -> Result<serde_json::Value> {
        ensure!(
            !(request.all && request.tail.is_some()),
            "all and tail are mutually exclusive"
        );
        validate_harness(request.harness.as_deref())?;
        let home = self.path(request.home.as_deref())?;
        let located = show::locate_entries(
            &history::all(&self.sources)?,
            &request.id,
            request.harness.as_deref(),
            home.as_deref(),
        )?;
        let tail = (!request.all).then(|| request.tail.unwrap_or(show::DEFAULT_TAIL));
        let export = transcript::export(&located.source, &located.key.harness, tail)?;
        Ok(show::json(&located, &export))
    }
}

fn disk_usage(path: &Path) -> u64 {
    std::fs::read_dir(path).map_or(0, |entries| {
        entries
            .flatten()
            .map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => disk_usage(&e.path()),
                _ => e.metadata().map_or(0, |m| m.len()),
            })
            .sum()
    })
}

pub fn validate_harness(name: Option<&str>) -> Result<()> {
    if let Some(name) = name {
        ensure!(
            harness::by_name(name).is_some_and(|s| s.transcript.available),
            "{name} has no supported conversation history"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pending_and_failed_semantic_reads_are_explicit_and_words_remain_available() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("claude");
        let source = home.join("projects/fixture/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.jsonl");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        let transcript = format!(
            "{}\n",
            json!({
                "type":"user", "cwd":"/fixture", "timestamp":"2026-09-23T00:00:00Z",
                "message":{"content":"A parser decision"}
            })
        );
        std::fs::write(&source, &transcript).unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(state.join("search/models")).unwrap();
        // Obstruct the model directory, before the loader could open a model or run curl.
        std::fs::write(
            crate::search::model_directory(&state.join("search/models")),
            "blocked",
        )
        .unwrap();
        let mut service = Service::new(
            vec![history::Source {
                harness: crate::config::HarnessKind::Claude,
                home,
            }],
            Some(state),
            dir.path().to_owned(),
        );
        let request = Search {
            query: "parser".into(),
            mode: Mode::Meaning,
            ..Default::default()
        };
        let pending = service
            .search(Search {
                wait_seconds: 0,
                ..request.clone()
            })
            .unwrap();
        assert!(pending.pending);
        assert!(!pending.complete);
        assert!(pending.error.is_none());
        assert_eq!(
            pending.total, 1,
            "metadata matches are usable while meaning is pending"
        );
        let failed = service.search(request).unwrap();
        assert!(!failed.pending && !failed.complete);
        assert!(failed.error.as_ref().is_some_and(|e| !e.is_empty()));
        assert_eq!(
            failed.total, 1,
            "a partial match does not hide the model error"
        );
        let words = service
            .search(Search {
                query: "parser".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(words.complete && !words.pending);
        assert!(words.error.is_none());
        assert_eq!(words.total, 1);
        let requests = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}
            }}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"cones_search","arguments":{"query":"parser","mode":"meaning","wait_seconds":0}
            }}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                "name":"cones_search","arguments":{"query":"parser","mode":"meaning"}
            }}),
        ].iter().map(|v| format!("{v}\n")).collect::<String>();
        let mut output = Vec::new();
        crate::history_mcp::serve(&mut service, requests.as_bytes(), &mut output).unwrap();
        let replies = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(replies.len(), 3);
        assert_eq!(replies[1]["result"]["isError"], false);
        assert_eq!(replies[1]["result"]["structuredContent"]["pending"], true);
        assert_eq!(replies[2]["result"]["isError"], true);
        assert_eq!(replies[2]["result"]["structuredContent"]["complete"], false);
        assert!(replies[2]["result"]["structuredContent"]["error"].is_string());
        assert_eq!(std::fs::read_to_string(source).unwrap(), transcript);
    }
}
