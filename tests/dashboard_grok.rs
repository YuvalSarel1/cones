//! Grok Build in the native viewer, driven through the real dashboard.
//!
//! The client is `tests/fixtures/fake_grok.py` on the fixture HOME's launch path, never the
//! machine's grok: it draws only on stderr, as Grok 1.0.77 does, and keeps its session folder
//! the way the CLI does. The flow covers launching grok with an instruction from the composer,
//! the pane showing what it drew, its row reading the session it holds, Left returning from an
//! empty line, Tab and Left staying native with a drafted one, ctrl+z returning either way, Enter re-entering the same
//! process with its draft, and ctrl+x twice stopping it until the process is gone.
use crate::dashboard::*;
use serde_json::Value;
use std::{
    fs,
    time::{Duration, Instant},
};

const LIST_FOOTER: &str = "enter return · ctrl+x stop · ? shortcuts";
const PANE_FOOTER: &str = "← back · ctrl+\\ full screen";
const DRAFT_FOOTER: &str = "ctrl+z back · ctrl+\\ full screen";

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The list line for the grok session titled `title` in `state`.
fn row<'a>(screen: &'a str, title: &str, state: &str) -> Option<&'a str> {
    screen.lines().find(|l| {
        let words: Vec<&str> = l.split_whitespace().collect();
        words.contains(&"⊘") && words.contains(&state) && l.contains(title)
    })
}

fn hosted(d: &Dashboard) -> Vec<Value> {
    fs::read_dir(d.path("state/terminals"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .map(|p| serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap())
        .collect()
}

#[test]
fn a_composer_grok_draws_in_its_viewer_returns_with_ctrl_z_and_stops_with_ctrl_x() {
    let mut d = Dashboard::new("grok_viewer", &["grok"]);
    d.install("grok", "fake_grok.py");
    let project = d.project().canonicalize().unwrap();
    d.open_folders(std::slice::from_ref(&project));
    d.start();
    d.wait_text("grok › Type to start a new agent");
    d.capture("first-frame");

    d.typed("fix the tests");
    d.press("enter", b"\r");
    // Grok draws on stderr only: the pane shows that screen, not a blank one.
    let screen = d.wait_text("argv: --trust --always-approve -- fix the tests");
    d.capture("launched");
    wait_until("the host record", || hosted(&d).len() == 1);
    let pid = hosted(&d)[0]["session"]["pid"].as_u64().unwrap() as u32;
    assert!(
        screen.contains(&format!("reply {pid}: fix the tests")),
        "{screen}"
    );
    let screen = d.wait_for("the idle grok row", |s| {
        row(s, "Fixture fix the tests", "idle").is_some()
    });
    d.capture("idle-row");
    let line = row(&screen, "Fixture fix the tests", "idle").unwrap();
    assert!(
        line.contains("        0%"),
        "the row shows Grok's context as a share of its window: {line}"
    );
    assert!(screen.contains("▁ 1 idle"), "{screen}");

    d.press("enter", b"\r");
    d.wait_text(PANE_FOOTER);
    // With an empty line Tab stays Grok's and Left returns to the list.
    d.press("tab", b"\t");
    std::thread::sleep(Duration::from_millis(200));
    assert!(d.screen().contains(PANE_FOOTER), "{}", d.screen());
    d.press("left", b"\x1b[D");
    d.wait_text(LIST_FOOTER);
    d.capture("left-from-empty");
    d.press("enter", b"\r");
    d.wait_text(PANE_FOOTER);
    // With a drafted line Tab and Left stay Grok's; only ctrl+z leaves.
    d.typed("hello grok");
    d.wait_text(DRAFT_FOOTER);
    for (label, key) in [("tab", &b"\t"[..]), ("left", &b"\x1b[D"[..])] {
        d.press(label, key);
        std::thread::sleep(Duration::from_millis(200));
        assert!(d.screen().contains(DRAFT_FOOTER), "{label}: {}", d.screen());
    }
    d.capture("drafted");
    d.press("ctrl+z", b"\x1a");
    d.wait_text(LIST_FOOTER);
    d.capture("back-with-draft");

    // Enter reaches the same process, which still holds the draft.
    d.press("enter", b"\r");
    d.wait_text(DRAFT_FOOTER);
    d.press("enter", b"\r");
    let screen = d.wait_text(&format!("reply {pid}: hello grok"));
    d.capture("submitted");
    assert_eq!(
        hosted(&d).len(),
        1,
        "Enter started no second grok: {screen}"
    );
    d.press("ctrl+z", b"\x1a");
    d.wait_for("the row's last reply", |s| {
        row(s, "Fixture fix the tests", "idle").is_some_and(|l| l.contains("hello grok"))
            || s.contains(&format!("reply {pid}: hello grok"))
    });
    d.wait_text(LIST_FOOTER);

    d.press("ctrl+x", b"\x18");
    d.wait_text("ctrl+x again to stop");
    d.press("ctrl+x", b"\x18");
    wait_until("the fixture grok to exit", || !alive(pid));
    wait_until("the host record to go", || hosted(&d).is_empty());
    d.wait_for("the row to leave", |s| {
        row(s, "Fixture fix the tests", "idle").is_none()
    });
    d.capture("stopped");
    d.quit();
}
