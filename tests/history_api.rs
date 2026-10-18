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
        at: None,
        context: None,
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
                let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
                assert_eq!(names, ["cones_search", "cones_show", "cones_ls", "cones_launch", "cones_resume", "cones_fork", "cones_send", "cones_wait", "cones_rename", "cones_btw", "cones_compact", "cones_stop", "cones_rm"]);
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

/// `index status` counts without loading a model; `index` reports every phase change as a
/// JSON line and ends on the model's failure. The model directory is obstructed, so neither
/// can download or run one.
#[test]
fn index_status_and_index_report_progress_without_a_model() {
    let f = Fixture::new();
    let before = f.snapshot();
    let models = f.root.path().join("state/search/models");
    fs::create_dir_all(&models).unwrap();
    let blocked = cones::search::model_directory(&models);
    fs::write(&blocked, "blocked").unwrap();

    let out = f
        .command()
        .args(["index", "status", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["phase"], "embedding");
    assert_eq!(status["conversations"], indexed_sessions(&f));
    assert!(status["passages"].as_i64().unwrap() > 0);
    assert_eq!(status["embedded"], 0);
    assert_eq!(status["remaining"], status["passages"]);
    assert!(status["rate"].is_null() && status["eta_seconds"].is_null());
    assert_eq!(status["model_downloaded"], false);
    assert_eq!(status["cache"], json!(f.root.path().join("state/search")));
    assert!(status["cache_bytes"].as_u64().unwrap() > 0);
    assert!(status["status"].is_null() && status["error"].is_null());
    assert_eq!(fs::read_to_string(&blocked).unwrap(), "blocked");

    let out = f.command().args(["index", "status"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let summary = String::from_utf8(out.stdout).unwrap();
    assert!(
        summary.starts_with(&format!(
            "0/{0} passages embedded · {0} remaining · {1} conversations · model not downloaded",
            status["passages"], status["conversations"]
        )),
        "{summary}"
    );

    // Another process holding the batch claim leaves this one waiting, not embedding.
    let claim = fs::File::create(f.root.path().join("state/search/fill.lock")).unwrap();
    fs2::FileExt::lock_exclusive(&claim).unwrap();
    let out = f
        .command()
        .args(["index", "--json", "--timeout", "1"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let phases: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap()["phase"].clone())
        .collect();
    assert_eq!(
        phases,
        [json!("syncing"), json!("waiting"), json!("waiting")]
    );
    drop(claim);

    let out = f.command().args(["index", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "a model failure exits 1");
    let lines: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["phase"], "syncing");
    let last = lines.last().unwrap();
    assert_eq!(last["phase"], "failed");
    assert!(last["error"].as_str().is_some_and(|e| !e.is_empty()));
    assert_eq!(last["passages"], status["passages"]);
    assert_eq!(last["embedded"], 0);
    assert!(
        lines[1..lines.len() - 1]
            .iter()
            .all(|l| l["phase"] == "downloading_model" && l["remaining"] == status["passages"]),
        "{lines:?}"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("cones: indexing failed:"));
    assert_eq!(fs::read_to_string(&blocked).unwrap(), "blocked");

    // Piped without --json, progress is one plain line per change on stderr.
    let out = f
        .command()
        .args(["index", "--timeout", "0"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "a timeout leaves work remaining"
    );
    let progress = String::from_utf8(out.stderr).unwrap();
    assert_eq!(
        progress,
        format!(
            "Reading conversations…\nDownloading model ▕{}▏ 0/{}\n",
            "░".repeat(24),
            status["passages"]
        )
    );
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .starts_with(&format!("0/{} passages embedded", status["passages"]))
    );
    assert_eq!(f.snapshot(), before, "native archives are never written");
}

/// Three Claude conversations mentioning the parser cache: a strong match 200 days old, a
/// medium one 100 days old and a weak one from yesterday. The strong one is a longer exchange.
/// Dates follow the clock, so the `when` windows hold whenever the suite runs.
fn ranked_fixture(f: &Fixture) {
    let now = chrono::Utc::now();
    let day = |n: i64| (now - chrono::Duration::days(n)).to_rfc3339();
    let session = |id: &str, age: i64, messages: Vec<(&str, String)>| {
        let rows: Vec<Value> = messages
            .into_iter()
            .enumerate()
            .map(|(i, (role, text))| {
                let mut row = json!({"type":role,"timestamp":day(age),"message":{"content":text}});
                if i == 0 {
                    row["sessionId"] = json!(id);
                    row["cwd"] = json!(f.project);
                }
                row
            })
            .collect();
        write(
            &f.sources[0]
                .home
                .join(format!("projects/fixture/{id}.jsonl")),
            &rows,
        );
    };
    let mut strong = vec![("user", "start the old work".to_owned())];
    for i in 0..20 {
        strong.push((
            "assistant",
            if i == 10 {
                "parser cache: parser cache invalidation".to_owned()
            } else {
                format!("routine step {i}")
            },
        ));
    }
    session(OLD, 200, strong);
    session(
        MID,
        100,
        vec![
            ("user", "investigate slowness".into()),
            ("assistant", "the parser cache was slow".into()),
        ],
    );
    session(
        NEW,
        1,
        vec![(
            "user",
            "check whether the cache used by the parser module is stale before we ship this release"
                .into(),
        )],
    );
}

const OLD: &str = "eeeeeeee-0000-4000-8000-000000000000";
const NEW: &str = "ffffffff-0000-4000-8000-000000000000";
const MID: &str = "abababab-0000-4000-8000-000000000000";

#[test]
fn words_need_two_terms_when_nudges_a_window_up_and_show_opens_at_the_hit() {
    let f = Fixture::new();
    ranked_fixture(&f);
    let mut service = f.service();
    let ids = |r: &cones::history_api::Results| {
        r.entries
            .iter()
            .map(|e| e.key.session_id.clone())
            .collect::<Vec<_>>()
    };
    // A word the history lacks no longer hides passages holding the other two.
    let partial = service
        .search(Search {
            query: "needle cobalt zebra".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(partial.total, 5, "{:?}", ids(&partial));
    // One matching word of two is not a match, so a common word cannot flood the results.
    let lone = service
        .search(Search {
            query: "needle zebra".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(lone.total, 0, "{:?}", ids(&lone));
    let parser = |when| Search {
        query: "parser cache".into(),
        when,
        ..Default::default()
    };
    use cones::history_api::When;
    let any = service.search(parser(When::Any)).unwrap();
    assert_eq!(ids(&any), [OLD, MID, NEW], "BM25 order");
    let recent = service.search(parser(When::Recent)).unwrap();
    assert_eq!(
        ids(&recent),
        [OLD, NEW, MID],
        "the recent hit climbs past a weaker one, not past the strongest"
    );
    let old = service.search(parser(When::Old)).unwrap();
    assert_eq!(ids(&old), [OLD, MID, NEW]);
    let month = service.search(parser(When::Month)).unwrap();
    assert_eq!(
        ids(&month),
        [OLD, MID, NEW],
        "no hit in the window keeps the order"
    );

    let anchor = any.entries[0].hit.as_ref().unwrap().anchor.clone().unwrap();
    let shown = service
        .show(Show {
            id: OLD.into(),
            harness: None,
            home: None,
            tail: None,
            all: false,
            at: Some(anchor.offset),
            context: Some(2),
        })
        .unwrap();
    let messages = shown["messages"].as_array().unwrap();
    let texts: Vec<_> = messages
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        texts,
        [
            "routine step 8",
            "routine step 9",
            "parser cache: parser cache invalidation",
            "routine step 11",
            "routine step 12"
        ]
    );
    assert_eq!(shown["matched"], 2);
    assert_eq!(shown["omitted"], 9, "the first prompt and steps 0 to 7");
    assert_eq!(shown["omitted_after"], 7);

    let out = f
        .command()
        .args([
            "show",
            OLD,
            "--at",
            &anchor.offset.to_string(),
            "--context",
            "0",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("[11 earlier messages omitted"), "{text}");
    assert!(text.contains("[the search hit]\nassistant"), "{text}");
    assert!(
        text.contains("parser cache: parser cache invalidation"),
        "{text}"
    );
    assert!(text.contains("[9 later messages omitted"), "{text}");
    for args in [
        vec!["show", OLD, "--at", "0", "--tail", "1"],
        vec!["show", OLD, "--context", "1"],
        vec!["search", "parser", "--when", "someday"],
    ] {
        let out = f.command().args(&args).output().unwrap();
        assert!(!out.status.success(), "{args:?}");
    }
    let json = f
        .command()
        .args(["search", "parser cache", "--when", "recent", "--json"])
        .output()
        .unwrap();
    let json: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(json["entries"][1]["key"]["session_id"], NEW);
}
