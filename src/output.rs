use crate::{ledger::Ledger, private_dir};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

pub const MAX_LINE: u64 = 1024 * 1024;
const MAX_OUTPUT: u64 = 64 * 1024 * 1024;
const MAX_STDERR: u64 = 1024 * 1024;

pub struct RunOutput {
    pub events_path: PathBuf,
    pub stderr_path: PathBuf,
    events: File,
    stderr: Option<File>,
    size: u64,
}

fn create(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?)
}

impl RunOutput {
    pub fn new(state: &Path, run_id: &str) -> Result<Self> {
        uuid::Uuid::parse_str(run_id)?;
        let root = state.join("output");
        private_dir(&root)?;
        let dir = root.join(run_id);
        private_dir(&dir)?;
        let events_path = dir.join("events.jsonl");
        let stderr_path = dir.join("stderr.log");
        Ok(Self {
            events: create(&events_path)?,
            stderr: Some(create(&stderr_path)?),
            events_path,
            stderr_path,
            size: 0,
        })
    }
    pub fn record(&mut self, value: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        ensure!(
            self.size + bytes.len() as u64 <= MAX_OUTPUT,
            "run output exceeded 64 MiB"
        );
        self.events.write_all(&bytes)?;
        self.events.flush()?;
        self.size += bytes.len() as u64;
        Ok(())
    }
    pub fn sync(&self) -> Result<()> {
        self.events.sync_all()?;
        Ok(())
    }
    pub fn capture_stderr(&mut self, reader: impl Read + Send + 'static) {
        let mut file = self.stderr.take().expect("stderr capture starts once");
        thread::spawn(move || {
            let mut reader = reader;
            let mut buffer = [0u8; 8192];
            let mut written = 0u64;
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let keep = (MAX_STDERR - written).min(n as u64) as usize;
                        if keep > 0 && file.write_all(&buffer[..keep]).is_err() {
                            break;
                        }
                        written += keep as u64;
                        let _ = file.flush();
                    }
                }
            }
            let _ = file.sync_all();
        });
    }
}

pub fn open_read(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("read {}", path.display()))
}

/// Strip terminal control characters from model and tool text before rendering it.
pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

pub fn describe(event: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    match event["type"].as_str() {
        Some("assistant") => {
            if let Some(parts) = event["message"]["content"].as_array() {
                for part in parts {
                    match part["type"].as_str() {
                        Some("text") => lines.push(part["text"].as_str().unwrap_or("").into()),
                        Some("tool_use") => lines.push(format!(
                            "{}  {}",
                            part["name"].as_str().unwrap_or("tool"),
                            part["input"]
                                .get("command")
                                .or(part["input"].get("file_path"))
                                .map(|v| v
                                    .as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| v.to_string()))
                                .unwrap_or_default()
                        )),
                        _ => {}
                    }
                }
            }
        }
        Some("user") => {
            if let Some(parts) = event["message"]["content"].as_array() {
                for part in parts {
                    if part["type"] == "tool_result" {
                        let body = if let Some(text) = part["content"].as_str() {
                            text.to_owned()
                        } else {
                            part["content"]
                                .as_array()
                                .map(|items| {
                                    items
                                        .iter()
                                        .filter_map(|item| item["text"].as_str())
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                })
                                .unwrap_or_default()
                        };
                        if !body.is_empty() {
                            lines.push(body);
                        }
                    }
                }
            }
        }
        Some("cones_error") => lines.push(format!(
            "Error: {}",
            event["message"].as_str().unwrap_or("unknown error")
        )),
        Some("result") => lines.push(format!(
            "Result: {}{}",
            event["subtype"].as_str().unwrap_or("unknown"),
            event["total_cost_usd"]
                .as_f64()
                .map(|n| format!("  ${n:.6}"))
                .unwrap_or_default()
        )),
        Some("system") if event["subtype"] == "permission_denied" => {
            lines.push("Permission denied by the harness".into())
        }
        _ => {}
    }
    lines
        .into_iter()
        .flat_map(|s| clean(&s).lines().map(str::to_owned).collect::<Vec<_>>())
        .collect()
}

pub fn tail(path: &Path, bytes: u64) -> Result<String> {
    let mut file = open_read(path)?;
    let len = file.metadata()?.len();
    let offset = len.saturating_sub(bytes);
    file.seek(SeekFrom::Start(offset))?;
    let mut text = String::new();
    // Lossy decoding permits a tail starting inside a UTF-8 character.
    let mut data = Vec::new();
    file.take(bytes).read_to_end(&mut data)?;
    if offset > 0 {
        if let Some(i) = data.iter().position(|b| *b == b'\n') {
            data.drain(..=i);
        } else {
            data.clear();
        }
    }
    text.push_str(&String::from_utf8_lossy(&data));
    Ok(text)
}

pub fn snapshot(events: Option<&Path>, stderr: Option<&Path>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(path) = events
        && let Ok(text) = tail(path, 256 * 1024)
    {
        for raw in text.lines() {
            if let Ok(event) = serde_json::from_str(raw) {
                lines.extend(describe(&event));
            }
        }
    }
    if let Some(path) = stderr
        && let Ok(text) = tail(path, 16 * 1024)
        && !text.trim().is_empty()
    {
        lines.push("Harness stderr:".into());
        lines.extend(clean(&text).lines().map(str::to_owned));
    }
    lines
}

pub fn logs(ledger: &Ledger, id: &str, follow: bool, raw: bool) -> Result<()> {
    let run = ledger.resolve(id)?;
    let path = run
        .started
        .output
        .as_deref()
        .context("this run has no captured output; use native resume")?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let sigint = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&cancelled))?;
    let sigterm =
        signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&cancelled))?;
    let result = (|| -> Result<()> {
        let mut reader = BufReader::new(open_read(path)?);
        let mut pending = Vec::new();
        loop {
            let n = reader
                .by_ref()
                .take(MAX_LINE + 1 - pending.len() as u64)
                .read_until(b'\n', &mut pending)?;
            ensure!(pending.len() <= MAX_LINE as usize, "oversized output event");
            if pending.ends_with(b"\n") {
                let event: Value = serde_json::from_slice(&pending)?;
                if raw {
                    writeln!(std::io::stdout(), "{}", serde_json::to_string(&event)?)?;
                } else {
                    for line in describe(&event) {
                        writeln!(std::io::stdout(), "{line}")?;
                    }
                }
                pending.clear();
                continue;
            }
            if n == 0 {
                if cancelled.load(Ordering::Relaxed)
                    || !follow
                    || ledger.resolve(id)?.status() != "started"
                {
                    break;
                }
                thread::sleep(Duration::from_millis(150));
            }
        }
        if !raw
            && let Some(path) = run.started.stderr.as_deref()
            && let Ok(text) = tail(path, MAX_STDERR)
            && !text.trim().is_empty()
        {
            writeln!(std::io::stdout(), "Harness stderr:\n{}", clean(&text))?;
        }
        Ok(())
    })();
    signal_hook::low_level::unregister(sigint);
    signal_hook::low_level::unregister(sigterm);
    match result {
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) =>
        {
            Ok(())
        }
        other => other,
    }
}
