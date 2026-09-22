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

/// `cones show` exports, as opposed to previews: whole messages, an explicit tail and an
/// honest report of what the harness has not finished writing.
mod export {
    use super::{claude, records};
    use cones::transcript::{self, Export, Role, Source};
    use serde_json::{Value, json};
    use std::{fs, path::Path, process::Command};

    fn file(dir: &Path, name: &str, values: &[Value]) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, records(values)).unwrap();
        path
    }

    fn export(path: &Path, harness: &str, tail: Option<usize>) -> Export {
        transcript::export(&Source::Conversation(path.to_owned()), harness, tail).unwrap()
    }

    fn texts(export: &Export) -> Vec<&str> {
        export.messages.iter().map(|m| m.text.as_str()).collect()
    }

    #[test]
    fn exports_keep_whole_messages_and_name_what_the_tail_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let huge = "x".repeat(200 * 1024);
        let mut values: Vec<Value> = (0..99)
            .map(|i| {
                claude(
                    if i % 2 == 0 { "user" } else { "assistant" },
                    &format!("message {i}"),
                )
            })
            .collect();
        values.push(claude("assistant", &huge));
        let path = file(dir.path(), "long.jsonl", &values);

        let all = export(&path, "claude", None);
        assert_eq!(all.messages.len(), 100);
        assert_eq!(all.omitted, 0);
        assert!(!all.incomplete);
        assert_eq!(all.messages[0].text, "message 0");
        assert_eq!(all.messages[0].role, Role::User);
        assert_eq!(all.messages[1].role, Role::Assistant);
        // The preview bounds retained text for a pane; an export must not silently do that.
        assert_eq!(all.messages[99].text.len(), huge.len());
        let preview = transcript::parse("claude", &records(&values));
        assert!(
            preview.messages.last().unwrap().text.len() < huge.len(),
            "the preview is expected to trim, which is what the export must not inherit"
        );

        let tail = export(&path, "claude", Some(40));
        assert_eq!(tail.messages.len(), 40);
        assert_eq!(tail.omitted, 60);
        assert_eq!(tail.messages[0].text, "message 60");
        assert_eq!(tail.messages[39].text.len(), huge.len());

        let short = export(&path, "claude", Some(500));
        assert_eq!(short.messages.len(), 100);
        assert_eq!(short.omitted, 0);

        let error = transcript::export(&Source::Conversation(path), "claude", Some(0))
            .unwrap_err()
            .to_string();
        assert!(error.contains("positive number"), "{error}");
    }

    #[test]
    fn an_assistant_turn_split_across_records_stays_one_message_in_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let split = |text: &str| json!({"type":"assistant","message":{"id":"one","content":[{"type":"text","text":text}]}});
        let path = file(
            dir.path(),
            "split.jsonl",
            &[
                claude("user", "question"),
                split("first half"),
                split("second half"),
                claude("user", "follow up"),
            ],
        );
        let all = export(&path, "claude", None);
        assert_eq!(
            texts(&all),
            ["question", "first half\n\nsecond half", "follow up"]
        );
        // A continuation joins a message already counted, so it cannot evict the tail twice.
        let tail = export(&path, "claude", Some(2));
        assert_eq!(texts(&tail), ["first half\n\nsecond half", "follow up"]);
        assert_eq!(tail.omitted, 1);
    }

    #[test]
    fn every_harness_that_declares_a_reader_exports_and_the_rest_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let codex = file(
            dir.path(),
            "codex.jsonl",
            &[
                json!({"type":"event_msg","payload":{"item":{"type":"UserMessage","content":[{"type":"text","text":"question"}]}}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","id":"m","content":[{"type":"output_text","text":"answer"}]}}),
                json!({"type":"response_item","payload":{"type":"function_call","name":"exec","arguments":"cargo test"}}),
            ],
        );
        let exported = export(&codex, "codex", None);
        assert_eq!(texts(&exported)[..2], ["question", "answer"]);
        assert_eq!(exported.messages.last().unwrap().tools[0].name, "exec");

        let pi = file(
            dir.path(),
            "pi.jsonl",
            &[
                json!({"type":"session","id":"pi","cwd":"/fixture"}),
                json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"question"}]}}),
                json!({"type":"message","message":{"role":"toolResult","content":[{"type":"text","text":"tool noise"}]}}),
                json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"answer"}]}}),
            ],
        );
        assert_eq!(texts(&export(&pi, "pi", None)), ["question", "answer"]);

        let opencode = dir.path().join("opencode.db");
        let mut child = Command::new("sqlite3")
            .arg(&opencode)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(
            &mut child.stdin.take().unwrap(),
            include_bytes!("../assets/harnesses/fixtures/opencode.sql"),
        )
        .unwrap();
        assert!(child.wait().unwrap().success());
        let exported = transcript::export(
            &Source::Opencode {
                database: opencode,
                session_id: "ses_fixture".into(),
            },
            "opencode",
            None,
        )
        .unwrap();
        assert_eq!(
            texts(&exported),
            [
                "Inspect the fixture\nand explain it",
                "Reading",
                "Done\nAdditional detail"
            ]
        );

        // A harness cones cannot read is refused by name, not answered with an empty export.
        let error = transcript::export(&Source::Conversation(codex.clone()), "gemini", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no transcript cones can read"), "{error}");
        let error = transcript::export(
            &Source::Run {
                events: Some(codex),
                stderr: None,
            },
            "claude",
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("not a conversation"), "{error}");
    }

    #[test]
    fn malformed_and_half_written_records_are_skipped_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.jsonl");
        let mut bytes = records(&[
            claude("user", "first"),
            json!({"type":"user","uuid":"u1","message":{"content":[{"type":"text","text":"once"}]}}),
            json!({"type":"user","uuid":"u1","message":{"content":[{"type":"text","text":"once"}]}}),
        ]);
        bytes.extend_from_slice(b"{ not json at all }\n");
        bytes.extend_from_slice(
            records(&[claude("assistant", "\x1b[31mred\x1b[0m and \x07bell")]).as_slice(),
        );
        // A record a harness is still writing has no newline yet.
        bytes.extend_from_slice(format!("{}", claude("assistant", "half written")).as_bytes());
        fs::write(&path, &bytes).unwrap();

        let exported = export(&path, "claude", None);
        assert_eq!(texts(&exported), ["first", "once", "red and bell"]);
        assert!(exported.incomplete);

        // Completing the record makes it readable and the export complete again.
        fs::write(&path, [bytes.as_slice(), b"\n"].concat()).unwrap();
        let exported = export(&path, "claude", None);
        assert_eq!(texts(&exported).last(), Some(&"half written"));
        assert!(!exported.incomplete);
    }
}

/// `cones show` end to end: identity from history discovery, text on stdout, and a
/// session that cannot tell it was read.
mod show {
    use super::{claude, records};
    use serde_json::{Value, json};
    use std::{
        fs,
        path::{Path, PathBuf},
        process::{Command, Output},
        time::SystemTime,
    };

    const ALPHA: &str = "11111111-1111-4111-8111-111111111111";
    const BETA: &str = "22222222-2222-4222-8222-222222222222";
    const GAMMA: &str = "22222222-2222-4222-8222-222222222223";

    /// Every file under a native home, so a read that changed one is visible.
    fn snapshot(root: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_owned()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let meta = entry.metadata().unwrap();
                if meta.is_dir() {
                    stack.push(entry.path());
                } else {
                    out.push((entry.path(), meta.len(), meta.modified().unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    fn conversation(texts: &[String]) -> Vec<Value> {
        let mut values = vec![json!({
            "type": "user", "cwd": "/fixture", "timestamp": "2026-09-16T12:00:00Z",
            "message": {"content": [{"type": "text", "text": texts[0]}]}
        })];
        values.extend(
            texts
                .iter()
                .enumerate()
                .skip(1)
                .map(|(i, text)| claude(if i % 2 == 0 { "user" } else { "assistant" }, text)),
        );
        values
    }

    #[test]
    fn show_exports_a_historical_session_from_a_custom_home_without_touching_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("claude-home");
        let project = home.join("projects").join("fixture");
        fs::create_dir_all(&project).unwrap();
        let long: Vec<String> = (0..50).map(|i| format!("message {i}")).collect();
        let alpha = project.join(format!("{ALPHA}.jsonl"));
        fs::write(&alpha, records(&conversation(&long))).unwrap();
        for id in [BETA, GAMMA] {
            fs::write(
                project.join(format!("{id}.jsonl")),
                records(&conversation(&[
                    format!("{id} asked"),
                    format!("{id} answered"),
                ])),
            )
            .unwrap();
        }

        let run = |args: &[&str]| -> Output {
            Command::new(env!("CARGO_BIN_EXE_cones"))
                .args([
                    "--state-dir",
                    dir.path().join("state").to_str().unwrap(),
                    "--jobs",
                    dir.path().join("jobs.yaml").to_str().unwrap(),
                ])
                .args(args)
                .env("TZ", "UTC")
                .env("HOME", dir.path())
                .env("CLAUDE_CONFIG_DIR", &home)
                .env("CODEX_HOME", dir.path().join("missing-codex"))
                .env("PI_CODING_AGENT_DIR", dir.path().join("missing-pi"))
                .env_remove("OPENCODE_DB")
                .env_remove("OPENCODE_TUI_CONFIG")
                .env_remove("XDG_DATA_HOME")
                .output()
                .unwrap()
        };
        let ok = |args: &[&str]| -> String {
            let out = run(args);
            assert!(
                out.status.success(),
                "cones {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                out.stderr.is_empty(),
                "diagnostics belong on stderr only when there are any: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        };
        let failure = |args: &[&str]| -> String {
            let out = run(args);
            assert!(!out.status.success(), "cones {} succeeded", args.join(" "));
            String::from_utf8(out.stderr).unwrap()
        };

        let before = snapshot(&home);

        // The default is a bounded tail that says so, labelled by role and native time.
        let text = ok(&["show", ALPHA]);
        assert!(
            text.starts_with("[10 earlier messages omitted; --all exports the whole conversation]"),
            "{text}"
        );
        assert!(text.contains("user 2026-09-16T12:00:00+00:00\n"), "{text}");
        assert!(
            text.contains("\nmessage 10\n") && text.contains("\nmessage 49\n"),
            "{text}"
        );
        assert!(!text.contains("message 9\n"), "{text}");

        let all = ok(&["show", ALPHA, "--all"]);
        assert!(!all.contains("omitted"), "{all}");
        assert!(
            all.contains("\nmessage 0\n") && all.contains("\nmessage 49\n"),
            "{all}"
        );
        assert_eq!(all.matches("\nassistant 2026").count(), 25, "{all}");

        let two = ok(&["show", ALPHA, "--tail", "2"]);
        assert!(two.starts_with("[48 earlier messages omitted"), "{two}");
        assert!(
            two.contains("\nmessage 48\n") && two.contains("\nmessage 49\n"),
            "{two}"
        );
        assert!(!two.contains("message 47"), "{two}");

        // A prefix is enough when it names one session, and an exact id beats a shared prefix.
        assert!(ok(&["show", "1111"]).contains("message 49"));
        assert!(ok(&["show", BETA, "--all"]).contains(&format!("{BETA} answered")));
        let ambiguous = failure(&["show", "2222"]);
        assert!(
            ambiguous.contains("matches 2 sessions") && ambiguous.contains(GAMMA),
            "{ambiguous}"
        );
        for unknown in ["99999999-9999-4999-8999-999999999999", "ab"] {
            let missing = failure(&["show", unknown]);
            assert!(missing.contains("no session"), "{missing}");
        }
        assert!(failure(&["show", ALPHA, "--tail", "0"]).contains("positive number"));
        // A bounded read and the whole conversation are different requests.
        assert!(
            !run(&["show", ALPHA, "--tail", "1", "--all"])
                .status
                .success()
        );

        assert_eq!(
            before,
            snapshot(&home),
            "reading a session changed its home"
        );
        assert!(
            !dir.path().join("state").exists(),
            "reading a session created cones state"
        );

        // A record the harness has not finished writing is left out, and said so on stderr.
        let mut partial = fs::read(&alpha).unwrap();
        partial.extend_from_slice(format!("{}", claude("assistant", "still writing")).as_bytes());
        fs::write(&alpha, partial).unwrap();
        let out = run(&["show", ALPHA, "--all"]);
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(!text.contains("still writing"), "{text}");
        let diagnostics = String::from_utf8(out.stderr).unwrap();
        assert!(diagnostics.contains("is being written"), "{diagnostics}");
    }
}
