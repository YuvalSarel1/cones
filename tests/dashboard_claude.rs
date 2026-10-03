//! Claude background sessions in the live list, driven through the real dashboard.
//!
//! The `claude` on the launch path is `tests/fixtures/fake_claude_fleet.py`: it lists sessions
//! in the fixture's Claude registry behind live `/bin/sleep` stand-ins and logs every call it
//! gets to `$HOME/claude-calls.jsonl`. The flows cover a composer launch (harness cycling,
//! the placeholder becoming the discovered row, the recovery and debug records that tie the
//! launch to its native id), a refused launch that keeps the instruction, pre-existing
//! sessions in every state grouped by folder and by state, arrows, the filter, pins and
//! highlights, and `ctrl+x` twice removing a session through `claude rm`.
use crate::dashboard::*;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command},
};

const DOWN: &[u8] = b"\x1b[B";
const UP: &[u8] = b"\x1b[A";
const SHIFT_TAB: &[u8] = b"\x1b[Z";
const CTRL_F: &[u8] = b"\x06";
const CTRL_P: &[u8] = b"\x10";
const CTRL_S: &[u8] = b"\x13";
const CTRL_T: &[u8] = b"\x14";
const CTRL_X: &[u8] = b"\x18";

/// A background session that existed before the dashboard started: a live stand-in daemon,
/// its registry entry, a transcript with an AI title, and a job record when `job` names one.
struct Seeded {
    id: String,
    short: String,
    daemon: Child,
}

fn seed(
    d: &Dashboard,
    dir: &Path,
    minute: u32,
    title: &str,
    status: &str,
    job: Option<&str>,
) -> Seeded {
    let daemon = Command::new("/bin/sleep")
        .arg("600")
        .process_group(0)
        .spawn()
        .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let short = id[..8].to_owned();
    let claude = d.home().join(".claude");
    let cwd = dir.to_string_lossy().to_string();
    let key: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let transcript = claude
        .join("projects")
        .join(key)
        .join(format!("{id}.jsonl"));
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let lines = [
        json!({"sessionId": id, "type": "user", "cwd": cwd,
               "timestamp": format!("2026-10-01T09:{minute:02}:00.000Z"),
               "message": {"role": "user", "content": format!("please {title}")}}),
        json!({"type": "ai-title", "sessionId": id, "aiTitle": title}),
    ];
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(&transcript, text).unwrap();
    if let Some(state) = job {
        let dir = claude.join("jobs").join(&short);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("state.json"),
            json!({"state": state, "tempo": "idle", "sessionId": id}).to_string(),
        )
        .unwrap();
    }
    fs::create_dir_all(claude.join("sessions")).unwrap();
    fs::write(
        claude
            .join("sessions")
            .join(format!("{}.json", daemon.id())),
        json!({"pid": daemon.id(), "sessionId": id, "cwd": cwd, "kind": "bg",
               "jobId": short, "status": status, "startedAt": 1_759_309_200_000u64})
        .to_string(),
    )
    .unwrap();
    Seeded { id, short, daemon }
}

fn records(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The argv of every `claude` call the fixture logged.
fn calls(d: &Dashboard) -> Vec<Vec<String>> {
    records(&fs::read_to_string(d.home().join("claude-calls.jsonl")).unwrap_or_default())
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect()
}

/// The registry entries the fixture's Claude home lists.
fn registry(d: &Dashboard) -> Vec<Value> {
    fs::read_dir(d.home().join(".claude/sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| serde_json::from_slice(&fs::read(e.path()).ok()?).ok())
        .collect()
}

/// The screen line that holds `text`.
fn line_of<'a>(screen: &'a str, text: &str) -> &'a str {
    screen
        .lines()
        .find(|l| l.contains(text))
        .unwrap_or_else(|| panic!("{text:?} is not on screen:\n{screen}"))
}

/// The selected row in the list: the one carrying the `▌` mark.
fn selected(screen: &str) -> &str {
    line_of(screen, "▌ ")
}

/// The list's line index of `text`, so tests can assert which group heading a row sits under.
fn row(screen: &str, text: &str) -> usize {
    screen
        .lines()
        .position(|l| l.contains(text))
        .unwrap_or_else(|| panic!("{text:?} is not on screen:\n{screen}"))
}

/// Open the project folder by its real path. Claude records the resolved cwd, and `/tmp` is a
/// symlink on macOS; the alias case is its own test below.
fn real_project(d: &Dashboard) -> std::path::PathBuf {
    let project = d.project().canonicalize().unwrap();
    d.open_folders(std::slice::from_ref(&project));
    project
}

/// Whether `screen` has a group heading `name`: the list side of a line, before the pane.
fn heading(screen: &str, name: &str) -> Option<usize> {
    screen
        .lines()
        .position(|l| l.split('│').next().unwrap().trim_end() == name)
}

/// The composer line, between the two rules above the footer.
fn composer(screen: &str) -> &str {
    line_of(screen, " › ")
}

#[test]
fn a_composer_launch_becomes_a_listed_background_session_with_its_native_id() {
    let mut d = Dashboard::new("claude-launch", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    d.start();
    let screen = d.wait_text("claude › ");
    assert!(
        composer(&screen).starts_with("✻ claude › Type to start a new agent"),
        "claude is the default harness: {screen}"
    );
    assert!(screen.contains("no sessions here"), "{screen}");
    d.capture("empty-fleet");

    // Shift+Tab cycles the enabled harnesses, then the terminal, and back.
    d.press("shift+tab", SHIFT_TAB);
    let screen = d.wait_text("terminal (zsh) › ");
    d.capture("terminal-selected");
    assert!(!composer(&screen).contains("claude"), "{screen}");
    d.press("shift+tab", SHIFT_TAB);
    d.wait_text("✻ claude › ");

    d.typed("count the flaky tests");
    let screen = d.capture("instruction-typed");
    assert!(
        composer(&screen).contains("✻ claude › count the flaky tests"),
        "{screen}"
    );
    d.press("enter", b"\r");

    // The placeholder becomes the session discovery reports: its AI title and live state.
    d.wait_text("Fixture title for the launch");
    let launched = d.capture("launched");
    let line = line_of(&launched, "Fixture title for the launch");
    assert!(line.contains("✻  working"), "{line}");
    assert!(
        line.starts_with("▌ "),
        "the launch stays selected: {launched}"
    );
    assert!(
        launched.contains(&format!("{}", project.display())),
        "the session is grouped under the folder it launched in:\n{launched}"
    );
    assert!(
        !launched.contains("count the flaky tests"),
        "the instruction left the composer and the placeholder left the list:\n{launched}"
    );
    assert!(
        composer(&launched).contains("Type to start a new agent"),
        "{launched}"
    );
    assert!(
        !launched.contains("no sessions here"),
        "one folder group, holding the session:\n{launched}"
    );
    assert!(launched.contains("1 working"), "{launched}");

    // The native launch the dashboard ran, with the instruction as its prompt.
    let entries = registry(&d);
    assert_eq!(entries.len(), 1, "{entries:?}");
    let session = entries[0]["sessionId"].as_str().unwrap().to_owned();
    let short = &session[..8];
    let launch = calls(&d)
        .into_iter()
        .find(|c| c.first().map(String::as_str) == Some("--bg"))
        .expect("a --bg launch");
    assert_eq!(
        launch[launch.len() - 2..],
        ["--".to_owned(), "count the flaky tests".to_owned()],
        "{launch:?}"
    );

    // The viewer attaches to the launched session by its short id.
    let screen = d.wait_text(&format!("fixture attached to {short}"));
    d.capture("attached");
    assert!(screen.contains(&format!("fixture attached to {short}")));

    // launches.jsonl records the submission before launch, and the debug log ties that
    // operation to the native id discovery resolved.
    let recovery = records(&d.wait_file("state/launches.jsonl", |t| !t.is_empty()));
    assert_eq!(recovery.len(), 1, "{recovery:?}");
    let submitted = &recovery[0];
    assert_eq!(submitted["event"], "launch.submitted");
    assert_eq!(submitted["level"], "recovery");
    assert_eq!(submitted["data"]["harness"], "claude");
    assert_eq!(submitted["data"]["prompt"], "count the flaky tests");
    assert_eq!(
        submitted["data"]["cwd"].as_str().map(Path::new),
        Some(project.as_path())
    );
    let operation = submitted["data"]["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(operation.starts_with("starting:"), "{operation}");
    let debug = d.wait_file("state/tui-debug.log", |t| {
        records(t).iter().any(|r| r["event"] == "row.reidentified")
    });
    let identified = records(&debug)
        .into_iter()
        .find(|r| r["event"] == "row.reidentified")
        .unwrap();
    assert_eq!(identified["data"]["before_id"], operation.as_str());
    assert_eq!(identified["data"]["session_id"], session.as_str());
    assert_eq!(identified["data"]["reason"], "native_identity_reported");
    d.keep("state/launches.jsonl");
    d.quit();
}

#[test]
fn a_refused_launch_keeps_the_instruction_and_shows_the_error() {
    let mut d = Dashboard::new("claude-refused", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    fs::write(d.home().join("fake-launch"), "fail").unwrap();
    d.start();
    d.wait_text("✻ claude › ");
    d.typed("count the flaky tests");
    d.press("enter", b"\r");
    let screen = d.wait_text("failed: ");
    d.capture("refused");
    let footer = format!("claude in {} failed: ", project.display());
    assert!(screen.contains(&footer), "{screen}");
    assert!(
        screen.contains("no sessions here"),
        "the placeholder left the list:\n{screen}"
    );
    assert!(screen.contains("0 working"), "{screen}");
    // The narrow footer clips the harness's words; the debug log keeps all of them.
    let debug = d.wait_file("state/tui-debug.log", |t| {
        records(t).iter().any(|r| r["event"] == "launch.applied")
    });
    let applied = records(&debug)
        .into_iter()
        .find(|r| r["event"] == "launch.applied")
        .unwrap();
    assert_eq!(applied["data"]["outcome"], "failed");
    let error = applied["data"]["error"].as_str().unwrap();
    assert!(error.contains("fixture refused this launch"), "{error}");

    // The instruction is restored to the folder's composer.
    let screen = d.wait_text("no sessions here");
    d.capture("instruction-restored");
    assert!(selected(&screen).contains("no sessions here"), "{screen}");
    assert!(
        composer(&screen).contains("✻ claude › count the flaky tests"),
        "the instruction is back in the composer:\n{screen}"
    );
    assert!(registry(&d).is_empty());
    let recovery = records(&fs::read_to_string(d.path("state/launches.jsonl")).unwrap());
    assert_eq!(recovery.len(), 1, "{recovery:?}");
    assert_eq!(recovery[0]["event"], "launch.submitted");
    assert_eq!(recovery[0]["data"]["prompt"], "count the flaky tests");

    // The kept instruction launches as is once the harness accepts it.
    fs::write(d.home().join("fake-launch"), "ok").unwrap();
    d.press("enter", b"\r");
    let screen = d.wait_text("Fixture title for the launch");
    d.capture("retried");
    assert!(!screen.contains("failed: "), "{screen}");
    assert!(
        composer(&screen).contains("Type to start a new agent"),
        "{screen}"
    );
    let bg: Vec<_> = calls(&d)
        .into_iter()
        .filter(|c| c.first().map(String::as_str) == Some("--bg"))
        .collect();
    assert_eq!(bg.len(), 2, "{bg:?}");
    assert!(
        bg.iter()
            .all(|c| c.last().unwrap() == "count the flaky tests")
    );
    assert_eq!(registry(&d).len(), 1);
    d.keep("state/launches.jsonl");
    d.quit();
}

#[test]
fn existing_sessions_group_navigate_filter_pin_and_highlight() {
    let mut d = Dashboard::new("claude-fleet", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    let other = d.path("other").canonicalize().unwrap_or_else(|_| {
        fs::create_dir_all(d.path("other")).unwrap();
        d.path("other").canonicalize().unwrap()
    });
    let fix = seed(&d, &project, 1, "Fix the parser", "busy", None);
    let ask = seed(&d, &project, 2, "Answer my question", "waiting", None);
    let rest = seed(&d, &project, 3, "Rest a while", "idle", None);
    let _ship = seed(&d, &project, 4, "Ship the release", "idle", Some("done"));
    let away = seed(&d, &other, 5, "Tidy the other folder", "idle", None);
    d.start();

    // By folder: each folder heads its own sessions, oldest first.
    let screen = d.wait_text("Tidy the other folder");
    d.capture("by-folder");
    assert!(
        screen.contains("1 working  ▇ 1 input  ▁ 2 idle  ✓ 1 done"),
        "{screen}"
    );
    let order = [
        row(&screen, &project.display().to_string()),
        row(&screen, "Fix the parser"),
        row(&screen, "Answer my question"),
        row(&screen, "Rest a while"),
        row(&screen, "Ship the release"),
    ];
    assert!(order.is_sorted(), "{order:?}\n{screen}");
    let other_heading = row(&screen, &other.display().to_string());
    assert!(
        other_heading + 1 == row(&screen, "Tidy the other folder"),
        "{screen}"
    );
    for (title, state) in [
        ("Fix the parser", "working"),
        ("Answer my question", "input"),
        ("Rest a while", "idle"),
        ("Ship the release", "done"),
        ("Tidy the other folder", "idle"),
    ] {
        assert!(
            line_of(&screen, title).contains(&format!("✻  {state}")),
            "{title} should be {state}:\n{screen}"
        );
    }

    // Arrows move the selection and the pane attaches to the selected session.
    assert!(
        selected(&screen).contains("Tidy the other folder"),
        "{screen}"
    );
    d.wait_text(&format!("fixture attached to {}", away.short));
    d.press("down", DOWN);
    let screen = d.wait_text(&format!("fixture attached to {}", fix.short));
    assert!(selected(&screen).contains("Fix the parser"), "{screen}");
    d.press("down", DOWN);
    let screen = d.wait_text(&format!("fixture attached to {}", ask.short));
    d.capture("down");
    assert!(selected(&screen).contains("Answer my question"), "{screen}");
    d.press("down", DOWN);
    d.press("up", UP);
    let screen = d.wait_for("the selection back on the input row", |s| {
        s.lines()
            .any(|l| l.starts_with("▌ ") && l.contains("Answer my question"))
    });
    assert!(selected(&screen).contains("Answer my question"), "{screen}");

    // By state: input first, then working, idle and done, each under its heading.
    d.press("ctrl+s", CTRL_S);
    let screen = d.wait_for("state groups", |s| heading(s, "input").is_some());
    d.capture("by-state");
    let headings = ["input", "working", "idle", "done"]
        .map(|h| heading(&screen, h).unwrap_or_else(|| panic!("no {h} heading:\n{screen}")));
    assert!(headings.is_sorted(), "{headings:?}\n{screen}");
    let under = |title: &str| {
        let at = row(&screen, title);
        headings.iter().rposition(|&h| h < at).unwrap()
    };
    assert_eq!(under("Answer my question"), 0, "{screen}");
    assert_eq!(under("Fix the parser"), 1, "{screen}");
    assert_eq!(under("Rest a while"), 2, "{screen}");
    assert_eq!(under("Tidy the other folder"), 2, "{screen}");
    assert_eq!(under("Ship the release"), 3, "{screen}");
    assert!(
        selected(&screen).contains("Answer my question"),
        "grouping keeps the selection:\n{screen}"
    );
    d.press("ctrl+s", CTRL_S);
    let screen = d.wait_for("folder grouping again", |s| heading(s, "input").is_none());
    assert!(screen.contains(&project.display().to_string()), "{screen}");

    // The filter narrows the list; Enter keeps it, Escape clears it.
    d.press("ctrl+f", CTRL_F);
    d.typed("release");
    let screen = d.wait_for("the filtered list", |s| !s.contains("Fix the parser"));
    d.capture("filter-typed");
    assert!(screen.contains("Ship the release"), "{screen}");
    for gone in [
        "Answer my question",
        "Rest a while",
        "Tidy the other folder",
    ] {
        assert!(
            !screen.contains(gone),
            "{gone} should be filtered out:\n{screen}"
        );
    }
    d.press("enter", b"\r");
    let screen = d.capture("filter-kept");
    assert!(screen.contains("Ship the release"), "{screen}");
    assert!(
        !screen.contains("Fix the parser"),
        "Enter keeps the filter:\n{screen}"
    );
    d.press("ctrl+f", CTRL_F);
    d.press("escape", b"\x1b");
    let screen = d.wait_text("Fix the parser");
    d.capture("filter-cleared");
    for back in [
        "Answer my question",
        "Rest a while",
        "Ship the release",
        "Tidy the other folder",
    ] {
        assert!(screen.contains(back), "{back} should be back:\n{screen}");
    }

    // ctrl+t pins the selected session above every group and saves the pin.
    while !selected(&d.screen()).contains("Rest a while") {
        let before = d.screen();
        d.press("down", DOWN);
        assert_ne!(before, d.screen(), "the cursor stopped before Rest a while");
    }
    d.press("ctrl+t", CTRL_T);
    let screen = d.wait_for("the pinned group", |s| heading(s, "pinned").is_some());
    d.capture("pinned");
    let pinned = heading(&screen, "pinned").unwrap();
    assert_eq!(row(&screen, "Rest a while"), pinned + 1, "{screen}");
    assert!(
        pinned < row(&screen, &project.display().to_string()),
        "{screen}"
    );
    let pins: Vec<String> =
        serde_json::from_str(&d.wait_file("state/pins.json", |t| t.contains(&rest.id))).unwrap();
    assert_eq!(pins, std::slice::from_ref(&rest.id));
    d.keep("state/pins.json");

    // ctrl+p highlights the selected title in the configured colour, and again clears it.
    let plain = d.colour_of("Rest a while");
    assert_ne!(plain, Some(vt100::Color::Idx(5)), "{plain:?}");
    d.press("ctrl+p", CTRL_P);
    d.wait_for("the highlight", |_| {
        d.colour_of("Rest a while") == Some(vt100::Color::Idx(5))
    });
    d.capture("highlighted");
    assert_ne!(d.colour_of("Ship the release"), Some(vt100::Color::Idx(5)));
    d.press("ctrl+p", CTRL_P);
    d.wait_for("the highlight cleared", |_| {
        d.colour_of("Rest a while") == plain
    });

    // A second ctrl+t unpins it back into its folder.
    d.press("ctrl+t", CTRL_T);
    let screen = d.wait_for("the pin removed", |s| heading(s, "pinned").is_none());
    d.capture("unpinned");
    assert!(row(&screen, "Rest a while") > row(&screen, &project.display().to_string()));
    assert_eq!(fs::read_to_string(d.path("state/pins.json")).unwrap(), "[]");
    d.quit();
}

#[test]
fn ctrl_x_twice_removes_a_background_session_through_claude_rm() {
    let mut d = Dashboard::new("claude-remove", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    let keep = seed(&d, &project, 1, "Fix the parser", "busy", None);
    let mut gone = seed(&d, &project, 2, "Rest a while", "idle", None);
    d.start();
    d.wait_text("Rest a while");
    d.press("down", DOWN);
    let screen = d.wait_text(&format!("fixture attached to {}", gone.short));
    assert!(selected(&screen).contains("Rest a while"), "{screen}");

    // The first ctrl+x arms the removal and paints the row red; another key keeps it.
    d.press("ctrl+x", CTRL_X);
    let screen = d.wait_text("ctrl+x again to delete");
    d.capture("armed");
    assert!(screen.contains("any other key keeps it"), "{screen}");
    assert_eq!(d.colour_of("Rest a while"), Some(vt100::Color::Idx(1)));
    d.press("down", DOWN);
    d.press("up", UP);
    let screen = d.wait_for("the armed removal kept", |s| !s.contains("ctrl+x again"));
    assert!(screen.contains("Rest a while"), "{screen}");
    assert!(!calls(&d).iter().any(|c| c[0] == "rm"));

    // Twice in a row removes it through the native command, by its short id.
    d.press("ctrl+x", CTRL_X);
    d.wait_text("ctrl+x again to delete");
    d.press("ctrl+x", CTRL_X);
    let screen = d.wait_for("the row removed", |s| !s.contains("Rest a while"));
    d.capture("removed");
    assert!(screen.contains("Fix the parser"), "{screen}");
    let rm: Vec<_> = calls(&d).into_iter().filter(|c| c[0] == "rm").collect();
    assert_eq!(rm, [vec!["rm".to_owned(), gone.short.clone()]]);
    let left: Vec<_> = registry(&d)
        .iter()
        .map(|r| r["sessionId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(left, std::slice::from_ref(&keep.id));
    gone.daemon.wait().unwrap();
    // The removal holds through later refreshes: the row does not come back.
    // A session that arrives later proves a refresh ran after the removal.
    let _later = seed(&d, &project, 3, "Arrive later", "idle", None);
    d.wait_text("Arrive later");
    let screen = d.capture("after-refresh");
    assert!(!screen.contains("Rest a while"), "{screen}");
    assert!(
        screen.contains("1 working  ▇ 0 input  ▁ 1 idle"),
        "{screen}"
    );
    d.keep("home/claude-calls.jsonl");
    d.quit();
}

#[test]
fn a_launch_in_a_folder_opened_through_a_symlink_lists_under_that_folder() {
    let mut d = Dashboard::new("claude-alias", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    d.start();
    d.wait_text("✻ claude › ");
    d.typed("count the flaky tests");
    d.press("enter", b"\r");
    let screen = d.wait_text("Fixture title for the launch");
    d.capture("launched");
    assert!(
        !screen.contains("no sessions here"),
        "the open folder and the session's resolved cwd are one folder:\n{screen}"
    );
    d.quit();
}

#[test]
fn a_refused_launch_shows_its_instruction_without_moving() {
    let mut d = Dashboard::new("claude-refused-selection", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    real_project(&d);
    fs::write(d.home().join("fake-launch"), "fail").unwrap();
    d.start();
    d.wait_text("✻ claude › ");
    d.typed("count the flaky tests");
    d.press("enter", b"\r");
    let screen = d.wait_text("failed: ");
    d.capture("refused");
    assert!(selected(&screen).contains("no sessions here"), "{screen}");
    assert!(
        composer(&screen).contains("✻ claude › count the flaky tests"),
        "{screen}"
    );
    d.quit();
}

#[test]
fn a_launch_refused_for_an_untrusted_folder_asks_claude_trust_question_in_the_pane() {
    let mut d = Dashboard::new("claude-untrusted", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    real_project(&d);
    fs::write(d.home().join("fake-launch"), "untrusted").unwrap();
    d.start();
    d.wait_text("✻ claude › ");
    d.typed("count the flaky tests");
    d.press("enter", b"\r");
    let screen = d.wait_text("Quick safety check: do you trust");
    d.capture("trust-question");
    assert!(!screen.contains("failed: "), "{screen}");
    assert!(
        selected(&screen).contains("count the flaky tests"),
        "the launch row stays listed while Claude asks:\n{screen}"
    );

    // Claude's own question is answered in its own client; cones grants nothing.
    assert!(!d.home().join("trusted").exists());
    d.press("enter", b"\r");
    d.press("enter", b"\r");
    let screen = d.wait_text("fixture trusted the folder and started: count the flaky tests");
    d.capture("trusted");
    assert!(d.home().join("trusted").exists());
    let launches: Vec<_> = calls(&d)
        .into_iter()
        .filter(|c| c.last().map(String::as_str) == Some("count the flaky tests"))
        .collect();
    assert_eq!(launches.len(), 2, "{launches:?}");
    assert_eq!(launches[0][0], "--bg", "{launches:?}");
    assert!(!launches[1].contains(&"--bg".to_owned()), "{launches:?}");
    assert_eq!(launches[0][1..], launches[1][..], "{launches:?}");
    assert!(
        registry(&d).iter().any(|r| r["kind"] == "interactive"),
        "{screen}"
    );
    let debug = d.wait_file("state/tui-debug.log", |t| {
        records(t).iter().any(|r| r["event"] == "launch.applied")
    });
    let applied = records(&debug)
        .into_iter()
        .find(|r| r["event"] == "launch.applied")
        .unwrap();
    assert_eq!(applied["data"]["outcome"], "foreground_for_trust");
    d.quit();
}
