//! Detached `cones launch` against disposable homes and fixture launch targets.
//! No model is contacted and no native home outside the fixture is read or written.
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};

/// A fixture HOME with its own native homes, state directory and launch targets.
struct Fixture {
    root: tempfile::TempDir,
    jobs: PathBuf,
    work: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        // Short prefix: the terminal host puts its socket under /tmp and macOS bounds the path.
        let root = tempfile::Builder::new()
            .prefix("cones-launch-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = root.path();
        for dir in ["state", "claude", "pi", "work", ".local/bin"] {
            fs::create_dir_all(path.join(dir)).unwrap();
        }
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_harness.py");
        for name in ["claude", "pi"] {
            let target = path.join(".local/bin").join(name);
            std::os::unix::fs::symlink(&fixture, &target).unwrap();
        }
        let jobs = path.join("jobs.yaml");
        // Only the two harnesses under test are offered, so no other native home is scanned
        // and the machine's own clients cannot reach these assertions.
        fs::write(
            &jobs,
            "version: 4\ndefaults:\n  codex_enabled: false\n  opencode_enabled: false\n  \
             gemini_enabled: false\n  cursor_enabled: false\n  copilot_enabled: false\n  \
             amp_enabled: false\n  droid_enabled: false\n  kimi_enabled: false\njobs: []\n",
        )
        .unwrap();
        let work = path.join("work").canonicalize().unwrap();
        Self { root, jobs, work }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn state(&self) -> PathBuf {
        self.path().join("state")
    }

    fn cones(&self, mode: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cones"))
            .args([
                "--jobs",
                self.jobs.to_str().unwrap(),
                "--state-dir",
                self.state().to_str().unwrap(),
            ])
            .args(args)
            .env("HOME", self.path())
            .env("FAKE_LAUNCH", mode)
            .env("CLAUDE_CONFIG_DIR", self.path().join("claude"))
            .env("PI_CODING_AGENT_DIR", self.path().join("pi"))
            .env("CODEX_HOME", self.path().join("missing-codex"))
            .env_remove("OPENCODE_DB")
            .env_remove("OPENCODE_TUI_CONFIG")
            .output()
            .unwrap()
    }

    fn launch(&self, mode: &str, harness: &str, prompt: &str) -> Output {
        self.cones(
            mode,
            &[
                "launch",
                "--dir",
                self.work.to_str().unwrap(),
                "--harness",
                harness,
                prompt,
            ],
        )
    }

    /// The rows `cones ls` reports for this fixture's folder.
    fn rows(&self) -> Vec<Value> {
        let out = self.cones(
            "ok",
            &["ls", "--dir", self.work.to_str().unwrap(), "--json"],
        );
        assert!(out.status.success(), "{}", stderr(&out));
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn recovery(&self) -> Vec<Value> {
        let path = self.state().join("launches.jsonl");
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// The stand-in daemons behind this fixture's background records.
    fn background_pids(&self) -> Vec<u32> {
        let Ok(entries) = fs::read_dir(self.path().join("claude/sessions")) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| {
                let record: Value = serde_json::from_slice(&fs::read(e.ok()?.path()).ok()?).ok()?;
                record["pid"].as_u64().map(|pid| pid as u32)
            })
            .collect()
    }

    /// Every terminal host this fixture owns, so each test cleans up what it started.
    fn hosted_pids(&self) -> Vec<u32> {
        let Ok(entries) = fs::read_dir(self.state().join("terminals")) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| {
                let path = e.ok()?.path();
                (path.extension()? == "json").then_some(())?;
                let record: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
                record["session"]["pid"].as_u64().map(|pid| pid as u32)
            })
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A detached launch is meant to outlive its launcher, so the test has to end it.
        for pid in self.hosted_pids().into_iter().chain(self.background_pids()) {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_owned()
}

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{what} did not happen within 10s");
}

#[test]
fn a_background_launch_returns_the_exact_session_id_the_roster_carries() {
    let f = Fixture::new();
    let out = f.launch("ok", "claude", "count the flaky tests");
    assert!(out.status.success(), "{}", stderr(&out));
    let id = stdout(&out);

    // Exact, not the eight-character id the harness printed: the whole session id.
    assert_eq!(id.len(), 36, "expected a full session id, got {id:?}");
    assert!(
        uuid::Uuid::parse_str(&id).is_ok(),
        "{id} is not a session id"
    );
    assert!(
        stderr(&out).contains("native session id"),
        "the reply must say which kind of id it returned: {}",
        stderr(&out)
    );

    // Accepted by the command an agent reads the folder with.
    let rows = f.rows();
    let row = rows
        .iter()
        .find(|r| r["session"]["session_id"] == id.as_str())
        .unwrap_or_else(|| panic!("cones ls does not carry {id}: {rows:?}"));
    assert_eq!(row["session"]["harness"], "claude");
    assert_eq!(
        row["session"]["cwd"].as_str().map(PathBuf::from),
        Some(f.work.clone())
    );
}

#[test]
fn recovery_is_written_before_the_launch_and_completed_with_the_resolved_id() {
    let f = Fixture::new();
    let id = stdout(&f.launch("ok", "claude", "fix the parser"));
    let records = f.recovery();
    let submitted = &records[0];
    assert_eq!(submitted["event"], "launch.submitted");
    assert_eq!(submitted["level"], "recovery");
    assert_eq!(submitted["data"]["harness"], "claude");
    assert_eq!(submitted["data"]["prompt"], "fix the parser");
    assert_eq!(
        submitted["data"]["cwd"].as_str().map(PathBuf::from),
        Some(f.work.clone())
    );
    // The submitted record predates any native id, so it carries only cones' own key.
    assert!(submitted["data"]["session_id"].is_null());

    let identified = &records[1];
    assert_eq!(identified["event"], "launch.identified");
    assert_eq!(identified["data"]["session_id"], id.as_str());
    assert_eq!(
        identified["data"]["operation_id"], submitted["data"]["operation_id"],
        "both records must name the same launch"
    );
    assert_eq!(records.len(), 2, "{records:?}");
}

#[test]
fn concurrent_identical_prompts_in_one_folder_get_their_own_identities() {
    let f = Fixture::new();
    let prompt = "the very same instruction";
    let launches: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..3)
            .map(|_| scope.spawn(|| f.launch("ok", "claude", prompt)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut ids: Vec<String> = launches
        .iter()
        .map(|out| {
            assert!(out.status.success(), "{}", stderr(out));
            stdout(out)
        })
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(
        ids.len(),
        3,
        "three launches of one prompt in one folder must be three identities"
    );

    let carried: Vec<String> = f
        .rows()
        .iter()
        .filter_map(|r| r["session"]["session_id"].as_str().map(str::to_owned))
        .collect();
    for id in &ids {
        assert!(carried.contains(id), "cones ls lost {id}: {carried:?}");
    }
}

#[test]
fn a_harness_that_prints_no_identity_is_reported_as_unnamed_rather_than_guessed() {
    let f = Fixture::new();
    let out = f.launch("no-id", "claude", "work with no id");
    assert!(!out.status.success(), "{}", stdout(&out));
    let error = stderr(&out);
    assert!(
        error.contains("no background id") && error.contains("started, somewhere"),
        "the error must quote what the harness printed instead: {error}"
    );
    assert!(stdout(&out).is_empty(), "no id may be printed");

    // The launch happened, so the prompt and folder stay recoverable.
    let records = f.recovery();
    assert_eq!(records[0]["event"], "launch.submitted");
    assert_eq!(records[1]["event"], "launch.unnamed");
    assert!(records[1]["data"]["session_id"].is_null());
    assert!(
        records[1]["data"]["error"]
            .as_str()
            .unwrap()
            .contains("no background id"),
        "{records:?}"
    );
    // A session that started but could not be named is still the user's to find.
    assert_eq!(
        f.rows()
            .iter()
            .filter(|r| r["session"]["harness"] == "claude")
            .count(),
        1,
        "the unnamed session must still be listed"
    );
}

#[test]
fn a_native_launch_failure_is_reported_with_its_status_and_diagnostic() {
    let f = Fixture::new();
    let out = f.launch("fail", "claude", "this will not start");
    assert!(!out.status.success());
    let error = stderr(&out);
    assert!(
        error.contains("exited 3") && error.contains("fixture refused this launch"),
        "{error}"
    );
    assert!(stdout(&out).is_empty());
    assert_eq!(f.recovery()[1]["event"], "launch.unnamed");
}

#[test]
fn a_terminal_harness_keeps_running_after_its_launcher_exits() {
    let f = Fixture::new();
    let out = f.launch("ok", "pi", "hold this terminal");
    assert!(out.status.success(), "{}", stderr(&out));
    let id = stdout(&out);
    assert!(!id.is_empty());

    // The launcher has already returned; the native client belongs to a detached host.
    let hosted = f.hosted_pids();
    assert_eq!(hosted.len(), 1, "one host record per launch: {hosted:?}");
    let pid = hosted[0];
    assert!(alive(pid), "the native client died with its launcher");

    let rows = f.rows();
    let row = rows
        .iter()
        .find(|r| r["session"]["session_id"] == id.as_str())
        .unwrap_or_else(|| panic!("cones ls does not carry {id}: {rows:?}"));
    assert_eq!(row["session"]["harness"], "pi");
    assert_eq!(row["session"]["pid"].as_u64(), Some(u64::from(pid)));

    // Still alive a moment later: nothing about reading the roster ends it.
    assert!(alive(pid));

    // The host's own stop path ends the native process, which is what cleanup relies on.
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    wait_until("the fixture client to exit", || !alive(pid));
}

#[test]
fn two_hosted_launches_in_one_folder_stay_two_sessions() {
    let f = Fixture::new();
    let first = stdout(&f.launch("ok", "pi", "same words"));
    let second = stdout(&f.launch("ok", "pi", "same words"));
    assert_ne!(
        first, second,
        "two hosted clients must not share an identity"
    );
    let hosted = f.hosted_pids();
    assert_eq!(hosted.len(), 2, "{hosted:?}");
    assert_ne!(hosted[0], hosted[1]);

    let carried: Vec<String> = f
        .rows()
        .iter()
        .filter_map(|r| r["session"]["session_id"].as_str().map(str::to_owned))
        .collect();
    for id in [&first, &second] {
        assert!(carried.contains(id), "cones ls lost {id}: {carried:?}");
    }
}

#[test]
fn a_launch_with_no_prompt_detaches_and_is_named_like_any_other() {
    // A worker started with nothing to do is waiting for input, not a failed launch: it is
    // named, listed and left alone.
    for harness in ["claude", "pi"] {
        let f = Fixture::new();
        let out = f.cones(
            "ok",
            &[
                "launch",
                "--dir",
                f.work.to_str().unwrap(),
                "--harness",
                harness,
            ],
        );
        assert!(out.status.success(), "{harness}: {}", stderr(&out));
        let id = stdout(&out);
        assert!(!id.is_empty(), "{harness} returned no identifier");
        assert_eq!(f.recovery()[0]["data"]["prompt"], "");
        let carried: Vec<String> = f
            .rows()
            .iter()
            .filter_map(|r| r["session"]["session_id"].as_str().map(str::to_owned))
            .collect();
        assert!(carried.contains(&id), "{harness}: lost {id}: {carried:?}");
        for pid in f.hosted_pids() {
            assert!(
                alive(pid),
                "{harness}: the waiting client was not left running"
            );
        }
    }
}

#[test]
fn print_command_starts_nothing_and_keeps_naming_the_folder() {
    let f = Fixture::new();
    for harness in ["claude", "pi"] {
        let out = f.cones(
            "ok",
            &[
                "launch",
                "--dir",
                f.work.to_str().unwrap(),
                "--harness",
                harness,
                "--print-command",
                "a prompt",
            ],
        );
        assert!(out.status.success(), "{}", stderr(&out));
        let printed = stdout(&out);
        assert!(
            printed.starts_with(&format!("cd '{}' &&", f.work.display())),
            "{printed}"
        );
        assert!(printed.contains("a prompt"), "{printed}");
        assert!(
            printed.contains(&format!("/.local/bin/{harness}")),
            "{printed}"
        );
    }
    assert!(
        f.hosted_pids().is_empty(),
        "--print-command must start nothing"
    );
    assert!(
        f.recovery().is_empty(),
        "--print-command must record no launch"
    );
    assert!(
        f.rows().is_empty(),
        "--print-command must leave the folder empty"
    );
}

#[test]
fn a_disabled_or_unknown_harness_is_refused_before_anything_starts() {
    let f = Fixture::new();
    for (harness, expected) in [
        ("codex", "turned off in the configuration"),
        ("nonesuch", "unknown harness nonesuch"),
    ] {
        let out = f.launch("ok", harness, "no");
        assert!(!out.status.success(), "{harness} was not refused");
        assert!(stderr(&out).contains(expected), "{}", stderr(&out));
    }
    assert!(f.recovery().is_empty());
    assert!(f.hosted_pids().is_empty());
}

#[test]
fn a_launch_directory_that_is_not_a_folder_is_refused() {
    let f = Fixture::new();
    let file = f.path().join("notadir");
    fs::write(&file, "x").unwrap();
    for dir in [file.to_str().unwrap(), "/no/such/folder/here"] {
        let out = f.cones(
            "ok",
            &["launch", "--dir", dir, "--harness", "claude", "hello"],
        );
        assert!(!out.status.success(), "{dir} was accepted");
        assert!(
            stderr(&out).contains("launch directory") || stderr(&out).contains("is not a folder"),
            "{}",
            stderr(&out)
        );
    }
    assert!(f.recovery().is_empty());
}
