//! Codex 0.154 rollout, ps and lsof fixtures; no harness or model calls.
use cones::codex::{
    Meta, Process, attribute, cwds, home, home_of, homes, meta, processes, rows, sessions, tail,
    titles,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

const CODEX_META: &str = r#"{"timestamp":"2026-09-12T09:17:08.160Z","ordinal":0,"type":"session_meta","payload":{"session_id":"01a094e7-c194-7980-9804-34f24290597e","id":"01a094e7-c194-7980-9804-34f24290597e","timestamp":"2026-09-12T09:16:51.535Z","cwd":"/Users/me/work/pocs/workbench","originator":"codex-tui","cli_version":"0.154.0","source":"vscode","base_instructions":{"text":"You are Codex"}}}"#;
const CODEX_TURN: &str = r#"{"timestamp":"2026-09-12T09:33:11.539Z","ordinal":539,"type":"event_msg","payload":{"type":"task_started","turn_id":"01a094f6"}}
{"timestamp":"2026-09-12T09:33:11.540Z","ordinal":540,"type":"turn_context","payload":{"turn_id":"01a094f6","cwd":"/Users/me/work/pocs/workbench","model":"openai.gpt-6-astra","approval_policy":"never"}}
{"timestamp":"2026-09-12T09:33:11.600Z","ordinal":540,"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>/Users/me</cwd>\n</environment_context>"}]}}
{"timestamp":"2026-09-12T09:33:11.700Z","ordinal":541,"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"so how do i run computer use here"}]}}
{"timestamp":"2026-09-12T09:35:32.562Z","ordinal":602,"type":"response_item","payload":{"type":"message","id":"msg_6f82","role":"assistant","content":[{"type":"output_text","text":"\n**For native Computer Use**, the verified setup is the desktop app.\n\nOpen this workspace:"}]}}
{"timestamp":"2026-09-12T09:35:32.606Z","ordinal":603,"type":"token_usage_record","payload":{"usage":{"input_tokens":184106,"output_tokens":1543}}}
not json
{"timestamp":"2026-09-12T09:35:32.613Z","ordinal":605,"type":"event_msg","payload":{"type":"task_complete","turn_id":"01a094f6","last_agent_message":"For native Computer Use"}}"#;

fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(s).unwrap().to_utc()
}

#[test]
fn processes_come_from_the_program_name_not_the_command_text() {
    let ps = "\
53203 Sun Sep 13 15:19:19 2026     /bin/zsh -c eval 'codex app-server' && echo codex
53210 Sun Sep 13 15:19:19 2026     codex
53211 Thu Sep  3 09:05:07 2026     /Users/me/.codex/packages/standalone/current/bin/codex exec --json \"fix the widget\"
53212 Sun Sep 13 15:20:00 2026     codex app-server
53213 Sun Sep 13 15:20:01 2026     codex \"fix the bug\"
  999 Sun Sep 13 15:20:02 2026     node /opt/homebrew/lib/node_modules/@openai/codex/bin/codex.js
garbage
";
    let procs = processes(ps);
    let pids: Vec<u32> = procs.iter().map(|p| p.pid).collect();
    assert_eq!(pids, [53210, 53211, 53213]);
    assert_eq!(
        procs[1].started.to_rfc3339(),
        "2026-09-03T09:05:07+00:00",
        "lstart under UTC, the day padded with a space"
    );
    assert!(procs.iter().all(|p| p.cwd.is_none()), "cwd waits for lsof");
    let cwds = cwds("p53210\nfcwd\nn/Users/me/src/app\np53213\nfcwd\nn/tmp/x\n");
    assert_eq!(
        cwds.get(&53210).map(|p| p.to_string_lossy().into_owned()),
        Some("/Users/me/src/app".to_owned())
    );
    assert_eq!(cwds.len(), 2);
}

#[test]
fn rollout_lines_give_meta_last_reply_and_turn_state() {
    let m = meta(CODEX_META).unwrap();
    assert_eq!(m.session_id, "01a094e7-c194-7980-9804-34f24290597e");
    assert_eq!(m.cwd.to_string_lossy(), "/Users/me/work/pocs/workbench");
    assert_eq!(m.started.to_rfc3339(), "2026-09-12T09:16:51.535+00:00");
    assert!(meta(CODEX_TURN.lines().next().unwrap()).is_none());
    assert!(
        meta(&CODEX_META.replace("01a094e7-c194-7980-9804-34f24290597e", "../x")).is_none(),
        "an id that is not a plain token names no row"
    );

    let t = tail(CODEX_TURN);
    assert_eq!(
        t.last.as_deref(),
        Some("For native Computer Use, the verified setup is the desktop app.")
    );
    assert_eq!(t.state, Some("done"));
    assert_eq!(t.model.as_deref(), Some("openai.gpt-6-astra"));
    assert_eq!(t.bypass, Some(true));
    assert_eq!(
        tail(&CODEX_TURN.replace("\"never\"", "\"on-request\"")).bypass,
        Some(false)
    );
    assert_eq!(
        t.last_activity.unwrap().to_rfc3339(),
        "2026-09-12T09:35:32.613+00:00"
    );
    let mid_turn: String = CODEX_TURN.lines().take(4).collect::<Vec<_>>().join("\n");
    assert_eq!(tail(&mid_turn).state, Some("active"));
    assert_eq!(tail("").state, None, "no turn recorded, no state");

    let titles = titles(
        "{\"id\":\"a\",\"thread_name\":\"Old name\"}\n{\"id\":\"a\",\"thread_name\":\"Polish filters\"}\n{\"id\":\"b\",\"thread_name\":\"\"}\nbroken\n",
    );
    assert_eq!(titles.get("a").map(String::as_str), Some("Polish filters"));
    assert!(!titles.contains_key("b"));
}

#[test]
fn a_rollout_belongs_to_the_only_process_in_its_directory() {
    let process = |pid, started, cwd: &str| Process {
        pid,
        started: at(started),
        cwd: Some(PathBuf::from(cwd)),
        thread: None,
        remote: false,
        prompt: None,
    };
    let rollout = |name: &str, started, cwd: &str| {
        (
            PathBuf::from(name),
            Meta {
                forked_from: None,
                session_id: name.into(),
                cwd: PathBuf::from(cwd),
                started: at(started),
            },
        )
    };
    let a = process(1, "2026-09-13T10:00:00Z", "/repo");
    let b = process(2, "2026-09-13T10:05:00Z", "/repo");
    let c = process(3, "2026-09-13T10:00:00Z", "/other");
    let rollouts = [
        rollout("r0", "2026-09-13T09:00:00Z", "/repo"),
        rollout("r1", "2026-09-13T10:00:00.5Z", "/repo"),
        rollout("r2", "2026-09-13T10:05:01Z", "/repo"),
        rollout("c1", "2026-09-13T10:00:01Z", "/other"),
        rollout("c2", "2026-09-13T10:30:00Z", "/other"),
    ];
    let owned = attribute(&[a.clone(), b, c], &rollouts);
    assert_eq!(
        owned[&1].1.session_id, "r1",
        "r0 predates a, and r2 could be b's"
    );
    assert!(
        !owned.contains_key(&2),
        "two processes could have written r2"
    );
    assert_eq!(
        owned[&3].1.session_id, "c2",
        "the newest rollout is the live thread"
    );
    let alone = attribute(std::slice::from_ref(&a), &rollouts);
    assert_eq!(alone[&1].1.session_id, "r2");
    let unknown_cwd = Process { cwd: None, ..a };
    assert!(attribute(&[unknown_cwd], &rollouts).is_empty());
}

#[test]
fn rows_read_the_rollout_and_the_session_index() {
    let dir = tempfile::tempdir().unwrap();
    let codex = dir.path();
    let day = codex.join("sessions/2026/09/12");
    fs::create_dir_all(&day).unwrap();
    let rollout =
        day.join("rollout-2026-09-12T12-16-51-01a094e7-c194-7980-9804-34f24290597e.jsonl");
    fs::write(&rollout, format!("{CODEX_META}\n{CODEX_TURN}\n")).unwrap();
    fs::write(
        codex.join("session_index.jsonl"),
        "{\"id\":\"01a094e7-c194-7980-9804-34f24290597e\",\"thread_name\":\"Polish shared filter controls\",\"updated_at\":\"2026-09-12T09:17:16Z\"}\n",
    )
    .unwrap();
    let procs = [
        Process {
            pid: 700,
            started: at("2026-09-12T09:16:50Z"),
            cwd: Some("/Users/me/work/pocs/workbench".into()),
            thread: None,
            remote: false,
            prompt: None,
        },
        Process {
            pid: 701,
            started: at("2026-09-12T09:10:00Z"),
            cwd: Some("/Users/me/elsewhere".into()),
            thread: None,
            remote: false,
            prompt: None,
        },
    ];
    let list = rows(codex, &procs);
    assert_eq!(list.len(), 2);
    let (bare, matched) = (&list[0], &list[1]);
    assert_eq!(
        bare.session_id, "codex-701",
        "oldest first; without a rollout the row is named by its pid"
    );
    assert_eq!(
        (
            bare.state.as_str(),
            bare.title.as_deref(),
            bare.last.as_deref()
        ),
        ("-", None, None)
    );
    assert_eq!(
        (bare.started, bare.last_activity, bare.model.as_deref()),
        (Some(at("2026-09-12T09:10:00Z")), None, None),
        "the process start is reported; nothing else is, and nothing stands in for it"
    );
    assert!(bare.transcript_path.is_none());

    assert_eq!(matched.session_id, "01a094e7-c194-7980-9804-34f24290597e");
    assert_eq!(matched.harness, "codex");
    assert_eq!(
        matched.title.as_deref(),
        Some("Polish shared filter controls")
    );
    assert_eq!(matched.state, "done");
    assert_eq!(
        matched.last.as_deref(),
        Some("For native Computer Use, the verified setup is the desktop app.")
    );
    assert_eq!(matched.last_activity, Some(at("2026-09-12T09:35:32.613Z")));
    assert_eq!(matched.model.as_deref(), Some("openai.gpt-6-astra"));
    assert_eq!(
        (matched.bypass, bare.bypass),
        (Some(true), None),
        "approval_policy never runs without prompts; without a rollout nothing is reported"
    );
    assert_eq!(matched.started, Some(at("2026-09-12T09:16:50Z")));
    assert_eq!(matched.pid, Some(700));
    assert_eq!(matched.transcript_path.as_deref(), Some(rollout.as_path()));
    assert_eq!(
        (
            matched.tokens_in,
            matched.tokens_out,
            matched.context_tokens,
            matched.cost_usd
        ),
        (None, None, None, None),
        "no token or dollar figure for Codex"
    );
    assert_eq!(cones::fleet::tokens(matched), "-");

    assert_eq!(
        cones::fleet::tail(&rollout, 5).1,
        ["For native Computer Use, the verified setup is the desktop app."]
    );
    assert_eq!(
        cones::fleet::exchange(&rollout),
        [
            "> so how do i run computer use here",
            "",
            "For native Computer Use, the verified setup is the desktop app.",
            "",
            "Open this workspace:"
        ],
        "the environment block Codex files as a user message is not a prompt"
    );
    assert!(rows(codex, &[]).is_empty());
}

#[test]
fn no_codex_home_means_no_process_scan() {
    let dir = tempfile::tempdir().unwrap();
    let claude = dir.path().join(".claude");
    if std::env::var_os("CODEX_HOME").is_none_or(|d| d.is_empty()) {
        assert_eq!(home(&claude), dir.path().join(".codex"));
    }
    assert!(sessions(&dir.path().join(".codex")).unwrap().is_empty());
    assert!(
        cones::fleet::all(&claude)
            .unwrap()
            .iter()
            .all(|session| session.harness != "codex"),
        "an unrelated installed harness does not give Codex a native home"
    );
}

/// A process table that cannot be read means unknown, not "no Codex or pi client is running":
/// swallowing it dropped every native row for that read, so the rows flickered.
#[test]
fn an_unreadable_process_table_is_an_error_not_an_empty_fleet() {
    let dir = tempfile::tempdir().unwrap();
    let codex = dir.path().join(".codex");
    std::fs::create_dir_all(codex.join("sessions")).unwrap();
    let pi = dir.path().join(".pi");
    std::fs::create_dir_all(&pi).unwrap();
    assert!(cones::codex::sessions_from("/no/such/ps", &codex).is_err());
    assert!(cones::pi::sessions_from("/no/such/ps", &pi).is_err());
    assert!(cones::codex::sessions_from("/bin/ps", &codex).is_ok());
}

#[test]
fn a_sibling_codex_home_is_read_beside_the_default_one() {
    // A pinned `CODEX_HOME` is the only home, so the sibling scan never runs.
    if std::env::var_os("CODEX_HOME").is_some_and(|d| !d.is_empty()) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let claude = dir.path().join(".claude");
    let blue = dir.path().join(".codex-blue");
    fs::create_dir_all(&blue).unwrap();
    fs::write(blue.join("config.toml"), "model = \"blue\"\n").unwrap();
    // A directory without a config is not a home.
    fs::create_dir_all(dir.path().join(".codex-scratch")).unwrap();
    assert_eq!(homes(&claude), vec![dir.path().join(".codex"), blue]);
}

#[test]
fn a_rollout_names_the_home_that_holds_it() {
    let blue = Path::new("/u/.codex-blue");
    let rollout = blue.join("sessions/2026/09/15/rollout-2026-09-15T23-37-15-01a0a6c9.jsonl");
    assert_eq!(
        home_of(&rollout),
        Some(blue),
        "a join resumes against the daemon of the home that holds the thread"
    );
    assert_eq!(home_of(Path::new("/tmp/loose.jsonl")), None);
}

/// A remote Codex client refuses permission flags on resume and fork ("Permission overrides
/// are not supported when resuming a remote task", Codex 0.154 to 0.163), so a resume from
/// history passes none whatever the skip setting. A fork is made by the daemon instead, which
/// takes the skip setting as the fork's approval and sandbox, and the client joins the thread
/// it made. The `codex` here answers only the daemon's address and the fork probe.
#[test]
fn a_codex_fork_takes_the_skip_setting_through_the_daemon_and_a_resume_passes_no_flag() {
    use cones::{config::Policy, harness, history};
    use std::os::unix::fs::PermissionsExt;
    const CHILD: &str = "CONES_CODEX_COMMAND_FIXTURE";
    if let Some(home) = std::env::var_os(CHILD) {
        let home = PathBuf::from(home);
        let codex = home.join(".codex");
        let calls = crate::stop::codex_daemon_in(&codex, &[]);
        let rollout = home.join("rollout.jsonl");
        fs::write(&rollout, format!("{CODEX_META}\n")).unwrap();
        let id = "01a094e7-c194-7980-9804-34f24290597e";
        let entry = history::Entry {
            key: history::Key {
                harness: "codex".into(),
                home: codex.clone(),
                session_id: id.into(),
            },
            cwd: home.clone(),
            moved_to: None,
            transcript: rollout,
            archived: false,
            started: None,
            last_activity: None,
            title: None,
            columns: None,
            hit: None,
        };
        let args = |c: &std::process::Command| -> Vec<String> {
            c.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        let fork = |policy: &Policy| match harness::fork(&entry, None, policy).unwrap() {
            harness::Start::Foreground(c) => args(&c),
            harness::Start::Background(_) => panic!("a Codex fork is a terminal client"),
        };
        let forks = || -> Vec<serde_json::Value> {
            calls
                .lock()
                .unwrap()
                .drain(..)
                .filter(|call| call["method"] == "thread/fork")
                .map(|call| call["params"].clone())
                .collect()
        };
        let remote = format!(
            "unix://{}",
            codex
                .join("app-server-control/app-server-control.sock")
                .display()
        );
        let prompting = Policy {
            codex_skip_permissions: Some(false),
            ..Policy::default()
        };
        for (policy, overrides) in [
            (
                &Policy::default(),
                serde_json::json!({"approvalPolicy": "never", "sandbox": "danger-full-access"}),
            ),
            (&prompting, serde_json::json!({})),
        ] {
            assert_eq!(
                fork(policy),
                ["--remote", &remote, "resume", "--", crate::stop::FORK]
            );
            let mut expected =
                serde_json::json!({"threadId": id, "cwd": home, "excludeTurns": true});
            expected
                .as_object_mut()
                .unwrap()
                .extend(overrides.as_object().unwrap().clone());
            assert_eq!(forks(), [expected]);
            assert_eq!(
                args(&harness::resume_history(&entry, policy).unwrap()),
                ["--remote", &remote, "resume", "--", id]
            );
        }
        println!("codex command fixture ran");
        return;
    }
    // Short: the daemon's socket lives under this folder and macOS bounds a socket path.
    let dir = tempfile::Builder::new()
        .prefix("cx-")
        .tempdir_in("/tmp")
        .unwrap();
    let bin = dir.path().join(".local/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(dir.path().join(".codex")).unwrap();
    let fake = bin.join("codex");
    fs::write(
        &fake,
        format!(
            "#!/bin/sh\ncase \"$*\" in\n  'app-server daemon start') echo '{{\"socketPath\":\"{}\"}}' ;;\n  --help) echo 'fork  fork a thread' ;;\n  *) exit 91 ;;\nesac\n",
            dir.path()
                .join(".codex/app-server-control/app-server-control.sock")
                .display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "codex::a_codex_fork_takes_the_skip_setting_through_the_daemon_and_a_resume_passes_no_flag",
            "--nocapture",
        ])
        .env(CHILD, dir.path())
        .env("HOME", dir.path())
        .env_remove("CODEX_HOME")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("codex command fixture ran"),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A daemon-connected launch has no process row, but its dashboard placeholder must
/// survive a completed refresh and then become exactly one identified conversation.
#[cfg(target_os = "macos")]
#[test]
fn a_daemon_client_launch_keeps_its_placeholder_until_the_thread_arrives() {
    use crate::dashboard::Dashboard;
    use serde_json::json;
    use std::{
        os::unix::{fs::PermissionsExt, net::UnixListener},
        time::Duration,
    };

    let mut d = Dashboard::new("codex-pending-daemon-client", &["codex"]);
    let home = d.home().join(".codex");
    fs::create_dir_all(home.join("app-server-daemon")).unwrap();
    fs::create_dir_all(home.join("thread-writer-locks")).unwrap();
    fs::write(
        home.join("app-server-daemon/app-server.pid"),
        json!({"pid": std::process::id()}).to_string(),
    )
    .unwrap();
    let listener = UnixListener::bind(home.join("fixture.sock")).unwrap();
    let program = d.home().join(".local/bin/codex");
    fs::write(
        &program,
        r#"#!/usr/bin/python3
import json, os, pathlib, socket, sys, time
home = pathlib.Path(os.environ["CODEX_HOME"])
if sys.argv[1:] == ["app-server", "daemon", "start"]:
    print(json.dumps({"cliVersion": "0.163.0", "socketPath": str(home / "fixture.sock")}))
    sys.exit(0)
sock = socket.socket(socket.AF_UNIX)
sock.connect(str(home / "fixture.sock"))
(home / "viewer.pid").write_text(str(os.getpid()))
while True:
    time.sleep(0.1)
"#,
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    d.start();
    d.typed("keep the pending launch");
    d.press("launch", b"\r");
    let pid: u32 = d
        .wait_file("home/.codex/viewer.pid", |s| s.parse::<u32>().is_ok())
        .trim()
        .parse()
        .unwrap();
    // Close only the fixture client on failure too; the daemon here is the test process.
    struct Client(u32);
    impl Drop for Client {
        fn drop(&mut self) {
            unsafe {
                libc::kill(self.0 as i32, libc::SIGTERM);
            }
        }
    }
    let _client = Client(pid);
    let _connection = crate::homes::accept_connection(&listener);
    d.press("back to list", b"\x1a");
    let cwd = d.project().canonicalize().unwrap();
    let write_thread = |id: &str, title: &str| {
        let day = home.join("sessions/2026/10/16");
        fs::create_dir_all(&day).unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        fs::write(
            day.join(format!("rollout-{id}.jsonl")),
            format!(
                "{}\n{}\n{}\n",
                json!({"type":"session_meta", "payload":{"id":id,"session_id":id,
                "timestamp":now,"cwd":cwd}}),
                json!({"type":"event_msg","payload":{"item":{"type":"UserMessage",
                "content":[{"type":"text","text":title}]}}}),
                json!({"type":"event_msg","payload":{"type":"task_started"}})
            ),
        )
        .unwrap();
        fs::File::create(home.join(format!("thread-writer-locks/{id}.lock"))).unwrap()
    };
    let _marker = write_thread("01a14435-4399-7903-96fb-35d94a9f6201", "refresh marker");
    // Seeing the newly created marker proves discovery completed after the socket connected.
    d.wait_text("refresh marker");
    let pending = d.capture("pending-after-refresh");
    assert_eq!(
        pending.matches("keep the pending launch").count(),
        1,
        "{pending}"
    );
    let id = "01a14435-4399-7903-96fb-35d94a9f6202";
    let _thread = write_thread(id, "keep the pending launch");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !cones::codex::threads(&d.path("state"))
        .iter()
        .any(|t| t.id == id)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "launch identity was not recorded: {}",
            d.screen()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    d.wait_for("one identified launch", |s| {
        s.matches("keep the pending launch").count() == 1
    });
    let identified = d.capture("identified-launch");
    assert_eq!(
        identified.matches("keep the pending launch").count(),
        1,
        "{identified}"
    );
    assert!(
        cones::fleet::alive(pid),
        "identity replacement preserves the viewer"
    );
    d.quit();
}
