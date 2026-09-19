use crate::{
    config::{Overlap, ResolvedJob},
    harness::{self, Invocation, Outcome},
    ledger::{Ledger, Record, Run, Status},
    output::RunOutput,
    private_dir, private_file,
};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use signal_hook::consts::{SIGINT, SIGTERM, SIGUSR1};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        process::{CommandExt, ExitStatusExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

const GRACE: Duration = Duration::from_secs(2);

pub fn process_exists(pid: i32) -> bool {
    pid > 1
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}
fn group_exists(pgid: i32) -> bool {
    pgid > 1 && unsafe { libc::kill(-pgid, 0) } == 0
}
fn signal_group(pgid: i32, signal: i32) {
    if pgid > 1 {
        unsafe { libc::kill(-pgid, signal) };
    }
}
fn cleanup(pgid: i32) {
    signal_group(pgid, libc::SIGTERM);
    let until = Instant::now() + GRACE;
    while group_exists(pgid) && Instant::now() < until {
        thread::sleep(Duration::from_millis(25));
    }
    if group_exists(pgid) {
        signal_group(pgid, libc::SIGKILL);
    }
}

fn active(ledger: &Ledger, run: &Run) -> Result<bool> {
    if run.terminal.is_some() || run.started.status != Status::Started {
        return Ok(false);
    }
    if run.started.owns_run_lock == Some(true) {
        return Ok(ledger.run_lock(&run.started.run_id)?.is_none());
    }
    // Older records predate per-run leases. Never infer that a live legacy owner is dead.
    Ok(run.started.pid.is_some_and(|p| process_exists(p as i32)))
}

fn worker_identity(run: &Record) -> Result<u32> {
    let worker = run.pgid.context("run has no worker process")?;
    let parent = run.pid.context("run has no runner process")?;
    ensure!(worker > 1 && parent > 1, "invalid process identity");
    let info = Command::new("/bin/ps")
        .args(["-ww", "-p", &worker.to_string(), "-o", "ppid=,command="])
        .output()?;
    let info = String::from_utf8_lossy(&info.stdout);
    let args = info.split_whitespace().collect::<Vec<_>>();
    ensure!(
        args.first().is_some_and(|p| p.parse::<u32>() == Ok(parent))
            && args.contains(&"__worker")
            && args
                .windows(2)
                .any(|pair| pair == ["--run-id", run.run_id.as_str()]),
        "cannot identify this run's supervisor; refusing to signal a reused PID"
    );
    Ok(parent)
}

fn signal_run(run: &Record, signal: i32) -> Result<()> {
    let parent = worker_identity(run)?;
    ensure!(
        unsafe { libc::kill(parent as i32, signal) } == 0,
        "unable to signal runner"
    );
    Ok(())
}

// Called under admission lock; leases prove liveness and supervisor UUIDs authorize signalling.
// Return whether an orphan terminal record was written.
fn reap_run(ledger: &Ledger, run: &Run, replaced: bool) -> Result<bool> {
    if active(ledger, run)? || ledger.resolve(&run.started.run_id)?.terminal.is_some() {
        return Ok(false);
    }
    if let Some(pgid) = run.started.pgid
        && group_exists(pgid)
    {
        let output = Command::new("/bin/ps")
            .args(["-ww", "-p", &pgid.to_string(), "-o", "command="])
            .output()?;
        let command = String::from_utf8_lossy(&output.stdout);
        let args = command.split_whitespace().collect::<Vec<_>>();
        ensure!(
            args.contains(&"__worker")
                && args
                    .windows(2)
                    .any(|pair| pair == ["--run-id", run.started.run_id.as_str()]),
            "cannot safely identify orphan process group {pgid}; refusing a new run"
        );
        cleanup(pgid);
    }
    let mut terminal = Record::new(
        run.started.run_id.clone(),
        if replaced {
            Status::Timeout
        } else {
            Status::Failed
        },
    );
    terminal.ended_at = Some(Utc::now());
    terminal.reason = Some(if replaced { "replaced" } else { "orphan" }.into());
    terminal.duration_s = run
        .started
        .fired_at
        .map(|t| ((Utc::now() - t).num_milliseconds() as f64 / 1000.0).max(0.0));
    ledger.append(&terminal)?;
    Ok(true)
}

/// `CONES_NOTIFIER` overrides osascript and receives (title, message).
fn notify(job: &ResolvedJob, status: Status, reason: Option<&str>) {
    let wanted = matches!(status, Status::Failed | Status::Timeout);
    if !job.notify || !wanted {
        return;
    }
    let message = match reason {
        Some(r) => format!("{} {status}: {r}", job.name),
        None => format!("{} {status}", job.name),
    };
    let result = match std::env::var_os("CONES_NOTIFIER") {
        Some(command) => Command::new(command).args(["cones", &message]).status(),
        None => Command::new("/usr/bin/osascript")
            .args([
                "-e",
                &format!(
                    "display notification {} with title \"cones\"",
                    serde_json::json!(message)
                ),
            ])
            .status(),
    };
    if let Err(e) = result {
        eprintln!("cones: notification failed: {e}");
    }
}

fn job_runs(ledger: &Ledger, job: &str) -> Result<Vec<Run>> {
    Ok(ledger
        .runs()?
        .into_iter()
        .filter(|r| {
            r.started.job.as_deref() == Some(job)
                && r.started.status == Status::Started
                && r.terminal.is_none()
        })
        .collect())
}

fn replace_runs(ledger: &Ledger, runs: &[Run]) -> Result<()> {
    for run in runs {
        if active(ledger, run)? {
            // A finished worker may already be gone while its runner archives the transcript.
            if run.started.pgid.is_some_and(group_exists) {
                signal_run(&run.started, SIGUSR1)?;
            }
        } else {
            reap_run(ledger, run, true)?;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut waiting = false;
        for old in runs {
            let run = ledger.resolve(&old.started.run_id)?;
            if active(ledger, &run)? {
                waiting = true;
            } else {
                reap_run(ledger, &run, true)?;
            }
        }
        if !waiting {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "replacement did not confirm shutdown within 10 seconds; no new agent started"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

struct Signals {
    cancelled: Arc<AtomicBool>,
    replaced: Arc<AtomicBool>,
    ids: Vec<signal_hook::SigId>,
}
impl Signals {
    fn new() -> Result<Self> {
        let mut value = Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            replaced: Arc::new(AtomicBool::new(false)),
            ids: vec![],
        };
        for signal in [SIGTERM, SIGINT] {
            value.ids.push(signal_hook::flag::register(
                signal,
                Arc::clone(&value.cancelled),
            )?);
        }
        value.ids.push(signal_hook::flag::register(
            SIGUSR1,
            Arc::clone(&value.replaced),
        )?);
        Ok(value)
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

fn skipped(
    ledger: &Ledger,
    job: &ResolvedJob,
    trigger: &str,
    reason: &str,
    run_id: &str,
) -> Result<Status> {
    let mut r = Record::new(run_id.to_owned(), Status::Skipped);
    r.job = Some(job.name.clone());
    r.trigger = Some(trigger.into());
    r.fired_at = Some(Utc::now());
    r.ended_at = r.fired_at;
    r.harness = Some(job.harness);
    r.cwd = Some(job.cwd.clone());
    r.reason = Some(reason.into());
    ledger.append(&r)?;
    notify(job, Status::Skipped, Some(reason));
    let _ = writeln!(std::io::stdout(), "{}\tskipped\t{reason}", r.run_id);
    Ok(Status::Skipped)
}

enum Output {
    Line(String),
    Invalid,
    Done,
}
fn read_output(reader: impl Read + Send + 'static) -> Receiver<Output> {
    let (tx, rx) = mpsc::sync_channel(64);
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        loop {
            let mut bytes = Vec::new();
            let result = reader
                .by_ref()
                .take(1024 * 1024 + 1)
                .read_until(b'\n', &mut bytes);
            match result {
                Ok(0) => break,
                Ok(_) if bytes.len() <= 1024 * 1024 => {
                    let Ok(s) = String::from_utf8(bytes) else {
                        let _ = tx.send(Output::Invalid);
                        break;
                    };
                    if tx.send(Output::Line(s)).is_err() {
                        return;
                    }
                }
                _ => {
                    let _ = tx.send(Output::Invalid);
                    break;
                }
            }
        }
        let _ = tx.send(Output::Done);
    });
    rx
}

struct Guard {
    child: Child,
    armed: bool,
}
impl Drop for Guard {
    fn drop(&mut self) {
        if self.armed {
            cleanup(self.child.id() as i32);
            let _ = self.child.wait();
        }
    }
}

fn send(input: &mut Option<std::process::ChildStdin>, value: &serde_json::Value) -> Result<()> {
    let input = input.as_mut().context("harness input already closed")?;
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    input.write_all(&bytes)?;
    input.flush()?;
    Ok(())
}

fn terminal_failure(
    ledger: &Ledger,
    job: &ResolvedJob,
    initial: &Record,
    reason: String,
    output: &mut RunOutput,
) -> Result<Status> {
    output.record(&serde_json::json!({"type":"cones_error","message":reason}))?;
    output.sync()?;
    ledger.append(initial)?;
    let mut terminal = Record::new(initial.run_id.clone(), Status::Failed);
    terminal.ended_at = Some(Utc::now());
    terminal.duration_s = Some(0.0);
    terminal.reason = Some(reason);
    ledger.append(&terminal)?;
    notify(job, Status::Failed, terminal.reason.as_deref());
    let _ = writeln!(
        std::io::stdout(),
        "{}\tfailed\t{}",
        terminal.run_id,
        terminal.reason.as_deref().unwrap_or("")
    );
    Ok(Status::Failed)
}

pub fn run(job: &ResolvedJob, ledger: &Ledger, executable: &Path, trigger: &str) -> Result<Status> {
    let run_id = uuid::Uuid::new_v4().to_string();
    uuid::Uuid::parse_str(&run_id)?;
    if !job.enabled {
        return skipped(ledger, job, trigger, "disabled", &run_id);
    }
    let admission = ledger.admission_lock(&job.name)?;
    for run in job_runs(ledger, &job.name)? {
        if reap_run(ledger, &run, false)? {
            notify(job, Status::Failed, Some("orphan"));
        }
    }
    let previous = job_runs(ledger, &job.name)?;
    if job.overlap == Overlap::Skip && !previous.is_empty() {
        return skipped(ledger, job, trigger, "overlap", &run_id);
    }
    if job.overlap == Overlap::Replace
        && let Err(e) = replace_runs(ledger, &previous)
    {
        eprintln!("cones: {e:#}");
        return skipped(ledger, job, trigger, "replace_unconfirmed", &run_id);
    }
    let _run_lease = ledger
        .run_lock(&run_id)?
        .context("run ID is already active")?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let mut output = RunOutput::new(&ledger.state, &run_id)?;
    let mut initial = Record::new(run_id.clone(), Status::Started);
    initial.job = Some(job.name.clone());
    initial.trigger = Some(trigger.into());
    initial.fired_at = Some(Utc::now());
    initial.harness = Some(job.harness);
    initial.session_id = Some(session_id.clone());
    initial.cwd = Some(job.cwd.clone());
    initial.pid = Some(std::process::id());
    initial.owns_run_lock = Some(true);
    initial.timeout_s = Some(job.timeout_min * 60.0);
    initial.output = Some(output.events_path.clone());
    initial.stderr = Some(output.stderr_path.clone());
    initial.attach_mode = Some("events".into());
    let prepared = (|| -> Result<_> {
        let harness = harness::adapter(job.harness)?;
        let invocation = harness.compile(job, &session_id)?;
        Ok((harness, invocation))
    })();
    let (harness, invocation) = match prepared {
        Ok(p) => p,
        Err(e) => {
            return terminal_failure(
                ledger,
                job,
                &initial,
                format!("validation: {e:#}"),
                &mut output,
            );
        }
    };
    initial.policy_hash = Some(harness::policy_hash(job, &invocation)?);
    initial.policy = Some(harness::compiled_policy(job, &invocation)?);
    let child = match spawn_worker(executable, &run_id) {
        Ok(child) => child,
        Err(e) => {
            return terminal_failure(ledger, job, &initial, format!("spawn: {e:#}"), &mut output);
        }
    };
    let mut guard = Guard { child, armed: true };
    let pgid = guard.child.id() as i32;
    initial.pgid = Some(pgid);
    let signals = Signals::new()?;
    ledger.append(&initial)?;
    drop(admission);
    let _ = writeln!(std::io::stdout(), "{run_id}\tstarted\t{}", job.name);
    let _ = std::io::stdout().flush();
    let start = Instant::now();
    let mut terminal = Record::new(run_id.clone(), Status::Failed);
    let mut outcome = Outcome::default();
    let mut input = guard.child.stdin.take();
    let result = (|| -> Result<()> {
        // Release the local execution gate only after the durable start record.
        send(&mut input, &serde_json::to_value(&invocation)?)?;
        let events = read_output(guard.child.stdout.take().context("missing worker stdout")?);
        output.capture_stderr(guard.child.stderr.take().context("missing worker stderr")?);
        input.take();
        let mut done = false;
        let mut exited = None;
        loop {
            if signals.replaced.load(Ordering::Relaxed) {
                terminal.status = Status::Timeout;
                terminal.reason = Some("replaced".into());
                break;
            }
            if signals.cancelled.load(Ordering::Relaxed) {
                terminal.reason = Some("interrupted".into());
                break;
            }
            if start.elapsed().as_secs_f64() >= invocation.timeout_s {
                terminal.status = Status::Timeout;
                terminal.reason = Some("timeout".into());
                break;
            }
            match events.recv_timeout(Duration::from_millis(25)) {
                Ok(Output::Line(line)) if !line.trim().is_empty() => {
                    let event: serde_json::Value =
                        serde_json::from_str(&line).context("invalid harness event")?;
                    ensure!(event.is_object(), "harness event must be an object");
                    output.record(&event)?;
                    outcome.observe(&line, &session_id)?;
                }
                Ok(Output::Invalid) => anyhow::bail!("invalid or oversized harness event"),
                Ok(Output::Done) => done = true,
                Err(mpsc::RecvTimeoutError::Disconnected) => done = true,
                _ => {}
            }
            if outcome.session_mismatch {
                terminal.reason = Some("session_mismatch".into());
                break;
            }
            if outcome.permission_denied {
                terminal.reason = Some("permission".into());
                break;
            }
            if exited.is_none() {
                exited = guard.child.try_wait()?;
            }
            if done && let Some(exit) = exited {
                terminal.exit = exit.code().or_else(|| exit.signal().map(|s| 128 + s));
                if exit.success() && outcome.result_seen && !outcome.failed {
                    terminal.status = Status::Ok;
                } else {
                    terminal.reason = Some(outcome.reason.clone().unwrap_or_else(|| {
                        if !outcome.result_seen {
                            "missing_result".into()
                        } else {
                            "exit".into()
                        }
                    }));
                }
                break;
            }
            if done {
                thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    })();
    input.take();
    // Keep handlers installed through cleanup so another interrupt cannot skip the terminal record.
    cleanup(pgid);
    let wait = guard.child.wait();
    if let Ok(status) = wait {
        guard.armed = false;
        terminal.exit = terminal
            .exit
            .or(status.code())
            .or_else(|| status.signal().map(|s| 128 + s));
    }
    if let Err(e) = result {
        terminal.status = Status::Failed;
        terminal.reason = Some(format!("runner: {e:#}"));
        let _ =
            output.record(&serde_json::json!({"type":"cones_error","message":format!("{e:#}")}));
    }
    terminal.tokens_in = outcome.tokens_in;
    terminal.tokens_out = outcome.tokens_out;
    terminal.cost_usd = outcome.cost_usd;
    if job.archive_transcript {
        match archive(ledger, &run_id, &session_id, &job.cwd, harness.as_ref()) {
            Ok(path) => terminal.transcript = Some(path),
            Err(e) => terminal.archive_error = Some(e.to_string()),
        }
    }
    output.sync()?;
    terminal.ended_at = Some(Utc::now());
    terminal.duration_s = Some(start.elapsed().as_secs_f64());
    ledger.append(&terminal)?;
    notify(job, terminal.status, terminal.reason.as_deref());
    let _ = writeln!(
        std::io::stdout(),
        "{run_id}\t{}\t{}",
        terminal.status,
        terminal.reason.as_deref().unwrap_or("")
    );
    Ok(terminal.status)
}

pub fn stop(ledger: &Ledger, claude: &Path, id: &str) -> Result<bool> {
    let Ok(run) = ledger.resolve(id) else {
        return crate::fleet::stop(claude, id);
    };
    if run.terminal.is_some() || run.started.status == Status::Skipped {
        // A finished run can leave the client it claimed alive and listed: Claude hands a
        // headless run a background spare that outlives the run. Remove that session rather
        // than report a row nothing can clear. An id the harness no longer lists is gone.
        return match crate::fleet::control_session(claude, id)? {
            Some(_) => crate::fleet::stop(claude, id),
            None => Ok(false),
        };
    }
    signal_run(&run.started, SIGTERM)?;
    Ok(true)
}

/// Run the worker in a separate process group; receive its invocation over stdin.
pub fn spawn_worker(executable: &Path, run_id: &str) -> Result<Child> {
    Ok(Command::new(executable)
        .args(["__worker", "--run-id", run_id])
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?)
}

pub fn resume_finished(run: &Run, harness: &dyn harness::Harness) -> Result<Command> {
    // A skip has no terminal record either, and calling it active would be a lie: admission
    // refused it, so no session was ever started. `stop` already reads the two apart.
    ensure!(
        run.started.status != Status::Skipped,
        "run was skipped ({}) and never started a session",
        run.started
            .reason
            .as_deref()
            .unwrap_or("no reason recorded")
    );
    ensure!(
        run.terminal.is_some(),
        "run is still active; headless runs can be resumed after they finish"
    );
    harness.resume(
        run.started
            .session_id
            .as_deref()
            .context("run has no session ID")?,
        run.started
            .cwd
            .as_deref()
            .context("run has no working directory")?,
    )
}

fn archive(
    ledger: &Ledger,
    run_id: &str,
    session_id: &str,
    cwd: &Path,
    harness: &dyn harness::Harness,
) -> Result<std::path::PathBuf> {
    let source = harness.transcript(session_id, cwd)?;
    let meta = fs::symlink_metadata(&source)
        .with_context(|| format!("transcript unavailable: {}", source.display()))?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "transcript must be a regular file"
    );
    let root = ledger.state.join("transcripts");
    private_dir(&root)?;
    let dir = root.join(run_id);
    private_dir(&dir)?;
    let dest = dir.join(format!("{session_id}.jsonl"));
    let mut output = private_file(&dest)?;
    let mut input = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&source)?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    output.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(dest)
}

pub fn worker(run_id: &str) -> Result<i32> {
    uuid::Uuid::parse_str(run_id)?;
    let mut gate = BufReader::new(std::io::stdin());
    let mut bytes = Vec::new();
    gate.by_ref()
        .take(4 * 1024 * 1024 + 1)
        .read_until(b'\n', &mut bytes)?;
    ensure!(
        bytes.len() <= 4 * 1024 * 1024 && bytes.ends_with(b"\n"),
        "worker received no complete execution gate"
    );
    let invocation: Invocation =
        serde_json::from_slice(&bytes).context("worker received no complete execution gate")?;
    ensure!(
        invocation.timeout_s.is_finite() && invocation.timeout_s > 0.0,
        "invalid worker timeout"
    );
    let pgid = unsafe { libc::getpgrp() };
    ensure!(
        pgid == std::process::id() as i32,
        "worker must lead its own process group"
    );
    let cancelled = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGTERM, Arc::clone(&cancelled))?;
    signal_hook::flag::register(SIGINT, Arc::clone(&cancelled))?;
    let parent = unsafe { libc::getppid() };
    let mut child = Command::new(&invocation.program)
        .args(&invocation.args)
        .env_clear()
        .envs(&invocation.env)
        .current_dir(&invocation.cwd)
        .stdin(Stdio::null())
        .spawn()
        .context("spawn harness")?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status
                .code()
                .or_else(|| status.signal().map(|s| 128 + s))
                .unwrap_or(1));
        }
        if cancelled.load(Ordering::Relaxed)
            || unsafe { libc::getppid() } != parent
            || start.elapsed().as_secs_f64() > invocation.timeout_s + 1.0
        {
            signal_group(pgid, libc::SIGTERM);
            thread::sleep(GRACE);
            signal_group(pgid, libc::SIGKILL);
            return Ok(124);
        }
        thread::sleep(Duration::from_millis(25));
    }
}
