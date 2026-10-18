//! `cones show`: a session's conversation as text, for an agent reading a worker's result.
//!
//! Identity comes from history discovery, so a live session and a finished one resolve the
//! same way and a custom native home is honoured without a second convention. Reading opens
//! files for reading: nothing here attaches, resumes, wakes a session or starts a viewer.
//!
//! A session with no conversation to read, in a terminal cones hosts, has a screen instead.
//! It is printed marked as one, because a screen is what a client last drew and not a record
//! of who said what.
use crate::{fleet::Session, history, terminal_host, transcript};
use anyhow::{Context, Result, ensure};
use chrono::Local;
use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

/// Messages a bare `cones show` returns. Enough to read what a worker concluded, short
/// enough that reading four workers is not a context budget decision.
pub const DEFAULT_TAIL: usize = 40;

/// Messages on each side of a search hit that `--at` returns.
pub const DEFAULT_CONTEXT: usize = 5;

/// The shortest prefix allowed to stand in for a session id. Shorter matches too much.
pub(crate) const MIN_PREFIX: usize = 4;

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
    // A live client names the home it runs against; nothing else has looked for one yet.
    crate::observe::reset();
    crate::fleet::detect_all_homes(claude);
    locate_entries(&history::all(&history::sources(claude))?, id, harness, home)
}

/// The history entry `id` names, by exact id or by an unambiguous prefix of one, for the
/// commands that act on a conversation rather than print it.
pub fn entry(claude: &Path, id: &str) -> Result<history::Entry> {
    crate::observe::reset();
    crate::fleet::detect_all_homes(claude);
    find_entry(&history::all(&history::sources(claude))?, id, None, None).cloned()
}

pub(crate) fn locate_entries(
    entries: &[history::Entry],
    id: &str,
    harness: Option<&str>,
    home: Option<&Path>,
) -> Result<Located> {
    let entry = find_entry(entries, id, harness, home)?;
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

fn find_entry<'a>(
    entries: &'a [history::Entry],
    id: &str,
    harness: Option<&str>,
    home: Option<&Path>,
) -> Result<&'a history::Entry> {
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
         no readable transcript shows a screen only in a terminal cones hosts, and a \
         transcript that has been deleted cannot be exported"
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
    Ok(found[0])
}

/// Machine-readable export with the same messages and omissions as the text command.
pub fn json(located: &Located, export: &transcript::Export) -> serde_json::Value {
    serde_json::json!({
        "session": located.key,
        "cwd": located.cwd,
        "omitted": export.omitted,
        "omitted_after": export.omitted_after,
        "matched": export.matched,
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
    for (i, message) in export.messages.iter().enumerate() {
        if export.matched == Some(i) {
            out.push_str("[the search hit]\n");
        }
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
    if export.omitted_after > 0 {
        let _ = writeln!(
            out,
            "[{} later message{} omitted; --all exports the whole conversation]",
            export.omitted_after,
            if export.omitted_after == 1 { "" } else { "s" }
        );
    }
    out
}

/// A hosted terminal's screen, read through its host without attaching to it.
pub struct Screen {
    pub session: Session,
    pub text: terminal_host::Text,
}

/// What a live row's id names once history has no conversation under it.
pub enum Live {
    Conversation(Located),
    Screen(Box<Screen>),
}

/// Resolve an id `cones ls` or `cones launch` printed for a live row. A `client_pid` harness
/// is listed by its process, so the conversation it reports is read under that row's native
/// id; a row with none, in a terminal cones hosts, has that terminal's screen. Ids resolve as
/// `cones stop` resolves them: the host's own id, or the row discovery gave the client.
pub fn live(
    state: &Path,
    claude: &Path,
    id: &str,
    harness: Option<&str>,
    home: Option<&Path>,
) -> Result<Option<Live>> {
    let named = |h: &str| harness.is_none_or(|wanted| wanted == h);
    let session = crate::fleet::control_session(claude, id)?.filter(|s| named(&s.harness));
    let records = terminal_host::records(state);
    let hosts: Vec<_> = records
        .iter()
        .filter(|r| named(&r.session.harness) && r.matches_session(id, session.as_ref()))
        .collect();
    ensure!(
        hosts.len() < 2,
        "{id} names {} cones terminals; read them by their own ids",
        hosts.len()
    );
    // The host record carries what the client's own reporter named, which discovery may lack.
    let listed = hosts.first().map(|record| {
        let mut listed = record.session.clone();
        crate::fleet::identify(&mut listed);
        listed
    });
    for row in [&session, &listed].into_iter().flatten() {
        if let Some(native) = &row.native_id
            && let Ok(located) = locate_scoped(claude, native, Some(&row.harness), home)
        {
            return Ok(Some(Live::Conversation(located)));
        }
    }
    let (Some(record), Some(listed)) = (hosts.first(), listed) else {
        return Ok(None);
    };
    Ok(Some(Live::Screen(Box::new(Screen {
        text: terminal_host::text(record)
            .with_context(|| format!("reading the cones terminal for {id}"))?,
        session: session.unwrap_or(listed),
    }))))
}

impl Screen {
    /// The lines a read returns and how many it left out above them. The default is what the
    /// terminal shows now; `tail` counts lines back from its last one; `all` adds everything
    /// that scrolled off. Blank rows below the last drawn one are not lines.
    fn lines(&self, tail: Option<usize>, all: bool) -> (usize, Vec<&str>) {
        let drawn = self
            .text
            .screen
            .iter()
            .rposition(|line| !line.is_empty())
            .map_or(0, |last| last + 1);
        let lines: Vec<&str> = self
            .text
            .scrollback
            .iter()
            .chain(&self.text.screen[..drawn])
            .map(String::as_str)
            .collect();
        let keep = match tail {
            _ if all => lines.len(),
            Some(tail) => tail,
            None => drawn,
        }
        .min(lines.len());
        let omitted = lines.len() - keep;
        (omitted, lines[omitted..].to_vec())
    }

    pub fn json(&self, tail: Option<usize>, all: bool) -> serde_json::Value {
        let (omitted, lines) = self.lines(tail, all);
        serde_json::json!({
            "session": {"harness": self.session.harness, "session_id": self.session.session_id},
            "cwd": self.session.cwd,
            "terminal": {
                "rows": self.text.rows,
                "cols": self.text.cols,
                "alternate": self.text.alternate,
                "omitted": omitted,
                "lines": lines,
            },
        })
    }

    /// The screen as text. The first line says it is a screen, so it is never read as a
    /// conversation, and a read that left lines out says how many.
    pub fn render(&self, tail: Option<usize>, all: bool) -> String {
        let (omitted, lines) = self.lines(tail, all);
        let mut out = format!(
            "[terminal screen of {} {}, {}x{}; a screen, not a conversation: cones has no \
             transcript to read for this session]\n",
            self.session.harness, self.session.session_id, self.text.cols, self.text.rows
        );
        if omitted > 0 {
            let _ = writeln!(
                out,
                "[{omitted} earlier line{} omitted; --all prints the scrollback]",
                if omitted == 1 { "" } else { "s" }
            );
        }
        out.push('\n');
        for line in lines {
            out.push_str(&transcript::plain(line));
            out.push('\n');
        }
        out
    }
}
