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

struct OwnedChild(std::process::Child);

impl std::ops::Deref for OwnedChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

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
    fn start(&self) -> (OwnedChild, String) {
        let mut child = OwnedChild(
            self.command()
                .args(["run", "test"])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.contains("\tstarted\t"), "{line}");
        (child, line.split('\t').next().unwrap().to_owned())
    }
    fn stop(&self, id: &str, child: &mut std::process::Child) {
        // The dashboard stops a run through the library, which is now the only way in.
        let stopped =
            cones::runner::stop(&self.ledger(), &self.dir.path().join(".claude"), id).unwrap();
        assert!(stopped, "the run was already finished");
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
        .args(["__attach", &r.started.run_id, "--print-command"])
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
            && printed.contains("'--bg' '--resume'")
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
fn launch_prints_the_composer_command_for_a_named_folder() {
    let f = Fixture::new("ok", 5.0);
    let folder = f.dir.path().join("project");
    fs::create_dir_all(&folder).unwrap();
    let out = f
        .command()
        .args([
            "launch",
            "--harness",
            "claude",
            "--dir",
            folder.to_str().unwrap(),
            "--print-command",
            "fix the tests",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed = String::from_utf8_lossy(&out.stdout);
    let canonical = folder.canonicalize().unwrap();
    assert!(
        printed.contains(&format!("cd '{}'", canonical.display()))
            && printed.contains(".local/bin/claude'")
            && printed.contains("'--bg'")
            && printed.contains("'--' 'fix the tests'"),
        "the launcher starts the composer's own background command: {printed}"
    );
}

#[test]
fn launch_puts_the_model_and_effort_on_the_native_command() {
    let f = Fixture::new("ok", 5.0);
    let out = f
        .command()
        .args([
            "launch",
            "--harness",
            "claude",
            "--dir",
            f.dir.path().to_str().unwrap(),
            "--model",
            "opus",
            "--effort",
            "high",
            "--print-command",
            "hi",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(
        printed.contains("'--model' 'opus'") && printed.contains("'--effort' 'high'"),
        "the launcher passes the flags ctrl+o sets: {printed}"
    );
}

#[test]
fn launch_refuses_an_effort_the_harness_has_no_flag_for() {
    let f = Fixture::new("ok", 5.0);
    let out = f
        .command()
        .args([
            "launch",
            "--harness",
            "codex",
            "--dir",
            f.dir.path().to_str().unwrap(),
            "--effort",
            "high",
            "--print-command",
            "hi",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("takes no --effort"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn launch_requires_a_folder_rather_than_taking_the_current_one() {
    let f = Fixture::new("ok", 5.0);
    let out = f
        .command()
        .args(["launch", "--harness", "claude", "--print-command", "hi"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--dir"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn launch_refuses_a_harness_the_configuration_turns_off() {
    let f = Fixture::new("ok", 5.0);
    fs::write(
        &f.jobs,
        format!(
            "{}defaults:\n  claude_enabled: false\n",
            fs::read_to_string(&f.jobs).unwrap()
        ),
    )
    .unwrap();
    let out = f
        .command()
        .args([
            "launch",
            "--harness",
            "claude",
            "--dir",
            f.dir.path().to_str().unwrap(),
            "--print-command",
            "hi",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("turned off"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// Timing-sensitive: it has flaked when other cargo test runs shared the machine and passed
// alone; rerun it alone before blaming a change.
#[test]
fn a_session_the_harness_reports_failed_ends_the_run_promptly() {
    let f = Fixture::new("failed", 1.0);
    let start = Instant::now();
    assert!(!f.output().status.success());
    assert!(start.elapsed() < Duration::from_secs(20));
    let r = f.ledger().runs().unwrap().remove(0).terminal.unwrap();
    assert_eq!(r.status, Status::Failed);
    assert_eq!(r.reason.as_deref(), Some("session_failed"));
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
fn a_finished_run_leaves_no_session_behind_and_keeps_its_log() {
    let f = Fixture::new("success", 1.0);
    assert!(f.output().status.success());
    let run = f.ledger().runs().unwrap().remove(0);
    assert_eq!(run.terminal.unwrap().status, Status::Ok);
    // The session the run held is gone: peeking it is for while the run is working.
    let left = cones::fleet::find(
        &f.dir.path().join(".claude"),
        run.started.session_id.as_ref().unwrap(),
    )
    .unwrap();
    assert!(left.is_none(), "{left:?}");
    let logs = f
        .command()
        .args(["__logs", &run.started.run_id])
        .output()
        .unwrap();
    assert!(
        logs.status.success(),
        "{}",
        String::from_utf8_lossy(&logs.stderr)
    );
    let text = String::from_utf8_lossy(&logs.stdout);
    assert!(
        text.contains(&format!(
            "Session: {}",
            run.started.session_id.as_ref().unwrap()
        )),
        "{text}"
    );
    assert!(text.contains("Session done"), "{text}");
}

#[test]
fn following_output_can_detach_without_stopping_the_job() {
    let f = Fixture::new("hang", 0.5);
    let (mut job, id) = f.start();
    let mut follower = OwnedChild(
        f.command()
            .args(["__logs", &id, "--follow", "--raw"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    let mut reader = BufReader::new(follower.stdout.take().unwrap());
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("\"type\":\"cones_launch\""), "{line}");
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
    let result = f
        .command()
        .args(["__install", "--dry-run"])
        .output()
        .unwrap();
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
    let until = Instant::now() + Duration::from_secs(10);
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
fn simultaneous_ticks_admit_one_run_and_record_every_other_skip() {
    let f = Fixture::new("barrier", 1.0);
    let barrier = std::sync::Barrier::new(4);
    let mut children = thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    OwnedChild(
                        f.command()
                            .args(["run", "test"])
                            .stdout(Stdio::null())
                            .spawn()
                            .unwrap(),
                    )
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().unwrap())
            .collect::<Vec<_>>()
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while f.ledger().runs().unwrap().len() != 4 {
        assert!(
            Instant::now() < deadline,
            "contenders did not reach admission"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let runs = f.ledger().runs().unwrap();
    assert_eq!(runs.len(), 4);
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
        3
    );
    fs::write(f.state.join("release"), "").unwrap();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    let runs = f.ledger().runs().unwrap();
    assert_eq!(runs.iter().filter(|r| r.terminal.is_some()).count(), 1);
}

#[test]
fn replacement_waiting_for_its_old_lease_does_not_block_another_job() {
    use fs2::FileExt;
    let f = Fixture::new("success", 1.0);
    f.add_options("    overlap: replace\n");
    let text = fs::read_to_string(&f.jobs).unwrap();
    let other = text
        .split_once("jobs:\n")
        .unwrap()
        .1
        .replace("name: test", "name: other");
    fs::write(&f.jobs, format!("{text}{other}")).unwrap();
    let ledger = f.ledger();
    let old_id = uuid::Uuid::new_v4().to_string();
    let lease = ledger.run_lock(&old_id).unwrap().unwrap();
    let mut old = cones::ledger::Record::new(old_id.clone(), Status::Started);
    old.job = Some("test".into());
    old.owns_run_lock = Some(true);
    old.fired_at = Some(chrono::Utc::now());
    // A supervisor can still own its lease after its worker has gone away.
    ledger.append(&old).unwrap();
    let admission = ledger.admission_lock("test").unwrap();
    FileExt::unlock(&admission).unwrap();
    let mut replacement = OwnedChild(
        f.command()
            .args(["run", "test"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match admission.try_lock_exclusive() {
            Ok(()) => FileExt::unlock(&admission).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("{error}"),
        }
        assert!(
            Instant::now() < deadline,
            "replacement never reached admission"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let mut independent = OwnedChild(
        f.command()
            .args(["run", "other"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    while independent.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "unrelated job blocked behind replacement"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(independent.wait().unwrap().success());
    assert!(replacement.try_wait().unwrap().is_none());
    drop(lease);
    assert!(replacement.wait().unwrap().success());
    assert_eq!(
        ledger
            .resolve(&old_id)
            .unwrap()
            .terminal
            .unwrap()
            .reason
            .as_deref(),
        Some("replaced")
    );
}
#[test]
fn a_launch_cones_cannot_watch_fails_closed() {
    // refused: the harness would not start. unnamed: it started something and named nothing.
    // mismatch: it named a session that is not the run's. missing: it named one and never
    // listed it. failed: the session itself ended badly.
    for mode in ["refused", "unnamed", "mismatch", "missing", "failed"] {
        let f = Fixture::new(mode, 1.0);
        assert!(!f.output().status.success(), "{mode}");
        let r = f.ledger().runs().unwrap().remove(0);
        assert_eq!(r.terminal.unwrap().status, Status::Failed, "{mode}");
    }
}
#[test]
fn killed_runner_leaves_one_orphan_and_next_tick_reaps_it() {
    let f = Fixture::new("hang", 0.5);
    let mut child = OwnedChild(
        f.command()
            .args(["run", "test"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
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
    assert_dead(f.state.join("child.pid"));
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
/// Move every timestamp in the ledger back, standing in for a Mac that was off that long.
fn age_ledger(path: &PathBuf, hours: i64) {
    let text = fs::read_to_string(path).unwrap();
    let aged: Vec<_> = text
        .lines()
        .map(|line| {
            let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
            for key in ["fired_at", "ended_at"] {
                if let Some(at) = record.get(key).and_then(|v| v.as_str()) {
                    let at = chrono::DateTime::parse_from_rfc3339(at).unwrap()
                        - chrono::Duration::hours(hours);
                    record[key] = serde_json::Value::String(at.to_rfc3339());
                }
            }
            record.to_string()
        })
        .collect();
    fs::write(path, format!("{}\n", aged.join("\n"))).unwrap();
}

#[test]
fn catch_up_names_one_missed_tick_and_says_nothing_when_none_was_missed() {
    let f = Fixture::new("success", 1.0);
    f.add_options("    catch_up: once\n");
    let scheduled = f
        .command()
        .args(["run", "test", "--trigger", "schedule"])
        .output()
        .unwrap();
    assert!(scheduled.status.success());
    // The mark is that run, so no tick has passed unattended yet.
    let quiet = f.command().args(["catchup", "--dry-run"]).output().unwrap();
    assert!(quiet.status.success());
    assert_eq!(String::from_utf8_lossy(&quiet.stdout), "");
    // An hour off the mark. The fixture runs every minute, so 60 ticks were lost.
    age_ledger(&f.state.join("runs.jsonl"), 1);
    let out = f.command().args(["catchup", "--dry-run"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.starts_with("test\tmissed\t"), "{text}");
    // `once` means one run however many ticks passed.
    assert_eq!(text.lines().count(), 1, "{text}");
    // A job that never asked for it stays lost.
    let g = Fixture::new("success", 1.0);
    assert!(
        g.command()
            .args(["run", "test", "--trigger", "schedule"])
            .output()
            .unwrap()
            .status
            .success()
    );
    age_ledger(&g.state.join("runs.jsonl"), 1);
    let out = g.command().args(["catchup", "--dry-run"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
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
        .args(["__attach", &runs[0].started.run_id])
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
        ["cones test failed: session_failed"]
    );
}
// macOS can kill a renamed copy of a signed system binary before ps reads it.
fn fixture_client(dir: &std::path::Path) -> PathBuf {
    let source = dir.join("client.c");
    let binary = dir.join("claude");
    fs::write(
        &source,
        "#include <unistd.h>\nint main(void) { sleep(30); return 0; }\n",
    )
    .unwrap();
    let built = Command::new("/usr/bin/cc")
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    binary
}

#[test]
fn stopping_a_fleet_session_signals_only_a_verified_harness_process() {
    let f = Fixture::new("success", 1.0);
    let fake = fixture_client(f.dir.path());
    let mut claude_proc = OwnedChild(Command::new(&fake).arg("30").spawn().unwrap());
    let mut sleeper = OwnedChild(Command::new("/bin/sleep").arg("30").spawn().unwrap());
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
fn a_finished_run_still_removes_the_client_its_session_left_behind() {
    let f = Fixture::new("success", 1.0);
    assert!(f.output().status.success());
    let run = f.ledger().runs().unwrap().remove(0);
    let session = run.started.session_id.unwrap();
    let claude = f.dir.path().join("dot-claude");
    fs::create_dir_all(claude.join("sessions")).unwrap();
    // Nothing lists the session: the finished run has nothing left to clear.
    assert!(!cones::runner::stop(&f.ledger(), &claude, &session).unwrap());
    let fake = fixture_client(f.dir.path());
    let mut client = OwnedChild(Command::new(&fake).arg("30").spawn().unwrap());
    fs::write(
        claude.join("sessions").join("live.json"),
        serde_json::json!({"pid": client.id(), "sessionId": session, "cwd": f.dir.path(), "kind": "interactive", "status": "idle", "startedAt": 1i64}).to_string(),
    )
    .unwrap();
    assert!(
        cones::runner::stop(&f.ledger(), &claude, &session).unwrap(),
        "a live client under a finished run's session id is removable"
    );
    let until = Instant::now() + Duration::from_secs(3);
    while client.try_wait().unwrap().is_none() && Instant::now() < until {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(client.try_wait().unwrap().is_some(), "client kept running");
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
fn coordinator_start_fails_while_the_folder_has_a_live_coordinator() {
    let f = Fixture::new("success", 1.0);
    let dir = f.dir.path().canonicalize().unwrap();
    let claimed = cones::coordinator::directory(&f.state, &dir);
    fs::create_dir_all(&claimed).unwrap();
    let live = serde_json::json!({"cwd": dir, "pid": std::process::id(), "session": "8077985c"});
    fs::write(claimed.join("status.json"), live.to_string()).unwrap();
    let out = f
        .command()
        .args(["coordinator", "--dir", dir.to_str().unwrap(), "start"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a second coordinator must be refused"
    );
    let shown = String::from_utf8_lossy(&out.stderr);
    assert!(shown.contains("already running"), "{shown}");
    assert!(shown.contains("8077985c"), "{shown}");
    // No plugin is written for a folder somebody else is already coordinating.
    assert!(!f.state.join("coordinator/plugin").exists());
}
