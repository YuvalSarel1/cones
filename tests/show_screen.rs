//! `cones show ID` for the ids a live row is listed under: a hosted terminal with no
//! conversation on disk, and a client listed by its process that reports a conversation.
//! It also covers the conversations Copilot and Cursor keep on disk, in the record shapes
//! their installed CLIs wrote. Fixtures stand in for the CLIs; no model is contacted and no
//! native home outside the fixture is read or written.
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
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
            .prefix("cones-show-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = root.path();
        for dir in ["state", "claude", "work", ".local/bin", ".pi/agent"] {
            fs::create_dir_all(path.join(dir)).unwrap();
        }
        std::os::unix::fs::symlink(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_terminal.py"),
            path.join(".local/bin/gemini"),
        )
        .unwrap();
        let jobs = path.join("jobs.yaml");
        // Only the harness under test is offered, so the machine's own clients of the others
        // cannot reach these assertions.
        fs::write(
            &jobs,
            "version: 4\ndefaults:\n  claude_enabled: false\n  codex_enabled: false\n  \
             opencode_enabled: false\n  amp_enabled: false\n  droid_enabled: false\n  \
             kimi_enabled: false\n  grok_enabled: false\njobs: []\n",
        )
        .unwrap();
        let work = path.join("work").canonicalize().unwrap();
        Self { root, jobs, work }
    }

    fn cones(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cones"))
            .args(["--jobs", self.jobs.to_str().unwrap(), "--state-dir"])
            .arg(self.root.path().join("state"))
            .args(args)
            .env("HOME", self.root.path())
            .env("CLAUDE_CONFIG_DIR", self.root.path().join("claude"))
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("PI_CODING_AGENT_SESSION_DIR")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.cones(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// The host records this fixture owns: one per launch.
    fn hosts(&self) -> Vec<Value> {
        let Ok(entries) = fs::read_dir(self.root.path().join("state/terminals")) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| {
                let path = e.ok()?.path();
                (path.extension()? == "json").then_some(())?;
                serde_json::from_slice(&fs::read(path).ok()?).ok()
            })
            .collect()
    }

    /// Read until the fixture client has drawn `expected`, and return that read.
    fn shown(&self, args: &[&str], expected: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let out = self.cones(args);
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            if out.status.success() && text.contains(expected) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "{args:?} never showed {expected:?}: {text}{}",
                String::from_utf8_lossy(&out.stderr)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A detached launch is meant to outlive its launcher, so the test has to end it.
        for pid in self
            .hosts()
            .iter()
            .filter_map(|r| r["session"]["pid"].as_u64())
        {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    }
}

fn send(stream: &mut impl Write, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(&bytes).unwrap();
}

fn receive(stream: &mut impl Read) -> Value {
    let mut len = [0; 4];
    stream.read_exact(&mut len).unwrap();
    let mut bytes = vec![0; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn a_hosted_terminal_only_session_shows_its_screen_marked_as_one_and_is_left_untouched() {
    let f = Fixture::new();
    let id = f
        .ok(&[
            "launch",
            "--dir",
            f.work.to_str().unwrap(),
            "--harness",
            "gemini",
            "say hello",
        ])
        .trim()
        .to_owned();
    assert!(!id.is_empty());
    let hosts = f.hosts();
    assert_eq!(hosts.len(), 1, "one host per launch: {hosts:?}");
    let pid = hosts[0]["session"]["pid"].as_u64().unwrap() as i32;

    // The host takes its size from the launching terminal, which a test run does not have,
    // so the expected rows follow from the size the read itself reports.
    f.shown(&["show", &id], "fixture ready");
    let report: Value = serde_json::from_str(&f.ok(&["show", &id, "--json"])).unwrap();
    let terminal = &report["terminal"];
    let (rows, cols) = (
        terminal["rows"].as_u64().unwrap() as usize,
        terminal["cols"].as_u64().unwrap(),
    );
    assert!((10..=100).contains(&rows), "{terminal}");
    // 100 numbered rows and the ready line, with the cursor on the row under them.
    let (drawn, omitted) = (rows - 1, 101 - (rows - 1));
    let first = format!("row-{:03}", omitted + 1);

    // The same read as data: a terminal, and no messages to mistake it for a conversation.
    assert_eq!(
        report["session"],
        json!({"harness": "gemini", "session_id": id})
    );
    assert_eq!(report["cwd"], json!(f.work));
    assert!(report.get("messages").is_none(), "{report}");
    assert_eq!(terminal["alternate"], false);
    assert_eq!(terminal["omitted"].as_u64(), Some(omitted as u64));
    let lines: Vec<&str> = terminal["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap())
        .collect();
    assert_eq!(lines.len(), drawn);
    assert_eq!((lines[0], lines[drawn - 2]), (first.as_str(), "row-100"));
    // Whatever flags the launch policy adds, the last drawn line is the client's own.
    let ready = lines[drawn - 1];
    assert!(
        ready.starts_with("fixture ready: ") && ready.ends_with("--prompt-interactive=say hello"),
        "{ready}"
    );

    // The default read is what the terminal shows now, and it says it is not a conversation.
    let screen = f.ok(&["show", &id]);
    let mut text = screen.lines();
    let header = text.next().unwrap();
    assert!(
        header.starts_with(&format!("[terminal screen of gemini {id}, {cols}x{rows};"))
            && header.contains("not a conversation"),
        "{header}"
    );
    assert_eq!(
        text.next().unwrap(),
        format!("[{omitted} earlier lines omitted; --all prints the scrollback]")
    );
    assert_eq!(text.next(), Some(""));
    assert_eq!(text.collect::<Vec<_>>(), lines);

    // The scrollback is there when asked for, and a line count is a line count.
    let all = f.ok(&["show", &id, "--all"]);
    assert!(!all.contains("omitted"), "{all}");
    assert_eq!(all.lines().nth(2), Some("row-001"));
    assert_eq!(all.lines().count(), 2 + 101);
    let two = f.ok(&["show", &id, "--tail", "2"]);
    assert_eq!(
        two.lines().skip(1).collect::<Vec<_>>(),
        [
            "[99 earlier lines omitted; --all prints the scrollback]",
            "",
            "row-100",
            ready
        ]
    );
    assert!(!f.cones(&["show", &id, "--tail", "0"]).status.success());

    // The harness filter applies to a terminal as it does to a conversation.
    let other = f.cones(&["show", &id, "--harness", "pi"]);
    assert!(!other.status.success());
    assert!(
        String::from_utf8_lossy(&other.stderr).contains("no session"),
        "{}",
        String::from_utf8_lossy(&other.stderr)
    );

    // Reading beside an attached dashboard takes nothing from it: the dashboard keeps the
    // terminal, types into it, and the next read shows what the client answered.
    let mut dashboard = UnixStream::connect(hosts[0]["socket"].as_str().unwrap()).unwrap();
    dashboard
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    send(
        &mut dashboard,
        &json!({"Attach": {"version": 1, "id": hosts[0]["id"]}}),
    );
    assert_eq!(receive(&mut dashboard)["Attached"]["session"]["pid"], pid);
    assert_eq!(f.ok(&["show", &id]), screen, "a rerun reproduces the read");
    send(
        &mut dashboard,
        &json!({"Input": "typed-after-show\n".as_bytes()}),
    );
    f.shown(&["show", &id], "RECEIVED:typed-after-show");
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "reading ended the client");
    assert_eq!(f.hosts().len(), 1, "reading replaced the host record");
}

/// A client that only sleeps, under the name discovery looks for, in the fixture's folder.
struct Client(std::process::Child);

impl Client {
    fn start(f: &Fixture, name: &str) -> Self {
        let program = f.root.path().join(".local/bin").join(name);
        fs::write(
            &program,
            "#!/usr/bin/python3\nimport time\ntime.sleep(3600)\n",
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        Self(
            Command::new(&program)
                .current_dir(&f.work)
                .env("HOME", f.root.path())
                .env_remove("PI_CODING_AGENT_DIR")
                .env_remove("PI_CODING_AGENT_SESSION_DIR")
                .stdin(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    /// The row `cones ls` carries for this client once discovery reports when it started. A
    /// fresh script can sit in a first-exec scan for seconds before it is a process at all.
    fn listed(&self, f: &Fixture, harness: &str) -> Value {
        let id = format!("{harness}-{}", self.0.id());
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let row = f
                .ok(&["ls", "--dir", f.work.to_str().unwrap(), "--json"])
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .find(|row| row["session"]["session_id"] == id.as_str())
                .filter(|row| !row["session"]["started"].is_null());
            if let Some(row) = row {
                return row["session"].clone();
            }
            assert!(Instant::now() < deadline, "{id} never started running");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn jsonl(path: &Path, records: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        records.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
}

/// `cones ls` and `cones launch` name a pi client by its process. That id reads the
/// conversation the client reports, the same text its native id reads.
#[test]
fn a_process_id_reads_the_conversation_its_client_reports() {
    let f = Fixture::new();
    let client = Client::start(&f, "pi");
    let listed = format!("pi-{}", client.0.id());
    // pi stamps its header as it boots, so the header takes the start discovery reports.
    let started = client.listed(&f, "pi")["started"].clone();
    let unreported = f.cones(&["show", &listed]);
    assert!(
        !unreported.status.success()
            && String::from_utf8_lossy(&unreported.stderr).contains("no session"),
        "a client with no conversation and no cones terminal has nothing to show"
    );

    let native = "57575757-1111-4111-8111-111111111111";
    let dir = cones::pi::session_dir(&f.root.path().join(".pi/agent"), &f.work);
    jsonl(
        &dir.join(format!("{native}.jsonl")),
        &[
            json!({"type": "session", "id": native, "cwd": f.work, "timestamp": started}),
            json!({"type": "message", "id": "m1", "timestamp": started,
                   "message": {"role": "user", "content": "count the cones"}}),
            json!({"type": "message", "id": "m2", "timestamp": started,
                   "message": {"role": "assistant", "stopReason": "stop",
                               "content": [{"type": "text", "text": "there are three"}]}}),
        ],
    );
    assert_eq!(
        client.listed(&f, "pi")["native_id"],
        native,
        "ls names the conversation the client reports"
    );

    let by_process = f.ok(&["show", &listed]);
    assert!(
        by_process.contains("count the cones") && by_process.contains("there are three"),
        "{by_process}"
    );
    assert!(!by_process.contains("terminal screen"), "{by_process}");
    assert_eq!(by_process, f.ok(&["show", native]));
    let report: Value = serde_json::from_str(&f.ok(&["show", &listed, "--json"])).unwrap();
    assert_eq!(report["session"]["session_id"], native);
    assert_eq!(report["session"]["harness"], "pi");
    assert_eq!(report["messages"].as_array().unwrap().len(), 2);
    let other = f.cones(&["show", &listed, "--harness", "gemini"]);
    assert!(!other.status.success());
}

/// Copilot keeps each session's events in its own folder and locks the one a client holds
/// under that client's pid. The lock is what ties a live row to its conversation.
#[test]
fn a_copilot_process_reads_the_conversation_its_lock_names() {
    let f = Fixture::new();
    let client = Client::start(&f, "copilot");
    let listed = format!("copilot-{}", client.0.id());
    assert!(client.listed(&f, "copilot").get("native_id").is_none());

    let sessions = f.root.path().join(".copilot/session-state");
    let (held, stale) = (
        "58585858-1111-4111-8111-111111111111",
        "58585858-2222-4222-8222-222222222222",
    );
    let at = "2026-09-19T08:03:44.559Z";
    for (id, said) in [(held, "created it"), (stale, "an earlier client's reply")] {
        jsonl(
            &sessions.join(id).join("events.jsonl"),
            &[
                json!({"type": "session.start", "id": "e1", "timestamp": at, "parentId": null,
                       "data": {"sessionId": id, "version": 1, "producer": "copilot-agent",
                                "startTime": at, "context": {"cwd": f.work}}}),
                json!({"type": "session.model_change", "id": "e2", "timestamp": at,
                       "data": {"newModel": "auto"}}),
                json!({"type": "user.message", "id": "e3", "timestamp": at,
                       "data": {"content": "create sample.txt",
                                "transformedContent": "<current_datetime>now</current_datetime>",
                                "messageId": "u1"}}),
                json!({"type": "system.message", "id": "e4", "timestamp": at,
                       "data": {"role": "system", "content": "You are the GitHub Copilot CLI"}}),
                json!({"type": "assistant.message", "id": "e5", "timestamp": at,
                       "data": {"messageId": "a1", "content": said, "toolRequests": [
                           {"toolCallId": "c1", "name": "bash", "type": "function",
                            "arguments": {"command": "printf sample > sample.txt"}}]}}),
                json!({"type": "tool.execution_complete", "id": "e6", "timestamp": at,
                       "data": {"toolCallId": "c1", "result": {"content": "tool output"}}}),
            ],
        );
        fs::write(
            sessions
                .join(id)
                .join(format!("inuse.{}.lock", client.0.id())),
            "",
        )
        .unwrap();
    }
    // A lock written before this process began belongs to whoever had the pid before it.
    fs::File::options()
        .write(true)
        .open(
            sessions
                .join(stale)
                .join(format!("inuse.{}.lock", client.0.id())),
        )
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(86400))
        .unwrap();
    assert_eq!(client.listed(&f, "copilot")["native_id"], held);

    let shown = f.ok(&["show", &listed]);
    assert_eq!(shown, f.ok(&["show", held]));
    let lines: Vec<&str> = shown.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 5, "{shown}");
    assert!(lines[0].starts_with("user 2026-09-19T"), "{shown}");
    assert_eq!(lines[1], "create sample.txt");
    assert!(lines[2].starts_with("assistant 2026-09-19T"), "{shown}");
    assert_eq!(lines[3], "created it");
    assert_eq!(lines[4], "  bash: printf sample > sample.txt");
    let report: Value = serde_json::from_str(&f.ok(&["show", &listed, "--json"])).unwrap();
    assert_eq!(report["session"]["harness"], "copilot");
    assert_eq!(report["session"]["session_id"], held);
    assert_eq!(report["cwd"], json!(f.work));
    // The conversation outlives the client, under its own id.
    drop(client);
    assert_eq!(f.ok(&["show", held]), shown);
    assert!(f.ok(&["show", stale]).contains("an earlier client's reply"));
}

/// Cursor's transcript holds neither its id nor its folder: the id is the file's name and
/// the folder is in the chat record the CLI keeps beside it. A transcript with no chat record
/// is the editor's, and is not listed.
#[test]
fn a_cursor_conversation_is_read_by_its_file_id_and_takes_its_folder_from_the_chat_record() {
    let f = Fixture::new();
    let cursor = f.root.path().join(".cursor");
    let (cli, editor) = (
        "59595959-1111-4111-8111-111111111111",
        "59595959-2222-4222-8222-222222222222",
    );
    for id in [cli, editor] {
        jsonl(
            &cursor.join(format!(
                "projects/tmp-work/agent-transcripts/{id}/{id}.jsonl"
            )),
            &[
                json!({"role": "user", "message": {"content": [
                    {"type": "text", "text": "<user_query>\nmake the file\n</user_query>"}]}}),
                json!({"role": "assistant", "message": {"content": [
                    {"type": "text", "text": "Creating the file."},
                    {"type": "tool_use", "name": "Shell",
                     "input": {"command": "printf sample > sample.txt", "description": "write"}}]}}),
                json!({"role": "assistant", "message": {"content": [
                    {"type": "text", "text": "DONE"}]}}),
                json!({"type": "turn_ended", "status": "success"}),
            ],
        );
    }
    // Other files under a project folder are not conversations.
    jsonl(
        &cursor.join("projects/tmp-work/agent-tools/log.jsonl"),
        &[json!({"role": "user", "message": {"content": [{"type": "text", "text": "not one"}]}})],
    );
    let chat = cursor.join(format!("chats/0123abcd/{cli}/meta.json"));
    fs::create_dir_all(chat.parent().unwrap()).unwrap();
    fs::write(
        &chat,
        json!({"schemaVersion": 1, "createdAtMs": 1789808119982u64, "hasConversation": true,
               "cwd": f.work})
        .to_string(),
    )
    .unwrap();

    let shown = f.ok(&["show", cli]);
    assert_eq!(
        shown.lines().filter(|l| !l.is_empty()).collect::<Vec<_>>(),
        [
            "user",
            "<user_query>",
            "make the file",
            "</user_query>",
            "assistant",
            "Creating the file.",
            "  Shell: printf sample > sample.txt",
            "assistant",
            "DONE"
        ]
    );
    let report: Value = serde_json::from_str(&f.ok(&["show", cli, "--json"])).unwrap();
    assert_eq!(report["session"]["harness"], "cursor-agent");
    assert_eq!(report["session"]["session_id"], cli);
    assert_eq!(report["cwd"], json!(f.work));
    assert_eq!(report["messages"].as_array().unwrap().len(), 3);
    assert!(
        report["messages"][0]["at"].is_null(),
        "Cursor stamps no message"
    );
    let missing = f.cones(&["show", editor]);
    assert!(
        !missing.status.success()
            && String::from_utf8_lossy(&missing.stderr).contains("no session"),
        "a transcript with no chat record is not a CLI conversation"
    );
}
