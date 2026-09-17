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
        session_id: None,
        key: path.display().to_string(),
        path: path.to_owned(),
        harness: "claude".into(),
    }
}

fn read(reader: &mut Reader, target: Target) -> anyhow::Result<Arc<Transcript>> {
    read_response(reader, target)?.result
}

fn read_response(reader: &mut Reader, target: Target) -> anyhow::Result<transcript::Response> {
    assert!(reader.request(target)?);
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
    assert_eq!(doc.messages.len(), 2);
    assert_eq!(doc.messages[0].text, "<actual question>\nnext line");
    assert!(doc.messages[1].text.contains("let n = 1;"));
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
