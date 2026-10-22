//! Terminal launch and process discovery for harnesses without a full native adapter.
//! The row is a process: state and accounting stay absent. Copilot and Cursor keep
//! conversations cones can read afterwards, and Copilot names the one a live client holds.
use crate::{
    config::HarnessKind,
    cost::{Accounting, Adapter, Reader, Reading},
    fleet::Session,
    harness,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

/// What a saved conversation says about itself: its id, its folder and when it began.
/// Copilot writes all three in its first event. Cursor's transcript carries none of them,
/// so the id is the file's own name and the rest comes from the chat record Cursor keeps for
/// the same id; a transcript without that record, such as the editor's, is not listed.
pub(crate) fn meta(
    kind: HarnessKind,
    home: &Path,
    path: &Path,
    events: &[Value],
) -> Option<(String, PathBuf, Option<DateTime<Utc>>)> {
    let id = match kind {
        HarnessKind::Copilot => {
            let start = &events.iter().find(|v| v["type"] == "session.start")?["data"];
            let id = start["sessionId"].as_str()?;
            uuid::Uuid::parse_str(id).ok()?;
            return Some((
                id.into(),
                start["context"]["cwd"].as_str()?.into(),
                start["startTime"]
                    .as_str()
                    .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                    .map(Into::into),
            ));
        }
        HarnessKind::Cursor => path.file_stem()?.to_str()?,
        _ => return None,
    };
    uuid::Uuid::parse_str(id).ok()?;
    let folder = path.parent()?;
    if folder.file_name()? != id || folder.parent()?.file_name()? != "agent-transcripts" {
        return None;
    }
    std::fs::read_dir(home.join("chats"))
        .ok()?
        .flatten()
        .find_map(|workspace| {
            let chat = std::fs::read(workspace.path().join(id).join("meta.json")).ok()?;
            let chat: Value = serde_json::from_slice(&chat).ok()?;
            Some((
                id.to_owned(),
                chat["cwd"].as_str()?.into(),
                chat["createdAtMs"]
                    .as_i64()
                    .and_then(DateTime::from_timestamp_millis),
            ))
        })
}

/// The session each running Copilot client holds, from the `inuse.PID.lock` it keeps in that
/// session's folder, with when the lock was written: one older than the process is what an
/// earlier client with the same pid left behind.
// ponytail: one directory read per saved session, only while a Copilot client runs. Cache by
// the folder's mtime if a home with thousands of sessions makes a pass slow.
fn copilot_sessions(home: &Path) -> Vec<(u32, String, DateTime<Utc>)> {
    std::fs::read_dir(home.join("session-state"))
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|session| {
            let id = session.file_name().to_string_lossy().into_owned();
            std::fs::read_dir(session.path())
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(move |file| {
                    let pid = file
                        .file_name()
                        .to_str()?
                        .strip_prefix("inuse.")?
                        .strip_suffix(".lock")?
                        .parse()
                        .ok()?;
                    Some((
                        pid,
                        id.clone(),
                        file.metadata().ok()?.modified().ok()?.into(),
                    ))
                })
        })
        .collect()
}

#[cfg(target_os = "macos")]
pub fn sessions(kind: HarnessKind, home: &Path) -> Result<Vec<Session>> {
    // One table per pass, shared with every other adapter, rather than a table per adapter
    // kept for a hundred milliseconds: a refresh is the scope that shares an observation.
    let table = crate::fleet::pass_table("/bin/ps")?;
    let mut processes = harness::spec(kind).discovery.processes(&table);
    let (mut clients, mut reported) = (HashMap::new(), HashMap::new());
    if kind == HarnessKind::Copilot {
        // The npm loader waits on the packaged binary it spawns, and both are named copilot.
        // The loader is the one a terminal started, so it is the row and its child is not:
        // a host that owns the loader then finds its own pid in the roster.
        let loaders: std::collections::HashSet<_> = processes
            .iter()
            .filter(|p| {
                let mut words = p.command.split_whitespace();
                words
                    .next()
                    .is_some_and(|p| Path::new(p).file_name().is_some_and(|n| n == "node"))
                    && words
                        .next()
                        .and_then(|p| Path::new(p).canonicalize().ok())
                        .is_some_and(|p| p.ends_with("@github/copilot/npm-loader.js"))
            })
            .map(|p| p.pid)
            .collect();
        processes.retain(|p| match crate::process_info::parent(p.pid) {
            Some(parent) if loaders.contains(&parent) => {
                clients.insert(p.pid, parent);
                false
            }
            _ => true,
        });
        if !processes.is_empty() {
            // The lock names the packaged binary, which is the loader's child when there is one.
            for (pid, id, locked) in copilot_sessions(home) {
                let pid = clients.get(&pid).copied().unwrap_or(pid);
                if processes
                    .iter()
                    .any(|p| p.pid == pid && p.started <= locked)
                {
                    reported.insert(pid, id);
                }
            }
        }
    }
    let own = crate::fleet::own_home_processes(
        "/bin/ps",
        kind,
        home,
        &processes.iter().map(|p| p.pid).collect::<Vec<_>>(),
    );
    Ok(processes
        .into_iter()
        .filter(|p| own.contains(&p.pid))
        .filter_map(|p| {
            let cwd = crate::process_info::cwd(p.pid)?;
            Some(Session {
                session_id: format!("{kind}-{}", p.pid),
                harness: kind.to_string(),
                kind: None,
                cwd,
                state: "-".into(),
                started: Some(p.started),
                pid: Some(p.pid),
                last_activity: None,
                next_model: None,
                model: None,
                transcript_path: None,
                tokens_in: None,
                tokens_out: None,
                context_tokens: None,
                context_window: None,
                cost_usd: None,
                cost_info: None,
                effort: None,
                bypass: None,
                usage: None,
                tasks: None,
                plan: None,
                title: None,
                last: None,
                coordinator: false,
                forked_from: None,
                forked_from_harness: None,
                launched_by: None,
                activity: Vec::new(),
                moved_to: None,
                native_id: reported.remove(&p.pid),
            })
        })
        .collect())
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Unreported;

impl Adapter for Unreported {
    fn read<'a>(&'a mut self, _event: &'a Value) -> Reading<'a> {
        Reading::Gap("native_usage_unavailable")
    }
}

pub(crate) fn accounting(kind: HarnessKind) -> Box<dyn Reader> {
    assert!(kind.terminal_only());
    Box::new(Accounting::<Unreported>::default())
}

#[cfg(not(target_os = "macos"))]
pub fn sessions(_kind: HarnessKind, _home: &Path) -> Result<Vec<Session>> {
    anyhow::bail!("native process discovery requires macOS")
}
