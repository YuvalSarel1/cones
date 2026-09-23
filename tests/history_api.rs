//! Agent history access uses disposable native archives, never a harness or a model.
use cones::{
    config::HarnessKind,
    history::Source,
    history_api::{Search, Service, Show},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const C: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const D: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";

struct Fixture {
    root: tempfile::TempDir,
    sources: Vec<Source>,
    project: PathBuf,
}

fn write(path: &Path, rows: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        rows.iter().map(|v| format!("{v}\n")).collect::<String>(),
    )
    .unwrap();
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(project.join("child")).unwrap();
        let sources = [
            ("claude", HarnessKind::Claude),
            ("codex", HarnessKind::Codex),
            ("pi", HarnessKind::Pi),
            ("opencode", HarnessKind::Opencode),
        ]
        .into_iter()
        .map(|(name, harness)| Source {
            harness,
            home: root.path().join(name),
        })
        .collect::<Vec<_>>();
        for (id, cwd, at) in [
            (A, project.clone(), "2026-09-20T10:00:00Z"),
            (B, project.join("child"), "2026-09-21T10:00:00Z"),
        ] {
            write(
                &sources[0].home.join(format!("projects/fixture/{id}.jsonl")),
                &[
                    json!({"type":"user","sessionId":id,"cwd":cwd,"timestamp":at,"message":{"content":"Find needle cobalt שלום"}}),
                    json!({"type":"assistant","timestamp":at,"message":{"content":[
                        {"type":"text","text":"We found needle cobalt \u{1b}[31mclean\u{1b}[0m"},
                        {"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"result.rs"}}
                    ]}}),
                    json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"private tool result"}]}}),
                ],
            );
        }
        write(
            &sources[1]
                .home
                .join(format!("archived_sessions/rollout-{C}.jsonl")),
            &[
                json!({"type":"session_meta","payload":{"id":C,"cwd":"/other","source":"cli","timestamp":"2026-09-22T10:00:00Z"}}),
                json!({"type":"event_msg","timestamp":"2026-09-22T10:00:00Z","payload":{"item":{"type":"UserMessage","content":[{"type":"text","text":"needle cobalt archive"}]}}}),
                json!({"type":"response_item","timestamp":"2026-09-22T10:00:01Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Archive answer"}]}}),
            ],
        );
        write(
            &sources[2].home.join(format!("sessions/fixture/{D}.jsonl")),
            &[
                json!({"type":"session","id":D,"cwd":"/other","timestamp":"2026-09-22T11:00:00Z"}),
                json!({"type":"message","timestamp":"2026-09-22T11:00:00Z","message":{"role":"user","content":"needle cobalt pi"}}),
                json!({"type":"message","timestamp":"2026-09-22T11:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"Pi answer"}]}}),
            ],
        );
        fs::create_dir_all(&sources[3].home).unwrap();
        let db = rusqlite::Connection::open(sources[3].home.join("opencode.db")).unwrap();
        db.execute_batch(include_str!("../assets/harnesses/fixtures/opencode.sql"))
            .unwrap();
        db.execute(
            "UPDATE part SET data=?1 WHERE id='prt_b2'",
            [json!({"type":"text","text":"needle cobalt OpenCode"}).to_string()],
        )
        .unwrap();
        drop(db);
        Self {
            root,
            sources,
            project,
        }
    }

    fn service(&self) -> Service {
        Service::new(
            self.sources.clone(),
            Some(self.root.path().join("state")),
            self.root.path().to_owned(),
        )
    }

    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_cones"));
        c.args([
            "--state-dir",
            self.root.path().join("state").to_str().unwrap(),
        ])
        .env("HOME", self.root.path())
        .env("CLAUDE_CONFIG_DIR", &self.sources[0].home)
        .env("CODEX_HOME", &self.sources[1].home)
        .env("PI_CODING_AGENT_DIR", &self.sources[2].home)
        // The in-process service reads OpenCode below its canonical home.
        .env(
            "OPENCODE_DB",
            self.sources[3]
                .home
                .join("opencode.db")
                .canonicalize()
                .unwrap(),
        )
        .env("XDG_DATA_HOME", self.root.path())
        .env_remove("PI_CODING_AGENT_SESSION_DIR")
        .env_remove("OPENCODE_TUI_CONFIG");
        c
    }

    fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        fn visit(path: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(&path, out);
                } else {
                    out.push((path.clone(), fs::read(path).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        for source in &self.sources {
            visit(&source.home, &mut out);
        }
        out.sort();
        out
    }
}

fn query() -> Search {
    Search {
        query: "needle cobalt".into(),
        ..Default::default()
    }
}

fn exchange(input: &mut impl Write, output: &mut impl BufRead, request: Value) -> Value {
    writeln!(input, "{request}").unwrap();
    input.flush().unwrap();
    let mut line = String::new();
    assert!(output.read_line(&mut line).unwrap() > 0);
    serde_json::from_str(&line).unwrap()
}

fn indexed_sessions(f: &Fixture) -> i64 {
    let path = fs::read_dir(f.root.path().join("state/search"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "sqlite"))
        .unwrap();
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn search_scopes_before_paginating_and_keeps_other_projects_indexed() {
    let f = Fixture::new();
    let before = f.snapshot();
    let mut service = f.service();
    let found = service.search(query()).unwrap();
    assert_eq!(
        found.total,
        5,
        "{:?}",
        found.entries.iter().map(|e| &e.key).collect::<Vec<_>>()
    );
    assert_eq!(found.entries.len(), 5);
    assert!(found.complete && !found.pending);
    assert!(found.error.is_none() && found.status.is_none());
    assert_eq!(found.offset, 0);
    assert!(found.next_offset.is_none());
    assert_eq!(
        indexed_sessions(&f),
        7,
        "index includes nonmatching and archived sessions"
    );
    assert!(
        found
            .entries
            .iter()
            .any(|e| e.key.session_id == C && e.archived)
    );
    assert!(
        found
            .entries
            .iter()
            .any(|e| e.key.harness == "opencode" && e.key.session_id == "ses_fixture")
    );
    for e in &found.entries {
        assert!(e.key.home.is_absolute());
        assert!(e.transcript.is_file());
        let hit = e.hit.as_ref().unwrap();
        assert!(!hit.semantic && hit.score > 0.0);
        assert!(hit.snippet.contains("needle") && hit.snippet.contains("cobalt"));
        assert!(hit.anchor.is_some());
    }
    let alias = f.root.path().join("alias");
    std::os::unix::fs::symlink(&f.project, &alias).unwrap();
    let scoped = Search {
        dir: Some(alias),
        limit: 1,
        ..query()
    };
    let first = service.search(scoped.clone()).unwrap();
    assert_eq!(first.total, 2);
    assert_eq!(first.entries[0].key.session_id, B);
    assert_eq!(first.next_offset, Some(1));
    assert_eq!(
        indexed_sessions(&f),
        7,
        "scoping does not prune other projects"
    );
    let next = service
        .search(Search {
            offset: 1,
            ..scoped.clone()
        })
        .unwrap();
    assert_eq!(next.entries[0].key.session_id, A);
    assert_eq!(next.offset, 1);
    assert!(next.next_offset.is_none());
    let since = service
        .search(Search {
            since: Some("2026-09-21T00:00:00Z".parse().unwrap()),
            ..scoped
        })
        .unwrap();
    assert_eq!(since.total, 1);
    assert_eq!(since.entries[0].key.session_id, B);
    let codex = service
        .search(Search {
            harness: Some("codex".into()),
            ..query()
        })
        .unwrap();
    assert_eq!(codex.total, 1);
    assert_eq!(codex.entries[0].key.session_id, C);
    assert_eq!(service.search(query()).unwrap().total, 5);
    assert_eq!(
        f.snapshot(),
        before,
        "history access changes no native files"
    );
    assert!(
        fs::read_dir(f.root.path().join("state/search"))
            .unwrap()
            .all(|e| e.unwrap().file_type().unwrap().is_file()),
        "word search creates no model directory"
    );
    let path = f.sources[0]
        .home
        .join(format!("projects/fixture/{A}.jsonl"));
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{}", json!({"type":"assistant","timestamp":"2026-09-23T00:00:00Z","message":{"content":"newlydiscovered"}})).unwrap();
    let fresh = service
        .search(Search {
            query: "newlydiscovered".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        fresh.total, 1,
        "a persistent client refreshes changed archives"
    );
    assert_eq!(fresh.entries[0].key.session_id, A);
}

#[test]
fn show_returns_exact_identity_messages_tools_omissions_and_partial_record_state() {
    let mut f = Fixture::new();
    let second = f.root.path().join("second-claude");
    let original = f.sources[0]
        .home
        .join(format!("projects/fixture/{A}.jsonl"));
    let copy = second.join(format!("projects/fixture/{A}.jsonl"));
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    fs::copy(&original, &copy).unwrap();
    f.sources.push(Source {
        harness: HarnessKind::Claude,
        home: second,
    });
    let service = f.service();
    let request = || Show {
        id: A.into(),
        harness: Some("claude".into()),
        home: None,
        tail: Some(1),
        all: false,
    };
    assert!(
        service
            .show(request())
            .unwrap_err()
            .to_string()
            .contains("matches 2 sessions")
    );
    let read = || Show {
        home: Some(f.sources[0].home.clone()),
        ..request()
    };
    let result = service.show(read()).unwrap();
    assert_eq!(
        result["session"],
        json!({"harness":"claude","home":f.sources[0].home.canonicalize().unwrap(),"session_id":A})
    );
    assert_eq!(result["cwd"], json!(f.project));
    assert_eq!(result["omitted"], 1);
    assert_eq!(result["incomplete"], false);
    assert_eq!(result["messages"].as_array().unwrap().len(), 1);
    assert_eq!(result["messages"][0]["role"], "assistant");
    assert_eq!(result["messages"][0]["at"], "2026-09-20T10:00:00Z");
    assert_eq!(
        result["messages"][0]["text"],
        "We found needle cobalt clean"
    );
    assert_eq!(
        result["messages"][0]["tools"],
        json!([{"name":"Read","input":"result.rs"}])
    );
    let mut file = fs::OpenOptions::new().append(true).open(&original).unwrap();
    write!(file, "{{\"type\":\"assistant\"").unwrap();
    assert_eq!(service.show(read()).unwrap()["incomplete"], true);
    let full = service
        .show(Show {
            tail: None,
            all: true,
            ..read()
        })
        .unwrap();
    assert_eq!(full["messages"].as_array().unwrap().len(), 2);
    assert_eq!(full["omitted"], 0);
    assert!(!full.to_string().contains("private tool result"));
}

#[test]
fn cli_search_and_json_show_share_the_service_and_report_invalid_inputs() {
    let f = Fixture::new();
    let before = f.snapshot();
    let mut service = f.service();
    let expected = serde_json::to_value(service.search(query()).unwrap()).unwrap();
    assert_eq!(expected["query"], "needle cobalt");
    assert_eq!(expected["mode"], "words");
    let output = f
        .command()
        .args(["search", "needle cobalt", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        expected
    );
    let output = f
        .command()
        .args(["show", A, "--json", "--tail", "1", "--harness", "claude"])
        .arg("--home")
        .arg(&f.sources[0].home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shown: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(shown["session"]["session_id"], A);
    assert_eq!(shown["omitted"], 1);
    assert_eq!(shown["messages"][0]["text"], "We found needle cobalt clean");
    for args in [
        vec!["search", "", "--json"],
        vec!["search", "needle", "--limit", "0", "--json"],
        vec!["search", "needle", "--harness", "absent", "--json"],
        vec!["search", "needle", "--wait-seconds", "61", "--json"],
        vec!["search", "needle", "--since", "yesterday", "--json"],
        vec!["show", A, "--tail", "0", "--json"],
        vec!["show", A, "--tail", "1", "--all", "--json"],
    ] {
        let out = f.command().args(&args).output().unwrap();
        assert!(!out.status.success(), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(!out.stderr.is_empty(), "{args:?}");
    }
    let none = f
        .command()
        .args(["search", "zzzznomatcheszzzz", "--json"])
        .output()
        .unwrap();
    assert!(none.status.success());
    let none: Value = serde_json::from_slice(&none.stdout).unwrap();
    assert_eq!(none["entries"], json!([]));
    assert_eq!(none["total"], 0);
    assert_eq!(none["complete"], true);
    assert_eq!(f.snapshot(), before);
}

#[test]
fn two_mcp_clients_search_and_read_one_cache_without_native_changes() {
    let f = Fixture::new();
    let before = f.snapshot();
    std::thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| {
                let mut child = f.command().arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
                let mut input = child.stdin.take().unwrap();
                let mut output = BufReader::new(child.stdout.take().unwrap());
                let init = exchange(&mut input, &mut output, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                    "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}
                }}));
                assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
                assert_eq!(init["result"]["capabilities"], json!({"tools":{"listChanged":false}}));
                // Send the notification before a request: it must produce no response itself.
                writeln!(input, "{}", json!({"jsonrpc":"2.0","method":"notifications/initialized"})).unwrap();
                let list = exchange(&mut input, &mut output, json!({"jsonrpc":"2.0","id":"tools","method":"tools/list"}));
                assert_eq!(list["id"], "tools");
                let tools = list["result"]["tools"].as_array().unwrap();
                assert_eq!(tools.len(), 2);
                assert_eq!(tools[0]["name"], "cones_search");
                assert_eq!(tools[0]["inputSchema"]["required"], json!(["query"]));
                assert_eq!(tools[0]["inputSchema"]["properties"]["limit"]["maximum"], 100);
                assert_eq!(tools[1]["name"], "cones_show");
                assert_eq!(tools[1]["annotations"]["readOnlyHint"], true);
                for id in [2, 3] {
                    let result = exchange(&mut input, &mut output, json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
                        "name":"cones_search","arguments":{"query":"needle cobalt","harness":"claude","limit":1}
                    }}));
                    assert_eq!(result["id"], id);
                    assert_eq!(result["result"]["isError"], false, "{result}");
                    let data = &result["result"]["structuredContent"];
                    assert_eq!(data["total"], 2);
                    assert_eq!(data["entries"][0]["key"]["session_id"], B);
                    assert_eq!(data["next_offset"], 1);
                    assert_eq!(data["complete"], true);
                    assert_eq!(serde_json::from_str::<Value>(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap(), *data);
                }
                let show = exchange(&mut input, &mut output, json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"cones_show","arguments":{"id":A,"tail":1}}}));
                assert_eq!(show["result"]["structuredContent"]["omitted"], 1);
                assert_eq!(show["result"]["structuredContent"]["messages"][0]["text"], "We found needle cobalt clean");
                let bad = exchange(&mut input, &mut output, json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"cones_show","arguments":{"id":"unknown"}}}));
                assert_eq!(bad["result"]["isError"], true);
                let ping = exchange(&mut input, &mut output, json!({"jsonrpc":"2.0","id":6,"method":"ping"}));
                assert_eq!(ping["result"], json!({}));
                drop(input);
                let result = child.wait_with_output().unwrap();
                assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
                assert!(result.stderr.is_empty());
            });
        }
    });
    assert_eq!(f.snapshot(), before);
}

#[test]
fn clients_with_different_native_homes_cannot_prune_each_others_active_search() {
    let f = Fixture::new();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        for index in [0, 1] {
            let f = &f;
            let barrier = &barrier;
            scope.spawn(move || {
                let source = f.sources[index].clone();
                let mut service = Service::new(
                    vec![source],
                    Some(f.root.path().join("state")),
                    f.root.path().to_owned(),
                );
                let mut results = Vec::new();
                for _ in 0..12 {
                    barrier.wait();
                    results.push(service.search(query()));
                }
                // Assert after every rendezvous so a failure cannot strand the other client.
                for result in results {
                    let result = result.unwrap();
                    let mut ids = result
                        .entries
                        .iter()
                        .map(|e| e.key.session_id.as_str())
                        .collect::<Vec<_>>();
                    ids.sort();
                    assert_eq!(ids, if index == 0 { vec![A, B] } else { vec![C] });
                    assert_eq!(result.total, ids.len());
                    assert!(result.complete);
                }
            });
        }
    });
}

#[test]
fn mcp_lifecycle_framing_and_tool_errors_are_distinct() {
    let f = Fixture::new();
    let lines = [
        "not json".to_owned(),
        json!({"jsonrpc":"2.0","id":null,"method":"ping"}).to_string(),
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string(),
        json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"future","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}).to_string(),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string(),
        json!({"jsonrpc":"2.0","method":"unknown/notification"}).to_string(),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"cones_search","arguments":{"query":"needle","unexpected":true}}}).to_string(),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"not_a_tool"}}).to_string(),
        json!({"jsonrpc":"2.0","id":5,"method":"unknown/method"}).to_string(),
        "x".repeat(1024 * 1024 + 2),
        json!({"jsonrpc":"2.0","id":6,"method":"ping"}).to_string(),
    ].join("\n") + "\n";
    let mut output = Vec::new();
    cones::history_mcp::serve(&mut f.service(), lines.as_bytes(), &mut output).unwrap();
    let replies = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 9, "notifications have no reply");
    assert_eq!(replies[0]["error"]["code"], -32700);
    assert_eq!(replies[1]["error"]["code"], -32600);
    assert_eq!(replies[2]["error"]["code"], -32000);
    assert_eq!(replies[3]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(replies[4]["result"]["isError"], true);
    assert!(
        replies[4]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown field")
    );
    assert_eq!(replies[5]["error"]["code"], -32602);
    assert_eq!(replies[6]["error"]["code"], -32601);
    assert_eq!(replies[7]["error"]["code"], -32600);
    assert_eq!(replies[8]["id"], 6);
    assert_eq!(replies[8]["result"], json!({}));
}
