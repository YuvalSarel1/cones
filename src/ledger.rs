use crate::{config::HarnessKind, private_dir, private_file};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Started,
    Ok,
    Failed,
    Timeout,
    Skipped,
}
impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Started => "started",
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::Skipped => "skipped",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub v: u32,
    pub run_id: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fired_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pgid: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owns_run_lock: Option<bool>,
}
impl Record {
    pub fn new(run_id: String, status: Status) -> Self {
        Self {
            v: 1,
            run_id,
            status,
            job: None,
            trigger: None,
            fired_at: None,
            ended_at: None,
            harness: None,
            session_id: None,
            cwd: None,
            pid: None,
            pgid: None,
            policy_hash: None,
            timeout_s: None,
            duration_s: None,
            exit: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            reason: None,
            transcript: None,
            archive_error: None,
            output: None,
            stderr: None,
            attach_mode: None,
            policy: None,
            owns_run_lock: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub started: Record,
    pub terminal: Option<Record>,
}
impl Run {
    pub fn status(&self) -> String {
        if let Some(t) = &self.terminal {
            return t.status.to_string();
        }
        if self.started.status == Status::Skipped {
            return "skipped".into();
        }
        let elapsed = self
            .started
            .fired_at
            .map(|t| (Utc::now() - t).num_milliseconds() as f64 / 1000.0)
            .unwrap_or(0.0);
        if elapsed > self.started.timeout_s.unwrap_or(1800.0) + 5.0 {
            "crashed".into()
        } else {
            "started".into()
        }
    }
}

pub struct Ledger {
    pub state: PathBuf,
}
impl Ledger {
    pub fn new(state: &Path) -> Result<Self> {
        private_dir(state)?;
        private_dir(&state.join("locks"))?;
        Ok(Self {
            state: state.to_owned(),
        })
    }
    /// Serialize admission and reservations; release before concurrent execution.
    pub fn admission_lock(&self) -> Result<File> {
        let directory = self.state.join("locks").join("admission");
        private_dir(&directory)?;
        let f = private_file(&directory.join("global.lock"))?;
        f.lock_exclusive()?;
        Ok(f)
    }
    /// Try the run's lifetime lease; `None` means it is still held.
    pub fn run_lock(&self, run_id: &str) -> Result<Option<File>> {
        uuid::Uuid::parse_str(run_id)?;
        let directory = self.state.join("locks").join("runs");
        private_dir(&directory)?;
        let f = private_file(&directory.join(format!("{run_id}.lock")))?;
        match f.try_lock_exclusive() {
            Ok(()) => Ok(Some(f)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn append(&self, record: &Record) -> Result<()> {
        let mut f = private_file(&self.state.join("runs.jsonl"))?;
        f.lock_exclusive()?;
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes)?;
        let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        // A killed writer can leave a partial last line. Preserve complete records and repair
        // only this tail under the same lock used by all readers and writers.
        parse_records(&bytes[..complete])?;
        if complete < bytes.len() {
            let tail = &bytes[complete..];
            if let Ok(parsed) = serde_json::from_slice::<Record>(tail) {
                ensure!(parsed.v == 1, "unsupported ledger version");
                f.seek(SeekFrom::End(0))?;
                f.write_all(b"\n")?;
            } else {
                f.set_len(complete as u64)?;
            }
        }
        f.seek(SeekFrom::End(0))?;
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        f.write_all(&line)?;
        f.sync_all()?;
        Ok(())
    }
    pub fn runs(&self) -> Result<Vec<Run>> {
        let mut f = private_file(&self.state.join("runs.jsonl"))?;
        FileExt::lock_shared(&f)?;
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes)?;
        let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let mut records = parse_records(&bytes[..complete])?;
        if complete < bytes.len()
            && let Ok(r) = serde_json::from_slice::<Record>(&bytes[complete..])
        {
            ensure!(r.v == 1, "unsupported ledger version");
            records.push(r);
        }
        let mut runs: BTreeMap<String, Run> = BTreeMap::new();
        for r in records {
            if matches!(r.status, Status::Started | Status::Skipped) {
                ensure!(
                    !runs.contains_key(&r.run_id),
                    "duplicate initial record for {}",
                    r.run_id
                );
                runs.insert(
                    r.run_id.clone(),
                    Run {
                        started: r,
                        terminal: None,
                    },
                );
            } else {
                let run = runs
                    .get_mut(&r.run_id)
                    .with_context(|| format!("terminal record without start: {}", r.run_id))?;
                ensure!(
                    run.terminal.is_none(),
                    "duplicate terminal record for {}",
                    r.run_id
                );
                run.terminal = Some(r);
            }
        }
        let mut runs: Vec<_> = runs.into_values().collect();
        runs.sort_by_key(|r| r.started.fired_at);
        Ok(runs)
    }
    /// Hide a row without deleting its ledger record, output or transcript.
    pub fn hide(&self, run_id: &str) -> Result<()> {
        let mut f = private_file(&self.state.join("hidden"))?;
        f.seek(SeekFrom::End(0))?;
        writeln!(f, "{run_id}")?;
        Ok(())
    }
    /// Undo `hide`. Reviving a session is the un-forget: its row belongs in the list again.
    pub fn unhide(&self, run_id: &str) -> Result<()> {
        let hidden = self.hidden()?;
        if !hidden.contains(run_id) {
            return Ok(());
        }
        let mut f = private_file(&self.state.join("hidden"))?;
        f.set_len(0)?;
        for id in hidden.iter().filter(|id| *id != run_id) {
            writeln!(f, "{id}")?;
        }
        Ok(())
    }
    pub fn hidden(&self) -> Result<std::collections::BTreeSet<String>> {
        Ok(std::fs::read_to_string(self.state.join("hidden"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect())
    }
    pub fn folders(&self) -> Result<Vec<PathBuf>> {
        Ok(std::fs::read_to_string(self.state.join("folders"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect())
    }
    pub fn write_folders(&self, folders: &[PathBuf]) -> Result<()> {
        let mut f = private_file(&self.state.join("folders"))?;
        f.set_len(0)?;
        for p in folders {
            writeln!(f, "{}", p.display())?;
        }
        Ok(())
    }
    /// Remember up to 20 folders in first-seen order, newest first; write only when changed.
    pub fn recent(&self, seen: &[PathBuf]) -> Result<Vec<PathBuf>> {
        let mut recent: Vec<PathBuf> = std::fs::read_to_string(self.state.join("recent"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect();
        let mut new: Vec<PathBuf> = Vec::new();
        for p in seen {
            if !recent.contains(p) && !new.contains(p) {
                new.push(p.clone());
            }
        }
        if !new.is_empty() {
            new.append(&mut recent);
            new.truncate(20);
            recent = new;
            let mut f = private_file(&self.state.join("recent"))?;
            f.set_len(0)?;
            for p in &recent {
                writeln!(f, "{}", p.display())?;
            }
        }
        Ok(recent)
    }
    pub fn resolve(&self, id: &str) -> Result<Run> {
        let candidates: Vec<_> = self
            .runs()?
            .into_iter()
            .filter(|r| r.started.run_id == id || r.started.session_id.as_deref() == Some(id))
            .collect();
        ensure!(
            candidates.len() == 1,
            "expected one run matching {id}, found {}",
            candidates.len()
        );
        Ok(candidates.into_iter().next().unwrap())
    }
}

fn parse_records(bytes: &[u8]) -> Result<Vec<Record>> {
    bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
        .map(|(i, line)| {
            let record: Record = serde_json::from_slice(line)
                .with_context(|| format!("corrupt ledger at line {}", i + 1))?;
            ensure!(
                record.v == 1,
                "unsupported ledger version at line {}",
                i + 1
            );
            Ok(record)
        })
        .collect()
}
