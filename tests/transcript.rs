use cones::transcript::{self, Reader, Role, Target, Transcript};
use serde_json::{Value, json};
use std::{
    fs::{self, File, FileTimes, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::Path,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

fn records(values: &[Value]) -> Vec<u8> {
    values
        .iter()
        .map(|v| format!("{v}\n"))
        .collect::<String>()
        .into_bytes()
}

fn claude(role: &str, text: &str) -> Value {
    json!({"type":role,"timestamp":"2026-09-16T12:00:00Z","message":{"content":[{"type":"text","text":text}]}})
}

fn target(path: &Path) -> Target {
    Target {
        key: path.display().to_string(),
        source: transcript::Source::Conversation(path.to_owned()),
        harness: "claude".into(),
    }
}

fn read(reader: &mut Reader, target: Target) -> anyhow::Result<Arc<Transcript>> {
    read_response(reader, target)?.result
}

fn read_response(reader: &mut Reader, target: Target) -> anyhow::Result<transcript::Response> {
    assert!(reader.request(target)?);
    poll(reader)
}

fn poll(reader: &mut Reader) -> anyhow::Result<transcript::Response> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(response) = reader.poll() {
            return response;
        }
        assert!(Instant::now() < deadline, "preview worker did not respond");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn search_preview_opens_at_the_match_then_pages_both_directions_without_losing_messages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let data: Vec<_> = (0..120)
        .map(|i| claude("user", &format!("message {i}")))
        .collect();
    let before = records(&data[..50]).len() as u64;
    let end = before + records(&data[50..51]).len() as u64;
    fs::write(&path, records(&data)).unwrap();
    let mut target = target(&path);
    target.source = transcript::Source::Match {
        source: Box::new(target.source),
        anchor: cones::search::Anchor {
            offset: before,
            end,
            text: "message 50".into(),
        },
    };
    let mut reader = Reader::new().unwrap();
    let mut doc = (*read(&mut reader, target.clone()).unwrap()).clone();
    assert_eq!(doc.messages[doc.matched.unwrap()].text, "message 50");
    assert!(doc.messages.len() <= 5);
    while let Some(cursor) = doc.newer.clone() {
        assert!(reader.request_page(target.clone(), Some(cursor)).unwrap());
        doc.append((*poll(&mut reader).unwrap().result.unwrap()).clone());
    }
    while let Some(cursor) = doc.older.clone() {
        assert!(reader.request_page(target.clone(), Some(cursor)).unwrap());
        doc.prepend((*poll(&mut reader).unwrap().result.unwrap()).clone());
    }
    assert_eq!(
        doc.messages
            .iter()
            .map(|m| m.text.clone())
            .collect::<Vec<_>>(),
        (0..120).map(|i| format!("message {i}")).collect::<Vec<_>>()
    );
}

#[test]
fn search_preview_keeps_a_match_near_the_start_of_a_large_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let data = records(&[claude(
        "user",
        &format!("needle {}", "unrelated ".repeat(30000)),
    )]);
    fs::write(&path, &data).unwrap();
    let mut target = target(&path);
    target.source = transcript::Source::Match {
        source: Box::new(target.source),
        anchor: cones::search::Anchor {
            offset: 0,
            end: data.len() as u64,
            text: "needle".into(),
        },
    };
    let mut reader = Reader::new().unwrap();
    let doc = read(&mut reader, target.clone()).unwrap();
    assert!(
        doc.messages[doc.matched.unwrap()]
            .text
            .starts_with("needle")
    );
    assert!(doc.messages[0].text.len() < 128 * 1024);
    fs::write(&path, records(&[claude("user", "replacement")])).unwrap();
    assert!(read(&mut reader, target).is_err());
}

#[test]
fn earlier_pages_recover_the_whole_conversation_once_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let data: Vec<_> = (0..103)
        .map(|i| {
            claude(
                if i % 2 == 0 { "user" } else { "assistant" },
                &format!("message {i}"),
            )
        })
        .collect();
    fs::write(&path, records(&data)).unwrap();
    let mut reader = Reader::new().unwrap();
    let mut document = (*read(&mut reader, target(&path)).unwrap()).clone();
    assert_eq!(document.messages.len(), 40);
    let mut pages = 1;
    while let Some(cursor) = document.older.clone() {
        assert!(reader.request_page(target(&path), Some(cursor)).unwrap());
        let older = poll(&mut reader).unwrap().result.unwrap();
        assert!(older.messages.len() <= 40);
        document.prepend((*older).clone());
        pages += 1;
        assert!(pages <= 3);
    }
    assert_eq!(pages, 3);
    assert!(!document.earlier);
    assert_eq!(
        document
            .messages
            .iter()
            .map(|m| m.text.clone())
            .collect::<Vec<_>>(),
        (0..103).map(|i| format!("message {i}")).collect::<Vec<_>>()
    );
}

#[test]
fn earlier_pages_refuse_a_rewritten_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    fs::write(
        &path,
        records(
            &(0..50)
                .map(|i| claude("user", &i.to_string()))
                .collect::<Vec<_>>(),
        ),
    )
    .unwrap();
    let mut reader = Reader::new().unwrap();
    let first = read(&mut reader, target(&path)).unwrap();
    fs::write(&path, records(&[claude("user", "replacement")])).unwrap();
    assert!(
        reader
            .request_page(target(&path), first.older.clone())
            .unwrap()
    );
    let error = poll(&mut reader).unwrap().result.unwrap_err();
    assert!(error.to_string().contains("refresh before loading earlier"));
}

#[test]
fn run_previews_follow_complete_events_stderr_and_cache_changes() {
    let dir = tempfile::tempdir().unwrap();
    let events = dir.path().join("events.jsonl");
    let stderr = dir.path().join("stderr.log");
    let target = Target {
        key: "run:fixture".into(),
        harness: "claude".into(),
        source: transcript::Source::Run {
            events: Some(events.clone()),
            stderr: Some(stderr.clone()),
        },
    };
    let mut reader = Reader::new().unwrap();
    assert!(
        read(&mut reader, target.clone())
            .unwrap()
            .messages
            .is_empty()
    );
    fs::write(&events, records(&[
        claude("assistant", "Working"),
        json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}),
        json!({"type":"cones_error","message":"worker failed"}),
    ])).unwrap();
    let result = json!({"type":"result","subtype":"success","total_cost_usd":0.2}).to_string();
    OpenOptions::new()
        .append(true)
        .open(&events)
        .unwrap()
        .write_all(result.as_bytes())
        .unwrap();
    let first = read(&mut reader, target.clone()).unwrap();
    assert!(
        first.messages[0]
            .text
            .contains("Working\nBash  cargo test\nError: worker failed")
    );
    assert!(!first.messages[0].text.contains("Result:"));
    let cached = read_response(&mut reader, target.clone()).unwrap();
    assert!(cached.cache_hit && cached.bytes_read == 0);
    OpenOptions::new()
        .append(true)
        .open(&events)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    fs::write(&stderr, "\x1b[31mpermission denied\x1b[0m").unwrap();
    let finished = read(&mut reader, target.clone()).unwrap();
    assert!(finished.messages[0].text.contains("Result: success"));
    assert_eq!(
        finished.messages[1].text,
        "Harness stderr:\npermission denied"
    );
    fs::write(&stderr, "changed error").unwrap();
    let updated = read(&mut reader, target).unwrap();
    assert!(updated.messages[1].text.contains("changed error"));
}

#[test]
fn claude_keeps_conversation_text_and_reported_blocks_without_tools_or_thinking() {
    let data = records(&[
        json!({"type":"user","isMeta":true,"message":{"content":"injected instructions"}}),
        claude("user", "<real user XML>\nsecond line"),
        json!({"type":"user","message":{"content":[{"type":"tool_result","content":"tool noise"}]}}),
        json!({"type":"assistant","uuid":"one","message":{"id":"m","content":[{"type":"thinking","thinking":"private thinking"},{"type":"text","text":"hello"}]}}),
        json!({"type":"assistant","uuid":"two","message":{"id":"m","content":[{"type":"text","text":"hello"}]}}),
        json!({"type":"assistant","uuid":"two","message":{"id":"m","content":[{"type":"text","text":"hello"}]}}),
        json!({"type":"user","message":{"content":[{"type":"image","source":{"data":"not displayed"}}]}}),
    ]);
    let doc = transcript::parse("claude", &data);
    assert_eq!(doc.messages.len(), 3);
    assert_eq!(doc.messages[0].role, Role::User);
    assert_eq!(doc.messages[0].text, "<real user XML>\nsecond line");
    assert_eq!(
        doc.messages[1].text, "hello\n\nhello",
        "distinct blocks can repeat literal text"
    );
    assert_eq!(doc.messages[2].text, "[image]");
    assert!(doc.messages[0].at.is_some());
}

#[test]
fn codex_uses_the_ui_user_stream_and_one_assistant_stream() {
    let data = records(&[
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"AGENTS.md injected preamble"}]}}),
        json!({"type":"event_msg","payload":{"item":{"type":"UserMessage","content":[{"type":"text","text":"<actual question>\nnext line"}]}}}),
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","id":"m","content":[{"type":"output_text","text":"answer\n```rs\nlet n = 1;\n```"}]}}),
        json!({"type":"event_msg","payload":{"item":{"type":"AssistantMessage","content":[{"type":"text","text":"duplicate UI answer"}]}}}),
        json!({"type":"response_item","payload":{"type":"function_call","name":"exec","arguments":"tool input"}}),
    ]);
    let doc = transcript::parse("codex", &data);
    assert_eq!(doc.messages.len(), 3);
    assert_eq!(doc.messages[0].text, "<actual question>\nnext line");
    assert!(doc.messages[1].text.contains("let n = 1;"));
    assert_eq!(doc.messages[2].tools[0].name, "exec");
    assert_eq!(doc.messages[2].tools[0].input, "tool input");
    assert_eq!(cones::codex::prompt(std::str::from_utf8(&records(&[
        json!({"type":"event_msg","payload":{"item":{"type":"UserMessage","content":[{"text":"actual question\nnext line"}]}}})
    ])).unwrap()).as_deref(), Some("actual question"));
}

#[test]
fn pi_reads_user_and_assistant_messages_and_omits_tool_results() {
    let data = records(&[
        json!({"type":"session","id":"pi","cwd":"/fixture"}),
        json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"question"}]}}),
        json!({"type":"message","message":{"role":"toolResult","content":[{"type":"text","text":"tool noise"}]}}),
        json!({"type":"message","message":{"role":"assistant","content":[{"type":"thinking","thinking":"not displayed"},{"type":"toolCall","name":"read"},{"type":"text","text":"answer"}]}}),
    ]);
    let doc = transcript::parse("pi", &data);
    assert_eq!(
        doc.messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        ["question", "answer"]
    );
    assert_eq!(doc.messages[1].tools[0].name, "read");
}

#[test]
fn compact_tool_calls_keep_reported_names_and_inputs_without_tool_output_or_injected_calls() {
    let doc = transcript::parse(
        "claude",
        &records(&[
            json!({"type":"assistant","isMeta":true,"message":{"content":[{"type":"tool_use","name":"hidden","input":{"command":"injected"}}]}}),
            json!({"type":"assistant","message":{"id":"a","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","content":"not a conversation message"}]}}),
            claude("assistant", "The check passed"),
        ]),
    );
    assert_eq!(doc.messages.len(), 2);
    assert!(doc.messages[0].text.is_empty());
    assert_eq!(doc.messages[0].tools[0].name, "Bash");
    assert_eq!(doc.messages[0].tools[0].input, "cargo test");
    assert_eq!(doc.messages[1].text, "The check passed");
    let doc = transcript::parse(
        "codex",
        &records(&[
            json!({"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo fmt\\u001b[31m\"}"}}),
        ]),
    );
    assert_eq!(doc.messages[0].tools[0].input, "cargo fmt");
}

#[test]
fn older_codex_ui_events_keep_the_prompt_without_duplicating_the_assistant() {
    let user = json!({"type":"event_msg","payload":{"type":"user_message","message":"earlier format\nsecond line","images":[]}});
    let data = records(&[
        user.clone(),
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}}),
        json!({"type":"event_msg","payload":{"type":"agent_message","message":"answer"}}),
    ]);
    let doc = transcript::parse("codex", &data);
    assert_eq!(doc.messages.len(), 2);
    assert_eq!(doc.messages[0].text, "earlier format\nsecond line");
    assert_eq!(
        cones::codex::prompt(&user.to_string()).as_deref(),
        Some("earlier format")
    );
}

#[test]
fn control_sequences_cannot_reach_the_terminal() {
    let text = "a\r\n\tb\x1b[31mRED\x1b[0m\x1b]52;c;clipboard\x07ok\x1bPpayload\x1b\\!\u{009b}2J\u{009d}title\u{009c}\x08";
    let plain = transcript::plain(text);
    assert_eq!(plain, "a\n    bREDok!");
    assert!(!plain.chars().any(|c| c.is_control() && c != '\n'));
    let doc = transcript::parse("claude", &records(&[claude("assistant", text)]));
    assert_eq!(doc.messages[0].text, plain);
}

#[test]
fn previews_keep_recent_messages_and_bound_retained_text_on_utf8_boundaries() {
    let values: Vec<_> = (0..60)
        .map(|i| claude("user", &format!("message {i}")))
        .collect();
    let doc = transcript::parse("claude", &records(&values));
    assert_eq!(doc.messages.len(), 40);
    assert_eq!(doc.messages[0].text, "message 20");
    assert!(doc.earlier);
    let text = format!("{}THE END", "שלום🙂".repeat(30000));
    let doc = transcript::parse("claude", &records(&[claude("assistant", &text)]));
    assert!(doc.messages[0].text.len() <= 128 * 1024);
    assert!(doc.messages[0].text.ends_with("THE END"));
    assert!(doc.earlier);
}

#[test]
fn sparse_large_files_use_tail_windows_and_unchanged_previews_reuse_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut file = File::create(&path).unwrap();
    file.set_len(50 * 1024 * 1024).unwrap();
    file.seek(SeekFrom::End(0)).unwrap();
    file.write_all(b"\n").unwrap();
    file.write_all(&records(&[
        claude("user", "last question"),
        claude("assistant", "last answer"),
    ]))
    .unwrap();
    let mut reader = Reader::new().unwrap();
    let response = read_response(&mut reader, target(&path)).unwrap();
    assert!(!response.cache_hit);
    assert!(response.bytes_read > 0);
    assert!(response.elapsed_ms >= 0.0);
    let first = response.result.unwrap();
    assert_eq!(first.messages.len(), 2);
    assert!(first.earlier);
    assert!(first.bytes_read <= 256 * 1024 + 1);
    let response = read_response(&mut reader, target(&path)).unwrap();
    assert!(response.cache_hit);
    assert_eq!(
        response.bytes_read, 0,
        "cached snapshots do not report their original IO twice"
    );
    let again = response.result.unwrap();
    assert!(Arc::ptr_eq(&first, &again));
}

#[test]
fn larger_windows_recover_text_behind_tool_output_but_reads_remain_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut content = records(&[claude("user", "question"), claude("assistant", "answer")]);
    content.extend(records(&[json!({"type":"user","message":{"content":[{"type":"tool_result","content":"x".repeat(2 * 1024 * 1024)}]}})]));
    content.extend(b"{\"type\":\"assistant\",\"message\":");
    fs::write(&path, content).unwrap();
    let mut reader = Reader::new().unwrap();
    let doc = read(&mut reader, target(&path)).unwrap();
    assert_eq!(doc.messages.len(), 2);
    assert!(doc.bytes_read > 256 * 1024);
    assert!(doc.bytes_read <= 21 * 256 * 1024 + 3);
}

#[test]
fn a_same_size_rewrite_and_file_removal_do_not_return_stale_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    fs::write(&path, records(&[claude("user", "one")])).unwrap();
    let mut reader = Reader::new().unwrap();
    let first = read(&mut reader, target(&path)).unwrap();
    fs::write(&path, records(&[claude("user", "two")])).unwrap();
    File::open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(5)))
        .unwrap();
    let second = read(&mut reader, target(&path)).unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(second.messages[0].text, "two");
    fs::remove_file(path.clone()).unwrap();
    assert!(read(&mut reader, target(&path)).is_err());
    assert!(read(&mut reader, target(dir.path())).is_err());
}

#[test]
fn incomplete_and_malformed_records_are_ignored_and_cache_retention_is_limited() {
    let dir = tempfile::tempdir().unwrap();
    let mut reader = Reader::new().unwrap();
    let mut first = None;
    for i in 0..4 {
        let path = dir.path().join(format!("{i}.jsonl"));
        fs::write(
            &path,
            records(&[claude("assistant", &format!("answer {i}"))]),
        )
        .unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"not json\n{\"type\":\"assistant\"")
            .unwrap();
        let doc = read(&mut reader, target(&path)).unwrap();
        assert_eq!(doc.messages.len(), 1);
        if i == 0 {
            first = Some(doc);
        }
    }
    let reread = read(&mut reader, target(&dir.path().join("0.jsonl"))).unwrap();
    assert!(!Arc::ptr_eq(first.as_ref().unwrap(), &reread));
}
