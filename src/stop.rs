//! `cones stop ID`: end one session's work and keep its conversation.
//!
//! Stopping is not deleting and not hiding. Two mechanisms are supported, and a target served by
//! neither is refused rather than approximated:
//!
//! * a harness that declares a native `stop` operation — Claude's background sessions, whose
//!   record and transcript survive it, so `claude attach` and `claude --resume` still open them;
//! * a cones-owned persistent terminal, whose host confirms the native client it owns has exited.
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
    let records = crate::terminal_host::records(state);
    // Exact host ids also work for shells and clients with no native discovery.
    // Otherwise resolve the current row before matching its harness and owned pid.
    let session = if records.iter().any(|r| r.matches_session(id, None)) {
        None
    } else {
        crate::fleet::control_session(claude, id)?
    };
    let hosts: Vec<_> = records
        .iter()
        .filter(|r| r.matches_session(id, session.as_ref()))
        .collect();
    ensure!(
        hosts.len() < 2,
        "{id} names {} cones terminals; stop them by their own ids",
        hosts.len()
    );
    if let Some(record) = hosts.first() {
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
