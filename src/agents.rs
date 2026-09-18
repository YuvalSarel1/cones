//! Terminal launch and process discovery for harnesses without a native archive adapter.
//! A process is not a reported conversation: identity, state and accounting stay absent.
use crate::{
    config::HarnessKind,
    cost::{Accounting, Adapter, Reader, Reading},
    fleet::Session,
    harness,
};
use anyhow::Result;
use serde_json::Value;
use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

static PROCESSES: Mutex<Option<(Instant, String)>> = Mutex::new(None);

#[cfg(target_os = "macos")]
pub fn sessions(kind: HarnessKind, _home: &Path) -> Result<Vec<Session>> {
    let table = {
        let mut cached = PROCESSES.lock().unwrap_or_else(|e| e.into_inner());
        if cached
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() > Duration::from_millis(100))
        {
            *cached = Some((Instant::now(), crate::fleet::process_table("/bin/ps")?));
        }
        cached.as_ref().expect("process table").1.clone()
    };
    let mut processes = harness::spec(kind).discovery.processes(&table);
    if kind == HarnessKind::Copilot {
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
        let replaced: std::collections::HashSet<_> = processes
            .iter()
            .filter(|p| !loaders.contains(&p.pid))
            .filter_map(|p| crate::process_info::parent(p.pid))
            .filter(|p| loaders.contains(p))
            .collect();
        processes.retain(|p| !replaced.contains(&p.pid));
    }
    let own = crate::fleet::own_home_processes(
        "/bin/ps",
        kind,
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
                model: None,
                transcript_path: None,
                tokens_in: None,
                tokens_out: None,
                context_tokens: None,
                context_window: None,
                cost_usd: None,
                cost_info: None,
                effort: None,
                usage: None,
                title: None,
                last: None,
                coordinator: false,
                forked_from: None,
                activity: Vec::new(),
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
