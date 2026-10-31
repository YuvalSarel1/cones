//! Detached `cones launch` against disposable homes and fixture launch targets.
//! No model is contacted and no native home outside the fixture is read or written.
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
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
        for name in ["claude", "pi", "codex", "kimi"] {
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

    /// The same fixture offering Codex alone, with a native home of its own. That home makes
    /// cones scan the machine's Codex clients too, so these tests select rows by the ids and
    /// the folder they created.
    fn codex() -> Self {
        let f = Self::offering("codex");
        fs::create_dir_all(f.codex_home()).unwrap();
        f
    }

    /// The same fixture offering `harness` alone.
    fn offering(harness: &str) -> Self {
        let f = Self::new();
        let offered = fs::read_to_string(&f.jobs).unwrap().replace(
            &format!("{harness}_enabled: false"),
            "claude_enabled: false\n  pi_enabled: false",
        );
        fs::write(&f.jobs, offered).unwrap();
        f
    }

    /// Copilot as npm installs it: a loader on the launch path that waits on the native
    /// binary of a platform package, which `platform` places relative to the loader's own.
    fn copilot(platform: &str) -> Self {
        let f = Self::offering("copilot");
        let package = f.path().join(".local/lib/node_modules/@github/copilot");
        let native = package.join(platform);
        fs::create_dir_all(&native).unwrap();
        let loader = package.join("npm-loader.js");
        fs::write(
            &loader,
            "#!/usr/bin/python3\nimport glob, os, subprocess, sys\n\
             here = os.path.dirname(os.path.realpath(__file__))\n\
             native = glob.glob(here + '/../copilot-darwin-*/copilot') + \
             glob.glob(here + '/node_modules/@github/copilot-darwin-*/copilot')\n\
             sys.exit(subprocess.call([native[0], *sys.argv[1:]]))\n",
        )
        .unwrap();
        fs::set_permissions(&loader, fs::Permissions::from_mode(0o755)).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_harness.py");
        std::os::unix::fs::symlink(fixture, native.join("copilot")).unwrap();
        std::os::unix::fs::symlink(&loader, f.path().join(".local/bin/copilot")).unwrap();
        f
    }

    fn codex_home(&self) -> PathBuf {
        self.path().join("codex")
    }

    /// The threads the fixture's daemon made, in the order it made them.
    fn codex_threads(&self) -> Vec<String> {
        fs::read_to_string(self.codex_home().join("fixture-threads"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn state(&self) -> PathBuf {
        self.path().join("state")
    }

    fn cones(&self, mode: &str, args: &[&str]) -> Output {
        self.command(mode, args).output().unwrap()
    }

    fn command(&self, mode: &str, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cones"));
        command
            .args([
                "--jobs",
                self.jobs.to_str().unwrap(),
                "--state-dir",
                self.state().to_str().unwrap(),
            ])
            .args(args)
            .env("HOME", self.path())
            .env("FAKE_LAUNCH", mode)
            .env("CONES_TEST_FAST", "1")
            .env("CLAUDE_CONFIG_DIR", self.path().join("claude"))
            .env("PI_CODING_AGENT_DIR", self.path().join("pi"))
            .env("CODEX_HOME", self.codex_home())
            .env_remove("OPENCODE_DB")
            .env_remove("OPENCODE_TUI_CONFIG");
        command
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
        let daemon = fs::read(self.codex_home().join("app-server-daemon/app-server.pid"))
            .ok()
            .and_then(|pid| serde_json::from_slice::<Value>(&pid).ok())
            .and_then(|pid| pid["pid"].as_u64())
            .map(|pid| pid as u32);
        for pid in self
            .hosted_pids()
            .into_iter()
            .chain(self.background_pids())
            .chain(daemon)
        {
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

/// A hosted client that leaves during the launch is not a worker. The launch has to say so
/// with what the client showed, since the host and its scrollback go with the client.
fn assert_no_worker(f: &Fixture, out: &Output, shown: &[&str]) {
    assert!(!out.status.success(), "{}", stdout(out));
    assert!(stdout(out).is_empty(), "no id may be printed");
    let error = stderr(out);
    for text in shown {
        assert!(error.contains(text), "missing {text:?}: {error}");
    }
    assert!(
        !error.contains("running detached"),
        "nothing is running: {error}"
    );
    wait_until("the host to retire", || f.hosted_pids().is_empty());
    let rows = f.rows();
    assert!(rows.is_empty(), "nothing may be listed: {rows:?}");
    let records = f.recovery();
    assert_eq!(records[1]["event"], "launch.unnamed");
    assert!(records[1]["data"]["session_id"].is_null());
    let recorded = records[1]["data"]["error"].as_str().unwrap();
    assert!(recorded.contains(shown[0]), "{recorded}");
}

#[test]
fn a_hosted_client_that_exits_on_its_sign_in_screen_fails_the_launch_with_that_screen() {
    let f = Fixture::new();
    let out = f.launch("login", "pi", "work nobody will do");
    assert_no_worker(
        &f,
        &out,
        &[
            "pi exited 1",
            "No API key found. Starting login flow...",
            "Would you like to log in to the fixture? [(y)es, (n)o]:",
        ],
    );
}

#[test]
fn a_hosted_client_that_refuses_to_start_fails_the_launch_with_its_diagnostic() {
    let f = Fixture::new();
    let out = f.launch("fail", "pi", "this will not start");
    assert_no_worker(&f, &out, &["pi exited 3", "fixture refused this launch"]);
}

#[test]
fn a_hosted_client_waiting_on_a_screen_is_named_with_what_it_shows() {
    let f = Fixture::new();
    let out = f.launch("stuck", "pi", "work it has not taken");
    // It is alive, so it is named: cones cannot tell a sign-in question from a working client.
    assert!(out.status.success(), "{}", stderr(&out));
    let id = stdout(&out);
    let hosted = f.hosted_pids();
    assert_eq!(hosted.len(), 1, "{hosted:?}");
    assert!(alive(hosted[0]));
    assert!(
        f.rows()
            .iter()
            .any(|r| r["session"]["session_id"] == id.as_str()),
        "cones ls does not carry {id}"
    );
    // What it can say is that the harness reported nothing, and what the client shows.
    let said = stderr(&out);
    assert!(
        said.contains("pi has reported no conversation")
            && said.contains("Would you like to log in to the fixture? [(y)es, (n)o]:"),
        "{said}"
    );
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
fn a_codex_launch_returns_the_thread_its_daemon_made_and_leaves_no_client_behind() {
    let f = Fixture::codex();
    let out = f.launch("ok", "codex", "rebase onto main\nand keep the tests green");
    assert!(out.status.success(), "{}", stderr(&out));
    let id = stdout(&out);
    assert_eq!(
        f.codex_threads(),
        std::slice::from_ref(&id),
        "the thread the daemon made"
    );
    assert!(
        stderr(&out).contains("native session id"),
        "{}",
        stderr(&out)
    );

    // The daemon holds the thread, so the launching client is gone and the folder has one
    // row for one agent, which the dashboard joins like any other daemon thread.
    let rows = f.rows();
    let codex: Vec<&Value> = rows
        .iter()
        .filter(|r| r["session"]["harness"] == "codex")
        .collect();
    assert_eq!(codex.len(), 1, "{codex:?}");
    assert_eq!(codex[0]["session"]["session_id"], id.as_str());
    assert_eq!(codex[0]["session"]["kind"], "daemon");
    assert_eq!(codex[0]["session"]["title"], "rebase onto main");
    assert_eq!(
        f.hosted_pids(),
        [] as [u32; 0],
        "the client was left hosted"
    );

    let recovery = f.recovery();
    assert_eq!(recovery[0]["event"], "launch.submitted");
    assert_eq!(recovery[1]["event"], "launch.identified");
    assert_eq!(recovery[1]["data"]["session_id"], id.as_str());
}

#[test]
fn two_codex_threads_for_one_prompt_are_refused_rather_than_chosen_between() {
    let f = Fixture::codex();
    let out = f.launch("twice", "codex", "same words");
    assert!(!out.status.success());
    assert_eq!(stdout(&out), "", "an id was guessed");
    assert!(stderr(&out).contains("will not choose"), "{}", stderr(&out));
    assert_eq!(f.codex_threads().len(), 2);
    // Unnamed is a reporting failure: the client is still hosted and still running.
    let hosted = f.hosted_pids();
    assert_eq!(hosted.len(), 1, "{hosted:?}");
    assert!(alive(hosted[0]));
    let recovery = f.recovery();
    assert_eq!(recovery[1]["event"], "launch.unnamed");
    assert!(recovery[1]["data"]["session_id"].is_null());
    // The client is a viewer of those threads, so the folder lists the threads and not it.
    let mut listed: Vec<String> = f
        .rows()
        .iter()
        .filter(|r| r["session"]["harness"] == "codex")
        .map(|r| r["session"]["session_id"].as_str().unwrap().to_owned())
        .collect();
    listed.sort();
    let mut threads = f.codex_threads();
    threads.sort();
    assert_eq!(listed, threads);
}

/// Codex reports nothing that ties a thread to the launch that asked for it, so two launches
/// of one prompt into one folder could each be handed the other's thread. The ledger every
/// launch writes first is what shows the overlap, and an overlap is refused.
#[test]
fn a_codex_launch_overlapping_another_of_the_same_prompt_is_refused_and_a_later_one_is_named() {
    let f = Fixture::codex();
    let ledger = f.state().join("launches.jsonl");
    let line = |event: &str| {
        serde_json::json!({
            "v": 1, "timestamp": chrono::Utc::now().to_rfc3339(), "pid": 1,
            "dashboard_id": null, "event": event, "level": "recovery",
            "data": {
                "operation_id": "codex:start:rival", "harness": "codex",
                "cwd": f.work, "prompt": "same words",
            },
        })
        .to_string()
            + "\n"
    };
    fs::write(&ledger, line("launch.submitted")).unwrap();

    let out = f.launch("ok", "codex", "same words");
    assert!(!out.status.success());
    assert_eq!(stdout(&out), "", "an id was guessed");
    assert!(stderr(&out).contains("will not choose"), "{}", stderr(&out));
    let refused = f.codex_threads();
    assert_eq!(refused.len(), 1, "the refused launch still made its thread");
    let hosted = f.hosted_pids();
    assert_eq!(hosted.len(), 1, "{hosted:?}");
    assert!(alive(hosted[0]), "an unnamed launch must be left running");
    assert_eq!(f.recovery().last().unwrap()["event"], "launch.unnamed");

    // Another prompt was never in question, whatever else is waiting, even one that only
    // shares the first line.
    let out = f.launch("ok", "codex", "same words\nwith its own second line");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(f.codex_threads()[1], stdout(&out));

    // Once the rival is settled, the same prompt is named again, and by its own thread: the
    // refused launch's thread is older than this launch and is not a candidate.
    let mut settled = fs::read_to_string(&ledger).unwrap();
    settled.push_str(&line("launch.unnamed"));
    fs::write(&ledger, settled).unwrap();
    let out = f.launch("ok", "codex", "same words");
    assert!(out.status.success(), "{}", stderr(&out));
    let threads = f.codex_threads();
    assert_eq!(threads.len(), 3);
    assert_eq!(threads[2], stdout(&out));
    assert_ne!(threads[2], refused[0]);
    assert_eq!(
        f.recovery().last().unwrap()["data"]["session_id"],
        threads[2].as_str()
    );
}

/// A lead agent starting several workers at once gives them one shared preamble and its own
/// task each. Every launch is named by its own thread, and none leaves a client behind.
#[test]
fn simultaneous_codex_launches_that_share_a_first_line_are_each_named() {
    let f = Fixture::codex();
    let prompts: Vec<String> = (1..=3)
        .map(|n| format!("You are a worker for the lead.\n\nYOUR TASK:\nTask {n}"))
        .collect();
    // Output goes to files and each launch has a deadline: a refused launch leaves its
    // client hosted, and that must fail the test rather than hang it.
    let launches: Vec<(std::process::Child, PathBuf)> = prompts
        .iter()
        .enumerate()
        .map(|(n, prompt)| {
            let out = f.path().join(format!("launch-{n}.out"));
            let work = f.work.to_str().unwrap();
            let child = f
                .command(
                    "ok",
                    &["launch", "--dir", work, "--harness", "codex", prompt],
                )
                .stdout(fs::File::create(&out).unwrap())
                .stderr(fs::File::create(out.with_extension("err")).unwrap())
                .spawn()
                .unwrap();
            (child, out)
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut named: Vec<String> = launches
        .into_iter()
        .map(|(mut child, out)| {
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() > deadline {
                    child.kill().unwrap();
                    panic!("a launch did not finish within 60s");
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            let err = fs::read_to_string(out.with_extension("err")).unwrap();
            assert!(status.success(), "{err}");
            fs::read_to_string(&out).unwrap().trim().to_owned()
        })
        .collect();
    named.sort();
    let mut threads = f.codex_threads();
    threads.sort();
    assert_eq!(named, threads, "each launch names its own thread");
    assert_eq!(f.hosted_pids(), [] as [u32; 0], "a client was left hosted");
    let mut listed: Vec<String> = f
        .rows()
        .iter()
        .filter(|r| r["session"]["harness"] == "codex")
        .map(|r| r["session"]["session_id"].as_str().unwrap().to_owned())
        .collect();
    listed.sort();
    assert_eq!(listed, threads);
}

/// A detached client has no screen to answer Codex's trust question on, so cones saves the
/// folder's trust in the daemon's config first, as answering the question would.
#[test]
fn a_codex_launch_trusts_a_new_folder_through_the_daemon_and_keeps_a_named_one() {
    use tungstenite::Message;
    let f = Fixture::codex();
    let control = f.codex_home().join("app-server-control");
    fs::create_dir_all(&control).unwrap();
    let listener =
        std::os::unix::net::UnixListener::bind(control.join("app-server-control.sock")).unwrap();
    let (home, writes) = (
        f.codex_home(),
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    );
    let seen = writes.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut socket = tungstenite::accept(stream.unwrap()).unwrap();
            while let Ok(Message::Text(text)) = socket.read() {
                let request: Value = serde_json::from_str(&text).unwrap();
                let trusted = home.join("trusted");
                let result = match request["method"].as_str().unwrap() {
                    "initialized" => continue,
                    "initialize" => serde_json::json!({}),
                    "config/read" => {
                        let projects: serde_json::Map<String, Value> = fs::read_to_string(&trusted)
                            .unwrap_or_default()
                            .lines()
                            .map(|d| (d.to_owned(), serde_json::json!({"trust_level": "trusted"})))
                            .collect();
                        serde_json::json!({"config": {"projects": projects}, "origins": {}})
                    }
                    "config/batchWrite" => {
                        let edit = request["params"]["edits"][0].clone();
                        seen.lock().unwrap().push(edit.clone());
                        let mut names = fs::read_to_string(&trusted).unwrap_or_default();
                        for (dir, project) in edit["value"].as_object().unwrap() {
                            assert_eq!(project["trust_level"], "trusted");
                            names.push_str(&format!("{dir}\n"));
                        }
                        fs::write(&trusted, names).unwrap();
                        serde_json::json!({"status": "ok", "version": "1", "filePath": "config.toml"})
                    }
                    other => panic!("cones sent the daemon {other}"),
                };
                let reply = serde_json::json!({"id": request["id"], "result": result});
                socket
                    .send(Message::Text(reply.to_string().into()))
                    .unwrap();
            }
        }
    });

    let out = f.launch("trust", "codex", "review the plan");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(f.codex_threads(), [stdout(&out)]);
    let folder = f.work.to_str().unwrap();
    assert_eq!(
        *writes.lock().unwrap(),
        [serde_json::json!({
            "keyPath": "projects", "mergeStrategy": "upsert",
            "value": {folder: {"trust_level": "trusted"}},
        })]
    );

    // A folder the config already names keeps its setting: nothing more is written.
    let out = f.launch("trust", "codex", "review it again");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(f.codex_threads().len(), 2);
    assert_eq!(writes.lock().unwrap().len(), 1);
}

/// Codex keeps nothing for a client that has not taken a turn, so the only id such a launch
/// could return is one that stops resolving at the first turn.
#[test]
fn a_codex_launch_with_no_prompt_is_refused_before_anything_is_started() {
    let f = Fixture::codex();
    let out = f.cones(
        "ok",
        &[
            "launch",
            "--dir",
            f.work.to_str().unwrap(),
            "--harness",
            "codex",
        ],
    );
    assert!(!out.status.success());
    assert_eq!(stdout(&out), "", "an id was returned");
    assert!(stderr(&out).contains("needs a prompt"), "{}", stderr(&out));
    assert_eq!(f.hosted_pids(), [] as [u32; 0], "a client was started");
    assert_eq!(f.codex_threads(), [] as [String; 0]);
    assert!(f.recovery().is_empty(), "{:?}", f.recovery());
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
fn a_copilot_launch_is_one_row_for_its_native_client_wherever_npm_put_it() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    // Beside the loader's package, and nested inside it as Copilot 1.0.95 installs.
    for platform in [
        format!("../copilot-darwin-{arch}"),
        format!("node_modules/@github/copilot-darwin-{arch}"),
    ] {
        let f = Fixture::copilot(&platform);
        let out = f.launch("ok", "copilot", "tidy the imports");
        assert!(out.status.success(), "{platform}: {}", stderr(&out));
        let id = stdout(&out);
        let hosted = f.hosted_pids();
        assert_eq!(hosted.len(), 1, "{platform}: {hosted:?}");
        let pid = hosted[0];
        assert_eq!(id, format!("copilot-{pid}"), "{platform}");

        // The loader waits on a second process of the same name. Hosting it would list
        // one agent twice, and only the loader's row would stop.
        let rows = f.rows();
        let copilot: Vec<&Value> = rows
            .iter()
            .filter(|r| r["session"]["harness"] == "copilot")
            .collect();
        assert_eq!(copilot.len(), 1, "{platform}: {copilot:?}");
        assert_eq!(copilot[0]["session"]["session_id"], id.as_str());
        assert_eq!(copilot[0]["session"]["pid"].as_u64(), Some(u64::from(pid)));
        assert_eq!(copilot[0]["session"]["title"], "tidy the imports");

        // The returned id is the one `cones stop` takes, and stopping leaves nothing behind.
        let stopped = f.cones("ok", &["stop", &id]);
        assert!(stopped.status.success(), "{platform}: {}", stderr(&stopped));
        wait_until("the native client to exit", || !alive(pid));
        assert_eq!(f.hosted_pids(), [] as [u32; 0], "{platform}");
        assert_eq!(f.rows(), [] as [Value; 0], "{platform}");
    }
}

#[test]
fn a_harness_with_no_interactive_instruction_refuses_one_and_starts_without() {
    // Kimi 2 prints a `--prompt` answer and exits, and refuses the flag beside `--auto`, so
    // an instruction would start a client that is gone before anyone opens it.
    let f = Fixture::offering("kimi");
    for extra in [&[][..], &["--print-command"][..]] {
        let mut args = vec!["launch", "--dir", f.work.to_str().unwrap()];
        args.extend(["--harness", "kimi"]);
        args.extend(extra);
        args.push("fix the parser");
        let out = f.cones("ok", &args);
        assert!(!out.status.success(), "{extra:?}: {}", stdout(&out));
        assert_eq!(
            stdout(&out),
            "",
            "{extra:?}: an id or a command was printed"
        );
        assert!(
            stderr(&out).contains("kimi cannot open an interactive session with an instruction"),
            "{extra:?}: {}",
            stderr(&out)
        );
    }
    assert!(f.recovery().is_empty(), "a refused launch was recorded");
    assert!(
        f.hosted_pids().is_empty(),
        "a refused launch started a client"
    );

    let out = f.cones(
        "ok",
        &[
            "launch",
            "--dir",
            f.work.to_str().unwrap(),
            "--harness",
            "kimi",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let hosted = f.hosted_pids();
    assert_eq!(hosted.len(), 1, "{hosted:?}");
    assert_eq!(stdout(&out), format!("kimi-{}", hosted[0]));
    let rows = f.rows();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["session"]["session_id"], stdout(&out).as_str());
    assert_eq!(rows[0]["session"]["harness"], "kimi");
    assert!(alive(hosted[0]), "the waiting client was not left running");
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

/// `--name` goes to the harness's own launch flag, ahead of the prompt. A harness with none
/// names its session by renaming it once it runs, which a printed command cannot do.
#[test]
fn print_command_carries_a_name_only_through_a_native_flag() {
    let launch = |f: &Fixture, harness: &str| {
        f.cones(
            "ok",
            &[
                "launch",
                "--dir",
                f.work.to_str().unwrap(),
                "--harness",
                harness,
                "--name",
                "Fix the parser",
                "--print-command",
                "a prompt",
            ],
        )
    };
    let f = Fixture::new();
    for harness in ["claude", "pi"] {
        let out = launch(&f, harness);
        assert!(out.status.success(), "{}", stderr(&out));
        let printed = stdout(&out);
        let (flag, prompt) = (
            printed.find("'--name' 'Fix the parser'"),
            printed.find("a prompt"),
        );
        assert!(flag.is_some() && flag < prompt, "{harness}: {printed}");
    }
    let f = Fixture::codex();
    let out = launch(&f, "codex");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("codex has no launch flag for a name"),
        "{}",
        stderr(&out)
    );
    assert!(f.recovery().is_empty(), "nothing was launched");
}

/// Codex's command names its daemon's address, and asking for it used to start the daemon.
/// With none running the print refuses and names the command that starts one; with one running
/// it prints the address that daemon reports.
#[test]
fn print_command_for_codex_starts_no_daemon_and_prints_a_running_one() {
    let f = Fixture::codex();
    let pid_file = f.codex_home().join("app-server-daemon/app-server.pid");
    let print = || {
        f.cones(
            "ok",
            &[
                "launch",
                "--dir",
                f.work.to_str().unwrap(),
                "--harness",
                "codex",
                "--print-command",
                "a prompt",
            ],
        )
    };
    let refused = print();
    assert!(!refused.status.success(), "{}", stdout(&refused));
    assert!(
        stderr(&refused).contains("codex app-server daemon start"),
        "{}",
        stderr(&refused)
    );
    assert!(!pid_file.exists(), "--print-command started the daemon");

    // A real launch starts the daemon, after which the same print has an address to name.
    let launched = f.launch("ok", "codex", "rebase onto main");
    assert!(launched.status.success(), "{}", stderr(&launched));
    let daemon = fs::read_to_string(&pid_file).unwrap();
    let printed = print();
    assert!(printed.status.success(), "{}", stderr(&printed));
    assert!(
        stdout(&printed).contains("'--remote' 'unix://"),
        "{}",
        stdout(&printed)
    );
    assert_eq!(fs::read_to_string(&pid_file).unwrap(), daemon);
    assert_eq!(f.codex_threads().len(), 1, "the print made a thread");
}

/// A prompt carrying a one-time secret must not be kept by cones. The launch is still recorded,
/// so it can be found and a retry is visible, with the prompt absent from both records and
/// from every other file under the state directory.
#[test]
fn no_prompt_record_keeps_the_prompt_out_of_everything_cones_writes() {
    let f = Fixture::new();
    let secret = "deploy with token-9f3c1e7a";
    let out = f.cones(
        "ok",
        &[
            "launch",
            "--dir",
            f.work.to_str().unwrap(),
            "--harness",
            "claude",
            "--no-prompt-record",
            secret,
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let id = stdout(&out);
    let records = f.recovery();
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0]["event"], "launch.submitted");
    assert_eq!(records[1]["event"], "launch.identified");
    assert_eq!(records[1]["data"]["session_id"], id.as_str());
    for record in &records {
        assert!(record["data"]["prompt"].is_null(), "{record}");
        assert_eq!(record["data"]["harness"], "claude");
    }
    let mut stack = vec![f.state()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = fs::read(&path) {
                assert!(
                    !String::from_utf8_lossy(&bytes).contains("token-9f3c1e7a"),
                    "{} kept the prompt",
                    path.display()
                );
            }
        }
    }
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

/// `launch --wait` is how a launcher hears that its worker stopped: the command exits when the
/// harness reports the turn ended, and a caller running it in the background is woken by that
/// exit. The idle a background session reports before its first turn is not that end.
#[test]
fn a_waited_launch_returns_when_the_worker_s_turn_ends_and_not_before() {
    let f = Fixture::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cones"))
        .args([
            "--jobs",
            f.jobs.to_str().unwrap(),
            "--state-dir",
            f.state().to_str().unwrap(),
            "launch",
            "--dir",
            f.work.to_str().unwrap(),
            "--harness",
            "claude",
            "--wait",
            "--timeout",
            "30",
            "port the risk experiment",
        ])
        .env("HOME", f.path())
        .env("CONES_TEST_FAST", "1")
        .env("CLAUDE_CONFIG_DIR", f.path().join("claude"))
        .env("PI_CODING_AGENT_DIR", f.path().join("pi"))
        .env("CODEX_HOME", f.codex_home())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let registry = f.path().join("claude/sessions");
    let record = || fs::read_dir(&registry).ok()?.next()?.ok().map(|e| e.path());
    wait_until("the launch registers its session", || record().is_some());
    let record = record().unwrap();
    let set = |status: &str| {
        let mut entry: Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        entry["status"] = status.into();
        fs::write(&record, entry.to_string()).unwrap();
    };
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the idle reported before the first turn ended the wait"
    );
    set("busy");
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        child.try_wait().unwrap().is_none(),
        "a working session ended the wait"
    );
    set("idle");
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let id = record.file_stem().unwrap().to_str().unwrap().to_owned();
    assert_eq!(stdout(&out), format!("{id}\n{id}\tidle"));

    // A worker that never takes a turn leaves the wait to its timeout, exit 2.
    let out = f.cones(
        "ok",
        &[
            "launch",
            "--dir",
            f.work.to_str().unwrap(),
            "--harness",
            "claude",
            "--wait",
            "--timeout",
            "0.5",
            "sit idle",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stdout(&out).ends_with("\ntimeout"), "{}", stdout(&out));
}

/// A launch is listed beneath another session only when its launcher says so with `--parent`.
/// A helper that outlives its agent's process chain, or names an agent that has ended, must
/// not be placed beneath a session silently: the exported session id alone records nothing,
/// an explicit id is recorded whatever the chain, and an id naming no live session is refused
/// before anything starts.
#[test]
fn a_launch_is_listed_beneath_only_the_parent_it_names() {
    let f = Fixture::new();
    let parent = stdout(&f.launch("ok", "claude", "lead the work"));
    let relations = f.state().join("relations.json");
    let launch = |args: &[&str], env: &[(&str, &str)]| {
        let mut all = vec![
            "launch",
            "--dir",
            f.work.to_str().unwrap(),
            "--harness",
            "claude",
        ];
        all.extend(args);
        let mut command = f.command("ok", &all);
        command
            .env_remove("CODEX_THREAD_ID")
            .env_remove("CLAUDE_CODE_SESSION_ID");
        command.envs(env.iter().copied());
        command.output().unwrap()
    };

    // An agent's environment names its session, but the launch stands alone unless asked.
    let out = launch(&["alone"], &[("CLAUDE_CODE_SESSION_ID", &parent)]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!relations.exists(), "an unasked parent was recorded");

    // `self` takes the session the environment names.
    let out = launch(
        &["--parent", "self", "beside me"],
        &[("CLAUDE_CODE_SESSION_ID", &parent)],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let mine = stdout(&out);
    // A detached helper, with no agent in its chain or environment, names the parent itself.
    let out = launch(&["--parent", &parent[..8], "from a helper"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let helpers = stdout(&out);
    let links: Value = serde_json::from_slice(&fs::read(&relations).unwrap()).unwrap();
    let children: Vec<_> = links
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            assert_eq!(l["kind"], "launch", "{links}");
            assert_eq!(l["parent"], parent.as_str(), "{links}");
            l["child"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(children, [mine, helpers], "{links}");

    // `self` with no session to find, and a parent that has ended, start nothing.
    let started = f.background_pids().len();
    let out = launch(&["--parent", "self", "orphan"], &[]);
    assert!(!out.status.success(), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("no listed session runs this command"),
        "{}",
        stderr(&out)
    );
    let daemon = fs::read_dir(f.path().join("claude/sessions"))
        .unwrap()
        .find_map(|e| {
            let record: Value =
                serde_json::from_slice(&fs::read(e.unwrap().path()).unwrap()).unwrap();
            (record["sessionId"] == parent.as_str()).then(|| record["pid"].as_u64().unwrap() as u32)
        })
        .expect("the parent's registry record");
    unsafe { libc::kill(daemon as i32, libc::SIGKILL) };
    wait_until("the parent's daemon to exit", || !alive(daemon));
    let out = launch(&["--parent", &parent, "after it ended"], &[]);
    assert!(!out.status.success(), "{}", stdout(&out));
    assert!(stdout(&out).is_empty(), "no id may be printed");
    assert!(stderr(&out).contains("no live session"), "{}", stderr(&out));
    assert_eq!(
        f.background_pids().len(),
        started,
        "a refused launch started a session"
    );
    let after: Value = serde_json::from_slice(&fs::read(&relations).unwrap()).unwrap();
    assert_eq!(after, links);
}

/// A pi conversation in the fixture's history, as pi writes it.
fn pi_history(f: &Fixture, id: &str) -> PathBuf {
    let dir = f.path().join("pi/sessions/--work--");
    fs::create_dir_all(&dir).unwrap();
    let transcript = dir.join(format!("2026-10-17_{id}.jsonl"));
    let lines = [
        serde_json::json!({"type":"session","id":id,"cwd":f.work,"timestamp":"2026-10-17T09:00:00Z"}),
        serde_json::json!({"type":"message","timestamp":"2026-10-17T09:00:01Z","message":{"role":"user","content":[{"type":"text","text":"Sort the imports"}]}}),
        serde_json::json!({"type":"message","timestamp":"2026-10-17T09:00:02Z","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"Sorted"}]}}),
    ];
    fs::write(
        &transcript,
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    transcript.canonicalize().unwrap()
}

fn args_of(pid: u32) -> String {
    let out = Command::new("/bin/ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn resume_and_fork_reopen_a_conversation_detached_and_record_the_fork_s_parent() {
    let f = Fixture::offering("pi");
    let id = "0199f2a4-7c1e-7000-8000-00000000a11d";
    let transcript = pi_history(&f, id);

    // Resume reopens the conversation in a cones terminal and prints its own id.
    let out = f.cones("ok", &["resume", &id[..8]]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), id);
    let hosted = f.hosted_pids();
    assert_eq!(hosted.len(), 1, "{hosted:?}");
    assert!(
        alive(hosted[0]),
        "the resumed client died with its launcher"
    );
    assert!(
        args_of(hosted[0]).ends_with(&format!("--session {}", transcript.display())),
        "{}",
        args_of(hosted[0])
    );
    let recovery = f.recovery();
    assert_eq!(recovery[0]["event"], "resume.submitted");
    assert_eq!(recovery[0]["data"]["operation_id"], id);

    // A second resume of a running conversation is refused, as the dashboard refuses it.
    let out = f.cones("ok", &["resume", id]);
    assert!(!out.status.success(), "{}", stdout(&out));
    assert!(stderr(&out).contains("already running"), "{}", stderr(&out));
    assert_eq!(f.hosted_pids().len(), 1);

    // Fork starts a new pi session under a new id and records the parent.
    let out = f.cones("ok", &["fork", id]);
    assert!(out.status.success(), "{}", stderr(&out));
    let forked = f
        .hosted_pids()
        .into_iter()
        .find(|&p| p != hosted[0])
        .expect("the fork has a host of its own");
    let args = args_of(forked);
    assert!(
        args.contains(&format!("--fork {}", transcript.display())),
        "{args}"
    );
    let child = args.rsplit(' ').next().unwrap().to_owned();
    assert!(uuid::Uuid::parse_str(&child).is_ok(), "{args}");
    assert_ne!(child, id);
    let links: Value =
        serde_json::from_slice(&fs::read(f.state().join("relations.json")).unwrap()).unwrap();
    assert_eq!(links.as_array().unwrap().len(), 1, "{links}");
    assert_eq!(links[0]["parent"], id);
    assert_eq!(links[0]["child"], child.as_str());
    assert_eq!(links[0]["harness"], "pi");
}

/// Copilot and Cursor Agent resume a saved conversation by its id, as `copilot --resume ID`
/// and `cursor-agent --resume ID` reopen it, in the folder it was recorded in.
#[test]
fn copilot_and_cursor_conversations_resume_by_their_native_id() {
    for (harness, key) in [("copilot", "copilot"), ("cursor-agent", "cursor")] {
        let f = Fixture::offering(key);
        std::os::unix::fs::symlink(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_harness.py"),
            f.path().join(".local/bin").join(harness),
        )
        .unwrap();
        let id = "58585858-1111-4111-8111-111111111111";
        let at = "2026-10-17T09:00:00.000Z";
        let (transcript, lines) = if harness == "copilot" {
            (
                f.path()
                    .join(format!(".copilot/session-state/{id}/events.jsonl")),
                vec![
                    serde_json::json!({"type": "session.start", "id": "e1", "timestamp": at,
                        "data": {"sessionId": id, "startTime": at, "context": {"cwd": f.work}}}),
                    serde_json::json!({"type": "user.message", "id": "e2", "timestamp": at,
                        "data": {"content": "Sort the imports"}}),
                ],
            )
        } else {
            let meta = f
                .path()
                .join(format!(".cursor/chats/0123abcd/{id}/meta.json"));
            fs::create_dir_all(meta.parent().unwrap()).unwrap();
            fs::write(
                &meta,
                serde_json::json!({"schemaVersion": 1, "createdAtMs": 1789808119982u64,
                    "hasConversation": true, "cwd": f.work})
                .to_string(),
            )
            .unwrap();
            (
                f.path().join(format!(
                    ".cursor/projects/work/agent-transcripts/{id}/{id}.jsonl"
                )),
                vec![serde_json::json!({"role": "user", "message": {"content": [
                    {"type": "text", "text": "<user_query>\nSort the imports\n</user_query>"}]}})],
            )
        };
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(
            &transcript,
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .unwrap();

        let out = f.cones("ok", &["resume", &id[..8]]);
        assert!(out.status.success(), "{harness}: {}", stderr(&out));
        assert_eq!(stdout(&out), id, "{harness}");
        let hosted = f.hosted_pids();
        assert_eq!(hosted.len(), 1, "{harness}: {hosted:?}");
        assert!(alive(hosted[0]), "{harness}: the resumed client died");
        assert!(
            args_of(hosted[0]).contains(&format!(" --resume {id}")),
            "{harness}: {}",
            args_of(hosted[0])
        );
        assert_eq!(f.recovery()[0]["data"]["cwd"], f.work.to_str().unwrap());
    }
}

#[test]
fn a_fork_of_a_harness_with_no_native_fork_is_refused_before_anything_starts() {
    let f = Fixture::offering("pi");
    let id = "0199f2a4-7c1e-7000-8000-00000000b0b0";
    pi_history(&f, id);
    // The fixture's pi --help names no --fork, so the fork operation's probe refuses it.
    let out = f.cones("nofork", &["fork", id]);
    assert!(!out.status.success(), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("does not support native forks"),
        "{}",
        stderr(&out)
    );
    assert!(f.hosted_pids().is_empty());
}

/// `cones wait` watches sessions it did not launch, a job's session included, which `ls` shows
/// as its run instead. A settled session answers at once, a working one when it settles, and
/// an id that is not live is refused rather than waited on forever.
#[test]
fn wait_returns_when_a_named_session_settles_including_a_job_s() {
    use cones::ledger::{Ledger, Record, Status};
    let f = Fixture::new();
    let out = f.cones("ok", &["wait", "nobody-here"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("no live session nobody-here"),
        "{}",
        stderr(&out)
    );

    let out = f.launch("ok", "claude", "port the risk experiment");
    assert!(out.status.success(), "{}", stderr(&out));
    let id = stdout(&out);
    // The session belongs to a job's run: `ls` lists it as the session, carrying its run.
    let mut started = Record::new("run-wait-0001".into(), Status::Started);
    started.job = Some("nightly".into());
    started.session_id = Some(id.clone());
    started.cwd = Some(f.work.clone());
    Ledger::new(&f.state()).unwrap().append(&started).unwrap();
    let rows = f.rows();
    let row = rows
        .iter()
        .find(|r| r["session"]["session_id"] == id.as_str())
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!(row["kind"], "session");
    assert_eq!(row["status"], "idle", "the live session's own state");
    assert_eq!(row["run"]["run_id"], "run-wait-0001");
    assert_eq!(row["run"]["job"], "nightly");
    assert_eq!(row["run"]["status"], "started");
    assert_eq!(
        rows.len(),
        1,
        "one row for the run and its session: {rows:?}"
    );

    let out = f.cones("ok", &["wait", &id]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), format!("{id}\tidle"));
    // One unknown id among several refuses the wait too.
    let out = f.cones("ok", &["wait", "nobody-yet", &id]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));

    let record = f.path().join("claude/sessions").join(format!("{id}.json"));
    let set = |status: &str| {
        let mut entry: Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        entry["status"] = status.into();
        fs::write(&record, entry.to_string()).unwrap();
    };
    set("busy");
    let out = f.cones("ok", &["wait", &id, "--timeout", "0.5"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert_eq!(stdout(&out), "timeout");

    let mut child = Command::new(env!("CARGO_BIN_EXE_cones"))
        .args([
            "--jobs",
            f.jobs.to_str().unwrap(),
            "--state-dir",
            f.state().to_str().unwrap(),
            "wait",
            &id,
            "--timeout",
            "30",
        ])
        .env("HOME", f.path())
        .env("CONES_TEST_FAST", "1")
        .env("CLAUDE_CONFIG_DIR", f.path().join("claude"))
        .env("PI_CODING_AGENT_DIR", f.path().join("pi"))
        .env("CODEX_HOME", f.codex_home())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        child.try_wait().unwrap().is_none(),
        "a working session ended the wait"
    );
    set("idle");
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), format!("{id}\tidle"));
}

impl Fixture {
    /// A Codex job named `name` in the fixture's folder, unattended as Codex jobs must be.
    fn codex_job(&self, name: &str, timeout_min: f64) {
        let jobs = fs::read_to_string(&self.jobs).unwrap().replace(
            "jobs: []\n",
            &format!(
                "jobs:\n  - name: {name}\n    schedule: \"0 3 * * *\"\n    harness: codex\n    \
                 codex_full_access: true\n    timeout_min: {timeout_min}\n    cwd: {}\n    \
                 prompt: \"triage the flaky tests\"\n",
                self.work.display()
            ),
        );
        fs::write(&self.jobs, jobs).unwrap();
    }

    /// `cones run NAME`, started in the background so the test can move its thread along.
    fn run_job(&self, name: &str) -> std::process::Child {
        Command::new(env!("CARGO_BIN_EXE_cones"))
            .args([
                "--jobs",
                self.jobs.to_str().unwrap(),
                "--state-dir",
                self.state().to_str().unwrap(),
                "run",
                name,
            ])
            .env("HOME", self.path())
            .env("CONES_TEST_FAST", "1")
            .env("CLAUDE_CONFIG_DIR", self.path().join("claude"))
            .env("PI_CODING_AGENT_DIR", self.path().join("pi"))
            .env("CODEX_HOME", self.codex_home())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// The run rows `cones ls --json` prints for the fixture.
    fn runs(&self) -> Vec<Value> {
        let out = self.cones("ok", &["ls", "--json"]);
        assert!(out.status.success(), "{}", stderr(&out));
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            // A run that started a session is that session's row, carrying the run.
            .filter_map(|row| match row["kind"].as_str() {
                Some("run") => Some(row),
                _ => row.get("run").cloned(),
            })
            .collect()
    }
}

/// A Codex job is a daemon thread started the way `cones launch` starts one. The run is
/// recorded under the thread the daemon made, ends `ok` when the rollout reports the turn
/// complete, and takes the thread off the list as it ends: stopped through the daemon, then
/// forgotten. The rollout stays.
#[test]
fn a_codex_job_runs_as_a_daemon_thread_and_ends_with_its_turn() {
    let f = Fixture::codex();
    let calls = crate::stop::codex_daemon_in(&f.codex_home(), &[]);
    f.codex_job("triage", 2.0);
    let child = f.run_job("triage");
    wait_until("the job's thread", || f.codex_threads().len() == 1);
    let thread = f.codex_threads()[0].clone();
    wait_until("the run to record its thread", || {
        f.runs()
            .iter()
            .any(|r| r["started"]["session_id"] == thread.as_str())
    });
    let rollout = harness_rollout(&f, &thread);
    let complete = serde_json::json!({"timestamp": "2026-10-17T09:00:09.000Z", "type": "event_msg",
        "payload": {"type": "task_complete", "turn_id": "t"}});
    let mut text = fs::read_to_string(&rollout).unwrap();
    text.push_str(&format!("{complete}\n"));
    fs::write(&rollout, text).unwrap();
    // The daemon reports the thread idle once its turn is over.
    fs::write(f.codex_home().join("app-server-control/idle"), &thread).unwrap();

    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let runs = f.runs();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["status"], "ok", "{runs:?}");
    assert_eq!(runs[0]["started"]["harness"], "codex");
    assert_eq!(runs[0]["started"]["session_id"], thread.as_str());
    assert!(
        crate::stop::methods(&calls).contains(&"thread/loaded/list".to_owned()),
        "the thread was stopped through its daemon before it was forgotten"
    );
    let saved = fs::read_to_string(f.state().join("codex-threads.json")).unwrap_or_default();
    assert!(!saved.contains(&thread), "{saved}");
    assert!(rollout.exists(), "the conversation stays resumable");
}

/// A Codex run that outlives `timeout_min` ends as `timeout`, and its thread goes with it.
#[test]
fn a_codex_job_past_its_timeout_ends_its_thread() {
    let f = Fixture::codex();
    let calls = crate::stop::codex_daemon_in(&f.codex_home(), &[]);
    // Six seconds, past which the worker removes the thread.
    f.codex_job("slow", 0.1);
    let out = f.run_job("slow").wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(124), "{}", stderr(&out));
    let thread = f.codex_threads()[0].clone();
    let runs = f.runs();
    assert_eq!(runs[0]["status"], "timeout", "{runs:?}");
    assert_eq!(runs[0]["terminal"]["reason"], "timeout", "{runs:?}");
    assert!(crate::stop::methods(&calls).contains(&"thread/loaded/list".to_owned()));
    let saved = fs::read_to_string(f.state().join("codex-threads.json")).unwrap_or_default();
    assert!(!saved.contains(&thread), "{saved}");
}

fn harness_rollout(f: &Fixture, thread: &str) -> PathBuf {
    let mut dirs = vec![f.codex_home().join("sessions")];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path
                .to_string_lossy()
                .ends_with(&format!("-{thread}.jsonl"))
            {
                return path;
            }
        }
    }
    panic!("no rollout for {thread}")
}

/// An agent with no shell reaches the control plane through `cones mcp`: each control tool is
/// the command of the same name, run against the server's own configuration and state.
#[test]
fn mcp_control_tools_launch_list_wait_and_stop_a_session_as_the_commands_do() {
    use std::io::{BufRead, BufReader, Write};
    let f = Fixture::offering("pi");
    let mut child = Command::new(env!("CARGO_BIN_EXE_cones"))
        .args([
            "--jobs",
            f.jobs.to_str().unwrap(),
            "--state-dir",
            f.state().to_str().unwrap(),
            "mcp",
        ])
        .env("HOME", f.path())
        .env("CONES_TEST_FAST", "1")
        .env("FAKE_LAUNCH", "ok")
        .env("CLAUDE_CONFIG_DIR", f.path().join("claude"))
        .env("PI_CODING_AGENT_DIR", f.path().join("pi"))
        .env("CODEX_HOME", f.codex_home())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut next = 0;
    let mut call = |method: &str, params: Value| -> Value {
        next += 1;
        writeln!(
            input,
            "{}",
            serde_json::json!({"jsonrpc": "2.0", "id": next, "method": method, "params": params})
        )
        .unwrap();
        if method == "initialize" {
            writeln!(
                input,
                "{}",
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
            )
            .unwrap();
        }
        input.flush().unwrap();
        let mut line = String::new();
        assert!(output.read_line(&mut line).unwrap() > 0);
        serde_json::from_str::<Value>(&line).unwrap()["result"].clone()
    };
    let tool = |call: &mut dyn FnMut(&str, Value) -> Value, name: &str, arguments: Value| {
        call(
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
        )
    };
    call(
        "initialize",
        serde_json::json!({"protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "fixture", "version": "1"}}),
    );

    let launched = tool(
        &mut call,
        "cones_launch",
        serde_json::json!({"dir": f.work, "harness": "pi", "prompt": "hold this terminal"}),
    );
    assert_eq!(launched["isError"], false, "{launched}");
    let id = launched["structuredContent"]["stdout"]
        .as_str()
        .unwrap()
        .trim()
        .to_owned();
    assert!(!id.is_empty(), "{launched}");
    assert_eq!(launched["content"][0]["text"], id.as_str());

    let listed = tool(&mut call, "cones_ls", serde_json::json!({"dir": f.work}));
    let rows = listed["structuredContent"]["rows"].as_array().unwrap();
    assert!(
        rows.iter()
            .any(|r| r["session"]["session_id"] == id.as_str()),
        "{listed}"
    );

    // pi reports no state of its own here, so the wait runs out, which is an answer.
    let waited = tool(
        &mut call,
        "cones_wait",
        serde_json::json!({"ids": [id], "timeout_seconds": 0.5}),
    );
    assert_eq!(waited["isError"], false, "{waited}");
    assert_eq!(waited["structuredContent"]["timed_out"], true, "{waited}");

    // A parent that names no live session is refused, as `cones launch --parent` refuses it.
    let orphan = tool(
        &mut call,
        "cones_launch",
        serde_json::json!({"dir": f.work, "harness": "pi", "parent": "no-such-session"}),
    );
    assert_eq!(orphan["isError"], true, "{orphan}");
    assert!(orphan.to_string().contains("no live session"), "{orphan}");

    // An argument the command does not take is refused before anything runs.
    let refused = tool(
        &mut call,
        "cones_stop",
        serde_json::json!({"id": id, "force": true}),
    );
    assert_eq!(refused["isError"], true);
    assert!(
        refused["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("takes no argument force"),
        "{refused}"
    );

    let hosted = f.hosted_pids();
    let stopped = tool(&mut call, "cones_stop", serde_json::json!({"id": id}));
    assert_eq!(stopped["isError"], false, "{stopped}");
    assert!(
        stopped["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("stopped the cones terminal running pi"),
        "{stopped}"
    );
    for pid in hosted {
        wait_until("the stopped client to exit", || !alive(pid));
    }
    drop(input);
    child.wait().unwrap();
}

/// OpenCode 2 prints help to stdout, runs each client through a shared service unless told
/// otherwise, and has no interactive `--model`. A launch still starts, in its own server, on
/// the model it was asked for. Help on stderr without `--standalone` is 1.x, and keeps the flag.
#[test]
fn an_opencode_launch_fits_the_version_installed() {
    let f = Fixture::new();
    fs::write(
        &f.jobs,
        fs::read_to_string(&f.jobs)
            .unwrap()
            .replace("opencode_enabled: false", "opencode_enabled: true"),
    )
    .unwrap();
    let native = f.path().join(".local/bin/opencode");
    let command = |help: &str, stream: &str| {
        fs::write(
            &native,
            format!("#!/bin/sh\nprintf '%s\\n' '{help}' >&{stream}\n"),
        )
        .unwrap();
        fs::set_permissions(&native, fs::Permissions::from_mode(0o700)).unwrap();
        let out = f.cones(
            "ok",
            &[
                "launch",
                "--dir",
                f.work.to_str().unwrap(),
                "--harness",
                "opencode",
                "--model",
                "provider/model",
                "--print-command",
                "go",
            ],
        );
        assert!(out.status.success(), "{}", stderr(&out));
        String::from_utf8(out.stdout).unwrap()
    };
    let two = command("--standalone --prompt --session", "1");
    assert!(two.contains("'--standalone'"), "{two}");
    assert!(!two.contains("'--model'"), "{two}");
    assert!(
        two.contains(r#"OPENCODE_CONFIG_CONTENT='{"model":"provider/model"}'"#),
        "{two}"
    );
    let one = command("--prompt --session --model", "2");
    assert!(!one.contains("--standalone"), "{one}");
    assert!(one.contains("'--model' 'provider/model'"), "{one}");
    assert!(!one.contains("OPENCODE_CONFIG_CONTENT"), "{one}");
}

/// A node process given a long argument was SIGKILLed on a real Mac before it ran a line, so a
/// long instruction reaches pi through its reporter instead of its command line.
#[test]
fn a_long_instruction_reaches_pi_off_its_command_line() {
    let f = Fixture::new();
    let print = |prompt: &str| {
        let out = f.cones(
            "ok",
            &[
                "launch",
                "--dir",
                f.work.to_str().unwrap(),
                "--harness",
                "pi",
                "--print-command",
                prompt,
            ],
        );
        assert!(out.status.success(), "{}", stderr(&out));
        String::from_utf8(out.stdout).unwrap()
    };
    let short = print("short instruction");
    assert!(
        short.trim_end().ends_with("'--' 'short instruction'"),
        "{short}"
    );
    assert!(!short.contains("CONES_PI_PROMPT"), "{short}");
    let prompt = "word ".repeat(400);
    let long = print(&prompt);
    assert!(long.contains("CONES_PI_PROMPT="), "{long}");
    assert!(long.contains(prompt.trim_end()), "{long}");
    assert!(!long.contains("'--'"), "{long}");
}

/// A session running under another listed one is its helper while it runs, with no record of
/// it: `opencode run` from an agent's shell has no `--parent` to name.
#[test]
fn a_session_running_under_another_is_listed_beneath_it() {
    let f = Fixture::new();
    let mut parent = Command::new("/bin/sh")
        .args(["-c", "sleep 30 & echo $!; wait"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(parent.stdout.take().unwrap()),
        &mut line,
    )
    .unwrap();
    let helper: u32 = line.trim().parse().unwrap();
    let mut unrelated = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let row = |id: &str, pid: u32| -> cones::fleet::Session {
        serde_json::from_value(serde_json::json!({
            "session_id": id, "harness": "opencode", "cwd": "/work", "state": "-", "pid": pid,
        }))
        .unwrap()
    };
    let mut rows = vec![
        row("agent", parent.id()),
        row("helper", helper),
        row("alone", unrelated.id()),
    ];
    cones::relations::apply(&[], &f.path().join("claude"), &mut rows, true);
    let launcher: Vec<_> = rows.iter().map(|s| s.launched_by.as_deref()).collect();
    assert_eq!(launcher, [None, Some("agent"), None]);
    // `cones ls` asks for forks alone, and places nothing.
    rows[1].launched_by = None;
    cones::relations::apply(&[], &f.path().join("claude"), &mut rows, false);
    assert_eq!(rows[1].launched_by, None);
    let _ = std::process::Command::new("/bin/kill")
        .arg(helper.to_string())
        .status();
    parent.kill().unwrap();
    parent.wait().unwrap();
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
}
