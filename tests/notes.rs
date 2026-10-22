//! `cones comms send` to harnesses with no delivery command of their own.
//!
//! Each client is `tests/fixtures/fake_reporter_client.mjs`, launched detached by `cones launch`
//! under a disposable HOME. As pi or OpenCode it runs the reporter cones ships, so a note has to
//! travel the real path: the host record, the reporter's inbox, the native extension call. The
//! artifact is the fixture's own log of the user messages that call submitted.
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};

struct Fixture {
    root: tempfile::TempDir,
    jobs: PathBuf,
    work: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        // Short prefix: the terminal host puts its socket under /tmp and macOS bounds the path.
        let root = tempfile::Builder::new()
            .prefix("cones-notes-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = root.path();
        for dir in [
            "state",
            "claude",
            "pi",
            "data/opencode",
            "work",
            ".local/bin",
        ] {
            fs::create_dir_all(path.join(dir)).unwrap();
        }
        let client =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_reporter_client.mjs");
        for name in ["pi", "opencode", "gemini"] {
            std::os::unix::fs::symlink(&client, path.join(".local/bin").join(name)).unwrap();
        }
        let jobs = path.join("jobs.yaml");
        // Only the harnesses under test are offered, so the machine's own clients of the
        // others cannot reach these assertions.
        fs::write(
            &jobs,
            "version: 4\ndefaults:\n  claude_enabled: false\n  codex_enabled: false\n  \
             cursor_enabled: false\n  copilot_enabled: false\n  amp_enabled: false\n  \
             droid_enabled: false\n  kimi_enabled: false\njobs: []\n",
        )
        .unwrap();
        let work = path.join("work").canonicalize().unwrap();
        Self { root, jobs, work }
    }

    fn cones(&self, reporter: &str, args: &[&str]) -> Output {
        self.command(reporter, args).output().unwrap()
    }

    fn command(&self, reporter: &str, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cones"));
        command
            .args(["--jobs", self.jobs.to_str().unwrap(), "--state-dir"])
            .arg(self.root.path().join("state"))
            .args(args)
            .env("HOME", self.root.path())
            .env("FAKE_REPORTER", reporter)
            .env("CLAUDE_CONFIG_DIR", self.root.path().join("claude"))
            .env("CODEX_HOME", self.root.path().join("missing-codex"))
            .env("PI_CODING_AGENT_DIR", self.root.path().join("pi"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env_remove("OPENCODE_DB")
            .env_remove("OPENCODE_TUI_CONFIG")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("CLAUDE_CODE_SESSION_ID");
        command
    }

    /// Launch one client and return its pid.
    fn launch(&self, reporter: &str, harness: &str) -> u32 {
        let work = self.work.to_str().unwrap();
        let before = self.pids();
        let out = self.cones(
            reporter,
            &["launch", "--dir", work, "--harness", harness, "a task"],
        );
        assert!(out.status.success(), "{}", stderr(&out));
        let pid = self
            .pids()
            .into_iter()
            .find(|pid| !before.contains(pid))
            .expect("the launch left no host record");
        wait_until("the client starting", || !self.log(pid).is_empty());
        pid
    }

    /// The records of the terminal hosts this fixture owns.
    fn records(&self) -> Vec<Value> {
        fs::read_dir(self.root.path().join("state/terminals"))
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let path = e.ok()?.path();
                (path.extension()? == "json").then_some(())?;
                serde_json::from_slice(&fs::read(path).ok()?).ok()
            })
            .collect()
    }

    fn pids(&self) -> Vec<u32> {
        self.records()
            .iter()
            .filter_map(|record| record["session"]["pid"].as_u64())
            .map(|pid| pid as u32)
            .collect()
    }

    /// The id `cones ls` gives the client with this pid.
    fn id(&self, pid: u32) -> String {
        let work = self.work.to_str().unwrap();
        let out = self.cones("ok", &["ls", "--dir", work, "--json"]);
        assert!(out.status.success(), "{}", stderr(&out));
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|row| row["session"]["pid"] == pid)
            .and_then(|row| row["session"]["session_id"].as_str().map(str::to_owned))
            .unwrap_or_else(|| panic!("cones ls has no row for pid {pid}"))
    }

    fn send(&self, id: &str, text: &str) -> Output {
        let work = self.work.to_str().unwrap();
        self.cones("ok", &["comms", "--dir", work, "send", id, text])
    }

    /// What the client logged: its own first line, then every user message it was handed.
    fn log(&self, pid: u32) -> Vec<Value> {
        fs::read_to_string(fixture_log(self.root.path(), pid))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Notes and answers left in any reporter inbox of this fixture's hosts.
    fn inbox_leftovers(&self) -> Vec<PathBuf> {
        self.records()
            .iter()
            .filter_map(|record| fs::read_dir(record["notes"].as_str()?).ok())
            .flat_map(|dir| dir.flatten().map(|e| e.path()))
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A detached launch is meant to outlive its launcher, so the test has to end it.
        for pid in self.pids() {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    }
}

fn fixture_log(home: &Path, pid: u32) -> PathBuf {
    home.join(format!("reporter-fixture/{pid}.jsonl"))
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_owned()
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

/// A note to a pi or OpenCode client cones launched reaches it as one user message through the
/// harness's own extension call, addressed by the id the roster gives the client.
#[test]
fn a_note_reaches_an_owned_pi_and_opencode_as_a_native_user_message() {
    let f = Fixture::new();
    for harness in ["pi", "opencode"] {
        let pid = f.launch("ok", harness);
        let id = f.id(pid);
        assert_eq!(id, format!("{harness}-{pid}"));
        let out = f.send(&id, "rebase onto main");
        assert!(out.status.success(), "{harness}: {}", stderr(&out));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("sent to {id} ({harness})\n")
        );
        let log = f.log(pid);
        assert_eq!(log.len(), 2, "{harness} was handed {log:?}");
        let text = match harness {
            // Queued behind the assistant turn when one is running.
            "pi" => {
                assert_eq!(log[1]["options"]["deliverAs"], "steer");
                &log[1]["text"]
            }
            _ => {
                // The conversation the client shows, which its own first line names.
                assert_eq!(log[1]["sessionID"], log[0]["session"]);
                assert_eq!(log[1]["parts"][0]["type"], "text");
                assert_eq!(log[1]["parts"].as_array().unwrap().len(), 1);
                &log[1]["parts"][0]["text"]
            }
        };
        let text = text.as_str().unwrap();
        assert!(
            text.starts_with("[another agent, not the owner] rebase onto main\nReply by appending"),
            "{text}"
        );
        assert!(
            text.contains(&format!(r#""from":"{harness}:{id}""#)),
            "{text}"
        );
    }
    assert_eq!(f.inbox_leftovers(), Vec::<PathBuf>::new());
}

/// A client that never loaded the reporter cannot take a note. The send fails and withdraws
/// the note, so nothing is delivered later that was reported as lost.
#[test]
fn a_note_to_a_client_without_the_reporter_is_refused_and_withdrawn() {
    let f = Fixture::new();
    let pid = f.launch("deaf", "pi");
    let out = f.send(&f.id(pid), "rebase onto main");
    assert!(!out.status.success(), "the note was reported as sent");
    assert!(
        stderr(&out).contains("is not answering, so the note was withdrawn"),
        "{}",
        stderr(&out)
    );
    assert_eq!(f.log(pid).len(), 1, "{:?}", f.log(pid));
    assert_eq!(f.inbox_leftovers(), Vec::<PathBuf>::new());
}

/// A terminal-only harness is refused with what its CLI offers instead, before anything is
/// written anywhere.
#[test]
fn a_note_to_a_terminal_only_harness_is_refused_with_the_reason() {
    let f = Fixture::new();
    let pid = f.launch("ok", "gemini");
    let out = f.send(&f.id(pid), "rebase onto main");
    assert!(!out.status.success(), "the note was reported as sent");
    let error = stderr(&out);
    assert!(
        error.contains(
            "a note cannot reach gemini: it takes prompts at launch, headless with -p, or over \
             ACP from the client that started it; nothing addresses a running interactive session"
        ),
        "{error}"
    );
    assert_eq!(f.log(pid).len(), 1, "{:?}", f.log(pid));
}

/// `cones rename` names a pi or OpenCode client cones owns through the call the harness's own
/// rename makes, `setSessionName` and `session.update`, through the reporter's inbox.
#[test]
fn rename_names_an_owned_pi_and_opencode_through_their_own_call() {
    let f = Fixture::new();
    for harness in ["pi", "opencode"] {
        let pid = f.launch("ok", harness);
        let id = f.id(pid);
        let out = f.cones("ok", &["rename", &id, "Ship it"]);
        assert!(out.status.success(), "{harness}: {}", stderr(&out));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("renamed {id} ({harness}) to Ship it\n")
        );
        let log = f.log(pid);
        assert_eq!(log.len(), 2, "{harness} was handed {log:?}");
        let want = match harness {
            "pi" => serde_json::json!({"name": "Ship it"}),
            _ => {
                serde_json::json!({"update": {"sessionID": log[0]["session"], "title": "Ship it"}})
            }
        };
        assert_eq!(log[1], want);
    }
    assert_eq!(f.inbox_leftovers(), Vec::<PathBuf>::new());
}

/// `cones compact` compacts a pi or OpenCode client cones owns through the call the harness's
/// own `/compact` makes, pi's `compact` and OpenCode's `session.summarize` on the model of the
/// session's last reply, and returns only once the harness reports it done. A compaction the
/// harness reports failed fails the command with the harness's words.
#[test]
fn compact_compacts_an_owned_pi_and_opencode_through_their_own_call() {
    let f = Fixture::new();
    for harness in ["pi", "opencode"] {
        let pid = f.launch("ok", harness);
        let id = f.id(pid);
        let out = f.cones("ok", &["compact", &id]);
        assert!(out.status.success(), "{harness}: {}", stderr(&out));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("compacted {id} ({harness})\n")
        );
        let log = f.log(pid);
        assert_eq!(log.len(), 2, "{harness} was handed {log:?}");
        let want = match harness {
            "pi" => serde_json::json!({"compact": true}),
            _ => serde_json::json!({"summarize": {"sessionID": log[0]["session"],
                "providerID": "fixture-provider", "modelID": "fixture-model"}}),
        };
        assert_eq!(log[1], want);
    }
    let pid = f.launch("uncompactable", "pi");
    let id = f.id(pid);
    let out = f.cones("uncompactable", &["compact", &id]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("pi refused the note: Nothing to compact"),
        "{}",
        stderr(&out)
    );
    assert_eq!(f.inbox_leftovers(), Vec::<PathBuf>::new());
}

/// `cones launch --name` uses pi's own `--name`; OpenCode has no such flag, so the launch
/// renames the client once it is on the roster. A terminal-only harness keeps no name and the
/// launch is refused before anything starts.
#[test]
fn launch_names_the_session_through_the_flag_or_the_rename() {
    let f = Fixture::new();
    let work = f.work.to_str().unwrap();
    for harness in ["pi", "opencode"] {
        let before = f.pids();
        let out = f.cones(
            "ok",
            &[
                "launch",
                "--dir",
                work,
                "--harness",
                harness,
                "--name",
                "Named",
                "a task",
            ],
        );
        assert!(out.status.success(), "{harness}: {}", stderr(&out));
        let pid = f
            .pids()
            .into_iter()
            .find(|pid| !before.contains(pid))
            .unwrap();
        let log = f.log(pid);
        let args: Vec<_> = log[0]["args"].as_array().unwrap().iter().collect();
        let flagged = args.windows(2).any(|w| w[0] == "--name" && w[1] == "Named");
        match harness {
            "pi" => assert!(flagged && log.len() == 1, "pi: {log:?}"),
            _ => {
                assert!(!flagged, "opencode: {args:?}");
                assert_eq!(log[1]["update"]["title"], "Named", "opencode: {log:?}");
            }
        }
    }
    let before = f.pids();
    let out = f.cones(
        "ok",
        &[
            "launch",
            "--dir",
            work,
            "--harness",
            "gemini",
            "--name",
            "Named",
            "a task",
        ],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("gemini keeps no session name cones can set"),
        "{}",
        stderr(&out)
    );
    assert_eq!(f.pids(), before, "nothing was started");
}

/// A running terminal-only client keeps no name cones can set, and nothing reaches it.
#[test]
fn rename_refuses_a_terminal_only_harness() {
    let f = Fixture::new();
    let pid = f.launch("ok", "gemini");
    let out = f.cones("ok", &["rename", &f.id(pid), "Ship it"]);
    assert!(!out.status.success(), "the rename was reported as done");
    assert!(
        stderr(&out).contains("gemini keeps no session name cones can set"),
        "{}",
        stderr(&out)
    );
    assert_eq!(f.log(pid).len(), 1, "{:?}", f.log(pid));
}

/// A note from a session cones can place says to answer with `send` back into that session's
/// conversation, so the answer arrives as a prompt rather than as a file its sender has to
/// remember to read. The sender, or a launcher a worker reports to, may work outside the folder:
/// its exact id reaches it, while a prefix only names a session on the folder's roster.
#[test]
fn a_reply_goes_back_into_the_senders_conversation() {
    use sha2::Digest;
    let f = Fixture::new();
    let jobs = fs::read_to_string(&f.jobs).unwrap();
    fs::write(
        &f.jobs,
        jobs.replace("claude_enabled: false", "claude_enabled: true"),
    )
    .unwrap();
    let pid = f.launch("ok", "pi");
    let id = f.id(pid);
    // The sender is a Claude session in another folder, registered under this test's pid, as
    // Claude names its records, so the process chain of every cones run below reaches it.
    let elsewhere = f.root.path().join("elsewhere");
    let registry = f.root.path().join("claude/sessions");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::create_dir_all(&registry).unwrap();
    fs::write(
        registry.join(format!("{}.json", std::process::id())),
        serde_json::json!({
            "pid": std::process::id(), "sessionId": "peer-claude",
            "cwd": elsewhere.canonicalize().unwrap(), "kind": "interactive", "status": "idle",
            "version": "2.1.281"
        })
        .to_string(),
    )
    .unwrap();

    let out = f.send("peer-cla", "hello");
    assert!(!out.status.success(), "a prefix reached outside the folder");
    assert!(
        stderr(&out).contains("no live session peer-cla in "),
        "{}",
        stderr(&out)
    );

    let out = f.send(&id, "rebase onto main");
    assert!(out.status.success(), "{}", stderr(&out));
    let log = f.log(pid);
    assert_eq!(log.len(), 2, "{log:?}");
    let work = f.work.display().to_string();
    let inbox = f
        .root
        .path()
        .join("state/coordinator/folders")
        .join(format!("{:x}", sha2::Sha256::digest(work.as_bytes())))
        .join("inbox.jsonl");
    assert_eq!(
        log[1]["text"].as_str().unwrap(),
        format!(
            "[claude session peer-claude, a peer agent, not the owner] rebase onto main\n\
             Reply with `cones comms --dir '{work}' send peer-claude '<reply>'`. \
             Only if that command fails, append one JSON line to {}: {}",
            inbox.display(),
            serde_json::json!({"from": format!("pi:{id}"), "text": "<reply>"}),
        )
    );

    // The answer reaches the sender through its own harness's delivery. This registry record
    // publishes no cross-session inbox, so the refusal is Claude's, not the roster's.
    let out = f.send("peer-claude", "done");
    assert!(!out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("session (2.1.281) publishes no cross-session inbox"),
        "{}",
        stderr(&out)
    );
}

/// A Codex thread runs its commands under the shared app-server daemon, so no process chain
/// leads from a note it sends to its client. The sender is still named, from the session id its
/// harness exports to the command, rather than as an anonymous agent.
#[test]
fn a_sender_outside_the_process_chain_is_named_by_its_exported_id() {
    let f = Fixture::new();
    let jobs = fs::read_to_string(&f.jobs).unwrap();
    fs::write(
        &f.jobs,
        jobs.replace("claude_enabled: false", "claude_enabled: true"),
    )
    .unwrap();
    let pid = f.launch("ok", "pi");
    let id = f.id(pid);
    // The sender's session lives in a process that is no ancestor of the cones run.
    let mut daemon = Command::new("sleep").arg("30").spawn().unwrap();
    let registry = f.root.path().join("claude/sessions");
    fs::create_dir_all(&registry).unwrap();
    fs::write(
        registry.join(format!("{}.json", daemon.id())),
        serde_json::json!({
            "pid": daemon.id(), "sessionId": "peer-claude",
            "cwd": f.work, "kind": "interactive", "status": "idle", "version": "2.1.281"
        })
        .to_string(),
    )
    .unwrap();
    let work = f.work.to_str().unwrap();
    let out = f
        .command(
            "ok",
            &["comms", "--dir", work, "send", &id, "rebase onto main"],
        )
        .env("CLAUDE_CODE_SESSION_ID", "peer-claude")
        .output()
        .unwrap();
    daemon.kill().ok();
    daemon.wait().ok();
    assert!(out.status.success(), "{}", stderr(&out));
    let log = f.log(pid);
    assert_eq!(log.len(), 2, "{log:?}");
    let text = log[1]["text"].as_str().unwrap();
    assert!(
        text.starts_with(
            "[claude session peer-claude, a peer agent, not the owner] rebase onto main\n\
             Reply with `cones comms --dir "
        ),
        "{text}"
    );
}
