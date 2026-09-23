//! `cones stop ID`, against a fake native CLI and a real detached host. No model calls.
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, fs::PermissionsExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

/// Answers `stop` the way the installed CLI does — idempotently — and records the id and native
/// home it was given. Every other subcommand fails, so a stop that reached for `rm` cannot pass
/// quietly. A `refuse` marker in the home turns the next stop into a native failure.
const FAKE: &str = r#"#!/bin/sh
if [ "$1" = stop ] && [ "$2" = --help ]; then
    echo "Usage: claude stop <id>"
    exit 0
fi
home=${CLAUDE_CONFIG_DIR:?a stop must name the native home of its session}
printf '%s\n' "$* $home" >> "$home/calls.log"
if [ "$1" != stop ]; then
    echo "cones asked for claude $1, which is not a stop" >&2
    exit 3
fi
if [ -e "$home/refuse" ]; then
    echo "background session $2 could not be stopped" >&2
    exit 1
fi
echo "stopped $2"
"#;

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("cones-stop-test-")
            .tempdir_in("/tmp")
            .unwrap();
        let bin = root.path().join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("claude"), FAKE).unwrap();
        fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(root.path().join(".claude/sessions")).unwrap();
        fs::create_dir_all(root.path().join("state")).unwrap();
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.path().join(".claude")
    }

    fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    /// A live registry entry for `id`, reported against a disposable process cones must not touch.
    fn live(&self, id: &str, kind: &str, pid: u32) {
        fs::write(
            self.home().join(format!("sessions/{id}.json")),
            json!({"sessionId": id, "pid": pid, "kind": kind, "cwd": self.root.path(), "status": "busy"})
                .to_string(),
        )
        .unwrap();
    }

    /// A transcript where Claude keeps one, so the tests can see it survive the stop.
    fn transcript(&self, id: &str) -> PathBuf {
        let project = self
            .root
            .path()
            .to_string_lossy()
            .replace(|c: char| !c.is_ascii_alphanumeric(), "-");
        let dir = self.home().join("projects").join(project);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(&path, "{\"type\":\"user\"}\n").unwrap();
        path
    }

    fn stop(&self, id: &str) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_cones"))
            .env("HOME", self.root.path())
            .env_remove("CLAUDE_CONFIG_DIR")
            .args(["--state-dir", self.state().to_str().unwrap(), "stop", id])
            .output()
            .unwrap()
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.home().join("calls.log")).unwrap_or_default()
    }

    fn pi(&self) -> PathBuf {
        fs::create_dir_all(self.root.path().join(".pi/agent")).unwrap();
        let program = self.root.path().join(".local/bin/pi");
        fs::write(
            &program,
            "#!/usr/bin/python3\nimport time\nwhile True:\n    time.sleep(1)\n",
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        program
    }

    fn rows(&self) -> Vec<serde_json::Value> {
        let out = Command::new(env!("CARGO_BIN_EXE_cones"))
            .env("HOME", self.root.path())
            .env_remove("CLAUDE_CONFIG_DIR")
            .args([
                "--state-dir",
                self.state().to_str().unwrap(),
                "ls",
                "--json",
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        text(&out.stdout)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

/// A process cones may observe but must never end on its own.
struct Idle(Child);
impl Idle {
    fn new() -> Self {
        Self(
            Command::new("/bin/sleep")
                .arg("120")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
    fn alive(&self) -> bool {
        unsafe { libc::kill(self.0.id() as i32, 0) == 0 }
    }
}
impl Drop for Idle {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn background_stop_names_the_session_home_keeps_the_conversation_and_signals_nothing() {
    let f = Fixture::new();
    let id = "aaaaaaaa-1111-4111-8111-111111111111";
    let idle = Idle::new();
    f.live(id, "bg", idle.0.id());
    let transcript = f.transcript(id);

    let out = f.stop(id);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let printed = text(&out.stdout);
    assert!(printed.contains("stopped claude session"), "{printed}");
    assert!(printed.contains(id), "{printed}");
    assert!(
        printed.contains(&f.home().to_string_lossy().into_owned()),
        "{printed}"
    );
    // The exact native target: the `stop` subcommand, the short id, and the session's own home.
    assert_eq!(
        f.calls().trim(),
        format!("stop aaaaaaaa {}", f.home().display())
    );
    assert!(
        transcript.exists(),
        "a stop must leave the conversation resumable"
    );
    assert!(
        f.home().join(format!("sessions/{id}.json")).exists(),
        "a stop must not remove the job record; that is what `claude rm` is for"
    );
    assert!(
        idle.alive(),
        "a stop must not signal the process behind the session"
    );

    // The installed `claude stop` is idempotent, so a second stop is a second native call whose
    // success is the CLI's answer and not a cones-side shortcut.
    assert!(f.stop(id).status.success());
    assert_eq!(f.calls().lines().count(), 2, "{}", f.calls());
}

/// A native refusal is the answer, not a cones-side interpretation of it: the message and the
/// exact command are reported and the exit is a failure.
#[test]
fn a_native_stop_failure_is_reported_with_the_command_that_produced_it() {
    let f = Fixture::new();
    fs::write(f.home().join("refuse"), "").unwrap();
    let id = "bbbbbbbb-2222-4222-8222-222222222222";
    let idle = Idle::new();
    f.live(id, "bg", idle.0.id());

    let out = f.stop(id);
    assert_eq!(out.status.code(), Some(1));
    let failure = text(&out.stderr);
    assert!(
        failure.contains("background session bbbbbbbb could not be stopped"),
        "{failure}"
    );
    assert!(failure.contains("claude stop bbbbbbbb"), "{failure}");
    assert!(idle.alive(), "a failed stop must not fall back to a signal");
}

/// A background session the daemon has already settled is still the native CLI's to answer for,
/// so the stop goes to it rather than cones deciding the work is over.
#[test]
fn an_exited_background_session_still_goes_to_the_native_stop() {
    let f = Fixture::new();
    let id = "cccccccc-3333-4333-8333-333333333333";
    let job = f.home().join("jobs/cccccccc");
    fs::create_dir_all(&job).unwrap();
    fs::write(
        job.join("state.json"),
        json!({"sessionId": id, "state": "stopped", "cwd": f.root.path()}).to_string(),
    )
    .unwrap();

    let out = f.stop(id);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        f.calls().trim(),
        format!("stop cccccccc {}", f.home().display())
    );
    assert!(job.join("state.json").exists(), "the record must survive");
}

#[test]
fn an_unknown_id_and_a_terminal_cones_does_not_own_are_refused_before_any_native_call() {
    let f = Fixture::new();
    let unknown = f.stop("dddddddd-4444-4444-8444-444444444444");
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        text(&unknown.stderr).contains("is not a live session"),
        "{}",
        text(&unknown.stderr)
    );

    let interactive = "eeeeeeee-5555-4555-8555-555555555555";
    let idle = Idle::new();
    f.live(interactive, "interactive", idle.0.id());
    let out = f.stop(interactive);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("runs in a terminal cones does not own"),
        "{}",
        text(&out.stderr)
    );
    assert!(idle.alive());
    assert_eq!(f.calls(), "", "a refusal must reach no native CLI");
}

/// The capability boundary is declared, not improvised: only a harness with a native `stop`
/// operation can be stopped, and Codex 0.155 has no per-thread stop to declare. Its `archive`
/// and `delete` are history operations and its daemon stop ends every other thread too.
#[test]
fn only_a_declared_native_stop_operation_makes_a_harness_stoppable() {
    let claude = cones::harness::by_name("claude").unwrap();
    assert_eq!(
        claude
            .operations
            .stop
            .as_ref()
            .map(|operation| operation.args.clone()),
        Some(vec!["stop".to_owned(), "{short_id}".to_owned()])
    );
    for name in ["codex", "pi", "opencode"] {
        let spec = cones::harness::by_name(name).unwrap();
        assert!(
            spec.operations.stop.is_none(),
            "{name} claims a native session stop; verify it before declaring one"
        );
    }
}

/// The host acknowledges a stop only after the native client it owns has exited, so a successful
/// return is the end of the work and not the end of a row.
#[test]
fn an_owned_terminal_stop_waits_for_its_native_client_to_exit() {
    let f = Fixture::new();
    let state = f.state();
    let session = "ffffffff-6666-4666-8666-666666666666";
    let host = Host::start(&f, "claude", session, Path::new("/bin/cat"));

    let message = cones::stop::session(&state, &f.home(), session).unwrap();
    assert!(
        message.contains("cones terminal running claude") && message.contains(session),
        "{message}"
    );
    assert_eq!(
        unsafe { libc::kill(host.pid(), 0) },
        -1,
        "the acknowledgement must follow the native client's exit"
    );
    assert!(!host.path(&f).exists());
    assert_eq!(f.calls(), "", "an owned terminal is stopped by its host");

    // With the host gone the same id is no longer stoppable, and says so rather than succeeding.
    let error = cones::stop::session(&state, &f.home(), session).unwrap_err();
    assert!(
        format!("{error:#}").contains("is not a live session"),
        "{error:#}"
    );
}

#[test]
fn a_discovered_native_id_stops_only_its_owned_client_and_keeps_history() {
    let f = Fixture::new();
    let host = Host::start(&f, "claude", "launch-placeholder", Path::new("/bin/cat"));
    let other = Host::start(&f, "claude", "other-launch", Path::new("/bin/cat"));
    let id = "12121212-1111-4111-8111-111111111111";
    f.live(id, "interactive", host.pid() as u32);
    let transcript = f.transcript(id);

    let out = f.stop(id);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains(id));
    assert!(!host.alive(), "the selected client must have exited");
    assert!(!host.path(&f).exists());
    assert!(other.alive(), "another owned session must keep running");
    assert!(other.path(&f).exists());
    assert!(transcript.exists(), "history must survive stopping");
    assert_eq!(f.calls(), "", "an owned client must use its host");
}

#[test]
fn the_pi_process_id_printed_by_ls_stops_its_owned_terminal() {
    let f = Fixture::new();
    let host = Host::start(&f, "pi", "pi-launch-placeholder", &f.pi());
    let id = format!("pi-{}", host.pid());
    let rows = f.rows();
    assert!(
        rows.iter().any(|row| row["session"]["session_id"] == id),
        "the process id must come from ls: {rows:?}"
    );

    let out = f.stop(&id);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains(&id));
    assert!(
        !host.alive(),
        "stop must wait for the listed client to exit"
    );
    assert!(!host.path(&f).exists());
    assert_eq!(f.calls(), "");
}

#[test]
fn a_pi_conversation_id_replacing_the_launch_id_stops_its_owned_terminal() {
    let f = Fixture::new();
    let host = Host::start(&f, "pi", "pi-launch-placeholder", &f.pi());
    let cwd = fs::canonicalize(f.root.path()).unwrap();
    let dir = cones::pi::session_dir(&f.root.path().join(".pi/agent"), &cwd);
    fs::create_dir_all(&dir).unwrap();
    let id = "56565656-1111-4111-8111-111111111111";
    let transcript = dir.join(format!("{id}.jsonl"));
    fs::write(
        &transcript,
        format!(
            "{}\n",
            json!({"type": "session", "id": id, "cwd": cwd,
                   "timestamp": chrono::Utc::now().to_rfc3339()})
        ),
    )
    .unwrap();
    let rows = f.rows();
    assert!(
        rows.iter().any(
            |row| row["session"]["session_id"] == format!("pi-{}", host.pid())
                && row["session"]["native_id"] == id
        ),
        "ls lists the client by its process and names the conversation it reports: {rows:?}"
    );

    let out = f.stop(id);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains(id));
    assert!(!host.alive());
    assert!(!host.path(&f).exists());
    assert!(transcript.exists());
    assert_eq!(f.calls(), "");
}

#[test]
fn an_ambiguous_owned_id_stops_neither_client() {
    let f = Fixture::new();
    let first = Host::start(&f, "claude", "same-id", Path::new("/bin/cat"));
    let second = Host::start(&f, "claude", "same-id", Path::new("/bin/cat"));

    let out = f.stop("same-id");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("names 2 cones terminals"),
        "{}",
        text(&out.stderr)
    );
    for host in [&first, &second] {
        assert!(host.alive());
        assert!(host.path(&f).exists());
    }
    assert_eq!(f.calls(), "");
}

#[test]
fn a_discovered_id_with_another_harness_does_not_claim_an_owned_process() {
    let f = Fixture::new();
    let host = Host::start(&f, "pi", "pi-launch-placeholder", Path::new("/bin/cat"));
    let id = "34343434-1111-4111-8111-111111111111";
    f.live(id, "interactive", host.pid() as u32);

    let out = f.stop(id);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("runs in a terminal cones does not own"),
        "{}",
        text(&out.stderr)
    );
    assert!(
        host.alive(),
        "matching only a pid must not stop another harness"
    );
    assert!(host.path(&f).exists());
    assert_eq!(f.calls(), "");
}

struct Host {
    child: Child,
    record: serde_json::Value,
}

impl Host {
    fn start(f: &Fixture, harness: &str, session: &str, program: &Path) -> Self {
        let state = f.state();
        fs::create_dir_all(state.join("terminals")).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let socket = PathBuf::from(format!("/tmp/cones-stop-test-{id}"));
        let record = json!({
            "id": id, "socket": socket, "what": "fixture",
            "session": {"session_id": session, "harness": harness, "kind": "interactive",
                        "cwd": f.root.path(), "state": "-"}
        });
        let launch = json!({
            "program": program.as_os_str().as_bytes(), "args": [],
            "env": [["HOME".as_bytes(), f.root.path().as_os_str().as_bytes()]],
            "cwd": f.root.path(), "rows": 12, "cols": 80,
            "colors": {"fg": "rgb:e4e4/e4e4/e4e4", "bg": "rgb:1414/1414/1414"},
            "shell": true, "record": record, "state": state
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_cones"))
            .arg("__terminal-host")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let bytes = serde_json::to_vec(&launch).unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        stdin.write_all(&bytes).unwrap();
        drop(stdin);
        let mut host = Self { child, record };
        host.record = ready(&mut host)["Ok"].clone();
        host
    }

    fn pid(&self) -> i32 {
        self.record["session"]["pid"].as_u64().unwrap() as i32
    }

    fn alive(&self) -> bool {
        unsafe { libc::kill(self.pid(), 0) == 0 }
    }

    fn path(&self, f: &Fixture) -> PathBuf {
        f.state().join(format!(
            "terminals/{}.json",
            self.record["id"].as_str().unwrap()
        ))
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none()
            && let Some(pid) = self.record["session"]["pid"].as_u64()
        {
            // Only the disposable client created by this fixture.
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(socket) = self.record["socket"].as_str() {
            let _ = fs::remove_file(socket);
        }
    }
}

/// The host's readiness reply, or a panic with whatever it said instead.
fn ready(host: &mut Host) -> serde_json::Value {
    let stdout = host.child.stdout.as_mut().unwrap();
    let mut descriptor = libc::pollfd {
        fd: stdout.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert!(
        unsafe { libc::poll(&mut descriptor, 1, 5000) } > 0,
        "the host sent no readiness reply"
    );
    let mut header = [0; 4];
    stdout.read_exact(&mut header).unwrap();
    let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
    stdout.read_exact(&mut bytes).unwrap();
    let reply: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(reply.get("Ok").is_some(), "{reply}");
    reply
}
