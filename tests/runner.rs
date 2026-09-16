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
        fs::write(&jobs,format!("version: 1\njobs:\n  - name: test\n    schedule: '* * * * *'\n    harness: claude\n    cwd: .\n    prompt: test\n    model: {mode}\n    timeout_min: {timeout}\n    archive_transcript: true\n    env: [FAKE_LEDGER, FAKE_CHILD_PID]\n")).unwrap();
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
// Timing-sensitive: it has flaked when other cargo test runs shared the machine and passed
// alone; rerun it alone before blaming a change.
#[test]
fn policy_denial_stops_a_running_harness_promptly() {
    let f = Fixture::new("permission", 1.0);
    let start = Instant::now();
    assert!(!f.output().status.success());
    assert!(start.elapsed() < Duration::from_secs(6));
    let r = f.ledger().runs().unwrap().remove(0).terminal.unwrap();
    assert_eq!(r.status, Status::Failed);
    assert_eq!(r.reason.as_deref(), Some("permission"));
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
fn read_only_allow_runs_can_overlap() {
    let f = Fixture::new("hang", 0.5);
    f.add_options("    overlap: allow\n");
    let (mut first, a) = f.start();
    let (mut second, b) = f.start();
    assert!(first.try_wait().unwrap().is_none() && second.try_wait().unwrap().is_none());
    let runs = f.ledger().runs().unwrap();
    assert_eq!(
        runs.iter()
            .filter(|r| r.started.status == Status::Started)
            .count(),
        2
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
fn two_writer_jobs_in_one_directory_both_run() {
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
    let result = f.command().args(["run", "other"]).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(first.try_wait().unwrap().is_none());
    let other = f.ledger().runs().unwrap().pop().unwrap();
    assert_eq!(other.started.job.as_deref(), Some("other"));
    assert_eq!(other.terminal.unwrap().status, Status::Ok);
    f.stop(&id, &mut first);
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
fn attaching_a_skipped_run_names_the_skip_rather_than_calling_it_active() {
    let f = Fixture::new("success", 1.0);
    f.add_options("    enabled: false\n");
    assert!(f.output().status.success());
    let runs = f.ledger().runs().unwrap();
    assert_eq!(runs[0].started.status, Status::Skipped);
    let out = f
        .command()
        .args(["attach", &runs[0].started.run_id])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("skipped (disabled)"), "{err}");
    assert!(!err.contains("still active"), "{err}");
}
#[test]
fn notify_fires_only_when_opted_in_on_failure() {
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
    let lines = fs::read_to_string(&log).unwrap();
    assert_eq!(
        lines.lines().collect::<Vec<_>>(),
        ["cones test failed: error_during_execution"]
    );
}
// Build a fixture binary: macOS kills renamed copies of signed system binaries
// before their process identity can be checked.
#[test]
fn stopping_a_fleet_session_signals_only_a_verified_harness_process() {
    let f = Fixture::new("success", 1.0);
    let fake = f.dir.path().join("claude");
    // Name the fixture claude so `ps` reports the expected harness.
    fs::copy("/bin/sleep", &fake).unwrap();
    let mut claude_proc = Command::new(&fake).arg("30").spawn().unwrap();
    let mut sleeper = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let claude = f.dir.path().join("dot-claude");
    fs::create_dir_all(claude.join("sessions")).unwrap();
    for (id, pid) in [("real", claude_proc.id()), ("reused", sleeper.id())] {
        fs::write(
            claude.join("sessions").join(format!("{id}.json")),
            serde_json::json!({"pid": pid, "sessionId": id, "cwd": f.dir.path(), "kind": "interactive", "status": "idle", "startedAt": 1i64}).to_string(),
        )
        .unwrap();
    }
    let ledger = f.ledger();
    assert!(
        cones::runner::stop(&ledger, &claude, "reused")
            .unwrap_err()
            .to_string()
            .contains("refusing")
    );
    assert!(cones::runner::stop(&ledger, &claude, "missing").is_err());
    assert!(cones::runner::stop(&ledger, &claude, "real").unwrap());
    let until = Instant::now() + Duration::from_secs(3);
    while claude_proc.try_wait().unwrap().is_none() && Instant::now() < until {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        claude_proc.try_wait().unwrap().is_some(),
        "harness kept running"
    );
    assert!(sleeper.try_wait().unwrap().is_none());
    sleeper.kill().unwrap();
    assert!(cones::runner::stop(&ledger, &claude, "real").is_err());
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

#[test]
fn coordinator_start_is_a_no_op_while_the_folder_has_a_live_coordinator() {
    let f = Fixture::new("success", 1.0);
    let dir = f.dir.path().canonicalize().unwrap();
    let status = f.dir.path().join(".claude/orchestrator");
    fs::create_dir_all(&status).unwrap();
    let live = serde_json::json!({"cwd": dir, "pid": std::process::id(), "jobId": "8077985c"});
    fs::write(status.join("x.json"), live.to_string()).unwrap();
    let out = f
        .command()
        .args(["coordinator", "start", dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("already running"));
    assert!(!f.state.join("coordinator").exists());
}
