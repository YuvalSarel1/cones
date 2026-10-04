//! History of past conversations in the real dashboard. Claude transcripts, a Codex rollout
//! and a pi session are written into the fixture homes before start; no native client is
//! ever started.
//!
//! Covered: `ctrl+h` lists them below the live list, newest first, titled by the custom
//! title or else the first instruction, with subagent transcripts left out; each selection
//! shows its read-only transcript in the pane; `ctrl+f` and plain typing search titles and
//! conversation text, Enter keeps the search, Escape clears it and then hides history;
//! hiding with `ctrl+h` clears the search; a focused preview starts at the latest message
//! and scrolls with arrows, page keys, Home and End; resuming a row whose transcript was
//! deleted is refused with a message, launches nothing, and reopening history drops it.
use crate::dashboard::*;
use serde_json::{Value, json};
use std::{fs, path::Path};

const LONG: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const NAMED: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const CODEX: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const PI: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";

const UP: &[u8] = b"\x1b[A";
const DOWN: &[u8] = b"\x1b[B";
const HOME: &[u8] = b"\x1b[H";
const END: &[u8] = b"\x1b[F";
const PAGE_UP: &[u8] = b"\x1b[5~";
const CTRL_F: &[u8] = b"\x06";
const CTRL_H: &[u8] = b"\x08";
const CTRL_Y: &[u8] = b"\x19";
const ESC: &[u8] = b"\x1b";

/// Newest first: pi, Codex, the named Claude conversation, the long Claude one.
const TITLES: [&str; 4] = [
    "π   Sort the imports",
    ">_  Rename the config loader",
    "✻   Changelog for 0.3",
    "✻   Fix the flaky login test",
];

fn write(path: &Path, records: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text: String = records.iter().map(|v| format!("{v}\n")).collect();
    fs::write(path, text).unwrap();
}

/// Within the hour before `days` ago, so each conversation keeps its order and its age.
fn ago(days: i64, minute: i64) -> String {
    let at = chrono::Utc::now() - chrono::Duration::days(days) - chrono::Duration::minutes(60)
        + chrono::Duration::minutes(minute);
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn claude_transcript(d: &Dashboard, id: &str) -> std::path::PathBuf {
    d.home()
        .join(".claude/projects/-project")
        .join(format!("{id}.jsonl"))
}

/// Four past conversations in the project folder, one per row, plus a subagent transcript
/// that history must leave out.
fn conversations(d: &Dashboard) {
    let cwd = d.project().display().to_string();
    let assistant = |id: &str, at: String, n: &str, text: &str| {
        json!({"type":"assistant","sessionId":id,"cwd":cwd,"timestamp":at,"message":{
            "id":n,"role":"assistant","model":"claude-opus-5",
            "content":[{"type":"text","text":text}],"usage":{"input_tokens":10,"output_tokens":2}}})
    };
    let mut long = vec![
        json!({"type":"user","sessionId":LONG,"cwd":cwd,"timestamp":ago(4, 0),"message":{"role":"user","content":"Fix the flaky login test"}}),
    ];
    for n in 0..40 {
        long.push(assistant(
            LONG,
            ago(4, n + 1),
            &format!("m{n}"),
            &format!("Login step {n:02}"),
        ));
    }
    write(&claude_transcript(d, LONG), &long);
    write(
        &claude_transcript(d, NAMED),
        &[
            json!({"type":"user","sessionId":NAMED,"cwd":cwd,"timestamp":ago(3, 0),"message":{"role":"user","content":"Write the release notes"}}),
            assistant(NAMED, ago(3, 1), "m1", "Release notes written"),
            json!({"type":"custom-title","customTitle":"Changelog for 0.3","sessionId":NAMED}),
        ],
    );
    write(
        &d.home()
            .join(".claude/projects/-project")
            .join(NAMED)
            .join("subagents/agent-1.jsonl"),
        &[
            json!({"type":"user","sessionId":NAMED,"cwd":cwd,"isSidechain":true,"timestamp":ago(1, 0),"message":{"role":"user","content":"Subagent chore"}}),
        ],
    );
    write(
        &d.home()
            .join(".codex/sessions/2026/09/12")
            .join(format!("rollout-2026-09-12T10-00-00-{CODEX}.jsonl")),
        &[
            json!({"type":"session_meta","timestamp":ago(2, 0),"payload":{"id":CODEX,"cwd":cwd,"timestamp":ago(2, 0),"source":"cli"}}),
            json!({"type":"event_msg","timestamp":ago(2, 1),"payload":{"type":"user_message","message":"Rename the config loader"}}),
            json!({"type":"response_item","timestamp":ago(2, 2),"payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Renamed the loader"}]}}),
        ],
    );
    write(
        &d.home()
            .join(".pi/sessions/--project--")
            .join(format!("start_{PI}.jsonl")),
        &[
            json!({"type":"session","id":PI,"cwd":cwd,"timestamp":ago(1, 0)}),
            json!({"type":"message","timestamp":ago(1, 1),"message":{"role":"user","content":[{"type":"text","text":"Sort the imports"}]}}),
            json!({"type":"message","timestamp":ago(1, 2),"message":{"role":"assistant","stopReason":"stop","model":"pi-model","content":[{"type":"text","text":"Imports sorted alphabetically"}]}}),
        ],
    );
}

/// The list: everything left of the pane's border.
fn list(screen: &str) -> String {
    screen
        .lines()
        .map(|l| l.chars().take(70).collect::<String>().trim_end().to_owned() + "\n")
        .collect()
}

/// The pane: everything right of its border.
fn pane(screen: &str) -> String {
    screen
        .lines()
        .map(|l| l.chars().skip(71).collect::<String>().trim().to_owned() + "\n")
        .collect()
}

fn history_rows(screen: &str) -> Vec<String> {
    list(screen)
        .lines()
        // A row names its folder; a search excerpt is indented beneath its row.
        .filter(|l| {
            l.contains("/tmp/…/project") || (l.starts_with("    ") && !l.starts_with("     "))
        })
        .map(str::to_owned)
        .collect()
}

fn started(test: &str) -> Dashboard {
    let mut d = Dashboard::new(test, &["claude", "codex", "pi"]);
    conversations(&d);
    d.start();
    d
}

fn open_history(d: &mut Dashboard) -> String {
    d.press("ctrl+h", CTRL_H);
    d.wait_for("four history rows and the first preview", |s| {
        history_rows(s).len() == 4 && pane(s).contains("Imports sorted alphabetically")
    })
}

/// The rows, in order, each with its harness icon and title.
fn assert_rows(screen: &str, titles: &[&str]) {
    let rows = history_rows(screen);
    assert_eq!(rows.len(), titles.len(), "{screen}");
    for (row, title) in rows.iter().zip(titles) {
        assert!(row.contains(title), "{title:?} in {row:?}\n{screen}");
    }
}

#[test]
fn history_lists_past_conversations_from_every_harness_and_previews_each_read_only() {
    let mut d = started("history-list");
    let screen = d.capture("no-history");
    assert!(!list(&screen).contains("history"), "{screen}");
    assert!(
        screen.contains("claude › Type to start a new agent"),
        "{screen}"
    );

    let screen = open_history(&mut d);
    d.capture("history-open");
    let listed = list(&screen);
    assert!(listed.contains("\nhistory\n"), "{screen}");
    assert!(listed.contains("title"), "{screen}");
    assert!(listed.contains("last active  folder"), "{screen}");
    assert_rows(&screen, &TITLES);
    assert!(history_rows(&screen)[0].starts_with("▌"), "{screen}");
    assert!(!screen.contains("Subagent chore"), "{screen}");
    assert!(!screen.contains("Write the release notes"), "{screen}");
    assert!(
        screen.contains("history words / Type to search by words"),
        "{screen}"
    );
    assert!(screen.contains("enter resume · ctrl+y fork"), "{screen}");
    let shown = pane(&screen);
    assert!(shown.contains("π pi · history · read only"), "{screen}");
    assert!(shown.contains("Sort the imports"), "{screen}");

    d.press("down", DOWN);
    let screen = d.wait_for("the Codex preview", |s| {
        pane(s).contains("Renamed the loader")
    });
    d.capture("codex-preview");
    let shown = pane(&screen);
    assert!(shown.contains(">_ codex · history · read only"), "{screen}");
    assert!(shown.contains("› Rename the config loader"), "{screen}");
    assert!(shown.contains("• Renamed the loader"), "{screen}");
    assert!(history_rows(&screen)[1].starts_with("▌"), "{screen}");

    d.press("down", DOWN);
    let screen = d.wait_for("the named Claude preview", |s| {
        pane(s).contains("Release notes written")
    });
    d.capture("claude-preview");
    let shown = pane(&screen);
    assert!(shown.contains("✻ claude · history · read only"), "{screen}");
    // The custom title names the row; the preview keeps the first instruction.
    assert!(shown.contains("❯ Write the release notes"), "{screen}");
    assert!(shown.contains("⏺ Release notes written"), "{screen}");
    assert!(!shown.contains("Subagent chore"), "{screen}");

    d.press("ctrl+h", CTRL_H);
    let screen = d.wait_for("history hidden", |s| history_rows(s).is_empty());
    d.capture("history-hidden");
    assert!(!list(&screen).contains("history"), "{screen}");
    assert!(
        screen.contains("claude › Type to start a new agent"),
        "{screen}"
    );
    assert!(!pane(&screen).contains("read only"), "{screen}");
    d.quit();

    // Browsing launched nothing and started no terminal.
    assert!(!d.path("state/launches.jsonl").exists());
    assert!(
        fs::read_dir(d.path("state/terminals"))
            .map(|e| e.count() == 0)
            .unwrap_or(true)
    );
}

#[test]
fn history_search_filters_titles_and_conversation_text_and_escape_backs_out() {
    let mut d = started("history-search");
    open_history(&mut d);

    // An explicit filter matches the custom title.
    d.press("ctrl+f", CTRL_F);
    d.typed("changelog");
    let screen = d.wait_for("one match for changelog", |s| {
        history_rows(s).len() == 1 && pane(s).contains("Release notes written")
    });
    d.capture("filter-typed");
    assert_rows(&screen, &["✻   Changelog for 0.3"]);
    assert!(screen.contains("/ changelog"), "{screen}");
    assert!(
        screen.contains("enter keep the search · esc clear it"),
        "{screen}"
    );

    d.press("enter", b"\r");
    let screen = d.wait_text("history words / changelog");
    d.capture("filter-kept");
    assert_rows(&screen, &["✻   Changelog for 0.3"]);
    assert!(
        screen.contains("search: changelog  enter resume"),
        "{screen}"
    );

    d.press("esc", ESC);
    let screen = d.wait_for("the search cleared", |s| history_rows(s).len() == 4);
    d.capture("filter-cleared");
    assert_rows(&screen, &TITLES);
    assert!(
        screen.contains("history words / Type to search by words"),
        "{screen}"
    );

    // Typing with history selected searches conversation text and shows the passage.
    d.typed("alphabetically");
    let screen = d.wait_for("one match for alphabetically", |s| {
        list(s).contains("Imports sorted alphabetically")
            && !list(s).contains("Changelog")
            && pane(s).contains("Imports sorted alphabetically")
    });
    d.capture("text-search");
    let rows = history_rows(&screen);
    assert_eq!(rows.len(), 2, "{screen}");
    assert!(rows[0].contains("π   Sort the imports"), "{screen}");
    assert_eq!(rows[1].trim(), "Imports sorted alphabetically", "{screen}");
    assert!(
        screen.contains("history words / alphabetically"),
        "{screen}"
    );
    assert!(
        pane(&screen).contains("Imports sorted alphabetically"),
        "{screen}"
    );

    // Escape clears the query, then hides history.
    d.press("esc", ESC);
    let screen = d.wait_for("the query cleared", |s| history_rows(s).len() == 4);
    d.capture("query-cleared");
    assert_rows(&screen, &TITLES);
    d.press("esc", ESC);
    let screen = d.wait_for("history hidden", |s| history_rows(s).is_empty());
    d.capture("escape-hid-history");
    assert!(!list(&screen).contains("history"), "{screen}");

    // Hiding with ctrl+h clears a kept search: reopening lists everything.
    open_history(&mut d);
    d.press("ctrl+f", CTRL_F);
    d.typed("changelog");
    d.press("enter", b"\r");
    d.wait_for("one match for changelog", |s| history_rows(s).len() == 1);
    d.press("ctrl+h", CTRL_H);
    let screen = d.wait_for("history hidden", |s| history_rows(s).is_empty());
    d.capture("ctrl-h-hid-search");
    assert!(!screen.contains("changelog"), "{screen}");
    let screen = open_history(&mut d);
    d.capture("reopened-unfiltered");
    assert_rows(&screen, &TITLES);
    assert!(
        screen.contains("history words / Type to search by words"),
        "{screen}"
    );
    d.quit();
}

#[test]
fn a_focused_history_preview_starts_at_the_latest_message_and_scrolls() {
    let mut d = started("history-scroll");
    open_history(&mut d);
    for _ in 0..3 {
        d.press("down", DOWN);
    }
    let screen = d.wait_for("the long preview", |s| pane(s).contains("Login step 39"));
    d.capture("latest");
    assert!(history_rows(&screen)[3].starts_with("▌"), "{screen}");
    assert!(
        !pane(&screen).contains("Fix the flaky login test"),
        "{screen}"
    );

    d.press("tab", b"\t");
    let screen = d.wait_text("↑ ↓ scroll · enter resume · → context · tab list");
    d.capture("focused");
    assert!(pane(&screen).contains("Login step 39"), "{screen}");

    d.press("up", UP);
    let screen = d.wait_for("one line back", |s| !pane(s).contains("Login step 39"));
    d.capture("up");
    assert!(pane(&screen).contains("Login step 38"), "{screen}");

    d.press("page up", PAGE_UP);
    let screen = d.wait_for("a page back", |s| !pane(s).contains("Login step 30"));
    d.capture("page-up");
    assert!(pane(&screen).contains("Login step 15"), "{screen}");

    d.press("home", HOME);
    let screen = d.wait_for("the first message", |s| {
        pane(s).contains("❯ Fix the flaky login test")
    });
    d.capture("home");
    assert!(pane(&screen).contains("⏺ Login step 00"), "{screen}");
    assert!(!pane(&screen).contains("Login step 39"), "{screen}");

    d.press("end", END);
    let screen = d.wait_for("the latest message", |s| pane(s).contains("Login step 39"));
    d.capture("end");
    assert!(
        !pane(&screen).contains("Fix the flaky login test"),
        "{screen}"
    );

    d.press("tab", b"\t");
    let screen = d.wait_text("History     enter resume");
    d.capture("back-to-list");
    assert!(history_rows(&screen)[3].starts_with("▌"), "{screen}");
    d.quit();
}

#[test]
fn resuming_a_history_row_whose_transcript_was_deleted_is_refused() {
    let mut d = started("history-deleted");
    open_history(&mut d);
    for _ in 0..3 {
        d.press("down", DOWN);
    }
    d.wait_for("the long preview", |s| pane(s).contains("Login step 39"));
    fs::remove_file(claude_transcript(&d, LONG)).unwrap();
    d.press("enter", b"\r");
    let screen = d.wait_text("attach failed: session transcript no longer exists");
    d.capture("refused");
    assert!(
        history_rows(&screen)[3].contains("Fix the flaky login test"),
        "{screen}"
    );
    // No viewer opened: the pane still shows the read-only preview.
    assert!(
        pane(&screen).contains("✻ claude · history · read only"),
        "{screen}"
    );

    // Reopening history rereads it and the row is gone.
    d.press("ctrl+h", CTRL_H);
    d.wait_for("history hidden", |s| history_rows(s).is_empty());
    d.press("ctrl+h", CTRL_H);
    let screen = d.wait_for("three history rows", |s| history_rows(s).len() == 3);
    d.capture("reloaded");
    assert_rows(&screen, &TITLES[..3]);
    d.quit();

    // The submission is recorded for recovery; nothing was launched or hosted.
    let launches = d.wait_file("state/launches.jsonl", |t| !t.is_empty());
    d.keep("state/launches.jsonl");
    let events: Vec<Value> = launches
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(events.len(), 1, "{launches}");
    assert_eq!(events[0]["event"], "resume.submitted");
    assert_eq!(events[0]["level"], "recovery");
    assert_eq!(events[0]["data"]["operation_id"], LONG);
    assert_eq!(events[0]["data"]["harness"], "claude");
    assert_eq!(events[0]["data"]["cwd"], d.project().display().to_string());
    assert_eq!(events[0]["data"]["prompt"], "Fix the flaky login test");
    assert!(
        fs::read_dir(d.path("state/terminals"))
            .map(|e| e.count() == 0)
            .unwrap_or(true)
    );
}

/// ctrl+y asks where the fork goes before anything starts. Picking another harness starts a
/// new session seeded with the conversation's tail, recorded in the launch ledger like any
/// launch and carrying no fork link. The codex on the launch path refuses to start, so no
/// native client runs.
#[test]
fn ctrl_y_asks_for_a_harness_and_seeds_a_cross_harness_fork() {
    let mut d = Dashboard::new("history-fork-picker", &["claude", "codex", "pi"]);
    conversations(&d);
    let codex = d.home().join(".local/bin/codex");
    fs::write(&codex, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&codex, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    d.start();
    open_history(&mut d);
    for _ in 0..3 {
        d.press("down", DOWN);
    }
    d.wait_for("the long preview", |s| pane(s).contains("Login step 39"));
    d.press("ctrl+y", CTRL_Y);
    let screen = d.wait_text("fork to");
    d.capture("fork-picker");
    // The overlay covers the bottom of the list; the pane keeps drawing beside it.
    let picker: Vec<&str> = screen
        .lines()
        .skip_while(|l| !l.contains("fork to"))
        .skip(1)
        .take(4)
        .collect();
    assert!(
        picker[0].contains("› claude fork"),
        "native fork leads\n{screen}"
    );
    assert!(
        picker[1].contains("  new codex session with this conversation"),
        "{screen}"
    );
    assert!(
        picker[2].contains("  new pi session with this conversation"),
        "{screen}"
    );
    assert!(
        !picker[3].contains("with this conversation"),
        "disabled harnesses are not offered\n{screen}"
    );
    assert!(
        !d.path("state/launches.jsonl").exists(),
        "nothing starts before a choice"
    );

    d.press("down", DOWN);
    d.press("enter", b"\r");
    let launches = d.wait_file("state/launches.jsonl", |t| !t.is_empty());
    d.quit();
    d.keep("state/launches.jsonl");
    let events: Vec<Value> = launches
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(events.len(), 1, "{launches}");
    assert_eq!(events[0]["event"], "launch.submitted");
    assert_eq!(events[0]["data"]["harness"], "codex");
    assert_eq!(events[0]["data"]["cwd"], d.project().display().to_string());
    let prompt = events[0]["data"]["prompt"].as_str().unwrap();
    assert!(
        prompt.starts_with("Continue from Fix the flaky login test\n"),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("held in claude (session {LONG})")),
        "{prompt}"
    );
    let transcript = claude_transcript(&d, LONG)
        .canonicalize()
        .unwrap()
        .display()
        .to_string();
    assert!(
        prompt.contains(&format!("transcript is {transcript};")),
        "{prompt}"
    );
    assert!(prompt.contains("wait for my next instruction"), "{prompt}");
    // The tail is the last 40 messages: every step, and the first instruction left out.
    for n in 0..40 {
        assert!(
            prompt.contains(&format!("Login step {n:02}")),
            "{n}: {prompt}"
        );
    }
    assert!(!prompt.contains("--all"), "{prompt}");
    assert!(!prompt.contains("user "), "{prompt}");
    // A seeded session is a new conversation: no fork link is recorded.
    assert!(!d.path("state/forks.json").exists());
}
