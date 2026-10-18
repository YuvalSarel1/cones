//! Detached launch for the CLI: start a native session that keeps running after this
//! process exits, and return an identity the other public commands accept.
//!
//! Claude's background daemon owns its own lifetime and prints its own id, so the only work
//! here is turning that short id into the exact one the roster carries. Every other
//! launchable harness is a terminal client, so its launch is handed to the same detached
//! terminal host the dashboard uses, which calls `setsid` and outlives its launcher.
//!
//! Two identical prompts launched into one folder at the same moment are told apart by a key
//! the launch itself owns: Claude's returned background id, or the pid the host reports for
//! the native client it just spawned. Codex gives a launch no such key: its daemon reports
//! the new thread to the client that asked for it and to nobody else, and the client records
//! it nowhere a second process can read. Its thread is therefore named the way the dashboard
//! names it, by folder, start time and typed prompt, and only when exactly one thread fits
//! and no other launch was waiting on the same three. A launch cones cannot name is reported
//! as unnamed, never guessed.
//!
//! Permissions, model choice and the conversation stay with the harness. cones adds a
//! lifetime and a name for it.

use crate::{
    config::{HarnessKind, Policy},
    fleet::Session,
    harness::{self, spec::LaunchHandler},
    ledger::Ledger,
    terminal_host,
    tui::placeholder,
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

/// Start `kind` in `dir` and return the exact identity the public commands take for it.
/// `start` is the command `harness::start` compiled, so the flags, provider and native home
/// are the ones the dashboard's composer would have used.
#[allow(clippy::too_many_arguments)]
pub fn detached(
    state: &Path,
    claude: &Path,
    kind: HarnessKind,
    dir: &Path,
    prompt: &str,
    record_prompt: bool,
    policy: &Policy,
    start: harness::Start,
) -> Result<Launched> {
    let handler = harness::spec(kind).launch.as_ref().map(|l| l.handler);
    // The daemon makes a thread on the first turn and keeps nothing before it, so a waiting
    // client has only its pid, which stops naming a row the moment that turn arrives.
    ensure!(
        handler != Some(LaunchHandler::CodexRemote) || !prompt.is_empty(),
        "{kind} makes a thread on its first turn and has no lasting id before it, so a \
         detached launch needs a prompt; open an empty session from the dashboard"
    );
    let owned = format!("{kind}:start:{}", uuid::Uuid::new_v4());
    let since = chrono::Utc::now();
    // A prompt the caller keeps to itself is recorded as absent, never shortened or hashed.
    let recorded = record_prompt.then_some(prompt);
    // Written before anything is started. A launcher killed between here and the reply
    // leaves this line behind, so the work can be found and a retry is a duplicate someone
    // can see rather than an untracked second agent in the same folder.
    recovery(
        state,
        "launch.submitted",
        json!({
            "operation_id": owned, "harness": kind.to_string(),
            "cwd": dir.to_string_lossy(), "prompt": recorded,
        }),
    );
    let result = match (handler, start) {
        (Some(LaunchHandler::ClaudeBackground), harness::Start::Background(command)) => {
            background(state, claude, policy, kind, dir, command)
        }
        (Some(LaunchHandler::Terminal), harness::Start::Foreground(command)) => hosted(
            state,
            claude,
            policy,
            placeholder(kind, &owned, dir, prompt),
            prompt,
            command,
        ),
        (Some(LaunchHandler::CodexRemote), harness::Start::Foreground(command)) => host(
            state,
            placeholder(kind, &owned, dir, prompt),
            prompt,
            command,
        )
        .and_then(|record| {
            thread(
                state, claude, policy, kind, dir, prompt, &owned, since, &record,
            )
        }),
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
            "cwd": dir.to_string_lossy(), "prompt": recorded,
            "session_id": result.as_ref().ok().map(|l| l.id.clone()),
            "error": result.as_ref().err().map(|e| format!("{e:#}")),
        }),
    );
    result
}

/// Reopen the conversation `entry` names in a detached terminal host: the command the
/// dashboard runs for Enter on a history row, hosted the way a terminal launch is, so a
/// dashboard can attach to it later. Resume keeps the conversation's id, which is returned. A
/// conversation already on the roster or held by a live run is refused, as the dashboard
/// refuses it: message it instead.
pub fn resume(
    state: &Path,
    policy: &Policy,
    entry: &crate::history::Entry,
    rows: &[Session],
) -> Result<Launched> {
    let id = &entry.key.session_id;
    let kind = harness::by_name(&entry.key.harness)
        .context("unknown history harness")?
        .kind;
    let ledger = Ledger::new(state)?;
    ensure!(
        !rows.iter().any(|s| s.native() == id)
            // A client discovery still keys by its process is held by the host that opened it.
            && !terminal_host::records(state)
                .iter()
                .any(|r| r.session.session_id == *id)
            && !ledger.runs()?.iter().any(|r| {
                r.terminal.is_none() && r.started.session_id.as_deref() == Some(id.as_str())
            }),
        "{id} is already running; `cones send` messages it"
    );
    // A removed row is hidden by id, which would keep the revived session out of the list.
    ledger.unhide(id)?;
    recovery(
        state,
        "resume.submitted",
        json!({
            "operation_id": id, "harness": kind.to_string(), "cwd": entry.cwd.to_string_lossy(),
        }),
    );
    let command = harness::resume_history(entry, policy)?;
    let record = host(state, crate::tui::history_session(entry), "", command)?;
    let pid = record.session.pid;
    let mut watch = Watch::new(kind, record);
    let started = Instant::now();
    while started.elapsed() < watch_for() {
        watch.alive()?;
        std::thread::sleep(POLL);
    }
    watch.alive()?;
    Ok(Launched {
        id: id.clone(),
        identity: Identity::Native,
        harness: kind.to_string(),
        pid,
    })
}

/// Fork the conversation `entry` names through the harness's native fork, in a detached
/// terminal host, and record the parent once the child has an id of its own, so the
/// dashboard lists the fork under it.
pub fn fork(
    state: &Path,
    claude: &Path,
    policy: &Policy,
    entry: &crate::history::Entry,
) -> Result<Launched> {
    let spec = harness::by_name(&entry.key.harness)
        .filter(|s| s.operations.fork.is_some())
        .with_context(|| {
            format!(
                "{} has no verified native fork operation",
                entry.key.harness
            )
        })?;
    let kind = spec.kind;
    if spec.launch.as_ref().map(|l| l.handler) == Some(LaunchHandler::CodexRemote) {
        return codex_fork(state, policy, entry);
    }
    let expected = (kind == HarnessKind::Pi).then(|| uuid::Uuid::new_v4().to_string());
    let harness::Start::Foreground(command) = harness::fork(entry, expected.as_deref(), policy)?
    else {
        bail!("{kind} fork needs its own native client");
    };
    let owned = format!("{kind}:fork:{}", uuid::Uuid::new_v4());
    let title = entry.title.as_deref().unwrap_or("fork");
    let mut session = placeholder(kind, &owned, &entry.cwd, title);
    session.forked_from = Some(entry.key.session_id.clone());
    if kind == HarnessKind::Claude {
        session.kind = Some("interactive".into());
    }
    recovery(
        state,
        "fork.submitted",
        json!({
            "operation_id": owned, "harness": kind.to_string(),
            "cwd": entry.cwd.to_string_lossy(), "parent": entry.key.session_id,
        }),
    );
    let launched = hosted(state, claude, policy, session, "", command)?;
    let child = match launched.identity {
        Identity::Native => Some(launched.id.clone()),
        _ => expected,
    };
    match child {
        Some(child) => crate::forks::record(
            state,
            crate::forks::Link {
                harness: kind.to_string(),
                home: entry.key.home.clone(),
                cwd: entry.cwd.clone(),
                parent: entry.key.session_id.clone(),
                child,
                parent_harness: None,
            },
        )
        .context("the fork is running, but its parent was not recorded")?,
        None => eprintln!(
            "{kind} has reported no id for the fork yet, so the dashboard will not list it \
             under {}",
            entry.key.session_id
        ),
    }
    Ok(launched)
}

/// A Codex fork is a thread the daemon makes and names in its reply, so no client is hosted:
/// the daemon holds the fork as a launched thread is held once its client goes, and `cones
/// send` gives it work. The thread is saved, so its row stays after the daemon lets it go.
fn codex_fork(state: &Path, policy: &Policy, entry: &crate::history::Entry) -> Result<Launched> {
    ensure!(
        !entry.archived,
        "resume an archived session before forking it"
    );
    let owned = format!("codex:fork:{}", uuid::Uuid::new_v4());
    recovery(
        state,
        "fork.submitted",
        json!({
            "operation_id": owned, "harness": "codex",
            "cwd": entry.cwd.to_string_lossy(), "parent": entry.key.session_id,
        }),
    );
    let child = harness::codex_fork(entry, policy)?;
    crate::forks::record(
        state,
        crate::forks::Link {
            harness: "codex".into(),
            home: entry.key.home.clone(),
            cwd: entry.cwd.clone(),
            parent: entry.key.session_id.clone(),
            child: child.clone(),
            parent_harness: None,
        },
    )
    .context("the fork is running, but its parent was not recorded")?;
    // The daemon writes the fork's rollout before it answers. Saved, the thread stays a row
    // after the daemon lets it go, as a launched one does.
    let saved = harness::codex_rollout(&entry.key.home, &child).and_then(|rollout| {
        let first =
            std::io::BufRead::lines(std::io::BufReader::new(std::fs::File::open(&rollout)?))
                .next()
                .transpose()?
                .unwrap_or_default();
        let meta = crate::codex::meta(&first).context("the fork's rollout has no session_meta")?;
        crate::codex::remember(
            state,
            crate::codex::Thread {
                id: child.clone(),
                cwd: meta.cwd,
                started: meta.started,
                rollout,
            },
        )?;
        Ok(())
    });
    if let Err(error) = saved {
        eprintln!("cones: could not save thread {child}: {error:#}");
    }
    Ok(Launched {
        id: child,
        identity: Identity::Native,
        harness: "codex".into(),
        pid: None,
    })
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
    let stdout = start_background(kind, &mut command)?;
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

/// Run a background start and return what it printed, which carries the background id. The
/// one start both `cones launch` and the dashboard's composer use; each names the session its
/// own way afterwards.
pub fn start_background(kind: HarnessKind, command: &mut std::process::Command) -> Result<String> {
    let out = command
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("start {kind}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    ensure!(
        out.status.success(),
        "{kind} exited {}: {}",
        out.status.code().unwrap_or(-1),
        match stderr.trim() {
            "" => "no error output",
            said => said,
        }
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A terminal harness is its own client, so detaching it means giving it a terminal that is
/// not this one. The host is the dashboard's, unchanged: it calls `setsid`, keeps the pty and
/// the scrollback, and a dashboard attaches to it later. The prompt travels in the command's
/// environment and the host binds it to stdin, so it must not be bound here.
fn hosted(
    state: &Path,
    claude: &Path,
    policy: &Policy,
    session: Session,
    prompt: &str,
    command: std::process::Command,
) -> Result<Launched> {
    let kind = harness::by_name(&session.harness)
        .context("unknown harness")?
        .kind;
    let owned = session.session_id.clone();
    let owned = owned.as_str();
    let record = host(state, session, prompt, command)?;
    // Past here the native client is running. Failing to name it is a reporting failure and
    // never a reason to stop it: the host record names it for the dashboard either way. A
    // client that has already left is a different thing, and is reported as no launch at all.
    let pid = record.session.pid;
    let name = kind.to_string();
    // What discovery calls the client before the harness reports a conversation of its own.
    let process_key = pid.map(|pid| format!("{kind}-{pid}"));
    let started = Instant::now();
    let mut watch = Watch::new(kind, record);
    // The harness's own row is what the roster keeps once discovery reports the client, so it
    // is worth a short wait: returning ours first would name a row that is about to be
    // replaced. A harness discovery never reports keeps the owned id, which its host record
    // holds for as long as the host runs.
    let row = loop {
        watch.alive()?;
        let waited = started.elapsed();
        let found = settle(Duration::ZERO, claude, state, policy, |s| {
            (pid.is_some() && s.pid == pid && s.harness == name)
                || (waited >= DISCOVER && s.session_id == owned)
        })?;
        if let Some(row) = found.into_iter().next()
            && waited >= watch_for()
        {
            break row;
        }
        ensure!(
            waited < DISCOVER + SETTLE,
            "{kind} was hosted as {owned} with pid {} but no row for it reached the roster in \
             {}s. It is still running: reach it from the dashboard, or see {}",
            pid.map_or_else(|| "none".into(), |p| p.to_string()),
            (DISCOVER + SETTLE).as_secs(),
            state.join("launches.jsonl").display()
        );
        std::thread::sleep(POLL);
    };
    // The roster pass takes a moment of its own, and a row is no proof the client outlived it.
    watch.alive()?;
    let identity = match &row.session_id {
        id if *id == owned => Identity::Owned,
        id if Some(id.as_str()) == process_key.as_deref() => Identity::Process,
        _ => Identity::Native,
    };
    // A live client may still be asking to sign in, to trust the folder or to update. cones
    // reads no harness's screens, so it shows the screen instead of calling the prompt taken.
    if identity != Identity::Native && !watch.shown.is_empty() {
        eprintln!(
            "{kind} has reported no conversation of its own yet. Its screen shows:\n{}",
            watch.shown
        );
    }
    Ok(Launched {
        identity,
        id: row.session_id,
        harness: name,
        pid,
    })
}

/// How long a hosted client has to stay alive before its launch is called one. A client that
/// refuses to start, or leaves at a sign-in question, is gone well inside it. Shortened when
/// `CONES_TEST_FAST` is set, which changes how long cones waits and never what it decides.
fn watch_for() -> Duration {
    if std::env::var_os("CONES_TEST_FAST").is_some() {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(3)
    }
}

/// The launch's own view of the client it just hosted. The host keeps the pty only while the
/// client lives, so the screen has to be read before the exit, not after it. While this is
/// held a dashboard cannot open the same terminal, which is why it is dropped with the launch.
struct Watch {
    kind: HarnessKind,
    viewer: std::io::Result<crate::viewer::Viewer>,
    /// The last screen that had text on it, indented. A client leaving the alternate screen
    /// blanks its terminal on the way out, so the final screen alone says nothing.
    shown: String,
}

impl Watch {
    fn new(kind: HarnessKind, record: terminal_host::Record) -> Self {
        Self {
            kind,
            viewer: crate::viewer::Viewer::attach(record, Colors::default()),
            shown: String::new(),
        }
    }

    /// Fail the launch when the client has exited, with what it last showed.
    fn alive(&mut self) -> Result<()> {
        let kind = self.kind;
        let viewer = match &mut self.viewer {
            Ok(viewer) => viewer,
            Err(error) => bail!("{kind} exited before cones could read its screen: {error}"),
        };
        let gone = viewer.pump().is_err();
        let lines: Vec<String> = viewer
            .screen()
            .contents()
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .map(|line| format!("  {line}"))
            .collect();
        if !lines.is_empty() {
            // ponytail: the last twelve lines with text, enough for a question and its context.
            self.shown = lines[lines.len().saturating_sub(12)..].join("\n");
        }
        let status = viewer.exited();
        if status.is_none() && !gone {
            return Ok(());
        }
        let said = String::from_utf8_lossy(viewer.stderr_tail());
        bail!(
            "{kind} exited {} during its launch, so there is no session. {}{}",
            status
                .and_then(|s| s.code())
                .map_or_else(|| "with no status".into(), |code| code.to_string()),
            match self.shown.as_str() {
                "" => "It showed nothing on its screen.".into(),
                shown => format!("Its screen last showed:\n{shown}"),
            },
            match first_line(&said) {
                Some(line) => format!("\nIt reported: {line}"),
                None => String::new(),
            }
        )
    }
}

/// Give the client a detached terminal host and return its record.
fn host(
    state: &Path,
    session: Session,
    prompt: &str,
    command: std::process::Command,
) -> Result<terminal_host::Record> {
    // ponytail: the launching terminal's size, or a plain default when there is none. The
    // host resizes on attach, so this only decides what the client renders before anyone looks.
    let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((120, 40));
    let kind = session.harness.clone();
    terminal_host::launch(
        &std::env::current_exe().context("locating the cones executable")?,
        state,
        &command,
        session,
        prompt,
        rows,
        cols,
        Colors::default(),
        false,
    )
    .with_context(|| format!("host {kind}"))
}

/// Name the thread a hosted Codex client just opened, then let the client go. The client's
/// pid names nothing that lasts: the roster replaces its row with the daemon's thread as soon
/// as there is one. The thread is the one in this folder, started after this launch and
/// opened with this prompt, which is the dashboard's rule. Two that fit are refused rather
/// than chosen between, and so is one that fits while another launch waits on the same rule.
#[allow(clippy::too_many_arguments)]
fn thread(
    state: &Path,
    claude: &Path,
    policy: &Policy,
    kind: HarnessKind,
    dir: &Path,
    prompt: &str,
    owned: &str,
    since: chrono::DateTime<chrono::Utc>,
    record: &terminal_host::Record,
) -> Result<Launched> {
    let name = kind.to_string();
    let typed = crate::fleet::headline(prompt);
    let pid = record.session.pid;
    let fits = |s: &Session| {
        s.harness == name
            && s.kind == record.session.kind
            && s.cwd == dir
            && s.state != "-"
            && s.started.is_some_and(|at| at >= since)
            && s.transcript_path
                .as_deref()
                .and_then(crate::codex::prompt_of)
                == typed
    };
    let mut found = settle(SETTLE, claude, state, policy, fits)?;
    if found.len() == 1 {
        // ponytail: one more look, so a second thread made a moment after the first is seen
        // before one is named. A thread made later than that is still named wrongly.
        std::thread::sleep(POLL);
        found = settle(Duration::ZERO, claude, state, policy, fits)?;
    }
    ensure!(
        found.len() < 2,
        "{kind} reported {} new threads in {} with this prompt; cones will not choose between \
         them. All are running: `cones ls --dir {}` lists them",
        found.len(),
        dir.display(),
        dir.display()
    );
    let row = found.into_iter().next().with_context(|| {
        format!(
            "{kind} was hosted with pid {} but reported no thread for this prompt in {}s. It \
             is still running and may be waiting on a trust or sign-in screen: open it from \
             the dashboard, check `cones ls --dir {}` before launching it again, and see {}",
            pid.map_or_else(|| "none".into(), |p| p.to_string()),
            SETTLE.as_secs(),
            dir.display(),
            state.join("launches.jsonl").display()
        )
    })?;
    // Read after the thread is seen: a launch submitted later than this cannot have made it.
    let rivals = rivals(state, owned, &name, dir, &typed, since);
    ensure!(
        rivals == 0,
        "this prompt was launched into {} {rivals} more time(s) that were still unnamed when \
         this launch began, and {kind} reports nothing that tells their threads apart; cones \
         will not choose between them. All are running: `cones ls --dir {}` lists them",
        dir.display(),
        dir.display()
    );
    // The daemon drops its lock minutes after a thread goes quiet. The saved record keeps the
    // thread a row after that, as it does for one the dashboard launched.
    if let (Some(started), Some(rollout)) = (row.started, &row.transcript_path) {
        let saved = crate::codex::remember(
            state,
            crate::codex::Thread {
                id: row.session_id.clone(),
                cwd: row.cwd.clone(),
                started,
                rollout: rollout.clone(),
            },
        );
        if let Err(error) = saved {
            eprintln!("cones: could not save thread {}: {error}", row.session_id);
        }
    }
    // The daemon runs the thread, so the client has done its work once the first turn is
    // reported. Left hosted it would be a second row for the same agent; the dashboard joins
    // the thread with a client of its own.
    if let Err(error) = terminal_host::stop(record) {
        eprintln!("cones: the launching client is still hosted: {error}");
    }
    Ok(Launched {
        id: row.session_id,
        identity: Identity::Native,
        harness: name,
        pid: None,
    })
}

/// Other launches of this prompt into this folder that had not been named or given up on when
/// this one began, read from the ledger every launch writes before it starts anything. A
/// launch older than the naming wait has stopped looking for its thread and is not counted.
fn rivals(
    state: &Path,
    owned: &str,
    harness: &str,
    dir: &Path,
    typed: &Option<String>,
    since: chrono::DateTime<chrono::Utc>,
) -> usize {
    let lines: Vec<Value> = std::fs::read_to_string(state.join("launches.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let at = |line: &Value| {
        chrono::DateTime::parse_from_rfc3339(line["timestamp"].as_str()?)
            .ok()
            .map(|at| at.with_timezone(&chrono::Utc))
    };
    let settled = |operation: &Value| {
        lines.iter().any(|line| {
            matches!(
                line["event"].as_str(),
                Some("launch.identified" | "launch.unnamed")
            ) && line["data"]["operation_id"] == *operation
                && at(line).is_some_and(|at| at < since)
        })
    };
    lines
        .iter()
        .filter(|line| line["event"] == "launch.submitted")
        .filter(|line| {
            let data = &line["data"];
            data["operation_id"] != owned
                && data["harness"] == harness
                && data["cwd"].as_str().is_some_and(|cwd| {
                    Path::new(cwd)
                        .canonicalize()
                        .as_deref()
                        .unwrap_or(Path::new(cwd))
                        == dir
                })
                // A launch that kept its prompt to itself could be waiting on these words.
                && data["prompt"]
                    .as_str()
                    .is_none_or(|prompt| crate::fleet::headline(prompt) == *typed)
                && at(line).is_some_and(|at| at > since - SETTLE)
                && !settled(&data["operation_id"])
        })
        .count()
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

/// The id in `claude --bg`'s one line, `backgrounded · <short id> (idle)`. Claude colours the
/// id even when stdout is a pipe, so escape sequences are dropped before reading it.
pub fn background_id(status: &str) -> Option<String> {
    plain(status)
        .split("backgrounded · ")
        .nth(1)?
        .split_whitespace()
        .next()
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        .map(str::to_owned)
}

/// The text without its CSI escape sequences, such as colours.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.clone().next() == Some('[') {
            chars.next();
            // Parameters and intermediates run until the final byte, '@' through '~'.
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
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

/// Block until the harness reports that one of `ids` stopped moving on its own, and say which
/// and how: its turn ended, it asks for input, it failed or stopped, or it left the roster. The
/// caller runs this as a background task, so its exit is what wakes the caller; nothing is
/// injected into any session. With `since`, the moment a session was launched, a state
/// reported before its first turn is not an end: it has to have been seen working, or have
/// written after `since`. Without it, a session already settled answers at once, and an id
/// not live on the first look is an error. `None` is the timeout. A job's session is watched
/// like any other.
// ponytail: matches the row's session or native id; a pi row keyed by its process until the
// session file appears can read as gone, so watch the native id `cones ls` shows by then.
pub fn wait(
    state: &Path,
    claude: &Path,
    jobs: &Path,
    ids: &[String],
    since: Option<chrono::DateTime<chrono::Utc>>,
    timeout: Option<Duration>,
) -> Result<Option<String>> {
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut seen = vec![false; ids.len()];
    let mut moved = vec![since.is_none(); ids.len()];
    let (mut first, mut failures) = (true, 0);
    loop {
        match crate::tui::sessions(claude, state, &crate::config::defaults(jobs)) {
            Ok(rows) => {
                failures = 0;
                for (i, id) in ids.iter().enumerate() {
                    let row = rows
                        .iter()
                        .find(|s| s.session_id == *id || s.native_id.as_deref() == Some(id));
                    match row {
                        None if seen[i] => return Ok(Some(format!("{id}\tgone\n"))),
                        None => ensure!(
                            !first || since.is_some(),
                            "no live session {id}; `cones ls` lists them"
                        ),
                        Some(row) => {
                            seen[i] = true;
                            let busy = matches!(
                                row.state.as_str(),
                                "working" | "active" | "shell" | "-" | ""
                            );
                            moved[i] |= busy
                                || since.is_some_and(|since| {
                                    row.last_activity.is_some_and(|at| at >= since)
                                });
                            if moved[i] && !busy {
                                return Ok(Some(format!("{id}\t{}\n", row.state)));
                            }
                        }
                    }
                }
                first = false;
            }
            // A registry can be mid-rewrite; an hour of failures is a broken install.
            Err(e) => {
                failures += 1;
                if failures > 360 {
                    return Err(e.context("the roster has been unreadable for an hour"));
                }
            }
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(None);
        }
        let nap = crate::coordinator::beat();
        std::thread::sleep(match deadline {
            Some(d) => nap.min(d.saturating_duration_since(Instant::now())),
            None => nap,
        });
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
        assert_eq!(
            background_id("backgrounded · \x1b[36m9ab92c71\x1b[39m\n").as_deref(),
            Some("9ab92c71"),
            "claude colours the id even into a pipe"
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
}
