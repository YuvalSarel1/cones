//! `cones show`: a session's conversation as text, for an agent reading a worker's result.
//!
//! Identity comes from history discovery, so a live session and a finished one resolve the
//! same way and a custom native home is honoured without a second convention. Reading opens
//! files for reading: nothing here attaches, resumes, wakes a session or starts a viewer.
use crate::{history, transcript};
use anyhow::{Result, ensure};
use chrono::Local;
use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

/// Messages a bare `cones show` returns. Enough to read what a worker concluded, short
/// enough that reading four workers is not a context budget decision.
pub const DEFAULT_TAIL: usize = 40;

/// The shortest prefix allowed to stand in for a session id. Shorter matches too much.
const MIN_PREFIX: usize = 4;

/// A resolved session: which harness recorded it, where, and what to read.
pub struct Located {
    pub key: history::Key,
    pub cwd: PathBuf,
    pub source: transcript::Source,
}

/// Find the one session `id` names, by exact id or by an unambiguous prefix of one.
pub fn locate(claude: &Path, id: &str) -> Result<Located> {
    locate_scoped(claude, id, None, None)
}

/// Disambiguate identical native ids using the harness and home returned by search.
pub fn locate_scoped(
    claude: &Path,
    id: &str,
    harness: Option<&str>,
    home: Option<&Path>,
) -> Result<Located> {
    locate_entries(&history::all(&history::sources(claude))?, id, harness, home)
}

pub(crate) fn locate_entries(
    entries: &[history::Entry],
    id: &str,
    harness: Option<&str>,
    home: Option<&Path>,
) -> Result<Located> {
    let id = id.trim();
    ensure!(!id.is_empty(), "a session id is required");
    let home = home.map(|p| p.canonicalize().unwrap_or_else(|_| p.to_owned()));
    let entries: Vec<_> = entries
        .iter()
        .filter(|e| harness.is_none_or(|h| e.key.harness == h))
        .filter(|e| home.as_ref().is_none_or(|h| &e.key.home == h))
        .collect();
    let mut found: Vec<&history::Entry> = entries
        .iter()
        .copied()
        .filter(|e| e.key.session_id == id)
        .collect();
    if found.is_empty() && id.len() >= MIN_PREFIX {
        found = entries
            .iter()
            .copied()
            .filter(|e| e.key.session_id.starts_with(id))
            .collect();
    }
    ensure!(
        !found.is_empty(),
        "no session {id}. `cones ls` prints the ids of live sessions; a harness that keeps \
         no readable transcript, and a transcript that has been deleted, cannot be exported"
    );
    ensure!(
        found.len() == 1,
        "{id} matches {} sessions:\n{}",
        found.len(),
        found
            .iter()
            .map(|e| format!(
                "  {} {} in {}\n",
                e.key.session_id,
                e.key.harness,
                e.key.home.display()
            ))
            .collect::<String>()
    );
    let entry = found[0];
    Ok(Located {
        source: if entry.key.harness == "opencode" {
            transcript::Source::Opencode {
                database: entry.transcript.clone(),
                session_id: entry.key.session_id.clone(),
            }
        } else {
            transcript::Source::Conversation(entry.transcript.clone())
        },
        key: entry.key.clone(),
        cwd: entry.cwd.clone(),
    })
}

/// Machine-readable export with the same messages and omissions as the text command.
pub fn json(located: &Located, export: &transcript::Export) -> serde_json::Value {
    serde_json::json!({
        "session": located.key,
        "cwd": located.cwd,
        "omitted": export.omitted,
        "incomplete": export.incomplete,
        "messages": export.messages.iter().map(|m| serde_json::json!({
            "role": match m.role {
                transcript::Role::User => "user",
                transcript::Role::Assistant => "assistant",
                transcript::Role::Output => "output",
            },
            "at": m.at,
            "text": m.text,
            "tools": m.tools.iter().map(|t| serde_json::json!({
                "name": t.name, "input": t.input,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// The export as text. Roles are labelled, tool calls are listed under the turn that made
/// them, and a tail that left messages out says so on its own first line, so a bounded
/// read is never mistaken for a whole conversation.
pub fn render(export: &transcript::Export) -> String {
    let mut out = String::new();
    if export.omitted > 0 {
        let _ = writeln!(
            out,
            "[{} earlier message{} omitted; --all exports the whole conversation]\n",
            export.omitted,
            if export.omitted == 1 { "" } else { "s" }
        );
    }
    for message in &export.messages {
        let role = match message.role {
            transcript::Role::User => "user",
            transcript::Role::Assistant => "assistant",
            transcript::Role::Output => "output",
        };
        match message.at {
            Some(at) => {
                let _ = writeln!(out, "{role} {}", at.with_timezone(&Local).to_rfc3339());
            }
            None => {
                let _ = writeln!(out, "{role}");
            }
        }
        if !message.text.is_empty() {
            out.push_str(message.text.trim_end());
            out.push('\n');
        }
        for tool in &message.tools {
            let _ = match tool.input.is_empty() {
                true => writeln!(out, "  {}", tool.name),
                false => writeln!(out, "  {}: {}", tool.name, tool.input),
            };
        }
        out.push('\n');
    }
    out
}
