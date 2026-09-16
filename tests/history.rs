//! History fixtures use files only. No native client or model is started.
use cones::{
    config::HarnessKind,
    history::{Page, Query, Reader, Source},
};
use serde_json::{Value, json};
use std::{
    fs::{self, File, FileTimes, OpenOptions},
    io::{Seek, SeekFrom, Write},
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const C: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const D: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";

fn write(path: &Path, records: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text: String = records.iter().map(|v| format!("{v}\n")).collect();
    fs::write(path, text).unwrap();
}

fn claude(home: &Path, id: &str, at: &str) -> PathBuf {
    let path = home.join("projects/project").join(format!("{id}.jsonl"));
    write(
        &path,
        &[
            json!({"type":"user","sessionId":id,"cwd":"/repo","timestamp":"2026-09-10T10:00:00Z","message":{"content":"First instruction"}}),
            json!({"type":"assistant","timestamp":at,"message":{"id":"m1","model":"reported-model","content":[{"type":"text","text":"Last reply"}],"usage":{"input_tokens":5,"cache_read_input_tokens":10,"cache_creation_input_tokens":2,"output_tokens":3}}}),
            json!({"type":"custom-title","customTitle":format!("Session {id}")}),
        ],
    );
    path
}

fn codex(home: &Path, id: &str, archived: bool, at: &str) -> PathBuf {
    let path = home
        .join(if archived {
            "archived_sessions"
        } else {
            "sessions/2026/09/10"
        })
        .join(format!("rollout-{id}.jsonl"));
    write(
        &path,
        &[
            json!({"type":"session_meta","timestamp":"2026-09-10T10:00:00Z","payload":{"session_id":id,"cwd":"/repo","timestamp":"2026-09-10T10:00:00Z","source":"cli"}}),
            json!({"type":"event_msg","timestamp":"2026-09-10T10:00:01Z","payload":{"item":{"type":"UserMessage","content":[{"type":"text","text":"Codex instruction"}]}}}),
            json!({"type":"turn_context","payload":{"model":"codex-model"}}),
            json!({"type":"event_msg","timestamp":at,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":20},"last_token_usage":{"total_tokens":30},"model_context_window":200000}}}),
            json!({"type":"event_msg","timestamp":at,"payload":{"type":"task_started"}}),
        ],
    );
    path
}

fn pi(home: &Path, id: &str, at: &str) -> PathBuf {
    let path = home
        .join("sessions/--repo--")
        .join(format!("start_{id}.jsonl"));
    write(
        &path,
        &[
            json!({"type":"session","id":id,"cwd":"/repo","timestamp":"2026-09-10T10:00:00Z"}),
            json!({"type":"message","timestamp":"2026-09-10T10:00:01Z","message":{"role":"user","content":[{"type":"text","text":"Pi instruction"}]}}),
            json!({"type":"message","timestamp":at,"message":{"role":"assistant","stopReason":"toolUse","model":"pi-model","content":[{"type":"text","text":"Pi reply"}],"usage":{"input":5,"cacheRead":10,"cacheWrite":1,"output":2,"cost":{"total":0.125}}}}),
            json!({"type":"session_info","name":"Named pi"}),
        ],
    );
    path
}

fn source(home: &Path, harness: HarnessKind) -> Source {
    Source {
        harness,
        home: home.to_owned(),
    }
}

fn page(reader: &mut Reader, query: Query) -> anyhow::Result<Page> {
    assert!(reader.request(query)?);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(page) = reader.poll() {
            return page;
        }
        assert!(Instant::now() < deadline, "history worker did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn ids(page: &Page) -> Vec<&str> {
    page.entries
        .iter()
        .map(|e| e.key.session_id.as_str())
        .collect()
}

#[test]
fn all_harnesses_share_reported_time_order_and_only_requested_columns_are_read() {
    let dir = tempfile::tempdir().unwrap();
    let ch = dir.path().join(".claude");
    let co = dir.path().join(".codex");
    let ph = dir.path().join(".pi");
    claude(&ch, A, "2026-09-10T13:00:00Z");
    codex(&co, B, false, "2026-09-10T14:00:00Z");
    pi(&ph, C, "2026-09-10T12:00:00Z");
    write(
        &ch.join("statusline").join(format!("{A}.json")),
        &[json!({"context_window":{"context_window_size":1000000}})],
    );
    let mut reader = Reader::new(vec![
        source(&ch, HarnessKind::Claude),
        source(&co, HarnessKind::Codex),
        source(&ph, HarnessKind::Pi),
    ])
    .unwrap();
    let first = page(
        &mut reader,
        Query {
            limit: 2,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&first), [B, A]);
    assert_eq!(first.total, 3);
    assert_eq!(first.stats.metadata_reads, 3);
    assert_eq!(first.stats.hydrated_files, 0);
    assert!(first.entries.iter().all(|e| e.columns.is_none()));
    let second = page(
        &mut reader,
        Query {
            after: first.next,
            limit: 2,
            hydrate: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&second), [C]);
    assert!(second.next.is_none());
    assert_eq!(second.stats.metadata_reads, 0);
    let pi = second.entries[0].columns.as_ref().unwrap();
    assert_eq!(
        (pi.tokens_in, pi.tokens_out, pi.context_tokens),
        (Some(16), Some(2), Some(16))
    );
    assert_eq!(pi.context_window, None);
    assert_eq!(pi.cost_usd, Some(0.125));
    assert_eq!(second.entries[0].title.as_deref(), Some("Named pi"));
    let first = page(
        &mut reader,
        Query {
            limit: 2,
            hydrate: true,
            ..Query::default()
        },
    )
    .unwrap();
    let codex = first.entries[0].columns.as_ref().unwrap();
    assert_eq!(
        (codex.tokens_in, codex.tokens_out, codex.context_tokens),
        (Some(100), Some(20), Some(30))
    );
    assert_eq!(codex.context_window, Some(200000));
    let claude = first.entries[1].columns.as_ref().unwrap();
    assert_eq!(
        (claude.tokens_in, claude.tokens_out, claude.context_tokens),
        (Some(17), Some(3), Some(17))
    );
    assert_eq!(claude.context_window, Some(1000000));
    let serialized = serde_json::to_value(&first.entries).unwrap();
    assert!(
        serialized[0].get("state").is_none(),
        "an old task_started is not live state"
    );
    assert!(serialized[0].get("activity").is_none());
    assert!(
        serialized[0]["columns"].get("cost_usd").is_none(),
        "no price is invented"
    );
    let again = page(
        &mut reader,
        Query {
            hydrate: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(again.stats.hydrated_files, 0);
}

#[test]
fn exclusions_and_filtering_happen_before_pagination_and_equal_times_do_not_skip_rows() {
    let dir = tempfile::tempdir().unwrap();
    for id in [D, C, B, A] {
        claude(dir.path(), id, "2026-09-10T12:00:00Z");
    }
    let mut reader = Reader::new(vec![source(dir.path(), HarnessKind::Claude)]).unwrap();
    let all = page(&mut reader, Query::default()).unwrap();
    let excluded: std::collections::HashSet<_> = [all.entries[0].key.clone()].into_iter().collect();
    let first = page(
        &mut reader,
        Query {
            limit: 1,
            excluded: excluded.clone(),
            filter: "SESSION".into(),
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&first), [B]);
    assert_eq!(first.total, 3);
    let second = page(
        &mut reader,
        Query {
            limit: 1,
            excluded,
            after: first.next,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&second), [C]);
    let third = page(
        &mut reader,
        Query {
            limit: 1,
            after: second.next,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&third), [D]);
    assert!(third.next.is_none());
    let empty = page(
        &mut reader,
        Query {
            filter: "unmatched".into(),
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(empty.total, 0);
    assert!(empty.next.is_none());
}

#[test]
fn refresh_detects_same_size_rewrites_replacement_and_deletion_without_using_mtime_as_activity() {
    let dir = tempfile::tempdir().unwrap();
    let a = claude(dir.path(), A, "2026-09-10T12:00:00Z");
    let b = claude(dir.path(), B, "2026-09-10T11:00:00Z");
    let mut reader = Reader::new(vec![source(dir.path(), HarnessKind::Claude)]).unwrap();
    let first = page(
        &mut reader,
        Query {
            limit: 1,
            hydrate: true,
            ..Query::default()
        },
    )
    .unwrap();
    let cursor = first.next.unwrap();
    let contents = fs::read_to_string(&b).unwrap();
    let mtime = SystemTime::now() + Duration::from_secs(30);
    File::open(&b)
        .unwrap()
        .set_times(FileTimes::new().set_modified(mtime))
        .unwrap();
    let touch = page(
        &mut reader,
        Query {
            refresh: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(
        ids(&touch),
        [A, B],
        "touching a file cannot make its session more recent"
    );
    assert_eq!(touch.stats.metadata_reads, 1);
    assert_eq!(touch.generation, first.generation);
    fs::write(&b, contents.replace("11:00:00", "14:00:00")).unwrap();
    File::open(&b)
        .unwrap()
        .set_times(FileTimes::new().set_modified(mtime + Duration::from_secs(1)))
        .unwrap();
    let changed = page(
        &mut reader,
        Query {
            refresh: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&changed), [B, A]);
    assert_eq!(changed.stats.metadata_reads, 1);
    assert!(
        page(
            &mut reader,
            Query {
                after: Some(cursor),
                ..Query::default()
            }
        )
        .unwrap_err()
        .to_string()
        .contains("restart pagination")
    );
    fs::remove_file(&a).unwrap();
    let gone = page(
        &mut reader,
        Query {
            refresh: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&gone), [B]);
    assert_eq!(gone.stats.indexed_files, 1);
    assert_eq!(gone.stats.metadata_reads, 0);
    let tmp = dir.path().join("replacement");
    fs::write(&tmp, contents.replace("11:00:00", "15:00:00")).unwrap();
    File::open(&tmp)
        .unwrap()
        .set_times(FileTimes::new().set_modified(mtime + Duration::from_secs(1)))
        .unwrap();
    fs::rename(tmp, b).unwrap();
    let replacement = page(
        &mut reader,
        Query {
            refresh: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(
        replacement.stats.metadata_reads, 1,
        "an inode replacement invalidates even equal size and mtime"
    );
}

#[test]
fn native_homes_are_distinct_aliases_and_worktree_transcript_copies_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let one = dir.path().join("one");
    let two = dir.path().join("two");
    let alias = dir.path().join("alias");
    let original = claude(&one, A, "2026-09-10T12:00:00Z");
    claude(&two, A, "2026-09-10T13:00:00Z");
    symlink(&one, &alias).unwrap();
    let copy = one.join("projects/worktree").join(format!("{A}.jsonl"));
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    fs::write(
        &copy,
        fs::read_to_string(&original)
            .unwrap()
            .replace("12:00:00", "14:00:00"),
    )
    .unwrap();
    let mut reader = Reader::new(vec![
        source(&one, HarnessKind::Claude),
        source(&alias, HarnessKind::Claude),
        source(&two, HarnessKind::Claude),
    ])
    .unwrap();
    let all = page(&mut reader, Query::default()).unwrap();
    assert_eq!(all.total, 2);
    assert_eq!(all.stats.metadata_reads, 3);
    assert_eq!(all.entries[0].transcript, fs::canonicalize(copy).unwrap());
    assert_ne!(all.entries[0].key, all.entries[1].key);
}

#[test]
fn broken_and_partial_records_subagents_and_symlink_loops_do_not_become_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let ch = dir.path().join("claude");
    let co = dir.path().join("codex");
    let good = claude(&ch, A, "2026-09-10T12:00:00Z");
    let side = claude(&ch, B, "2026-09-10T13:00:00Z");
    fs::write(
        &side,
        fs::read_to_string(&side)
            .unwrap()
            .replace("\"cwd\"", "\"isSidechain\":true,\"cwd\""),
    )
    .unwrap();
    let sub = ch
        .join("projects/project/subagents")
        .join(format!("{C}.jsonl"));
    fs::create_dir_all(sub.parent().unwrap()).unwrap();
    fs::copy(&good, sub).unwrap();
    fs::write(ch.join("projects/project/broken.jsonl"), "not json\n").unwrap();
    let mut file = OpenOptions::new().append(true).open(&good).unwrap();
    writeln!(file, "not json").unwrap();
    write!(
        file,
        "{{\"timestamp\":\"2099-01-01T00:00:00Z\",\"unfinished\":"
    )
    .unwrap();
    let child = codex(&co, C, false, "2026-09-10T14:00:00Z");
    fs::write(
        &child,
        fs::read_to_string(&child).unwrap().replace(
            "\"source\":\"cli\"",
            "\"source\":{\"subagent\":{\"thread_spawn\":{}}}",
        ),
    )
    .unwrap();
    let no_cwd = codex(&co, D, false, "2026-09-10T15:00:00Z");
    fs::write(
        &no_cwd,
        fs::read_to_string(&no_cwd)
            .unwrap()
            .replace("\"cwd\":\"/repo\"", "\"cwd\":\"\""),
    )
    .unwrap();
    symlink(&co, co.join("sessions/loop")).unwrap();
    let mut reader = Reader::new(vec![
        source(&ch, HarnessKind::Claude),
        source(&co, HarnessKind::Codex),
    ])
    .unwrap();
    let all = page(
        &mut reader,
        Query {
            hydrate: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&all), [A]);
    assert_eq!(
        all.entries[0].last_activity.unwrap().to_rfc3339(),
        "2026-09-10T12:00:00+00:00"
    );
    assert_eq!(all.entries[0].columns.as_ref().unwrap().tokens_in, Some(17));
}

#[test]
fn archives_are_explicit_and_native_index_names_refresh_without_reopening_rollouts() {
    let dir = tempfile::tempdir().unwrap();
    codex(dir.path(), A, false, "2026-09-10T12:00:00Z");
    codex(dir.path(), B, true, "2026-09-10T13:00:00Z");
    let index = dir.path().join("session_index.jsonl");
    write(&index, &[json!({"id":A,"thread_name":"Native name"})]);
    let mut reader = Reader::new(vec![source(dir.path(), HarnessKind::Codex)]).unwrap();
    let ordinary = page(&mut reader, Query::default()).unwrap();
    assert_eq!(ids(&ordinary), [A]);
    assert_eq!(ordinary.entries[0].title.as_deref(), Some("Native name"));
    let all = page(
        &mut reader,
        Query {
            include_archived: true,
            hydrate: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&all), [B, A]);
    assert!(all.entries[0].archived);
    assert_eq!(all.entries[1].title.as_deref(), Some("Native name"));
    write(
        &index,
        &[json!({"id":A,"thread_name":"Renamed native thread"})],
    );
    let renamed = page(
        &mut reader,
        Query {
            refresh: true,
            hydrate: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(renamed.stats.metadata_reads, 0);
    assert_eq!(renamed.stats.hydrated_files, 0);
    assert_eq!(
        renamed.entries[0].title.as_deref(),
        Some("Renamed native thread")
    );
    fs::remove_file(index).unwrap();
    let fallback = page(
        &mut reader,
        Query {
            refresh: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(
        fallback.entries[0].title.as_deref(),
        Some("Codex instruction")
    );
}

#[test]
fn usage_is_absent_until_reported_and_claude_streaming_duplicates_are_counted_once() {
    let dir = tempfile::tempdir().unwrap();
    let ch = dir.path().join("claude");
    let ph = dir.path().join("pi");
    let a = claude(&ch, A, "2026-09-10T12:00:00Z");
    let lines = fs::read_to_string(&a).unwrap();
    let assistant = lines.lines().nth(1).unwrap();
    let mut file = OpenOptions::new().append(true).open(&a).unwrap();
    writeln!(file, "{assistant}").unwrap();
    writeln!(file, "{}", json!({"type":"assistant","message":{"id":"synthetic","model":"<synthetic>","usage":{"input_tokens":999,"output_tokens":999}}})).unwrap();
    let p = pi(&ph, B, "2026-09-10T13:00:00Z");
    let mut records: Vec<Value> = fs::read_to_string(&p)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    records[2]["message"]
        .as_object_mut()
        .unwrap()
        .remove("usage");
    write(&p, &records);
    let mut reader = Reader::new(vec![
        source(&ch, HarnessKind::Claude),
        source(&ph, HarnessKind::Pi),
    ])
    .unwrap();
    let all = page(
        &mut reader,
        Query {
            hydrate: true,
            ..Query::default()
        },
    )
    .unwrap();
    let pi = all
        .entries
        .iter()
        .find(|e| e.key.harness == "pi")
        .unwrap()
        .columns
        .as_ref()
        .unwrap();
    assert_eq!(
        (pi.tokens_in, pi.tokens_out, pi.context_tokens),
        (None, None, None)
    );
    let claude = all
        .entries
        .iter()
        .find(|e| e.key.harness == "claude")
        .unwrap()
        .columns
        .as_ref()
        .unwrap();
    assert_eq!((claude.tokens_in, claude.tokens_out), (Some(17), Some(3)));
    assert_eq!(claude.model.as_deref(), Some("reported-model"));
}

#[test]
fn growing_files_refuse_stale_hydration_and_refresh_invalidates_column_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = claude(dir.path(), A, "2026-09-10T12:00:00Z");
    let mut reader = Reader::new(vec![source(dir.path(), HarnessKind::Claude)]).unwrap();
    let before = page(
        &mut reader,
        Query {
            hydrate: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(before.stats.hydrated_files, 1);
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{}", json!({"type":"assistant","timestamp":"2026-09-10T13:00:00Z","message":{"id":"m2","usage":{"input_tokens":7,"output_tokens":11},"model":"new-model"}})).unwrap();
    assert!(
        page(
            &mut reader,
            Query {
                hydrate: true,
                ..Query::default()
            }
        )
        .unwrap_err()
        .to_string()
        .contains("refresh")
    );
    let after = page(
        &mut reader,
        Query {
            hydrate: true,
            refresh: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(after.stats.hydrated_files, 1);
    assert_eq!(
        after.entries[0].columns.as_ref().unwrap().tokens_in,
        Some(24)
    );
    assert_eq!(
        after.entries[0].columns.as_ref().unwrap().tokens_out,
        Some(14)
    );
}

#[test]
fn a_gibibyte_of_transcripts_is_indexed_from_bounded_windows_and_cached() {
    let dir = tempfile::tempdir().unwrap();
    let size = 50 * 1024 * 1024u64;
    for n in 0..26 {
        let id = format!("{n:08x}-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        let path = claude(dir.path(), &id, "2026-09-10T12:00:00Z");
        let mut f = OpenOptions::new().write(true).open(path).unwrap();
        f.set_len(size).unwrap();
        f.seek(SeekFrom::End(-100)).unwrap();
        writeln!(
            f,
            "\n{}",
            json!({"type":"event","timestamp":"2026-09-11T12:00:00Z"})
        )
        .unwrap();
        let end = f.stream_position().unwrap();
        f.set_len(end).unwrap();
    }
    let mut reader = Reader::new(vec![source(dir.path(), HarnessKind::Claude)]).unwrap();
    let started = Instant::now();
    assert!(reader.request(Query::default()).unwrap());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "request performed file IO"
    );
    assert!(
        !reader.request(Query::default()).unwrap(),
        "do not queue obsolete scroll requests"
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let first = loop {
        if let Some(result) = reader.poll() {
            break result.unwrap();
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(first.total, 26);
    assert_eq!(first.stats.metadata_reads, 26);
    assert!(first.stats.metadata_bytes <= 26 * 128 * 1024);
    assert!(
        first
            .entries
            .iter()
            .all(|e| e.last_activity.unwrap().date_naive().to_string() == "2026-09-11")
    );
    let warm = page(
        &mut reader,
        Query {
            refresh: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(warm.stats.metadata_reads, 0);
    assert_eq!(warm.stats.metadata_bytes, 0);
}

#[test]
fn unreadable_roots_report_failure_and_missing_roots_are_empty() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::write(&root, "").unwrap();
    let mut reader = Reader::new(vec![source(&root, HarnessKind::Claude)]).unwrap();
    assert!(page(&mut reader, Query::default()).is_err());
    fs::remove_file(&root).unwrap();
    let empty = page(&mut reader, Query::default()).unwrap();
    assert!(empty.entries.is_empty());
    assert!(
        page(
            &mut reader,
            Query {
                limit: 0,
                ..Query::default()
            }
        )
        .is_err()
    );
}

#[test]
fn missing_usage_counters_stay_absent_and_a_user_title_beats_later_generated_titles() {
    let dir = tempfile::tempdir().unwrap();
    let ch = dir.path().join("claude");
    let ph = dir.path().join("pi");
    let path = claude(&ch, A, "2026-09-10T12:00:00Z");
    let mut events: Vec<Value> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    events[1]["message"]["usage"] = json!({});
    events.push(json!({"type":"assistant","message":{"content":[{"type":"text","text":"x".repeat(256 * 1024)}]}}));
    events.push(json!({"type":"ai-title","aiTitle":"Generated title"}));
    write(&path, &events);
    let p = pi(&ph, B, "2026-09-10T13:00:00Z");
    let mut events: Vec<Value> = fs::read_to_string(&p)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    events[2]["message"]["usage"] = json!({"output":4});
    write(&p, &events);
    let mut reader = Reader::new(vec![
        source(&ch, HarnessKind::Claude),
        source(&ph, HarnessKind::Pi),
    ])
    .unwrap();
    for hydrate in [false, true] {
        let result = page(
            &mut reader,
            Query {
                hydrate,
                ..Query::default()
            },
        )
        .unwrap();
        let claude = result
            .entries
            .iter()
            .find(|e| e.key.harness == "claude")
            .unwrap();
        assert_eq!(
            claude.title.as_deref(),
            Some(format!("Session {A}").as_str())
        );
        if hydrate {
            let c = claude.columns.as_ref().unwrap();
            assert_eq!(
                (c.tokens_in, c.tokens_out, c.context_tokens),
                (None, None, None)
            );
            let p = result
                .entries
                .iter()
                .find(|e| e.key.harness == "pi")
                .unwrap()
                .columns
                .as_ref()
                .unwrap();
            assert_eq!(
                (p.tokens_in, p.tokens_out, p.context_tokens),
                (None, Some(4), None)
            );
        }
    }
}

#[test]
fn large_leading_snapshots_are_skipped_and_unrecoverable_tail_times_stay_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let a = claude(dir.path(), A, "2026-09-10T12:00:00Z");
    let contents = fs::read_to_string(&a).unwrap();
    fs::write(
        &a,
        format!(
            "{}\n{contents}",
            json!({"type":"file-history-snapshot","payload":"x".repeat(100 * 1024)})
        ),
    )
    .unwrap();
    let b = claude(dir.path(), B, "2026-09-10T13:00:00Z");
    let mut file = OpenOptions::new().append(true).open(&b).unwrap();
    writeln!(file, "{}", json!({"type":"event","timestamp":"2026-09-10T14:00:00Z","payload":"x".repeat(2 * 1024 * 1024)})).unwrap();
    let mut reader = Reader::new(vec![source(dir.path(), HarnessKind::Claude)]).unwrap();
    let result = page(&mut reader, Query::default()).unwrap();
    assert_eq!(ids(&result), [A, B], "unknown last activity sorts last");
    assert!(result.entries[0].last_activity.is_some());
    assert_eq!(
        result.entries[1].last_activity, None,
        "do not substitute head timestamps or file mtime"
    );
    assert!(result.stats.metadata_bytes < 2 * 1024 * 1024);
}

#[test]
fn statusline_changes_invalidate_columns_and_a_failed_refresh_keeps_the_prior_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    claude(dir.path(), A, "2026-09-10T12:00:00Z");
    let mut reader = Reader::new(vec![source(dir.path(), HarnessKind::Claude)]).unwrap();
    let before = page(
        &mut reader,
        Query {
            hydrate: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(
        before.entries[0].columns.as_ref().unwrap().context_window,
        None
    );
    write(
        &dir.path().join("statusline").join(format!("{A}.json")),
        &[json!({"context_window":{"context_window_size":200000}})],
    );
    let after = page(
        &mut reader,
        Query {
            hydrate: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(
        after.entries[0].columns.as_ref().unwrap().context_window,
        Some(200000)
    );
    let projects = dir.path().join("projects");
    fs::rename(&projects, dir.path().join("saved")).unwrap();
    fs::write(&projects, "").unwrap();
    assert!(
        page(
            &mut reader,
            Query {
                refresh: true,
                ..Query::default()
            }
        )
        .is_err()
    );
    let retained = page(&mut reader, Query::default()).unwrap();
    assert_eq!(retained.generation, before.generation);
    assert_eq!(ids(&retained), [A]);
}

#[test]
fn new_files_wait_for_refresh_and_old_column_summaries_are_evicted() {
    let dir = tempfile::tempdir().unwrap();
    for n in 0..130 {
        claude(
            dir.path(),
            &format!("{n:08x}-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
            "2026-09-10T12:00:00Z",
        );
    }
    let mut reader = Reader::new(vec![source(dir.path(), HarnessKind::Claude)]).unwrap();
    let first = page(
        &mut reader,
        Query {
            limit: 100,
            hydrate: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(first.stats.hydrated_files, 100);
    let old_id = first.entries[0].key.session_id.clone();
    claude(dir.path(), A, "2026-09-10T14:00:00Z");
    let rest = page(
        &mut reader,
        Query {
            after: first.next,
            hydrate: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(
        rest.total, 130,
        "new files do not change an in-progress snapshot"
    );
    assert_eq!(rest.stats.hydrated_files, 30);
    assert!(rest.next.is_none());
    let evicted = page(
        &mut reader,
        Query {
            filter: old_id.clone(),
            hydrate: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(
        evicted.stats.hydrated_files, 1,
        "column retention is bounded"
    );
    let cached = page(
        &mut reader,
        Query {
            filter: old_id,
            hydrate: true,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(cached.stats.hydrated_files, 0);
    let refresh = page(
        &mut reader,
        Query {
            refresh: true,
            limit: 1,
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(refresh.total, 131);
    assert_eq!(ids(&refresh), [A]);
    assert_eq!(refresh.stats.metadata_reads, 1);
}

#[test]
#[ignore = "read-only local measurement; set CONES_HISTORY_CLAUDE_HOME explicitly"]
fn measure_local_claude_archive() {
    let home = PathBuf::from(
        std::env::var_os("CONES_HISTORY_CLAUDE_HOME").expect("explicit fixture or native home"),
    );
    let mut reader = Reader::new(vec![source(&home, HarnessKind::Claude)]).unwrap();
    for refresh in [false, true] {
        let start = Instant::now();
        let result = page(
            &mut reader,
            Query {
                refresh,
                ..Query::default()
            },
        )
        .unwrap();
        eprintln!(
            "history refresh={refresh} sessions={} files={} reads={} bytes={} elapsed_ms={:.1}",
            result.total,
            result.stats.indexed_files,
            result.stats.metadata_reads,
            result.stats.metadata_bytes,
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
}

#[test]
fn viewport_hydration_reads_only_requested_keys_and_live_exclusions_accept_home_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("native");
    let alias = dir.path().join("alias");
    claude(&home, A, "2026-09-10T13:00:00Z");
    claude(&home, B, "2026-09-10T12:00:00Z");
    symlink(&home, &alias).unwrap();
    let mut reader = Reader::new(vec![source(&alias, HarnessKind::Claude)]).unwrap();
    let indexed = page(&mut reader, Query::default()).unwrap();
    assert_eq!(
        indexed.homes.get(&alias),
        Some(&home.canonicalize().unwrap())
    );
    let hydrated = page(
        &mut reader,
        Query {
            hydrate: true,
            hydrate_keys: Some([indexed.entries[1].key.clone()].into_iter().collect()),
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(hydrated.stats.hydrated_files, 1);
    assert!(hydrated.entries[0].columns.is_none());
    assert!(hydrated.entries[1].columns.is_some());
    let mut live = indexed.entries[0].key.clone();
    live.home = alias;
    let excluded = page(
        &mut reader,
        Query {
            excluded: [live].into_iter().collect(),
            ..Query::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&excluded), [B]);
}
