//! Grok Build through the `cones` binary: a detached launch, its row read from the session
//! folder the client holds open, the conversation, history, input, stop, resume and fork,
//! concurrent clients, a session folder Grok left half written, and a home of its own.
//!
//! The client is `tests/fixtures/fake_grok.py` on the fixture HOME's launch path, never the
//! machine's grok: a python3 stand-in that draws on stderr, as Grok 1.0.77 does, and writes
//! its session folder in the record shapes the CLI wrote. No model is contacted and no native
//! home outside the fixture is read or written.
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::net::UnixStream,
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
            .prefix("cones-grok-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = root.path();
        for dir in ["state", ".claude", "work", ".local/bin"] {
            fs::create_dir_all(path.join(dir)).unwrap();
        }
        std::os::unix::fs::symlink(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_grok.py"),
            path.join(".local/bin/grok"),
        )
        .unwrap();
        let jobs = path.join("jobs.yaml");
        // Only grok is offered, so the machine's own clients of the others cannot reach these
        // assertions.
        let mut text = String::from("version: 4\ndefaults:\n");
        for h in [
            "claude", "codex", "pi", "opencode", "gemini", "cursor", "copilot", "amp", "droid",
            "kimi",
        ] {
            text.push_str(&format!("  {h}_enabled: false\n"));
        }
        text.push_str("jobs: []\n");
        fs::write(&jobs, text).unwrap();
        let work = path.join("work").canonicalize().unwrap();
        Self { root, jobs, work }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cones"));
        command
            .args(["--jobs", self.jobs.to_str().unwrap(), "--state-dir"])
            .arg(self.root.path().join("state"))
            .args(args)
            .env("HOME", self.root.path())
            // Unset, so detection reads the home a live client names, as it does for a user.
            .env_remove("CLAUDE_CONFIG_DIR")
            .env("CONES_TEST_FAST", "1")
            .env_remove("GROK_HOME")
            .env_remove("FAKE_GROK_BROKEN")
            .stdin(Stdio::null());
        command
    }

    fn cones(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
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

    fn launch(&self, prompt: &str, env: &[(&str, &str)]) -> (String, u32) {
        let out = self
            .command(&[
                "launch",
                "--dir",
                self.work.to_str().unwrap(),
                "--harness",
                "grok",
                prompt,
            ])
            .envs(env.iter().copied())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let id = String::from_utf8(out.stdout).unwrap().trim().to_owned();
        let pid = id
            .strip_prefix("grok-")
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("a grok row is named by its client: {id}"));
        (id, pid)
    }

    /// Every grok row `cones ls` lists in the work folder.
    fn rows(&self) -> Vec<Value> {
        self.ok(&["ls", "--dir", self.work.to_str().unwrap(), "--json"])
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| row["session"]["harness"] == "grok")
            .map(|row| row["session"].clone())
            .collect()
    }

    /// The first row `found` accepts, once there is one.
    fn find(&self, found: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let rows = self.rows();
            if let Some(row) = rows.iter().find(|r| found(r)) {
                return row.clone();
            }
            assert!(Instant::now() < deadline, "no such row: {rows:#?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The row listed as `id`, once `ready` holds for it.
    fn row(&self, id: &str, ready: impl Fn(&Value) -> bool) -> Value {
        self.find(|r| r["session_id"] == id && ready(r))
    }

    /// Grok's session folders for the work folder, by id.
    fn sessions(&self, home: &Path) -> Vec<String> {
        let folder = home
            .join("sessions")
            .join(self.work.to_str().unwrap().replace('/', "%2F"));
        let mut ids: Vec<String> = fs::read_dir(folder)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        ids.sort();
        ids
    }

    /// Grok's home in the fixture, as a real path: the open files a row is read from are.
    fn home(&self) -> PathBuf {
        self.root.path().canonicalize().unwrap().join(".grok")
    }

    /// The host records this fixture owns: one per hosted client.
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

    fn host(&self, pid: u32) -> Value {
        self.hosts()
            .into_iter()
            .find(|h| h["session"]["pid"] == pid)
            .unwrap_or_else(|| panic!("no host for {pid}"))
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

/// A dashboard's view of one hosted client: its screen, and keys typed into it.
struct Pane {
    stream: UnixStream,
    parser: vt100::Parser,
}

impl Pane {
    fn attach(host: &Value) -> Self {
        let mut stream = UnixStream::connect(host["socket"].as_str().unwrap()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        send(
            &mut stream,
            &json!({"Attach": {"version": 1, "id": host["id"]}}),
        );
        assert_eq!(
            receive(&mut stream)["Attached"]["session"]["pid"],
            host["session"]["pid"]
        );
        Self {
            stream,
            parser: vt100::Parser::new(24, 120, 1000),
        }
    }

    fn until(&mut self, expected: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let contents = self.parser.screen().contents();
            if contents.contains(expected) {
                return contents;
            }
            assert!(
                Instant::now() < deadline,
                "the pane never showed {expected:?}: {contents}"
            );
            let reply = receive(&mut self.stream);
            if let Some(screen) = reply.get("Screen") {
                if screen["reset"] == true {
                    self.parser.process(b"\x1bc");
                }
                self.parser.screen_mut().set_size(
                    screen["rows"].as_u64().unwrap() as u16,
                    screen["cols"].as_u64().unwrap() as u16,
                );
                let bytes: Vec<u8> = serde_json::from_value(screen["bytes"].clone()).unwrap();
                self.parser.process(&bytes);
            }
        }
    }

    fn input(&mut self, text: &str) {
        send(&mut self.stream, &json!({"Input": text.as_bytes()}));
    }
}

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// A launch draws in its pane, names the conversation its client holds, and that
/// conversation is read, answered, stopped, resumed and forked through Grok's own records.
#[test]
fn a_launched_grok_is_read_controlled_resumed_and_forked_through_its_session_folder() {
    let f = Fixture::new();
    let (id, pid) = f.launch("fix the tests", &[]);
    let sessions = f.sessions(&f.home());
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let sid = sessions[0].clone();

    // Grok draws only on stderr; the pane shows it, and the launch trusted the folder it chose.
    let mut pane = Pane::attach(&f.host(pid));
    let screen = pane.until(&format!("reply {pid}: fix the tests"));
    assert!(screen.contains(&format!("grok fixture {sid}")), "{screen}");
    assert!(
        screen.contains("argv: --trust --always-approve -- fix the tests"),
        "{screen}"
    );

    // The row is the client, and everything on it is what Grok wrote about the session it holds.
    let row = f.row(&id, |r| r["state"] == "idle");
    let transcript = f
        .home()
        .join("sessions")
        .join(f.work.to_str().unwrap().replace('/', "%2F"))
        .join(&sid)
        .join("updates.jsonl");
    for (field, expected) in [
        ("native_id", json!(sid)),
        ("pid", json!(pid)),
        ("cwd", json!(f.work)),
        ("model", json!("grok-4.7")),
        ("effort", json!("high")),
        ("title", json!("Fixture fix the tests")),
        ("last", json!(format!("reply {pid}: fix the tests"))),
        ("tokens_in", json!(1000)),
        ("tokens_out", json!(10)),
        ("context_tokens", json!(1000)),
        ("context_window", json!(256000)),
        ("cost_usd", json!(0.12345)),
        ("transcript_path", json!(transcript)),
    ] {
        assert_eq!(row[field], expected, "{field}: {row:#}");
    }
    assert_eq!(row["cost_info"]["source"], "harness", "{row:#}");
    assert!(row["started"].is_string() && row["last_activity"].is_string());

    // The conversation, under either id, is Grok's own record of it, tool calls included.
    let conversation = f.ok(&["show", &id]);
    assert_eq!(conversation, f.ok(&["show", &sid]));
    for line in [
        "fix the tests",
        "  run_terminal_command: date",
        &format!("reply {pid}: fix the tests"),
    ] {
        assert!(
            conversation.lines().any(|l| l == line),
            "{line:?}: {conversation}"
        );
    }
    let found = f.ok(&["search", "--harness", "grok", "fix the tests"]);
    assert!(found.starts_with(&format!("{sid}\tgrok\t")), "{found}");

    // A permission question is input until it is answered, which is where `wait` returns;
    // typed lines are turns.
    pane.input("approve? the date\n");
    pane.until("Allow run_terminal_command?");
    let waited = f.ok(&["wait", &id, "--timeout", "20"]);
    assert_eq!(
        waited.trim_end().split('\t').next(),
        Some(id.as_str()),
        "{waited}"
    );
    let asking = f.row(&id, |r| r["state"] == "blocked");
    assert_eq!(asking["last"], json!(format!("reply {pid}: fix the tests")));
    pane.input("\n");
    let row = f.row(&id, |r| {
        r["state"] == "idle" && r["cost_usd"] == json!(0.2469)
    });
    assert_eq!(
        row["last"],
        json!(format!("reply {pid}: approve? the date"))
    );
    assert_eq!(row["context_tokens"], json!(2000));
    drop(pane);

    // Grok has nothing that takes a note into a running session, and says so.
    let note = f.cones(&["send", &id, "hello"]);
    assert!(!note.status.success());
    assert!(
        String::from_utf8_lossy(&note.stderr).contains("a note cannot reach grok"),
        "{}",
        String::from_utf8_lossy(&note.stderr)
    );

    // Stopped, the client is gone and the conversation stays readable from history.
    f.ok(&["stop", &id]);
    assert!(!alive(pid));
    assert!(f.rows().iter().all(|r| r["session_id"] != id.as_str()));
    assert!(f.ok(&["show", &sid]).contains("approve? the date"));

    // Resumed, a new client holds the same conversation.
    let resumed = f.ok(&["resume", &sid]).trim().to_owned();
    assert_eq!(resumed, sid, "resume prints the conversation it reopened");
    let row = f.find(|r| r["native_id"] == sid.as_str() && r["state"] == "idle");
    let again = row["pid"].as_u64().unwrap() as u32;
    assert_ne!(again, pid);
    assert_eq!(row["session_id"], json!(format!("grok-{again}")));
    assert_eq!(row["cost_usd"], json!(0.2469), "{row:#}");

    // Forked, Grok copies the conversation under the id cones chose and names its parent.
    let fork = f.ok(&["fork", &sid]).trim().to_owned();
    let row = f.find(|r| r["session_id"] == fork.as_str() && r["native_id"].is_string());
    let child = row["native_id"].as_str().unwrap().to_owned();
    assert_ne!(child, sid);
    assert_eq!(row["forked_from"], json!(sid), "{row:#}");
    assert_eq!(f.sessions(&f.home()), {
        let mut both = vec![sid.clone(), child.clone()];
        both.sort();
        both
    });
    let shown = f.ok(&["show", &child]);
    assert!(shown.contains("fix the tests"), "{shown}");
}

/// Two clients in one folder are two rows, each naming the conversation it holds open.
#[test]
fn concurrent_clients_in_one_folder_each_name_their_own_conversation() {
    let f = Fixture::new();
    let (first, one) = f.launch("first task", &[]);
    let (second, two) = f.launch("second task", &[]);
    let a = f.row(&first, |r| r["state"] == "idle");
    let b = f.row(&second, |r| r["state"] == "idle");
    assert_ne!(a["native_id"], b["native_id"]);
    assert_eq!(a["last"], json!(format!("reply {one}: first task")));
    assert_eq!(b["last"], json!(format!("reply {two}: second task")));
    let mut held = vec![
        a["native_id"].as_str().unwrap().to_owned(),
        b["native_id"].as_str().unwrap().to_owned(),
    ];
    held.sort();
    assert_eq!(held, f.sessions(&f.home()));
}

/// A session folder Grok left half written keeps the row and leaves its columns absent; one
/// with no readable summary is no conversation, and the row is the bare process.
#[test]
fn a_half_written_session_folder_leaves_columns_absent_rather_than_failing_the_row() {
    let f = Fixture::new();
    let (id, pid) = f.launch("broken", &[("FAKE_GROK_BROKEN", "1")]);
    let row = f.row(&id, |r| r["state"] == "idle");
    assert_eq!(row["last"], json!(format!("reply {pid}: broken")));
    assert!(row["native_id"].is_string(), "{row:#}");
    for field in ["cost_usd", "tokens_in", "context_tokens", "context_window"] {
        assert!(row[field].is_null(), "{field}: {row:#}");
    }
    let sid = row["native_id"].as_str().unwrap();
    let folder = f
        .home()
        .join("sessions")
        .join(f.work.to_str().unwrap().replace('/', "%2F"))
        .join(sid);
    fs::write(folder.join("summary.json"), "{\"info\":").unwrap();
    let row = f.row(&id, |r| r["native_id"].is_null());
    assert_eq!(row["state"], "-", "{row:#}");
    assert_eq!(row["pid"], json!(pid));
}

/// A client started with its own GROK_HOME is listed and read from that home while it runs;
/// once it has gone, its conversation is history only where `homes` names the home. A job is
/// refused, since cones cannot supervise a Grok run.
#[test]
fn a_custom_grok_home_is_read_live_and_by_configuration() {
    let f = Fixture::new();
    let custom = f.root.path().canonicalize().unwrap().join("custom-grok");
    let mut client = Command::new(f.root.path().join(".local/bin/grok"))
        .args(["--", "custom home"])
        .current_dir(&f.work)
        .env("HOME", f.root.path())
        .env("GROK_HOME", &custom)
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let id = format!("grok-{}", client.id());
    let row = f.row(&id, |r| r["state"] == "idle");
    let sid = f.sessions(&custom).remove(0);
    assert_eq!(row["native_id"], json!(sid));
    assert!(
        row["transcript_path"]
            .as_str()
            .unwrap()
            .starts_with(custom.to_str().unwrap()),
        "{row:#}"
    );
    assert!(f.ok(&["show", &id]).contains("custom home"));
    drop(client.stdin.take());
    client.wait().unwrap();
    assert!(
        !f.ok(&["search", "--harness", "grok", "custom"])
            .contains(&sid)
    );
    let config = fs::read_to_string(&f.jobs).unwrap();
    fs::write(
        &f.jobs,
        config.replace(
            "jobs: []\n",
            &format!("homes:\n  grok: [{}]\njobs: []\n", custom.display()),
        ),
    )
    .unwrap();
    let found = f.ok(&["search", "--harness", "grok", "custom"]);
    assert!(found.starts_with(&format!("{sid}\tgrok\t")), "{found}");

    fs::write(
        &f.jobs,
        fs::read_to_string(&f.jobs).unwrap().replace(
            "jobs: []\n",
            "jobs:\n  - name: nightly\n    schedule: \"0 9 * * *\"\n    harness: grok\n    cwd: .\n    prompt: p\n",
        ),
    )
    .unwrap();
    let check = f.cones(&["config", "--check"]);
    assert!(!check.status.success());
    assert!(
        String::from_utf8_lossy(&check.stderr).contains("cones cannot supervise a run on grok"),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
}
