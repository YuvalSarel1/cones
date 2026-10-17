//! Sessions in native homes other than the harness's own: the ones `homes` in jobs.yaml names
//! and the ones live clients of this user run against. Each is listed, read and controlled in
//! the home that recorded it. A fake native CLI stands in for Claude; no model is contacted.
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

/// Answers `stop` and records the id and the native home it was given, so a stop sent to the
/// wrong home is visible in the wrong home's log.
const FAKE: &str = r#"#!/bin/sh
if [ "$1" = stop ] && [ "$2" = --help ]; then
    echo "Usage: claude stop <id>"
    exit 0
fi
home=${CLAUDE_CONFIG_DIR:?a stop must name the native home of its session}
printf '%s\n' "$* $home" >> "$home/calls.log"
echo "stopped $2"
"#;

struct Fixture {
    root: tempfile::TempDir,
    children: Vec<Child>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("cones-homes-")
            .tempdir_in("/tmp")
            .unwrap();
        let bin = root.path().join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("claude"), FAKE).unwrap();
        fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o700)).unwrap();
        for dir in [".claude/sessions", "state"] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        fs::write(root.path().join("jobs.yaml"), "version: 4\njobs: []\n").unwrap();
        Self {
            root,
            children: Vec::new(),
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    /// A process cones may read about and must not touch.
    fn bystander(&mut self) -> u32 {
        let child = Command::new("/bin/sleep").arg("600").spawn().unwrap();
        let pid = child.id();
        self.children.push(child);
        pid
    }

    /// A Claude client as `ps` shows one, running against `home` under `user`'s HOME.
    fn client(&mut self, user: &Path, home: &Path) -> u32 {
        self.stand_in(
            "claude",
            &[],
            &[("HOME", user), ("CLAUDE_CONFIG_DIR", home)],
        )
    }

    /// A client of `harness` as `ps` shows one, started in the fixture's folder.
    fn stand_in(&mut self, harness: &str, args: &[&str], env: &[(&str, &Path)]) -> u32 {
        let program = self.path().join("standin").join(harness);
        fs::create_dir_all(program.parent().unwrap()).unwrap();
        fs::write(
            &program,
            "#!/usr/bin/python3\nimport time\ntime.sleep(600)\n",
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let mut command = Command::new(&program);
        command
            .args(args)
            .current_dir(self.path().canonicalize().unwrap())
            .stdin(Stdio::null());
        for (name, value) in env {
            command.env(name, value);
        }
        let child = command.spawn().unwrap();
        let pid = child.id();
        self.children.push(child);
        pid
    }

    /// A client whose command line starts with `name` itself, for a harness that reads its own
    /// arguments or is signalled by name: a script's interpreter would come first.
    fn named(&mut self, name: &str, args: &[&str], env: &[(&str, &Path)]) -> u32 {
        self.scripted(name, args, env, "setTimeout(() => {}, 600000)")
    }

    /// `named`, running `script` under node.
    fn scripted(&mut self, name: &str, args: &[&str], env: &[(&str, &Path)], script: &str) -> u32 {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("node");
        command
            .arg0(name)
            .arg("-")
            .args(args)
            .current_dir(self.path().canonicalize().unwrap())
            .stdin(Stdio::piped())
            .env("CONES_TEST_FLEET", self.path());
        for (name, value) in env {
            command.env(name, value);
        }
        // A dispatcher's run has no terminal, whatever the test was started from.
        super::stop::own_session(&mut command, false);
        let mut child = command.spawn().unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), script.as_bytes()).unwrap();
        let pid = child.id();
        self.children.push(child);
        pid
    }

    /// A stand-in Codex client with a real socket connection to this test process.
    #[cfg(target_os = "macos")]
    fn connected_codex(
        &mut self,
        home: &Path,
        args: &[&str],
        script: &str,
    ) -> (u32, std::os::unix::net::UnixStream) {
        use std::os::unix::net::UnixListener;
        let socket = self
            .path()
            .join(format!("client-{}.sock", self.children.len()));
        let listener = UnixListener::bind(&socket).unwrap();
        let user = self.path().to_owned();
        let pid = self.scripted(
            "codex",
            args,
            &[("HOME", &user), ("CODEX_HOME", home)],
            &format!(
                "{script}; require('net').createConnection({});",
                json!(socket)
            ),
        );
        (pid, accept_connection(&listener))
    }

    /// An OpenCode client under `user`'s HOME showing the fixture session, which the database
    /// under `data` holds. Returns its pid and that database.
    fn opencode(&mut self, user: &Path, data: &Path) -> (u32, PathBuf) {
        let cwd = self.path().canonicalize().unwrap();
        fs::create_dir_all(data.join("opencode")).unwrap();
        let db = data.join("opencode/opencode.db");
        let sql = include_str!("../assets/harnesses/fixtures/opencode.sql")
            .replace("/fixture", &cwd.to_string_lossy());
        let mut sqlite = Command::new("sqlite3")
            .arg(&db)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(sqlite.stdin.as_mut().unwrap(), sql.as_bytes()).unwrap();
        assert!(sqlite.wait().unwrap().success());
        let env = [("HOME", user), ("XDG_DATA_HOME", data)];
        let pid = self.named("opencode", &["--session", "ses_fixture"], &env);
        (pid, db)
    }

    /// The rows `cones ls` reports for `harness`, once one of them satisfies `ready`. The
    /// python3 shim hides a stand-in's environment until it re-executes the real interpreter.
    fn rows_when(&self, harness: &str, ready: impl Fn(&Value) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let rows: Vec<Value> = self
                .ok(&["ls", "--json"])
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .filter(|row| row["session"]["harness"] == harness)
                .map(|row| row["session"].clone())
                .collect();
            if rows.iter().any(&ready) || Instant::now() > deadline {
                return rows;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// A live registry entry in `home`, with the conversation Claude keeps beside it.
    fn session(&self, home: &Path, id: &str, kind: &str, pid: u32, said: &str) {
        fs::create_dir_all(home.join("sessions")).unwrap();
        fs::write(
            home.join(format!("sessions/{id}.json")),
            json!({"sessionId": id, "pid": pid, "kind": kind, "cwd": self.path(), "status": "busy"})
                .to_string(),
        )
        .unwrap();
        let project = self
            .path()
            .to_string_lossy()
            .replace(|c: char| !c.is_ascii_alphanumeric(), "-");
        let dir = home.join("projects").join(project);
        fs::create_dir_all(&dir).unwrap();
        let line = json!({
            "type": "user", "uuid": "11111111-1111-4111-8111-111111111111",
            "sessionId": id, "cwd": self.path(), "timestamp": "2026-10-13T09:00:00Z",
            "message": {"role": "user", "content": said},
        });
        fs::write(dir.join(format!("{id}.jsonl")), format!("{line}\n")).unwrap();
    }

    fn cones(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cones"))
            .env("HOME", self.path())
            .env("CONES_TEST_FLEET", self.path())
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("PI_CODING_AGENT_DIR")
            .args(["--jobs", self.path().join("jobs.yaml").to_str().unwrap()])
            .args(["--state-dir", self.path().join("state").to_str().unwrap()])
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.cones(args);
        assert!(
            out.status.success(),
            "cones {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// The Claude sessions `cones ls` reports, by id.
    fn sessions(&self) -> Vec<Value> {
        self.ok(&["ls", "--json"])
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| row["session"]["harness"] == "claude")
            .map(|row| row["session"].clone())
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Wait for a fixture client, but fail if it exits or never connects.
pub(super) fn accept_connection(
    listener: &std::os::unix::net::UnixListener,
) -> std::os::unix::net::UnixStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "fixture client did not connect within 3 seconds"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("accepting fixture client: {e}"),
        }
    }
}

fn ids(rows: &[Value]) -> Vec<&str> {
    let mut ids: Vec<_> = rows
        .iter()
        .filter_map(|r| r["session_id"].as_str())
        .collect();
    ids.sort();
    ids
}

const OWN: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const TASK: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const OTHER: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
/// Live Codex runs are read under any HOME, so concurrent tests see each other's: this one's id is its own.
const RUN: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";

/// A home `homes.claude` names is read beside Claude's own: its session is a full row, its
/// conversation is what `cones show` prints, and a stop goes to the home that holds it.
#[test]
fn a_configured_home_is_listed_read_and_stopped_in_its_own_home() {
    let mut f = Fixture::new();
    let own = f.path().join(".claude");
    let task = f.path().join("task-home");
    let (first, second) = (f.bystander(), f.bystander());
    f.session(&own, OWN, "bg", first, "tidy the changelog");
    f.session(&task, TASK, "bg", second, "rotate the staging keys");
    assert_eq!(ids(&f.sessions()), [OWN], "an unnamed home is not read");

    let listed = format!("[\"{}\"]", task.display());
    f.ok(&["config", "set", "homes.claude", &listed]);
    let rows = f.sessions();
    assert_eq!(ids(&rows), [OWN, TASK]);
    let row = rows.iter().find(|r| r["session_id"] == TASK).unwrap();
    assert_eq!(row["state"], "active", "{row}");
    assert_eq!(row["title"], "rotate the staging keys", "{row}");
    assert_eq!(
        row["transcript_path"]
            .as_str()
            .map(PathBuf::from)
            .map(|p| p.starts_with(&task)),
        Some(true),
        "{row}"
    );

    let shown = f.ok(&["show", TASK]);
    assert!(shown.contains("rotate the staging keys"), "{shown}");

    f.ok(&["stop", &TASK[..8]]);
    f.ok(&["stop", OWN]);
    let log = |home: &Path| fs::read_to_string(home.join("calls.log")).unwrap_or_default();
    // `claude stop` takes the short id, the first block of the UUID.
    assert_eq!(log(&task), format!("stop bbbbbbbb {}\n", task.display()));
    assert_eq!(log(&own), format!("stop aaaaaaaa {}\n", own.display()));

    // Taking the home out of the file takes its sessions out of the list.
    f.ok(&["config", "unset", "homes.claude"]);
    assert_eq!(ids(&f.sessions()), [OWN]);
}

/// A relative home would be read against whatever directory cones was started in.
#[test]
fn a_home_that_is_not_absolute_or_names_no_harness_is_refused() {
    let f = Fixture::new();
    for (key, value) in [
        ("homes.claude", "[task-home]"),
        ("homes.nothing", "[/tmp/x]"),
    ] {
        let out = f.cones(&["config", "set", key, value]);
        assert!(!out.status.success(), "{key} {value} was accepted");
    }
    assert_eq!(
        fs::read_to_string(f.path().join("jobs.yaml")).unwrap(),
        "version: 4\njobs: []\n",
        "a refused setting changed the file"
    );
}

/// A live client of this user that points Claude at another home is this fleet's: cones reads
/// that home with no configuration. A client under another HOME is another fleet's and stays out.
#[test]
fn a_live_client_of_this_user_names_a_home_that_is_then_read() {
    let mut f = Fixture::new();
    let (task, other) = (f.path().join("task-home"), f.path().join("other-home"));
    let elsewhere = f.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let user = f.path().to_owned();
    let mine = f.client(&user, &task);
    let theirs = f.client(&elsewhere, &other);
    f.session(&task, TASK, "interactive", mine, "rotate the staging keys");
    f.session(&other, OTHER, "interactive", theirs, "not this fleet");

    // The python3 shim hides its environment until it re-executes the real interpreter.
    let deadline = Instant::now() + Duration::from_secs(30);
    let rows = loop {
        let rows = f.sessions();
        if !rows.is_empty() {
            break rows;
        }
        assert!(
            Instant::now() < deadline,
            "the client's home was never read"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(ids(&rows), [TASK]);
    assert_eq!(rows[0]["pid"], mine);
    assert_eq!(rows[0]["title"], "rotate the staging keys");
    let shown = f.ok(&["show", TASK]);
    assert!(shown.contains("rotate the staging keys"), "{shown}");
    // By now the other client's environment is readable too, and it is still not listed.
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(ids(&f.sessions()), [TASK]);
}

/// pi keeps a conversation in a file under its home, named after the folder it runs in. A
/// client pointed at another home is attributed to the file in that home, so its row has the
/// conversation's title and `cones show` reads it.
#[test]
fn a_pi_client_in_another_home_is_a_row_with_its_conversation() {
    let mut f = Fixture::new();
    let task = f.path().join("task-pi");
    let cwd = f.path().canonicalize().unwrap();
    let user = f.path().to_owned();
    let pid = f.stand_in(
        "pi",
        &[],
        &[("HOME", &user), ("PI_CODING_AGENT_DIR", &task)],
    );
    let folder = cwd
        .to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "-");
    let dir = task.join(format!("sessions/--{folder}--"));
    fs::create_dir_all(&dir).unwrap();
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let header = json!({"type": "session", "version": 3, "id": TASK, "timestamp": now, "cwd": cwd});
    let said = json!({"type": "message", "id": "m1", "parentId": null, "timestamp": now,
        "message": {"role": "user", "content": [{"type": "text", "text": "rotate the staging keys"}]}});
    fs::write(
        dir.join(format!("{TASK}.jsonl")),
        format!("{header}\n{said}\n"),
    )
    .unwrap();

    let rows = f.rows_when("pi", |row| row["pid"] == pid && row["title"].is_string());
    let mine: Vec<_> = rows.iter().filter(|row| row["pid"] == pid).collect();
    assert_eq!(mine.len(), 1, "{rows:?}");
    assert_eq!(mine[0]["title"], "rotate the staging keys", "{}", mine[0]);
    assert_eq!(
        mine[0]["transcript_path"]
            .as_str()
            .map(|p| Path::new(p).starts_with(&task)),
        Some(true),
        "{}",
        mine[0]
    );
    let shown = f.ok(&["show", TASK]);
    assert!(shown.contains("rotate the staging keys"), "{shown}");
}

/// OpenCode keeps every conversation in one database under its data home. A client pointed
/// at another data home, showing a session that database holds, is a row with that session's
/// title, read from that database.
#[test]
fn an_opencode_client_in_another_data_home_is_a_row_with_its_session() {
    let mut f = Fixture::new();
    let data = f.path().join("task-xdg");
    let user = f.path().to_owned();
    let (pid, db) = f.opencode(&user, &data);

    let rows = f.rows_when("opencode", |row| {
        row["pid"] == pid && row["title"].is_string()
    });
    let mine: Vec<_> = rows.iter().filter(|row| row["pid"] == pid).collect();
    assert_eq!(mine.len(), 1, "{rows:?}");
    assert_eq!(mine[0]["title"], "Named OpenCode", "{}", mine[0]);
    assert_eq!(
        mine[0]["transcript_path"].as_str().map(PathBuf::from),
        Some(db),
        "{}",
        mine[0]
    );
}

/// A headless `codex exec` run by a dispatcher has its own HOME and a home under a temporary
/// directory, which macOS keeps behind a symlink: the client names one spelling and the
/// configuration the other. It is still a row, titled by its prompt and not by its subcommand.
#[test]
fn a_headless_codex_run_is_a_row_when_its_configured_home_is_spelled_differently() {
    let mut f = Fixture::new();
    let task = f.path().join("task-codex");
    let elsewhere = f.path().join("elsewhere");
    for dir in [&task, &elsewhere] {
        fs::create_dir_all(dir).unwrap();
    }
    let real = task.canonicalize().unwrap();
    assert_ne!(real, task, "the fixture root is not behind a symlink");
    let pid = f.stand_in(
        "codex",
        &[
            "exec",
            "--json",
            "-m",
            "gpt-5",
            "-o",
            "last.txt",
            // A prompt after `--` is a prompt even when it starts like a subcommand.
            "--",
            "update the docs",
        ],
        &[("HOME", &elsewhere), ("CODEX_HOME", &task)],
    );
    f.ok(&[
        "config",
        "set",
        "homes.codex",
        &format!("[\"{}\"]", real.display()),
    ]);

    let rows = f.rows_when("codex", |row| row["pid"] == pid);
    let mine: Vec<_> = rows.iter().filter(|row| row["pid"] == pid).collect();
    assert_eq!(mine.len(), 1, "{rows:?}");
    assert_eq!(mine[0]["title"], "update the docs", "{}", mine[0]);
    // No rollout reports a model yet, so the one it was started with stands in.
    assert_eq!(mine[0]["model"], "gpt-5", "{}", mine[0]);
    assert_eq!(
        mine[0]["cwd"].as_str().map(PathBuf::from),
        f.path().canonicalize().ok(),
        "{}",
        mine[0]
    );
}

/// A dispatcher gives each run a home with a random suffix, so no fixed list can name it. A
/// `homes` entry ending in `*` names them all: a live client in a matching home is a row
/// under any HOME, and one in a home that does not match stays another fleet's.
#[test]
fn a_homes_pattern_lists_live_clients_in_run_directories() {
    let mut f = Fixture::new();
    let elsewhere = f.path().join("elsewhere");
    let (run, stray) = (f.path().join("run.1.x7"), f.path().join("other.1"));
    for dir in [&elsewhere, &run, &stray] {
        fs::create_dir_all(dir).unwrap();
    }
    let args = ["exec", "review the diff"];
    let mine = f.stand_in(
        "codex",
        &args,
        &[("HOME", &elsewhere), ("CODEX_HOME", &run)],
    );
    let theirs = f.stand_in(
        "codex",
        &args,
        &[("HOME", &elsewhere), ("CODEX_HOME", &stray)],
    );
    let claude = f.client(&elsewhere, &run);
    f.session(&run, TASK, "interactive", claude, "rotate the staging keys");
    let (opencode, db) = f.opencode(&elsewhere, &run);

    let pattern = format!(
        "[\"{}\"]",
        f.path().canonicalize().unwrap().join("run.*").display()
    );
    for harness in ["codex", "claude", "opencode"] {
        f.ok(&["config", "set", &format!("homes.{harness}"), &pattern]);
    }

    let rows = f.rows_when("codex", |row| row["pid"] == mine);
    let row = rows
        .iter()
        .find(|r| r["pid"] == mine)
        .expect("no codex row");
    assert_eq!(row["title"], "review the diff", "{row}");
    let rows = f.rows_when("claude", |row| row["pid"] == claude);
    assert_eq!(ids(&rows), [TASK]);
    assert_eq!(rows[0]["title"], "rotate the staging keys", "{}", rows[0]);
    let rows = f.rows_when("opencode", |row| {
        row["pid"] == opencode && row["title"].is_string()
    });
    let row = rows
        .iter()
        .find(|r| r["pid"] == opencode)
        .expect("no opencode row");
    assert_eq!(row["title"], "Named OpenCode", "{row}");
    assert_eq!(
        row["transcript_path"].as_str().map(PathBuf::from),
        Some(db),
        "{row}"
    );
    // By now the other client's environment is readable too, and it is still not listed.
    std::thread::sleep(Duration::from_secs(1));
    let rows = f.rows_when("codex", |row| row["pid"] == mine);
    assert!(rows.iter().all(|row| row["pid"] != theirs), "{rows:?}");

    for (key, refused) in [
        ("homes.codex", "[/tmp/*/run]"),
        ("homes.codex", "[/tmp/run.**]"),
        ("homes.codex", "[/tmp/*]"),
        // pi hides its environment and gemini has no home variable: nothing to match.
        ("homes.pi", "[/tmp/run.*]"),
        ("homes.gemini", "[/tmp/run.*]"),
    ] {
        let out = f.cones(&["config", "set", key, refused]);
        assert!(!out.status.success(), "{key} {refused} was accepted");
    }
}

/// What a finished run left in a directory a pattern names is still read: no client is live
/// there, and `cones show` prints the conversation.
#[test]
fn a_finished_run_in_a_pattern_home_is_read_as_history() {
    let mut f = Fixture::new();
    let run = f.path().join("run.2.k3");
    let gone = f.bystander();
    f.session(&run, TASK, "interactive", gone, "rotate the staging keys");
    fs::remove_dir_all(run.join("sessions")).unwrap();
    assert!(
        !f.cones(&["show", TASK]).status.success(),
        "read with no pattern"
    );

    let pattern = format!("[\"{}\"]", f.path().join("run.*").display());
    f.ok(&["config", "set", "homes.claude", &pattern]);
    assert_eq!(
        ids(&f.sessions()),
        [] as [&str; 0],
        "a finished run is not a live row"
    );
    let shown = f.ok(&["show", TASK]);
    assert!(shown.contains("rotate the staging keys"), "{shown}");
}

/// A headless Codex run in a configured home is read and controlled there: the row carries
/// what its rollout reports, `cones show` prints the conversation, and a stop ends its process.
#[test]
fn a_headless_codex_run_in_a_configured_home_is_shown_and_stopped() {
    let mut f = Fixture::new();
    let (task, elsewhere) = (f.path().join("task-codex"), f.path().join("elsewhere"));
    let cwd = f.path().canonicalize().unwrap();
    let day = task.join("sessions/2026/10/13");
    for dir in [&day, &elsewhere] {
        fs::create_dir_all(dir).unwrap();
    }
    let pid = f.named(
        "codex",
        &["exec", "-m", "gpt-4.1", "review the diff"],
        &[("HOME", &elsewhere), ("CODEX_HOME", &task)],
    );
    let at = |secs| {
        (chrono::Utc::now() + chrono::TimeDelta::seconds(secs))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    };
    let lines = [
        json!({"timestamp": at(1), "type": "session_meta", "payload": {"session_id": OTHER, "id": OTHER, "timestamp": at(1), "cwd": cwd, "originator": "codex_exec"}}),
        json!({"timestamp": at(2), "type": "event_msg", "payload": {"type": "task_started", "turn_id": "t1"}}),
        json!({"timestamp": at(2), "type": "turn_context", "payload": {"turn_id": "t1", "cwd": cwd, "model": "gpt-5"}}),
        json!({"timestamp": at(2), "type": "event_msg", "payload": {"type": "user_message", "message": "review the diff"}}),
    ];
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(
        day.join(format!("rollout-2026-10-13T12-00-00-{OTHER}.jsonl")),
        text,
    )
    .unwrap();
    f.ok(&[
        "config",
        "set",
        "homes.codex",
        &format!("[\"{}\"]", task.display()),
    ]);

    let rows = f.rows_when("codex", |row| row["pid"] == pid);
    let row = rows.iter().find(|r| r["pid"] == pid).expect("no codex row");
    assert_eq!(row["session_id"], OTHER, "{row}");
    assert_eq!(row["state"], "active", "{row}");
    // What the rollout reports wins over what the command line asked for.
    assert_eq!(row["model"], "gpt-5", "{row}");
    assert_eq!(row["title"], "review the diff", "{row}");

    let shown = f.ok(&["show", OTHER]);
    assert!(shown.contains("review the diff"), "{shown}");

    // No client is attached to take a note, and saying so beats a note nobody reads.
    let dir = f.path().to_str().unwrap();
    let out = f.cones(&["comms", "--dir", dir, "send", OTHER, "status?"]);
    assert!(!out.status.success(), "a note was sent to a headless run");

    // Nothing but a signal ends a run with no terminal, so a stop sends one and sees it exit.
    let said = f.ok(&["stop", &OTHER[..8]]);
    assert!(
        said.contains(&format!("stopped the headless codex run {OTHER}")),
        "{said}"
    );
    let child = f.children.last_mut().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the stopped run is still alive");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A prompt is whatever bytes its author wrote. One with an em dash or an accent is listed
/// and titled as written, like an ASCII one.
#[test]
fn a_codex_run_whose_prompt_is_not_ascii_is_listed() {
    let mut f = Fixture::new();
    let (task, elsewhere) = (f.path().join("seat-utf8"), f.path().join("elsewhere"));
    for dir in [&task, &elsewhere] {
        fs::create_dir_all(dir).unwrap();
    }
    let pid = f.stand_in(
        "codex",
        &["exec", "--", "reply done — café"],
        &[("HOME", &elsewhere), ("CODEX_HOME", &task)],
    );
    let home = format!("[\"{}\"]", task.canonicalize().unwrap().display());
    f.ok(&["config", "set", "homes.codex", &home]);

    let rows = f.rows_when("codex", |row| row["pid"] == pid);
    let row = rows.iter().find(|r| r["pid"] == pid).expect("no codex row");
    assert_eq!(row["title"], "reply done — café", "{row}");
}

/// A dispatcher's `codex exec` runs under its own HOME and home, which no setting names. The
/// run holds its rollout open, and that file says which home to read: the run is a full row.
/// A client under another HOME with no rollout open stays out.
#[test]
fn a_codex_run_under_another_home_is_listed_by_its_open_rollout() {
    let mut f = Fixture::new();
    let (task, stray) = (f.path().join("seat.x7"), f.path().join("stray"));
    let elsewhere = f.path().join("elsewhere");
    let cwd = f.path().canonicalize().unwrap();
    let day = task.join("sessions/2026/10/14");
    for dir in [&day, &stray, &elsewhere] {
        fs::create_dir_all(dir).unwrap();
    }
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let lines = [
        json!({"timestamp": now, "type": "session_meta", "payload": {"session_id": RUN, "id": RUN, "timestamp": now, "cwd": cwd, "originator": "codex_exec"}}),
        json!({"timestamp": now, "type": "event_msg", "payload": {"type": "task_started", "turn_id": "t1"}}),
        json!({"timestamp": now, "type": "turn_context", "payload": {"turn_id": "t1", "cwd": cwd, "model": "gpt-5"}}),
        json!({"timestamp": now, "type": "event_msg", "payload": {"type": "user_message", "message": "update the docs"}}),
    ];
    let rollout = day.join(format!("rollout-2026-10-14T07-00-00-{RUN}.jsonl"));
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(&rollout, text).unwrap();
    let args = ["exec", "--json", "--", "update the docs"];
    let script = format!(
        "require('fs').openSync({:?}, 'a'); setTimeout(() => {{}}, 600000)",
        rollout.to_str().unwrap()
    );
    let pid = f.scripted(
        "codex",
        &args,
        &[("HOME", &elsewhere), ("CODEX_HOME", &task)],
        &script,
    );
    let theirs = f.named(
        "codex",
        &args,
        &[("HOME", &elsewhere), ("CODEX_HOME", &stray)],
    );

    let rows = f.rows_when("codex", |row| row["pid"] == pid);
    let row = rows.iter().find(|r| r["pid"] == pid).expect("no codex row");
    assert_eq!(row["session_id"], RUN, "{row}");
    assert_eq!(row["model"], "gpt-5", "{row}");
    assert_eq!(row["title"], "update the docs", "{row}");
    assert_eq!(
        row["transcript_path"].as_str().map(PathBuf::from),
        rollout.canonicalize().ok(),
        "{row}"
    );
    let shown = f.ok(&["show", RUN]);
    assert!(shown.contains("update the docs"), "{shown}");
    assert!(rows.iter().all(|row| row["pid"] != theirs), "{rows:?}");

    // The user's own dashboard on this machine is outside the test's fleet: it never lists
    // the stand-in, or every gate run flickers rows into it.
    let outside = Command::new(env!("CARGO_BIN_EXE_cones"))
        .env("HOME", f.path())
        .env_remove("CONES_TEST_FLEET")
        .env_remove("CODEX_HOME")
        .args(["--jobs", f.path().join("jobs.yaml").to_str().unwrap()])
        .args(["--state-dir", f.path().join("outside").to_str().unwrap()])
        .args(["ls", "--json"])
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&outside.stdout);
    assert!(outside.status.success(), "{outside:?}");
    assert!(!listed.contains(RUN), "{listed}");
}

/// An npm install runs Codex under a node wrapper that passes its arguments on, so the process
/// table shows the client twice. The child is the client: one row, and it is the child's.
#[test]
fn an_npm_wrapper_and_the_client_it_runs_are_one_row() {
    let mut f = Fixture::new();
    let user = f.path().to_owned();
    fs::create_dir_all(f.path().join(".codex")).unwrap();
    let socket = f.path().join("wrapper.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let client = format!(
        "require('net').createConnection({}); setInterval(() => process.ppid === 1 && process.exit(), 200)",
        json!(socket)
    );
    // The child leaves with its wrapper, which the fixture ends.
    let wrapper = f.scripted(
        "codex",
        &["exec", "review the wrapper"],
        &[("HOME", &user)],
        &format!(r#"const child = require("child_process").spawn(process.execPath, ["-", ...process.argv.slice(2)],
            {{ argv0: "codex", stdio: ["pipe", "ignore", "ignore"] }});
        child.stdin.end({});
        setTimeout(() => {{}}, 600000)"#, json!(client)),
    );
    // Both processes are in the table before either row is judged.
    let rows = f.rows_when("codex", |row| {
        row["title"] == "review the wrapper" && row["pid"] != wrapper
    });
    let mine: Vec<_> = rows
        .iter()
        .filter(|r| r["title"] == "review the wrapper")
        .collect();
    assert_eq!(mine.len(), 1, "{rows:?}");
    assert_ne!(mine[0]["pid"], wrapper, "{}", mine[0]);
    let _connection = accept_connection(&listener);
    #[cfg(target_os = "macos")]
    {
        let dir = f.path().join(".codex/app-server-daemon");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("app-server.pid"),
            json!({"pid":std::process::id()}).to_string(),
        )
        .unwrap();
        let listed = f.ok(&["ls", "--json"]);
        assert!(!listed.contains("review the wrapper"), "{listed}");
        let artifact = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("codex-daemon-wrapper.json");
        fs::write(artifact, listed).unwrap();
    }
}

/// External clients have real socket connections but no `--resume` identity. Only the
/// daemon's thread is a row, including when the client names a different Codex home.
#[cfg(target_os = "macos")]
#[test]
fn external_codex_daemon_clients_have_no_duplicate_row() {
    let artifact = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("codex-daemon-clients.json");
    let mut cases = Vec::new();
    for other_home in [false, true] {
        let mut f = Fixture::new();
        let daemon_home = f.path().join(".codex");
        let client_home = if other_home {
            f.path().join("task-codex")
        } else {
            daemon_home.clone()
        };
        fs::create_dir_all(&client_home).unwrap();
        fs::write(client_home.join("config.toml"), "").unwrap();
        let (pid, _connection) = f.connected_codex(&client_home, &[], "");
        let (prompted, _prompt_connection) =
            f.connected_codex(&client_home, &["fix the widget"], "");
        // First prove both stand-ins are discoverable without a known daemon.
        let before = f.rows_when("codex", |row| row["pid"] == pid);
        assert!(
            before.iter().any(|row| row["pid"] == prompted),
            "{before:?}"
        );
        fs::create_dir_all(daemon_home.join("app-server-daemon")).unwrap();
        fs::write(
            daemon_home.join("app-server-daemon/app-server.pid"),
            json!({"pid":std::process::id()}).to_string(),
        )
        .unwrap();
        let day = daemon_home.join("sessions/2026/10/11");
        fs::create_dir_all(&day).unwrap();
        fs::create_dir_all(daemon_home.join("thread-writer-locks")).unwrap();
        fs::write(
            day.join(format!("rollout-{RUN}.jsonl")),
            format!(
                "{}\n",
                json!({"type":"session_meta","payload":{"id":RUN,"session_id":RUN,
                "timestamp":"2026-10-11T10:00:05Z","cwd":f.path()}})
            ),
        )
        .unwrap();
        let _lock =
            fs::File::create(daemon_home.join(format!("thread-writer-locks/{RUN}.lock"))).unwrap();
        let after = f.rows_when("codex", |row| row["session_id"] == RUN);
        let stop = f.cones(&["stop", &format!("codex-{pid}")]);
        let result = json!({
            "other_home":other_home,
            "client_rows":after.iter().filter(|row| row["pid"] == pid || row["pid"] == prompted).count(),
            "thread_rows":after.iter().filter(|row| row["session_id"] == RUN).count(),
            "stop_succeeded":stop.status.success(),
            "client_alive":cones::fleet::alive(pid),
        });
        assert_eq!(
            result,
            json!({"other_home":other_home, "client_rows":0,
            "thread_rows":1, "stop_succeeded":false, "client_alive":true})
        );
        cases.push(result);
    }
    fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    fs::write(&artifact, serde_json::to_string_pretty(&cases).unwrap()).unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<Value>>(&fs::read_to_string(artifact).unwrap()).unwrap(),
        cases
    );
}

#[cfg(target_os = "macos")]
#[test]
fn a_socket_connected_codex_writer_keeps_its_row() {
    let mut f = Fixture::new();
    let home = f.path().join(".codex");
    fs::create_dir_all(home.join("thread-writer-locks")).unwrap();
    fs::create_dir_all(home.join("app-server-daemon")).unwrap();
    fs::write(
        home.join("app-server-daemon/app-server.pid"),
        json!({"pid":std::process::id()}).to_string(),
    )
    .unwrap();
    let script = format!(
        "require('fs').openSync({}, 'w')",
        json!(home.join("thread-writer-locks/standalone.lock"))
    );
    let (pid, _connection) = f.connected_codex(&home, &[], &script);
    let rows = f.rows_when("codex", |row| row["pid"] == pid);
    assert_eq!(
        rows.iter().filter(|row| row["pid"] == pid).count(),
        1,
        "{rows:?}"
    );
    let artifact = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("codex-socket-writer.json");
    let result = json!({"writer_rows":rows.iter().filter(|row| row["pid"] == pid).count()});
    fs::write(artifact, serde_json::to_string_pretty(&result).unwrap()).unwrap();
    assert_eq!(result, json!({"writer_rows":1}));
}
