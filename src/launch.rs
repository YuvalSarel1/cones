//! Detached launch for the CLI: start a native session that keeps running after this
//! process exits, and return an identity the other public commands accept.
//!
//! Claude's background daemon owns its own lifetime and prints its own id, so the only work
//! here is turning that short id into the exact one the roster carries. Every other
//! launchable harness is a terminal client, so its launch is handed to the same detached
//! terminal host the dashboard uses, which calls `setsid` and outlives its launcher.
//!
//! Nothing in this module matches a launch by prompt, folder or start time. Two identical
//! prompts launched into one folder at the same moment are told apart by a key the launch
//! itself owns: Claude's returned background id, or the pid the host reports for the native
//! client it just spawned. A launch cones cannot name is reported as unnamed, never guessed.
//!
//! Permissions, model choice and the conversation stay with the harness. cones adds a
//! lifetime and a name for it.

use crate::{
    config::{HarnessKind, Policy},
    fleet::Session,
    harness::{self, spec::LaunchHandler},
    ledger::Ledger,
    terminal_host,
    viewer::Colors,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, Instant},
};

/// Which kind of name `detached` returned. The two are not interchangeable: one is the
/// harness's own and survives cones, the other is cones' own and lives as long as its host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// The harness's own session id, as its registry or session file reports it.
    Native,
    /// The client process cones discovered. The harness's own id replaces it as soon as the
    /// harness reports one, which for pi is when it first writes its session file.
    Process,
    /// A cones-owned terminal id, durable in `STATE_DIR/terminals` while its host runs.
    Owned,
}

impl Identity {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Native => "native session id",
            Self::Process => "client process id, until the harness reports its own",
            Self::Owned => "cones-owned terminal id",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Launched {
    pub id: String,
    pub identity: Identity,
    pub harness: String,
    /// The native client's pid, when the launch owns a process rather than a daemon record.
    pub pid: Option<u32>,
}

/// How long to wait for the roster to carry a launch this process owns. This bounds naming,
/// never the run: the session is already executing when the wait starts, and a wait that
/// expires reports an unnamed launch rather than stopping anything.
const SETTLE: Duration = Duration::from_secs(20);
/// A hosted client is already named by its host record, so only the harness's own row is
/// worth waiting for, and only for as long as process discovery plausibly takes.
const DISCOVER: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(150);

/// Whether this harness's launch leaves work running past the launching terminal.
/// Codex is launched as a remote client of its own daemon and reports no thread id, so a
/// launch could be detached or named but not both; see `docs/harness.md`.
pub fn detaches(kind: HarnessKind) -> bool {
    matches!(
        harness::spec(kind).launch.as_ref().map(|l| l.handler),
        Some(LaunchHandler::ClaudeBackground | LaunchHandler::Terminal)
    )
}

/// Start `kind` in `dir` and return the exact identity the public commands take for it.
/// `start` is the command `harness::start` compiled, so the flags, provider and native home
/// are the ones the dashboard's composer would have used.
pub fn detached(
    state: &Path,
    claude: &Path,
    kind: HarnessKind,
    dir: &Path,
    prompt: &str,
    policy: &Policy,
    start: harness::Start,
) -> Result<Launched> {
    let owned = format!("{kind}:start:{}", uuid::Uuid::new_v4());
    // Written before anything is started. A launcher killed between here and the reply
    // leaves this line behind, so the work can be found and a retry is a duplicate someone
    // can see rather than an untracked second agent in the same folder.
    recovery(
        state,
        "launch.submitted",
        json!({
            "operation_id": owned, "harness": kind.to_string(),
            "cwd": dir.to_string_lossy(), "prompt": prompt,
        }),
    );
    let handler = harness::spec(kind).launch.as_ref().map(|l| l.handler);
    let result = match (handler, start) {
        (Some(LaunchHandler::ClaudeBackground), harness::Start::Background(command)) => {
            background(state, claude, policy, kind, dir, command)
        }
        (Some(LaunchHandler::Terminal), harness::Start::Foreground(command)) => {
            hosted(state, claude, policy, kind, dir, prompt, &owned, command)
        }
        _ => bail!("{kind} has no detached launch"),
    };
    recovery(
        state,
        match &result {
            Ok(_) => "launch.identified",
            Err(_) => "launch.unnamed",
        },
        json!({
            "operation_id": owned, "harness": kind.to_string(),
            "cwd": dir.to_string_lossy(), "prompt": prompt,
            "session_id": result.as_ref().ok().map(|l| l.id.clone()),
            "error": result.as_ref().err().map(|e| format!("{e:#}")),
        }),
    );
    result
}

/// Claude's `--bg` records the session with its daemon and returns; the daemon runs it.
/// Its one line of output is the only per-launch key there is, so it is captured rather
/// than passed through, and the roster turns it into the full id.
fn background(
    state: &Path,
    claude: &Path,
    policy: &Policy,
    kind: HarnessKind,
    dir: &Path,
    mut command: std::process::Command,
) -> Result<Launched> {
    let out = command
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("start {kind}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    ensure!(
        out.status.success(),
        "{kind} exited {}: {}",
        out.status.code().unwrap_or(-1),
        first_line(&stderr).unwrap_or("no error output")
    );
    let short = background_id(&stdout).with_context(|| {
        format!(
            "{kind} started but printed no background id: {}",
            first_line(&stdout).unwrap_or("no output")
        )
    })?;
    let name = kind.to_string();
    let found = settle(SETTLE, claude, state, policy, |s| {
        s.harness == name && s.session_id.starts_with(&short)
    })?;
    ensure!(
        found.len() < 2,
        "{kind} returned background id {short}, which names {} sessions; \
         cones will not choose between them",
        found.len()
    );
    let row = found.into_iter().next().with_context(|| {
        format!(
            "{kind} returned background id {short} but no session with it reached the roster \
             in {}s. The session may be running: check `cones ls --dir {}` before launching \
             it again, and see {}",
            SETTLE.as_secs(),
            dir.display(),
            state.join("launches.jsonl").display()
        )
    })?;
    Ok(Launched {
        id: row.session_id,
        identity: Identity::Native,
        harness: name,
        pid: row.pid,
    })
}

/// A terminal harness is its own client, so detaching it means giving it a terminal that is
/// not this one. The host is the dashboard's, unchanged: it calls `setsid`, keeps the pty and
/// the scrollback, and a dashboard attaches to it later. The prompt travels in the command's
/// environment and the host binds it to stdin, so it must not be bound here.
#[allow(clippy::too_many_arguments)]
fn hosted(
    state: &Path,
    claude: &Path,
    policy: &Policy,
    kind: HarnessKind,
    dir: &Path,
    prompt: &str,
    owned: &str,
    command: std::process::Command,
) -> Result<Launched> {
    // ponytail: the launching terminal's size, or a plain default when there is none. The
    // host resizes on attach, so this only decides what the client renders before anyone looks.
    let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((120, 40));
    let record = terminal_host::launch(
        &std::env::current_exe().context("locating the cones executable")?,
        state,
        &command,
        crate::tui::placeholder(kind, owned, dir, prompt),
        prompt,
        rows,
        cols,
        Colors::default(),
        false,
    )
    .with_context(|| format!("host {kind}"))?;
    // Past here the native client is running. Failing to name it is a reporting failure and
    // never a reason to stop it: the host record names it for the dashboard either way.
    let pid = record.session.pid;
    let name = kind.to_string();
    // What discovery calls the client before the harness reports a conversation of its own.
    let process_key = pid.map(|pid| format!("{kind}-{pid}"));
    // The harness's own row is what the roster keeps once discovery reports the client, so it
    // is worth a short wait: returning ours first would name a row that is about to be
    // replaced. A harness discovery never reports keeps the owned id, which its host record
    // holds for as long as the host runs.
    let discovered = settle(DISCOVER, claude, state, policy, |s| {
        pid.is_some() && s.pid == pid && s.harness == name
    })?;
    let row = match discovered.into_iter().next() {
        Some(row) => row,
        None => settle(SETTLE, claude, state, policy, |s| s.session_id == owned)?
            .into_iter()
            .next()
            .with_context(|| {
                format!(
                    "{kind} was hosted as {owned} with pid {} but no row for it reached the \
                     roster in {}s. It is still running: reach it from the dashboard, or see {}",
                    pid.map_or_else(|| "none".into(), |p| p.to_string()),
                    SETTLE.as_secs(),
                    state.join("launches.jsonl").display()
                )
            })?,
    };
    Ok(Launched {
        identity: match &row.session_id {
            id if *id == owned => Identity::Owned,
            id if Some(id.as_str()) == process_key.as_deref() => Identity::Process,
            _ => Identity::Native,
        },
        id: row.session_id,
        harness: name,
        pid,
    })
}

/// Poll the roster `cones ls` reads until a row this launch owns appears, or the naming
/// deadline passes. `owns` is given an exact key; it never compares prompts or start times.
fn settle(
    within: Duration,
    claude: &Path,
    state: &Path,
    policy: &Policy,
    owns: impl Fn(&Session) -> bool,
) -> Result<Vec<Session>> {
    let ledger = Ledger::new(state)?;
    let deadline = Instant::now() + within;
    loop {
        // Each poll is its own observation pass, so no reading is answered from the last one.
        crate::observe::reset();
        let found: Vec<Session> = crate::tui::fleet_rows(claude, state, &ledger.runs()?, policy)?
            .into_iter()
            .filter(&owns)
            .collect();
        if !found.is_empty() || Instant::now() >= deadline {
            return Ok(found);
        }
        std::thread::sleep(POLL);
    }
}

/// The id in `claude --bg`'s one line, `backgrounded · <short id> (idle)`.
pub fn background_id(status: &str) -> Option<String> {
    status
        .split("backgrounded · ")
        .nth(1)?
        .split_whitespace()
        .next()
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        .map(str::to_owned)
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// The composer's recovery ledger, written by the CLI for the same reason: a prompt that
/// reached a harness cones then failed to name is still recoverable from this file.
/// `dashboard_id` is null because no dashboard submitted it.
fn recovery(state: &Path, event: &str, data: Value) {
    let path = state.join("launches.jsonl");
    let line = json!({
        "v": 1, "timestamp": chrono::Utc::now().to_rfc3339(),
        "pid": std::process::id(), "dashboard_id": Value::Null,
        "event": event, "level": "recovery", "data": data,
    });
    if let Err(error) = crate::tui::debug_line(&path, line) {
        eprintln!(
            "cones: could not record {event} in {}: {error}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_id_takes_only_the_native_line() {
        assert_eq!(
            background_id("backgrounded · 8be7ebf5 (idle)\n").as_deref(),
            Some("8be7ebf5")
        );
        assert_eq!(
            background_id("warming up\nbackgrounded · 16c712a9 (idle)").as_deref(),
            Some("16c712a9")
        );
        for other in [
            "",
            "starting\n",
            "backgrounded · \n",
            "backgrounded · ../../etc/passwd (idle)",
            "backgrounded · id;rm -rf (idle)",
        ] {
            assert_eq!(background_id(other), None, "accepted {other:?}");
        }
    }

    #[test]
    fn only_harnesses_with_a_detached_launch_are_offered_one() {
        for &kind in harness::launchable() {
            let handler = harness::spec(kind).launch.as_ref().map(|l| l.handler);
            assert_eq!(
                detaches(kind),
                handler != Some(LaunchHandler::CodexRemote),
                "{kind} detachment disagrees with its launch handler"
            );
        }
        assert!(detaches(HarnessKind::Claude));
        assert!(detaches(HarnessKind::Pi));
        assert!(detaches(HarnessKind::Opencode));
        assert!(
            !detaches(HarnessKind::Codex),
            "codex reports no thread id at launch, so a detached launch could not be named"
        );
    }
}
