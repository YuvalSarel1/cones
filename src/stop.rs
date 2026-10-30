//! `cones stop ID`: end one session's work and keep its conversation.
//!
//! Stopping is not deleting and not hiding. Four mechanisms are supported, and a target served
//! by none is refused rather than approximated:
//!
//! * a harness that declares a native `stop` operation — Claude's background sessions, whose
//!   record and transcript survive it, so `claude attach` and `claude --resume` still open them;
//! * a Codex daemon thread, whose running turn and background terminals the daemon ends over
//!   its own protocol, with or without a client attached;
//! * a cones-owned persistent terminal, whose host confirms the native client it owns has exited.
//! * a headless run, a client with no terminal such as `codex exec` under a dispatcher, which
//!   nothing but a signal can end: its verified process is terminated and seen to exit.
//!
//! Signalling a daemon, removing a job record or closing an attach client would each report
//! success while meaning something else, so none of them stands in for a missing native stop.
use crate::harness;
use anyhow::{Context, Result, bail, ensure};
use std::{
    path::Path,
    process::{Command, Stdio},
};

/// End the work `id` names. Returns the line describing what was stopped; every other outcome,
/// including an unsupported target, is an error.
pub fn session(state: &Path, claude: &Path, id: &str) -> Result<String> {
    // A Codex thread runs in its daemon, so closing a client that shows it would end nothing.
    // Its id is settled here, before any terminal that might be showing it is considered.
    if let Some(thread) = crate::codex::thread(claude, state, id) {
        let home = harness::home_of(&thread, claude);
        let stopped = crate::codex::control::stop(&home, id).with_context(|| {
            format!(
                "stopping codex thread {id} through the daemon in {}",
                home.display()
            )
        })?;
        return Ok(match (stopped.turn, stopped.terminals) {
            (false, false) => format!(
                "codex thread {id} in {} is running no turn and no background terminal; \
                 nothing was stopped and its conversation is kept\n",
                home.display()
            ),
            (turn, terminals) => format!(
                "stopped codex thread {id} in {}: {}; its conversation is kept{}\n",
                home.display(),
                [
                    turn.then_some("interrupted its running turn"),
                    terminals.then_some("closed its background terminals"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" and "),
                if stopped.queued {
                    "; the note queued behind the turn now runs"
                } else {
                    ""
                }
            ),
        });
    }
    let (host, session) = host(state, claude, id)?;
    if let Some(record) = &host {
        // The host answers only once the native process it owns has exited, so this reply is the
        // verification and not an intention.
        crate::terminal_host::stop(record)
            .with_context(|| format!("stopping the cones terminal for {id}"))?;
        return Ok(format!(
            "stopped the cones terminal running {} for {id}\n",
            record.session.harness
        ));
    }
    let session = session.with_context(|| {
        format!("{id} is not a live session; `cones ls --json` lists the ids that can be stopped")
    })?;
    let spec = harness::by_name(&session.harness)
        .with_context(|| format!("unknown session harness {}", session.harness))?;
    let kind = spec.session(session.kind.as_deref());
    if kind.lifetime != harness::spec::Lifetime::Daemon
        && let Some(pid) = session.pid.filter(|&pid| process(pid).0 == "??")
    {
        ensure!(
            crate::fleet::terminate(pid, &spec.name)?,
            "{} run {id} had already exited",
            spec.name
        );
        // A process its parent has not collected yet has ended all the same.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !matches!(process(pid).1.chars().next(), None | Some('Z')) {
            ensure!(
                std::time::Instant::now() < deadline,
                "{} run {id} (pid {pid}) was signalled and has not exited",
                spec.name
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        return Ok(format!(
            "stopped the headless {} run {id} (pid {pid}); its conversation is kept\n",
            spec.name
        ));
    }
    if kind.lifetime != harness::spec::Lifetime::Daemon {
        bail!(
            "{} session {id} runs in a terminal cones does not own, so only that terminal can \
             end it",
            spec.name
        );
    }
    let Some(operation) = &spec.operations.stop else {
        bail!(
            "{} has no native session stop, so cones cannot end {id} without deleting work or \
             signalling the daemon that owns every other session",
            spec.name
        );
    };
    harness::check_operation(spec, &spec.operations.stop, "stop")?;
    let program = harness::executable(&spec.name, &harness::launch_path())
        .with_context(|| format!("{} not found on the launch PATH", spec.name))?;
    let home = harness::home_of(&session, claude);
    let short = id.get(..8).context("invalid session id")?;
    let args = harness::spec::args(
        &operation.args,
        &[("id", id.as_ref()), ("short_id", short.as_ref())],
    )?;
    let mut command = Command::new(&program);
    if !spec.home.env.is_empty() {
        command.env(&spec.home.env, &home);
    }
    // No working directory: the registry the stop addresses lives under the native home, and a
    // session whose worktree has since been removed must still be stoppable.
    let out = command
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {}", program.display()))?;
    let trimmed = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_owned();
    ensure!(
        out.status.success(),
        "{} {}: {}",
        spec.name,
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" "),
        [trimmed(&out.stderr), trimmed(&out.stdout)]
            .iter()
            .find(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("exited {}", out.status))
    );
    Ok(format!(
        "stopped {} session {id} in {}; its conversation is kept\n",
        spec.name,
        home.display()
    ))
}

/// The controlling terminal and state of `pid` as `ps` prints them: `??` for no terminal, and
/// an empty state once the process is gone.
fn process(pid: u32) -> (String, String) {
    let out = Command::new("/bin/ps")
        .args(["-o", "tty=,stat=", "-p", &pid.to_string()])
        .output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let mut words = text.as_deref().unwrap_or("").split_whitespace();
    let mut next = || words.next().unwrap_or("").to_owned();
    (next(), next())
}

/// The cones terminal `id` names, if one does, and otherwise the session it names.
fn host(
    state: &Path,
    claude: &Path,
    id: &str,
) -> Result<(
    Option<crate::terminal_host::Record>,
    Option<crate::fleet::Session>,
)> {
    let records = crate::terminal_host::records(state);
    // Exact host ids also work for shells and clients with no native discovery.
    // Otherwise resolve the current row before matching its harness and owned pid.
    let session = if records.iter().any(|r| r.matches_session(id, None)) {
        None
    } else {
        crate::fleet::control_session(claude, id)?
    };
    let mut hosts: Vec<_> = records
        .into_iter()
        .filter(|r| r.matches_session(id, session.as_ref()))
        .collect();
    ensure!(
        hosts.len() < 2,
        "{id} names {} cones terminals; stop them by their own ids",
        hosts.len()
    );
    Ok((hosts.pop(), session))
}

/// `cones rm ID` for a session: what the dashboard's confirmed `ctrl+x` does to its row. A cones
/// terminal is stopped; any other session goes through [`remove_session`]. Runs belong to the
/// jobs client, `runner::remove`. Conversations and output stay.
pub fn remove(
    state: &Path,
    claude: &Path,
    rows: &[crate::fleet::Session],
    id: &str,
) -> Result<String> {
    if let (Some(record), _) = host(state, claude, id)? {
        crate::terminal_host::stop(&record)
            .with_context(|| format!("stopping the cones terminal for {id}"))?;
        // A Codex daemon keeps the thread after its terminal goes; forget it as a client row is.
        if let Some(session) = rows.iter().find(|s| {
            s.session_id == id
                && harness::by_name(&s.harness).is_some_and(|spec| {
                    spec.session(s.kind.as_deref()).remove == harness::spec::Remove::ForgetClient
                })
        }) {
            let session = crate::fleet::Session {
                pid: None,
                ..session.clone()
            };
            return remove_session(state, claude, &session);
        }
        return Ok(format!(
            "stopped the cones terminal running {} for {id}\n",
            record.session.harness
        ));
    }
    let session = rows
        .iter()
        .find(|s| s.session_id == id)
        .with_context(|| format!("no run or session {id}"))?;
    remove_session(state, claude, session)
}

/// Take one live session off every list: stop it, then remove it through the harness's own
/// remove. A stop that fails fails the remove, and nothing is removed. A harness with no remove
/// of its own is only stopped, and the line says so. The transcript stays either way.
pub fn remove_session(
    state: &Path,
    claude: &Path,
    session: &crate::fleet::Session,
) -> Result<String> {
    let id = session.session_id.as_str();
    let spec = harness::by_name(&session.harness).context("unknown session harness")?;
    let kind = spec.session(session.kind.as_deref());
    let not_removed = || format!("{id} was not stopped, so it was not removed");
    if kind.remove == harness::spec::Remove::ForgetClient
        && crate::codex::daemon_pid(&harness::home_of(session, claude)).is_none()
    {
        // A saved thread with no daemon running has nothing running to stop. `cones stop`
        // reports the missing daemon instead, since it was asked to end work.
    } else if kind.lifetime == harness::spec::Lifetime::Daemon {
        self::session(state, claude, id).with_context(not_removed)?;
    } else {
        // A client in someone's terminal gets the dashboard's stop: the process is checked to be
        // this harness's before the signal. One that already ended is stopped all the same.
        let pid = session.pid.with_context(not_removed)?;
        crate::fleet::terminate(pid, &spec.name).with_context(not_removed)?;
    }
    Ok(match kind.remove {
        harness::spec::Remove::Native => {
            // The harness's own refusal is the useful line, such as a worktree with unmerged work.
            crate::fleet::remove_native(claude, session)?;
            format!(
                "stopped and removed {} session {id}; its transcript is kept\n",
                spec.name
            )
        }
        harness::spec::Remove::ForgetClient => {
            crate::codex::forget(state, id)?;
            // The daemon holds the thread for minutes after its client goes, so the saved record
            // alone would bring the row back.
            crate::ledger::Ledger::new(state)?.hide(id)?;
            if let Some(pid) = session.pid {
                crate::fleet::terminate(pid, &spec.name)?;
            }
            format!(
                "stopped and forgot {} thread {id}; it stays resumable from history\n",
                spec.name
            )
        }
        harness::spec::Remove::None => format!(
            "stopped {} session {id}; {} keeps no list of its own to remove it from, so its \
             row leaves with its process and its conversation is kept\n",
            spec.name, spec.name
        ),
    })
}
