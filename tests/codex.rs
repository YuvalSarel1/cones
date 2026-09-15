//! Codex in the fleet: fixture strings from a real Codex 0.154 rollout, `ps` and `lsof`. No
//! Codex process runs and no model token is spent.
use cones::codex::{
    Meta, Process, attribute, cwds, home, meta, processes, rows, sessions, tail, titles,
};
use std::{fs, path::PathBuf};

/// The first rollout line, trimmed to the fields cones reads.
const CODEX_META: &str = r#"{"timestamp":"2026-09-12T09:17:08.160Z","ordinal":0,"type":"session_meta","payload":{"session_id":"01a094e7-c194-7980-9804-34f24290597e","id":"01a094e7-c194-7980-9804-34f24290597e","timestamp":"2026-09-12T09:16:51.535Z","cwd":"/Users/me/work/pocs/workbench","originator":"codex-tui","cli_version":"0.154.0","source":"vscode","base_instructions":{"text":"You are Codex"}}}"#;
/// One turn: started, its context, the environment block and the prompt as user messages, a
/// reply, usage, a torn line, complete.
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
    };
    let rollout = |name: &str, started, cwd: &str| {
        (
            PathBuf::from(name),
            Meta {
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
        },
        Process {
            pid: 701,
            started: at("2026-09-12T09:10:00Z"),
            cwd: Some("/Users/me/elsewhere".into()),
            thread: None,
            remote: false,
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

    // The rollout is the session's log: `logs` and the details pane read Codex messages too.
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

/// A live `codex` on the developer's machine must not leak into a test's fleet: a temp Claude
/// dir has no `.codex` beside it, so the process table is never read.
#[test]
fn no_codex_home_means_no_process_scan() {
    let dir = tempfile::tempdir().unwrap();
    let claude = dir.path().join(".claude");
    if std::env::var_os("CODEX_HOME").is_none_or(|d| d.is_empty()) {
        assert_eq!(home(&claude), dir.path().join(".codex"));
    }
    assert!(sessions(&dir.path().join(".codex")).is_empty());
    assert!(cones::fleet::all(&claude).unwrap().is_empty());
}
