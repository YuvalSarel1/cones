use cones::ledger::{Ledger, Status};
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    jobs: PathBuf,
    state: PathBuf,
}
impl Fixture {
    fn new(mode: &str, timeout: f64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("claude");
        fs::write(&fake, include_bytes!("fixtures/fake_claude.py")).unwrap();
        fs::write(
            bin.join("claude-read-permissions.jsonl"),
            include_bytes!("fixtures/claude-read-permissions.jsonl"),
        )
        .unwrap();
        fs::set_permissions(fake, fs::Permissions::from_mode(0o700)).unwrap();
        let state = dir.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let jobs = dir.path().join("jobs.yaml");
        fs::write(&jobs,format!("version: 1\njobs:\n  - name: test\n    schedule: '* * * * *'\n    harness: claude\n    cwd: .\n    prompt: test\n    model: {mode}\n    timeout_min: {timeout}\n    budget_usd: 0.1\n    archive_transcript: true\n    env: [FAKE_LEDGER, FAKE_CHILD_PID]\n")).unwrap();
        Self { dir, jobs, state }
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_cones"));
        c.env("HOME", self.dir.path())
            .env("FAKE_LEDGER", self.state.join("runs.jsonl"))
            .env("FAKE_CHILD_PID", self.state.join("child.pid"))
            .args([
                "--jobs",
                self.jobs.to_str().unwrap(),
                "--state-dir",
                self.state.to_str().unwrap(),
            ]);
        c
    }
    fn ledger(&self) -> Ledger {
        Ledger::new(&self.state).unwrap()
    }
    fn output(&self) -> std::process::Output {
        self.command().args(["run", "test"]).output().unwrap()
    }
    fn add_options(&self, options: &str) {
        let text = fs::read_to_string(&self.jobs).unwrap();
        fs::write(&self.jobs, format!("{text}{options}")).unwrap();
    }
    fn start(&self) -> (std::process::Child, String) {
        let mut child = self
            .command()
            .args(["run", "test"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.contains("\tstarted\t"), "{line}");
        (child, line.split('\t').next().unwrap().to_owned())
    }
    fn stop(&self, id: &str, child: &mut std::process::Child) {
        let result = self.command().args(["stop", id]).output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(child.wait().unwrap().code(), Some(1));
    }
}

#[test]
fn durable_start_success_cost_and_archived_native_resume() {
    let f = Fixture::new("success", 1.0);
    let output = f.output();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let runs = f.ledger().runs().unwrap();
    assert_eq!(runs.len(), 1);
    let r = &runs[0];
    assert_eq!(r.terminal.as_ref().unwrap().status, Status::Ok);
    assert_eq!(r.terminal.as_ref().unwrap().cost_usd, Some(0.01));
    let archive = r.terminal.as_ref().unwrap().transcript.as_ref().unwrap();
    assert!(archive.is_file());
    assert_eq!(
        fs::metadata(archive).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let attach = f
        .command()
        .args(["attach", &r.started.run_id, "--print-command"])
        .output()
        .unwrap();
    assert!(
        attach.status.success(),
        "{}",
        String::from_utf8_lossy(&attach.stderr)
    );
    let printed = String::from_utf8_lossy(&attach.stdout);
    assert!(
        printed.contains(r.started.session_id.as_ref().unwrap())
            && printed.contains("--bg --resume")
            && printed.contains("attach"),
        "a finished session resumes in the background and is attached: {printed}"
    );
    assert_eq!(
        fs::read_to_string(f.state.join("runs.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}
#[test]
fn policy_denial_stops_a_running_harness_promptly() {
    for mode in ["permission", "sandbox"] {
        let f = Fixture::new(mode, 1.0);
        let start = Instant::now();
        assert!(!f.output().status.success());
        assert!(start.elapsed() < Duration::from_secs(6));
        let r = f.ledger().runs().unwrap().remove(0).terminal.unwrap();
        assert_eq!(r.status, Status::Failed);
        assert_eq!(r.reason.as_deref(), Some("permission"));
    }
}
#[test]
fn timeout_kills_descendants_that_ignore_sigterm() {
    let f = Fixture::new("hang", 0.1);
    assert_eq!(f.output().status.code(), Some(124));
    let r = f.ledger().runs().unwrap().remove(0);
    assert_eq!(r.terminal.unwrap().status, Status::Timeout);
    assert_dead(f.state.join("child.pid"));
}

#[test]
fn successful_read_of_permission_documentation_finishes_ok() {
    let f = Fixture::new("read-permissions", 1.0);
    assert!(f.output().status.success());
    let run = f.ledger().runs().unwrap().remove(0);
    assert_eq!(run.terminal.unwrap().status, Status::Ok);
    let logs = f
        .command()
        .args(["logs", &run.started.run_id])
        .output()
        .unwrap();
    assert!(logs.status.success());
    assert!(String::from_utf8_lossy(&logs.stdout).contains("permission denied"));
}

#[test]
fn following_output_can_detach_without_stopping_the_job() {
    let f = Fixture::new("hang", 0.5);
    let (mut job, id) = f.start();
    let mut follower = f
        .command()
        .args(["logs", &id, "--follow", "--raw"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    let mut reader = BufReader::new(follower.stdout.take().unwrap());
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("\"type\":\"system\""));
    unsafe {
        libc::kill(follower.id() as i32, libc::SIGINT);
    }
    assert!(follower.wait().unwrap().success());
    assert!(job.try_wait().unwrap().is_none());
    f.stop(&id, &mut job);
    assert_eq!(
        f.ledger()
            .resolve(&id)
            .unwrap()
            .terminal
            .unwrap()
            .reason
            .as_deref(),
        Some("interrupted")
    );
}

#[test]
fn read_only_allow_runs_can_overlap_but_reservations_are_atomic() {
    let f = Fixture::new("hang", 0.5);
    f.add_options("    overlap: allow\n    daily_budget_usd: 0.2\n");
    let (mut first, a) = f.start();
    let (mut second, b) = f.start();
    assert!(first.try_wait().unwrap().is_none() && second.try_wait().unwrap().is_none());
    assert!(f.output().status.success());
    let runs = f.ledger().runs().unwrap();
    assert_eq!(
        runs.iter()
            .filter(|r| r.started.status == Status::Started)
            .count(),
        2
    );
    assert_eq!(
        runs.last().unwrap().started.reason.as_deref(),
        Some("budget")
    );
    f.stop(&a, &mut first);
    f.stop(&b, &mut second);
}

#[test]
fn replace_waits_for_the_old_run_and_records_why_it_ended() {
    let f = Fixture::new("hang", 0.5);
    f.add_options("    overlap: replace\n");
    let (mut first, a) = f.start();
    let (mut second, b) = f.start();
    assert_eq!(first.wait().unwrap().code(), Some(124));
    let old = f.ledger().resolve(&a).unwrap();
    assert_eq!(old.terminal.as_ref().unwrap().status, Status::Timeout);
    assert_eq!(
        old.terminal.as_ref().unwrap().reason.as_deref(),
        Some("replaced")
    );
    let new = f.ledger().resolve(&b).unwrap();
    assert!(new.started.fired_at >= old.terminal.unwrap().ended_at);
    f.stop(&b, &mut second);
}

#[test]
fn different_writer_jobs_share_the_workspace_lock_even_with_replace() {
    let f = Fixture::new("hang", 0.5);
    f.add_options("    write: true\n");
    let text = fs::read_to_string(&f.jobs).unwrap();
    let other = text
        .split_once("jobs:\n")
        .unwrap()
        .1
        .replace("name: test", "name: other");
    fs::write(&f.jobs, format!("{text}{other}    overlap: replace\n")).unwrap();
    let (mut first, id) = f.start();
    let result = f.command().args(["run", "other"]).output().unwrap();
    assert!(result.status.success());
    assert!(first.try_wait().unwrap().is_none());
    assert_eq!(
        f.ledger()
            .runs()
            .unwrap()
            .last()
            .unwrap()
            .started
            .reason
            .as_deref(),
        Some("workspace")
    );
    f.stop(&id, &mut first);
}

#[test]
fn a_new_writer_reaps_a_different_jobs_orphan_before_starting() {
    let f = Fixture::new("hang", 0.5);
    f.add_options("    write: true\n");
    let text = fs::read_to_string(&f.jobs).unwrap();
    let other = text
        .split_once("jobs:\n")
        .unwrap()
        .1
        .replace("name: test", "name: other")
        .replace("model: hang", "model: success");
    fs::write(&f.jobs, format!("{text}{other}")).unwrap();
    let (mut first, id) = f.start();
    let until = Instant::now() + Duration::from_secs(8);
    while !f.state.join("child.pid").exists() {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(20));
    }
    first.kill().unwrap();
    first.wait().unwrap();
    let result = f.command().args(["run", "other"]).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        f.ledger()
            .resolve(&id)
            .unwrap()
            .terminal
            .unwrap()
            .reason
            .as_deref(),
        Some("orphan")
    );
    assert_dead(f.state.join("child.pid"));
}

#[test]
fn dry_run_warns_on_stderr_before_printing_injected_environment() {
    let f = Fixture::new("success", 1.0);
    let result = f.command().args(["install", "--dry-run"]).output().unwrap();
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("may contain secrets"));
    assert!(String::from_utf8_lossy(&result.stdout).starts_with("<?xml"));
}
#[test]
fn successful_harness_cannot_leave_a_shell_running() {
    let f = Fixture::new("descendant", 1.0);
    assert!(f.output().status.success());
    assert_dead(f.state.join("child.pid"));
}
fn assert_dead(file: PathBuf) {
    let pid = fs::read_to_string(file).unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let output = Command::new("/bin/ps")
            .args(["-p", pid.trim(), "-o", "stat="])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&output.stdout);
        if state.trim().is_empty() || state.trim().starts_with('Z') {
            break;
        }
        assert!(
            Instant::now() < until,
            "descendant {pid} still alive: {state}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}
#[test]
fn overlapping_ticks_produce_one_run_and_one_skip() {
    let f = Fixture::new("slow", 1.0);
    let mut first = f
        .command()
        .args(["run", "test"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut start = String::new();
    BufReader::new(first.stdout.take().unwrap())
        .read_line(&mut start)
        .unwrap();
    assert!(start.contains("started"));
    assert!(f.output().status.success());
    assert!(first.wait().unwrap().success());
    let runs = f.ledger().runs().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(
        runs.iter()
            .filter(|r| r.started.status == Status::Started)
            .count(),
        1
    );
    assert_eq!(
        runs.iter()
            .filter(|r| r.started.reason.as_deref() == Some("overlap"))
            .count(),
        1
    );
}
#[test]
fn malformed_missing_and_mismatched_events_fail_closed() {
    for mode in ["malformed", "missing", "mismatch", "failed", "oversized"] {
        let f = Fixture::new(mode, 1.0);
        assert!(!f.output().status.success(), "{mode}");
        let r = f.ledger().runs().unwrap().remove(0);
        assert_eq!(r.terminal.unwrap().status, Status::Failed, "{mode}");
    }
}
#[test]
fn killed_runner_leaves_one_orphan_and_next_tick_reaps_it() {
    let f = Fixture::new("hang", 0.5);
    let mut child = f
        .command()
        .args(["run", "test"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while !f.state.join("child.pid").exists() {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(20));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(f.ledger().runs().unwrap()[0].terminal.is_none());
    thread::sleep(Duration::from_secs(3));
    let text = fs::read_to_string(&f.jobs)
        .unwrap()
        .replace("model: hang", "model: success");
    fs::write(&f.jobs, text).unwrap();
    let out = f.output();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let runs = f.ledger().runs().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(
        runs[0].terminal.as_ref().unwrap().reason.as_deref(),
        Some("orphan")
    );
    assert_eq!(runs[1].terminal.as_ref().unwrap().status, Status::Ok);
    assert_dead(f.state.join("child.pid"));
}
#[test]
fn exhausted_daily_reservation_skips_without_spawning() {
    let f = Fixture::new("success", 1.0);
    let text = fs::read_to_string(&f.jobs).unwrap().replace(
        "budget_usd: 0.1",
        "budget_usd: 0.1\n    daily_budget_usd: 0.1",
    );
    fs::write(&f.jobs, text).unwrap();
    assert!(f.output().status.success());
    assert!(f.output().status.success());
    let runs = f.ledger().runs().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].started.reason.as_deref(), Some("budget"));
}
#[test]
fn notify_fires_only_when_opted_in_on_failure_and_budget_skip() {
    let f = Fixture::new("failed", 1.0);
    let log = f.dir.path().join("notified");
    let notifier = f.dir.path().join("notifier.sh");
    fs::write(
        &notifier,
        format!("#!/bin/sh\necho \"$@\" >> {}\n", log.display()),
    )
    .unwrap();
    fs::set_permissions(&notifier, fs::Permissions::from_mode(0o700)).unwrap();
    let run = |f: &Fixture| {
        f.command()
            .env("CONES_NOTIFIER", &notifier)
            .args(["run", "test"])
            .output()
            .unwrap()
    };
    run(&f);
    assert!(!log.exists(), "notify defaults to off");
    f.add_options("    notify: true\n");
    run(&f);
    f.add_options("    daily_budget_usd: 0.2\n");
    run(&f);
    let lines = fs::read_to_string(&log).unwrap();
    assert_eq!(
        lines.lines().collect::<Vec<_>>(),
        [
            "cones test failed: error_during_execution",
            "cones test skipped: budget"
        ]
    );
}
#[test]
fn stopping_a_fleet_session_signals_only_a_verified_harness_process() {
    let f = Fixture::new("success", 1.0);
    let fake = f.dir.path().join("claude");
    // ps reports argv[0]; a copied sleep binary named claude looks like the real harness.
    fs::copy("/bin/sleep", &fake).unwrap();
    let mut claude = Command::new(&fake).arg("30").spawn().unwrap();
    let mut sleeper = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let session = |id: &str, pid: u32| cones::fleet::Session {
        v: 1,
        session_id: id.into(),
        harness: "claude".into(),
        cwd: f.dir.path().into(),
        state: "idle".into(),
        updated: chrono::Utc::now(),
        event: None,
        tool: None,
        pid: Some(pid),
        transcript_path: None,
        tokens_in: None,
        tokens_out: None,
        cost_usd: None,
        title: None,
        last: None,
    };
    cones::fleet::write(&f.state, &session("real", claude.id())).unwrap();
    cones::fleet::write(&f.state, &session("reused", sleeper.id())).unwrap();
    let ledger = f.ledger();
    assert!(
        cones::runner::stop(&ledger, "reused")
            .unwrap_err()
            .to_string()
            .contains("refusing")
    );
    assert!(cones::runner::stop(&ledger, "missing").is_err());
    assert!(cones::runner::stop(&ledger, "real").unwrap());
    let until = Instant::now() + Duration::from_secs(3);
    while claude.try_wait().unwrap().is_none() && Instant::now() < until {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(claude.try_wait().unwrap().is_some(), "harness kept running");
    assert!(sleeper.try_wait().unwrap().is_none());
    sleeper.kill().unwrap();
    // A pid that is already gone is "already finished", not an error.
    assert!(!cones::runner::stop(&ledger, "real").unwrap());
}
#[test]
fn version_flag_prints_the_crate_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_cones"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!env!("CARGO_PKG_VERSION").is_empty());
    let expected = concat!("cones ", env!("CARGO_PKG_VERSION"));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), expected);
}
