//! Claude background sessions in the live list, driven through the real dashboard.
//!
//! The `claude` on the launch path is `tests/fixtures/fake_claude_fleet.py`: it lists sessions
//! in the fixture's Claude registry behind live `/bin/sleep` stand-ins and logs every call it
//! gets to `$HOME/claude-calls.jsonl`. The flows cover a composer launch (harness cycling,
//! the placeholder becoming the discovered row, the recovery and debug records that tie the
//! launch to its native id), a refused launch that keeps the instruction, an instruction
//! shorter than `start.min_prompt` that starts nothing until it is long enough, pre-existing
//! sessions in every state grouped by folder and by state, arrows, the filter, pins and
//! highlights, `ctrl+x` twice removing a session through `claude rm`, and a wrapped draft
//! whose cursor follows a click and the arrows, which reach the list only from its edge rows,
//! and a trackpad's flood of wheel reports reaching a focused pane whole and in order within a
//! few frames, while idle viewers cost no wait.
use crate::dashboard::*;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command},
};

const DOWN: &[u8] = b"\x1b[B";
const ESC: &[u8] = b"\x1b";
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

/// Claude's daemon can list a background session before `--bg` prints the id that pairs it with
/// the launch. The list held both rows for that moment: the placeholder and the session.
#[test]
fn a_session_listed_before_its_launch_returns_is_one_row() {
    let mut d = Dashboard::new("claude-launch-early-registry", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    real_project(&d);
    fs::write(d.home().join("print-delay"), "3").unwrap();
    d.start();
    d.wait_text("claude › ");
    d.typed("count the flaky tests");
    d.press("enter", b"\r");

    // The registry lists the session while `--bg` is still running; give discovery time to see it.
    d.wait_for("the session in the registry", |_| !registry(&d).is_empty());
    std::thread::sleep(std::time::Duration::from_secs(2));
    let early = d.capture("listed-before-launch-returned");
    assert!(
        !early.contains("Fixture title for the launch"),
        "the placeholder stands in until the launch returns:\n{early}"
    );
    assert!(early.contains("count the flaky tests"), "{early}");

    d.wait_text("Fixture title for the launch");
    let launched = d.capture("launched");
    assert!(!launched.contains("count the flaky tests"), "{launched}");

    // The session's row enters with the refresh that retires the placeholder, not seconds before.
    let session = registry(&d)[0]["sessionId"].as_str().unwrap().to_owned();
    let debug = d.wait_file("state/tui-debug.log", |t| {
        records(t).iter().any(|r| r["event"] == "row.reidentified")
    });
    let rows: Vec<_> = records(&debug)
        .into_iter()
        .filter(|r| r["event"] == "row.added" || r["event"] == "row.removed")
        .collect();
    let added = rows
        .iter()
        .position(|r| r["event"] == "row.added" && r["data"]["row_id"] == session.as_str())
        .expect("the session's row");
    let removed = rows
        .iter()
        .position(|r| {
            r["event"] == "row.removed"
                && r["data"]["row_id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("starting:"))
        })
        .expect("the placeholder retired");
    let at = |i: usize| {
        chrono::DateTime::parse_from_rfc3339(rows[i]["timestamp"].as_str().unwrap()).unwrap()
    };
    assert!(
        (at(added) - at(removed)).num_milliseconds().abs() < 500,
        "{:#?}",
        &rows[..=added.max(removed)]
    );
    d.quit();
}

/// A `claude` typed in a terminal runs the user's alias, so a launch takes the variables the
/// alias sets before its command, as Bedrock setups do, but not its flags.
#[test]
fn a_launch_takes_the_variables_the_user_s_claude_alias_sets() {
    let mut d = Dashboard::new("claude-alias-env", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    real_project(&d);
    fs::write(
        d.home().join(".zshrc"),
        "alias claude='AWS_PROFILE=claude CLAUDE_CODE_USE_BEDROCK=1 CONES_ALIAS_NOTE=\"two words\" command claude --verbose'\n",
    )
    .unwrap();
    d.start();
    d.wait_text("✻ claude › ");
    d.typed("count the flaky tests");
    d.press("enter", b"\r");
    d.wait_text("Fixture title for the launch");
    let launch = calls(&d)
        .iter()
        .position(|c| c.first().map(String::as_str) == Some("--bg"))
        .expect("a --bg launch");
    assert!(
        !calls(&d)[launch].contains(&"--verbose".to_owned()),
        "the alias's flags stay out: {:?}",
        calls(&d)[launch]
    );
    let env = &records(&fs::read_to_string(d.home().join("claude-env.jsonl")).unwrap())[launch];
    assert_eq!(env["AWS_PROFILE"], "claude", "{env}");
    assert_eq!(env["CLAUDE_CODE_USE_BEDROCK"], "1", "{env}");
    assert_eq!(env["CONES_ALIAS_NOTE"], "two words", "{env}");
    d.keep("home/claude-env.jsonl");
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

/// `--bg` launches the fixture logged, by their prompts.
fn launched_prompts(d: &Dashboard) -> Vec<String> {
    calls(d)
        .into_iter()
        .filter(|c| c.first().map(String::as_str) == Some("--bg"))
        .map(|c| c.last().unwrap().clone())
        .collect()
}

#[test]
fn an_instruction_shorter_than_the_minimum_starts_nothing() {
    let mut d = Dashboard::new("claude-min-prompt", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    real_project(&d);
    d.start();
    d.wait_text("✻ claude › ");
    // The default minimum is 4 characters, as in `claude agents`; padding does not count.
    d.typed("  ok ");
    d.press("enter", b"\r");
    let screen = d.wait_text("too short, describe the task");
    d.capture("too-short");
    assert!(composer(&screen).contains("✻ claude ›   ok"), "{screen}");
    assert!(screen.contains("no sessions here"), "{screen}");
    assert!(launched_prompts(&d).is_empty());
    assert!(!d.path("state/launches.jsonl").exists());

    d.typed("go");
    d.press("enter", b"\r");
    let screen = d.wait_text("Fixture title for the launch");
    d.capture("long-enough");
    assert!(
        composer(&screen).contains("Type to start a new agent"),
        "{screen}"
    );
    assert_eq!(launched_prompts(&d), ["ok go"]);
    d.quit();

    // 0 accepts any text.
    let mut d = Dashboard::new("claude-min-prompt-off", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    real_project(&d);
    let jobs = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    fs::write(
        d.path("jobs.yaml"),
        format!("{jobs}start:\n  min_prompt: 0\n"),
    )
    .unwrap();
    d.start();
    d.wait_text("✻ claude › ");
    d.typed("x");
    d.press("enter", b"\r");
    d.wait_text("Fixture title for the launch");
    d.capture("any-length");
    assert_eq!(launched_prompts(&d), ["x"]);
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

/// The 2026-10-14 report: one Esc on a session row, with nothing to clear, closed the dashboard.
#[test]
fn esc_on_a_session_row_quits_only_when_pressed_twice() {
    let mut d = Dashboard::new("claude-esc-quit", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    let mut done = seed(&d, &project, 1, "Ship the fix", "idle", None);
    d.start();
    let screen = d.wait_text(&format!("fixture attached to {}", done.short));
    assert!(selected(&screen).contains("Ship the fix"), "{screen}");

    d.press("esc", ESC);
    let screen = d.wait_text("esc again quits");
    d.capture("armed");
    assert!(d.running(), "one esc only arms:\n{screen}");
    // Each key arms its own quit, so a ctrl+c and then an Esc keep the dashboard open.
    d.press("ctrl+c", b"\x03");
    let screen = d.wait_text("ctrl+c again quits");
    assert!(d.running(), "ctrl+c after esc arms its own quit:\n{screen}");
    d.press("esc", ESC);
    let screen = d.wait_text("esc again quits");
    assert!(d.running(), "esc after ctrl+c arms its own quit:\n{screen}");
    d.press("esc", ESC);
    d.wait_exit();
    done.daemon.kill().unwrap();
    done.daemon.wait().unwrap();
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

/// Claude keeps a session whose worktree has commits no remote holds. When a local branch already
/// holds them, as after merging the worktree into main, the delete discards the worktree; work
/// held nowhere else stays, and the refusal is shown.
#[test]
fn ctrl_x_twice_removes_a_session_whose_worktree_is_merged_and_keeps_unmerged_work() {
    let mut d = Dashboard::new("claude-remove-worktree", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    let repo = d.home().join("repo");
    let git = |dir: &Path, args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
    let (merged_tree, open_tree) = (d.home().join("merged"), d.home().join("open"));
    for (tree, branch) in [(&merged_tree, "merged"), (&open_tree, "open")] {
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                tree.to_str().unwrap(),
            ],
        );
        git(tree, &["commit", "-q", "--allow-empty", "-m", branch]);
    }
    git(&repo, &["merge", "-q", "--ff-only", "merged"]);
    let mut merged = seed(&d, &project, 1, "Merged work", "idle", None);
    let open = seed(&d, &project, 2, "Open work", "idle", None);
    let claude = d.home().join(".claude");
    for (s, tree) in [(&merged, &merged_tree), (&open, &open_tree)] {
        fs::write(
            claude.join(format!("rm-worktree-{}", s.short)),
            tree.to_str().unwrap(),
        )
        .unwrap();
    }
    d.start();
    d.wait_text("Open work");
    d.press("down", DOWN);
    let screen = d.wait_text(&format!("fixture attached to {}", open.short));
    assert!(selected(&screen).contains("Open work"), "{screen}");
    d.press("ctrl+x", CTRL_X);
    d.wait_text("ctrl+x again to delete");
    d.press("ctrl+x", CTRL_X);
    let screen = d.wait_text("delete failed: Open work: worktree has unmerged work");
    d.capture("unmerged-kept");
    assert!(selected(&screen).contains("Open work"), "{screen}");
    let rm: Vec<_> = calls(&d).into_iter().filter(|c| c[0] == "rm").collect();
    assert_eq!(rm, [vec!["rm".to_owned(), open.short.clone()]]);

    d.press("up", UP);
    d.wait_for("Merged work selected", |s| {
        selected(s).contains("Merged work")
    });
    d.press("ctrl+x", CTRL_X);
    d.wait_text("ctrl+x again to delete");
    d.press("ctrl+x", CTRL_X);
    let screen = d.wait_for("the row removed", |s| !s.contains("Merged work"));
    d.capture("merged-removed");
    assert!(screen.contains("Open work"), "{screen}");
    let rm: Vec<_> = calls(&d)
        .into_iter()
        .filter(|c| c[0] == "rm")
        .skip(1)
        .collect();
    let head = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&merged_tree)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let token = format!("{}@0123abcd", head.trim());
    assert_eq!(
        rm,
        [
            vec!["rm".to_owned(), merged.short.clone()],
            vec![
                "rm".to_owned(),
                merged.short.clone(),
                "--discard-unpushed".to_owned(),
                token
            ],
        ]
    );
    merged.daemon.wait().unwrap();
    d.keep("home/claude-calls.jsonl");
    d.quit();
}

/// A session outlives its folder when an agent starts one in a scratch directory and deletes
/// it. Claude cannot wake it, so enter names the missing folder instead of a spawn error.
#[test]
fn entering_a_session_whose_folder_was_deleted_names_the_missing_folder() {
    let mut d = Dashboard::new("claude-folder-gone", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let scratch = real_project(&d).join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    let _gone = seed(&d, &scratch, 1, "Probe the scratch folder", "idle", None);
    fs::remove_dir(&scratch).unwrap();
    d.start();
    d.wait_text("Probe the scratch folder");
    d.press("down", DOWN);
    d.press("enter", b"\r");
    let screen = d.wait_text("attach failed: session directory no longer exists");
    d.capture("refused");
    assert!(
        selected(&screen).contains("Probe the scratch folder"),
        "{screen}"
    );
    assert!(
        !calls(&d).iter().any(|c| c[0] == "attach"),
        "{:?}",
        calls(&d)
    );
    d.quit();
}

/// Claude's fullscreen renderer hides the terminal cursor and draws its own caret. Left and
/// Tab still return from its empty composer; with a draft they reach Claude. Claude itself
/// treats Left on an empty prompt as "background this session", so a missed return forks it.
#[test]
fn left_and_tab_return_from_claude_s_fullscreen_composer_only_when_it_is_empty() {
    let mut d = Dashboard::new("claude-fullscreen-left", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    fs::write(d.home().join("fake-attach-fullscreen"), "").unwrap();
    let project = real_project(&d);
    let only = seed(&d, &project, 1, "Rest a while", "idle", None);
    d.start();
    d.wait_text(&format!("fixture attached to {}", only.short));
    let pane = "tab back · ctrl+\\ full screen";
    let draft = "ctrl+z back · ctrl+\\ full screen";

    d.press("right", b"\x1b[C");
    let screen = d.wait_text(pane);
    d.capture("empty-composer");
    assert!(screen.contains("❯"), "{screen}");
    d.typed("d");
    d.wait_text("❯ d");
    d.press("left", b"\x1b[D");
    let screen = d.wait_text(draft);
    d.capture("draft-keeps-left");
    assert!(!screen.contains(pane), "{screen}");
    d.press("backspace", b"\x7f");
    d.wait_text(pane);
    d.press("left", b"\x1b[D");
    let screen = d.wait_for("the list focused", |s| {
        !s.contains(pane) && !s.contains(draft)
    });
    d.capture("left-returns");
    assert!(selected(&screen).contains("Rest a while"), "{screen}");

    d.press("right", b"\x1b[C");
    d.wait_text(pane);
    d.press("tab", b"\t");
    d.wait_for("the list focused", |s| !s.contains(pane));
    let input: Vec<Value> =
        records(&fs::read_to_string(d.home().join("attach-input.jsonl")).unwrap());
    assert_eq!(input, [json!("d"), json!("\u{1b}[D"), json!("\u{7f}")]);
    d.keep("home/attach-input.jsonl");
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

    // With no Claude config to add the folder to, Claude's own client answers its question.
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

/// Idan's layout: `~/cyera` the main checkout, `~/cyera-cost-vis` a sibling worktree of it,
/// and sessions in a folder of that worktree. A worktree with two sessions heads them inside
/// the main checkout's folder. Opening the folder used to report "added" and draw nothing,
/// leaving the cursor on `+ add folder`; now the opened folder heads its own session, Enter
/// selects it, and the worktree's one remaining session joins the main checkout's own.
/// A main checkout (the project), a sibling worktree `cost-vis` and its folder
/// `research/cost_visibility`, with a session in each: "Rank tenants" in the checkout,
/// "Cost visibility dashboard" in the folder and "Autoscaling plan" at the worktree's top.
struct Worktree {
    d: Dashboard,
    main: std::path::PathBuf,
    tree: std::path::PathBuf,
    folder: std::path::PathBuf,
    _sessions: Vec<Seeded>,
}

fn worktree(test: &str) -> Worktree {
    let d = Dashboard::new(test, &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let main = real_project(&d);
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(["-c", "user.name=e2e", "-c", "user.email=e2e@example.com"])
            .args(args)
            .current_dir(&main)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&["init", "-q"]);
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    let tree = d.path("cost-vis");
    git(&["worktree", "add", "-q", tree.to_str().unwrap()]);
    let tree = tree.canonicalize().unwrap();
    let folder = tree.join("research/cost_visibility");
    fs::create_dir_all(&folder).unwrap();
    let _sessions = vec![
        seed(&d, &main, 1, "Rank tenants", "idle", None),
        seed(&d, &folder, 2, "Cost visibility dashboard", "idle", None),
        seed(&d, &tree, 3, "Autoscaling plan", "idle", None),
    ];
    Worktree {
        d,
        main,
        tree,
        folder,
        _sessions,
    }
}

/// Type `path` into `+ add folder` and press Enter.
fn add_folder(d: &mut Dashboard, path: &str) {
    for _ in 0..12 {
        if selected(&d.screen()).contains("+ add folder") {
            break;
        }
        d.press("down", DOWN);
    }
    d.wait_for("+ add folder selected", |s| {
        selected(s).contains("+ add folder")
    });
    d.typed(path);
    d.press("enter", b"\r");
}

#[test]
fn a_folder_opened_in_a_worktree_heads_its_own_sessions() {
    let Worktree {
        mut d,
        main,
        tree,
        folder,
        _sessions,
    } = worktree("claude-worktree-folder");
    d.start();

    let main_name = main.display().to_string();
    let tree_heading = format!("  ⑂ {}", tree.display());
    let folder_name = folder.display().to_string();
    let folder_heading = format!("  ⑂ {folder_name}");
    let screen = d.wait_text("Autoscaling plan");
    d.capture("worktree-nested");
    let at = heading(&screen, &main_name).unwrap_or_else(|| panic!("{screen}"));
    let nested = heading(&screen, &tree_heading).unwrap_or_else(|| panic!("{screen}"));
    let order = [
        at,
        row(&screen, "Rank tenants"),
        nested,
        row(&screen, "Cost visibility dashboard"),
        row(&screen, "Autoscaling plan"),
    ];
    assert!(order.is_sorted(), "{order:?}\n{screen}");
    assert_eq!(
        column(&screen, "Autoscaling plan"),
        column(&screen, "Rank tenants") + 2,
        "a session under a worktree heading is indented:\n{screen}"
    );

    add_folder(&mut d, &folder_name);
    let screen = d.wait_for("the folder's heading", |s| {
        heading(s, &folder_heading).is_some()
    });
    d.capture("opened");
    let opened = heading(&screen, &folder_heading).unwrap();
    let order = [
        heading(&screen, &main_name).unwrap_or_else(|| panic!("{screen}")),
        row(&screen, "Rank tenants"),
        row(&screen, "Autoscaling plan"),
        opened,
        row(&screen, "Cost visibility dashboard"),
    ];
    assert!(order.is_sorted(), "{order:?}\n{screen}");
    assert_eq!(order[4], opened + 1, "{screen}");
    assert!(
        heading(&screen, &tree_heading).is_none(),
        "one session nobody opened needs no heading:\n{screen}"
    );
    assert!(
        selected(&screen).contains("Cost visibility dashboard"),
        "Enter selects the opened folder's session:\n{screen}"
    );
    assert!(!screen.contains("no sessions here"), "{screen}");
    d.wait_file("state/open-folders.json", |t| t.contains(&folder_name));
    d.keep("state/open-folders.json");
    d.quit();
}

/// Where `text` starts on its line, in characters.
fn column(screen: &str, text: &str) -> usize {
    let line = line_of(screen, text);
    line[..line.find(text).unwrap()].chars().count()
}

/// With `worktrees: flat` the worktree's sessions sit among the main checkout's own, marked
/// `⑂`. Opening a folder in it still heads that folder's session, and the status line names
/// the setting that heads every worktree.
#[test]
fn a_flat_list_heads_only_the_worktree_folder_you_open() {
    let Worktree {
        mut d,
        main,
        folder,
        _sessions,
        ..
    } = worktree("claude-worktree-flat");
    let yaml = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    fs::write(d.path("jobs.yaml"), format!("{yaml}worktrees: flat\n")).unwrap();
    d.start();

    let main_name = main.display().to_string();
    let folder_name = folder.display().to_string();
    let folder_heading = format!("  ⑂ {folder_name}");
    let screen = d.wait_text("Autoscaling plan");
    d.capture("flat");
    assert!(
        !screen.contains("  ⑂ $ROOT") && !screen.contains("  ⑂ /"),
        "{screen}"
    );
    let at = heading(&screen, &main_name).unwrap_or_else(|| panic!("{screen}"));
    for title in [
        "Rank tenants",
        "Cost visibility dashboard",
        "Autoscaling plan",
    ] {
        assert!(row(&screen, title) > at, "{title}:\n{screen}");
    }
    assert_eq!(
        column(&screen, "⑂ Autoscaling plan"),
        column(&screen, "Rank tenants"),
        "{screen}"
    );

    add_folder(&mut d, &folder_name);
    let screen = d.wait_text("worktrees: nested heads every worktree");
    d.capture("opened");
    let opened = heading(&screen, &folder_heading).unwrap_or_else(|| panic!("{screen}"));
    assert_eq!(
        row(&screen, "Cost visibility dashboard"),
        opened + 1,
        "{screen}"
    );
    assert!(row(&screen, "Autoscaling plan") < opened, "{screen}");
    assert!(
        selected(&screen).contains("Cost visibility dashboard"),
        "{screen}"
    );
    d.quit();
}

/// A past conversation resumed from history in a worktree that no live session uses. Its
/// row stands in before Claude reports it, and the dashboard used to resolve worktrees
/// before adding that row: the worktree went unrecognised, opened as a folder of its own and
/// stayed open. Now the row sits among the repository's own, marked `⑂`, and no folder
/// opens.
#[test]
fn a_session_resumed_from_history_in_a_worktree_opens_no_folder() {
    let Worktree {
        mut d,
        main,
        _sessions,
        ..
    } = worktree("claude-worktree-resume");
    let old = d.path("old-tree");
    let out = Command::new("git")
        .args(["worktree", "add", "-q", old.to_str().unwrap()])
        .current_dir(&main)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let old = old.canonicalize().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let cwd = old.display().to_string();
    let key: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let transcript = d.home().join(".claude/projects").join(key);
    fs::create_dir_all(&transcript).unwrap();
    fs::write(
        transcript.join(format!("{id}.jsonl")),
        format!(
            "{}\n{}\n",
            json!({"sessionId": id, "type": "user", "cwd": cwd,
                   "timestamp": "2026-10-01T08:00:00.000Z",
                   "message": {"role": "user", "content": "please trim the old tree"}}),
            json!({"type": "ai-title", "sessionId": id, "aiTitle": "Old tree work"}),
        ),
    )
    .unwrap();
    // Claude reports the resumed session late, so its row stands in meanwhile.
    fs::write(d.home().join("registry-delay"), "3").unwrap();
    // Flat, so any heading of the worktree's own is a folder that opened.
    let yaml = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    fs::write(d.path("jobs.yaml"), format!("{yaml}worktrees: flat\n")).unwrap();
    d.start();
    d.wait_text("Autoscaling plan");

    d.press("ctrl+h", b"\x08");
    d.wait_text("Old tree work");
    for _ in 0..12 {
        if selected(&d.screen()).contains("Old tree work") {
            break;
        }
        d.press("down", DOWN);
    }
    d.wait_for("the history row selected", |s| {
        selected(s).contains("Old tree work")
    });
    d.press("enter", b"\r");
    let old_heading = format!("  ⑂ {cwd}");
    let screen = d.wait_for("the resumed row in the live list", |s| {
        s.contains("fixture attached to")
            && (s.contains("⑂ Old tree work") || heading(s, &old_heading).is_some())
    });
    d.capture("resumed");
    assert!(heading(&screen, &old_heading).is_none(), "{screen}");
    let at = heading(&screen, &main.display().to_string()).unwrap_or_else(|| panic!("{screen}"));
    assert!(row(&screen, "⑂ Old tree work") > at, "{screen}");
    let open = fs::read_to_string(d.path("state/open-folders.json")).unwrap_or_default();
    assert!(!open.contains(&cwd), "{open}");
    d.keep("state/open-folders.json");
    d.quit();
}

/// Every way the dashboard starts a Claude session follows the skip setting: a composer
/// launch, the coordinator, a resume from history and a native fork. Each built its own
/// command once, and only the composer passed the flag, so the others came up in the mode
/// Claude's own settings name.
#[test]
fn every_claude_start_follows_the_skip_permissions_setting() {
    const FLAG: &str = "--dangerously-skip-permissions";
    const CTRL_D: &[u8] = b"\x04";
    const CTRL_Y: &[u8] = b"\x19";
    for skip in [true, false] {
        let mut d = Dashboard::new(&format!("claude-skip-{skip}"), &["claude"]);
        d.install("claude", "fake_claude_fleet.py");
        if !skip {
            let yaml = fs::read_to_string(d.path("jobs.yaml")).unwrap();
            let yaml = yaml.replace("defaults:\n", "defaults:\n  skip_permissions: false\n");
            fs::write(d.path("jobs.yaml"), yaml).unwrap();
        }
        let project = real_project(&d);
        let _live = seed(&d, &project, 1, "Fix the parser", "idle", None);
        let old = uuid::Uuid::new_v4().to_string();
        let cwd = project.display().to_string();
        let key: String = cwd
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let dir = d.home().join(".claude/projects").join(key);
        fs::write(
            dir.join(format!("{old}.jsonl")),
            format!(
                "{}\n{}\n",
                json!({"sessionId": old, "type": "user", "cwd": cwd,
                       "timestamp": "2026-10-01T08:00:00.000Z",
                       "message": {"role": "user", "content": "please trim the tree"}}),
                json!({"type": "ai-title", "sessionId": old, "aiTitle": "Old tree work"}),
            ),
        )
        .unwrap();
        d.start();
        d.wait_text("Fix the parser");
        let started = |d: &Dashboard, what: &str, is: &dyn Fn(&[String]) -> bool| {
            d.wait_for(what, |_| calls(d).iter().any(|c| is(c)));
            calls(d).into_iter().find(|c| is(c)).unwrap()
        };

        d.typed("count the flaky tests");
        d.press("enter", b"\r");
        let launch = started(&d, "the launch", &|c| {
            c.iter().any(|a| a == "count the flaky tests")
        });
        d.wait_text("Fixture title for the launch");

        d.press("ctrl+d", CTRL_D);
        let coordinator = started(&d, "the coordinator", &|c| {
            c.iter().any(|a| a == "/cones:start-coordinator")
        });

        d.press("ctrl+h", b"\x08");
        d.wait_text("Old tree work");
        for _ in 0..12 {
            if selected(&d.screen()).contains("Old tree work") {
                break;
            }
            d.press("down", DOWN);
        }
        d.wait_for("the history row selected", |s| {
            selected(s).contains("Old tree work")
        });
        d.press("enter", b"\r");
        let resume = started(&d, "the resume", &|c| {
            c.iter().any(|a| a == "--resume") && !c.iter().any(|a| a == "--fork-session")
        });
        // The resumed session's viewer takes focus; back on its row, fork it.
        d.wait_text("ctrl+z back");
        d.press("ctrl+z", b"\x1a");
        d.wait_for("the list focused", |s| !s.contains("ctrl+z back"));
        // The fixture resumes under a new id, and that row can land under the cursor after
        // ctrl+z; fork the old conversation once it is listed.
        d.wait_for("the resumed copy listed", |s| {
            s.matches("Fixture title for the launch").count() >= 3
        });
        for _ in 0..12 {
            if selected(&d.screen()).contains("Old tree work") {
                break;
            }
            d.press("down", DOWN);
        }
        d.wait_for("the old conversation selected", |s| {
            selected(s).contains("Old tree work")
        });
        d.press("ctrl+y", CTRL_Y);
        d.wait_text("fork to");
        d.press("enter", b"\r");
        let fork = started(&d, "the fork", &|c| c.iter().any(|a| a == "--fork-session"));
        d.capture("started");
        d.keep("home/claude-calls.jsonl");
        d.quit();

        for (what, argv) in [
            ("launch", &launch),
            ("coordinator", &coordinator),
            ("fork", &fork),
            ("resume", &resume),
        ] {
            let at = argv.iter().position(|a| a == FLAG);
            let end = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
            assert_eq!(at.is_some(), skip, "{what}: {argv:?}");
            assert!(
                at.is_none_or(|i| i < end),
                "{what}: flag after --: {argv:?}"
            );
        }
        assert_eq!(
            fork[..3],
            [
                "--resume".to_owned(),
                old.clone(),
                "--fork-session".to_owned()
            ],
            "{fork:?}"
        );
        assert_eq!(
            resume[..3],
            ["--bg", "--resume", old.as_str()],
            "{resume:?}"
        );
    }
}

/// `tasks` shows the background tasks Claude lists in a job record, by kind; `children`
/// counts processes under the session. An interactive session writes no job record, so its
/// tasks are unknown. `commands`, the column's old name, still selects `children`.
#[test]
fn tasks_shows_what_claude_reports_and_children_counts_processes() {
    let mut d = Dashboard::new("claude-tasks-column", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    let busy = seed(&d, &project, 1, "Run the gate", "busy", Some("working"));
    let _quiet = seed(&d, &project, 2, "Just talking", "idle", None);
    fs::write(
        d.home()
            .join(".claude/jobs")
            .join(&busy.short)
            .join("state.json"),
        json!({"state": "working", "tempo": "active", "sessionId": busy.id,
               "inFlight": {"tasks": 3, "queued": 0, "kinds": ["local_bash", "monitor"]},
               "fan": [{"id": "b1", "kind": "shell"}, {"id": "m1", "kind": "monitor"},
                       {"id": "b2", "kind": "shell"}]})
        .to_string(),
    )
    .unwrap();
    let yaml = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    fs::write(
        d.path("jobs.yaml"),
        format!("{yaml}columns: [state, tasks, commands]\n"),
    )
    .unwrap();
    d.start();
    let screen = d.wait_text("2 shells 1 monitor");
    d.capture("tasks");
    let header = line_of(&screen, "title");
    assert!(
        header.contains("tasks") && header.contains("children"),
        "{header}"
    );
    let cells = |title: &str| -> Vec<String> {
        let line = line_of(&screen, title);
        let list = line.split('│').next().unwrap();
        list[list.find(title).unwrap() + title.len()..]
            .split("  ")
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(String::from)
            .collect()
    };
    assert_eq!(
        cells("Run the gate"),
        ["2 shells 1 monitor", "0"],
        "{screen}"
    );
    assert_eq!(cells("Just talking"), ["-", "0"], "{screen}");
    d.quit();
}

/// Claude's registry `shell` is a turn that ended with a background shell still running: not
/// working, not idle. The record is Claude Code 2.1.302's for that case. The row and the
/// summary say `shell` behind a blue `❯`, the `tasks` column names what it waits on, and the
/// ended turn is unread like any other, since its reply may ask something.
#[test]
fn a_turn_left_running_a_background_shell_shows_shell() {
    let mut d = Dashboard::new("claude-shell-state", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    let _busy = seed(&d, &project, 1, "Fix the parser", "busy", None);
    let waiting = seed(
        &d,
        &project,
        2,
        "Wait for the notes",
        "busy",
        Some("working"),
    );
    let yaml = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    fs::write(
        d.path("jobs.yaml"),
        format!("{yaml}columns: [state, tasks]\n"),
    )
    .unwrap();
    d.start();
    d.wait_text("2 working");
    fs::write(
        d.home()
            .join(".claude/jobs")
            .join(&waiting.short)
            .join("state.json"),
        json!({"state": "working", "tempo": "idle", "sessionId": waiting.id,
               "inFlight": {"tasks": 1, "queued": 0, "kinds": ["local_bash"], "drainableMonitors": 0},
               "fan": [{"id": "b1", "kind": "shell", "label": "sleep 15", "startedAt": 1}]})
        .to_string(),
    )
    .unwrap();
    let entry = d
        .home()
        .join(".claude/sessions")
        .join(format!("{}.json", waiting.daemon.id()));
    let mut record: Value = serde_json::from_slice(&fs::read(&entry).unwrap()).unwrap();
    record["status"] = "shell".into();
    fs::write(&entry, record.to_string()).unwrap();
    let screen = d.wait_text("✉ Wait for the notes");
    d.capture("shell");
    assert!(
        screen.contains("1 working  ❯ 1 shell  ▇ 0 input"),
        "{screen}"
    );
    let line = line_of(&screen, "Wait for the notes");
    let list = line.split('│').next().unwrap();
    let cells: Vec<&str> = list
        .trim_start_matches('▌')
        .split("  ")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect();
    assert_eq!(
        cells,
        ["❯", "✻", "shell", "✉ Wait for the notes", "1 shell"],
        "{screen}"
    );
    assert!(
        line_of(&screen, "Fix the parser").contains("working"),
        "{screen}"
    );
    d.quit();
}

/// `plan` names the cloud whose response ids a session carries, Bedrock's `msg_bdrk_` and
/// Vertex's `msg_vrtx_`, and for a first-party response the signed-in account's plan from
/// `.claude.json`, past a leftover one that names no account. A session with no response yet
/// shows `-`.
#[test]
fn plan_shows_the_cloud_or_the_signed_in_account_plan() {
    let mut d = Dashboard::new("claude-plan-column", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    let mut seeded = Vec::new();
    for (minute, title, id) in [
        (1, "On bedrock", Some("msg_bdrk_abc")),
        (2, "On vertex", Some("msg_vrtx_abc")),
        (3, "On the account", Some("msg_01abc")),
        (4, "Not answered", None),
    ] {
        let s = seed(&d, &project, minute, title, "idle", None);
        if let Some(message) = id {
            let transcript = fs::read_dir(d.home().join(".claude/projects"))
                .unwrap()
                .map(|e| e.unwrap().path().join(format!("{}.jsonl", s.id)))
                .find(|p| p.is_file())
                .unwrap();
            let reply = json!({"type": "assistant", "sessionId": s.id,
                "timestamp": format!("2026-10-01T09:{minute:02}:30.000Z"),
                "message": {"id": message, "model": "claude-opus-5-5", "role": "assistant",
                    "usage": {"input_tokens": 1, "output_tokens": 1},
                    "content": [{"type": "text", "text": "done"}]}});
            let mut text = fs::read_to_string(&transcript).unwrap();
            text.push_str(&format!("{reply}\n"));
            fs::write(&transcript, text).unwrap();
        }
        seeded.push(s);
    }
    fs::write(
        d.home().join(".claude.json"),
        json!({"oauthAccount": {"organizationType": "claude_max"}}).to_string(),
    )
    .unwrap();
    // A leftover config inside the default folder names no account and must not hide it.
    fs::write(
        d.home().join(".claude/.claude.json"),
        json!({"userID": "abc"}).to_string(),
    )
    .unwrap();
    let yaml = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    fs::write(
        d.path("jobs.yaml"),
        format!("{yaml}columns: [state, plan]\n"),
    )
    .unwrap();
    d.start();
    let screen = d.wait_text("bedrock");
    d.capture("plan");
    assert!(line_of(&screen, "title").contains("plan"), "{screen}");
    let plan = |title: &str| -> String {
        let line = line_of(&screen, title);
        let list = line.split('│').next().unwrap();
        list.split("  ")
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .last()
            .unwrap()
            .to_owned()
    };
    assert_eq!(plan("On bedrock"), "bedrock", "{screen}");
    assert_eq!(plan("On vertex"), "vertex", "{screen}");
    assert_eq!(plan("On the account"), "max", "{screen}");
    assert_eq!(plan("Not answered"), "-", "{screen}");
    d.quit();
}

/// A left press on the 0-based screen cell where `text` starts, as an SGR mouse report.
fn click_on(d: &mut Dashboard, screen: &str, text: &str) {
    let (row, column) = screen
        .lines()
        .enumerate()
        .find_map(|(r, l)| l.find(text).map(|b| (r, l[..b].chars().count())))
        .unwrap_or_else(|| panic!("{text:?} is not on screen:\n{screen}"));
    let report = format!("\x1b[<0;{};{}M", column + 1, row + 1);
    d.press(&format!("click {text:?}"), report.as_bytes());
}

#[test]
fn a_click_in_the_draft_moves_the_composer_cursor_there() {
    let mut d = Dashboard::new("claude-composer-click", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    d.start();
    d.wait_text("✻ claude › ");
    // Sixty words wrap the draft over several rows of the list's half of the frame.
    let draft: Vec<String> = (1..=60).map(|n| format!("w{n:02}")).collect();
    d.typed(&draft.join(" "));
    let screen = d.wait_text("w60");
    assert!(
        !line_of(&screen, "w60").contains("w01"),
        "the draft did not wrap:\n{screen}"
    );
    d.capture("wrapped-draft");

    // On a character of the first row, and of the wrapped row: typing lands before it.
    click_on(&mut d, &screen, "w07");
    d.typed("A");
    let screen = d.wait_text("w06 Aw07 w08");
    click_on(&mut d, &screen, "w55");
    d.typed("B");
    let screen = d.wait_text("w54 Bw55 w56");
    // Left of the text, on the harness name, is the start of the draft.
    click_on(&mut d, &screen, "claude › ");
    d.typed("C");
    let screen = d.wait_text("claude › Cw01 w02");
    // Past the end of the last row, still inside the box, is the end of the draft.
    let (row, column) = screen
        .lines()
        .enumerate()
        .find_map(|(r, l)| l.find("w60").map(|b| (r, b + 10)))
        .unwrap();
    let report = format!("\x1b[<0;{};{}M", column + 1, row + 1);
    d.press("click past the end", report.as_bytes());
    d.typed("D");
    let screen = d.wait_text("w59 w60D");
    d.capture("edited-by-clicks");
    assert!(screen.contains("w06 Aw07 w08"), "{screen}");
    assert!(screen.contains("w54 Bw55 w56"), "{screen}");
    assert!(screen.contains("claude › Cw01 w02"), "{screen}");
    assert!(
        screen.contains("no sessions here"),
        "a click started something:\n{screen}"
    );
    d.quit();
}

#[test]
fn arrows_walk_the_rows_of_a_wrapped_draft_before_they_reach_the_list() {
    let mut d = Dashboard::new("claude-composer-rows", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    d.start();
    d.wait_text("✻ claude › ");
    // Sixty words fill four rows; the cursor ends on the last one.
    let draft: Vec<String> = (1..=60).map(|n| format!("w{n:02}")).collect();
    d.typed(&draft.join(" "));
    let screen = d.wait_text("w60");
    assert!(selected(&screen).contains("no sessions here"), "{screen}");

    // Up from the last row moves the cursor a row up and leaves the list alone.
    d.press("up", UP);
    d.typed("Q");
    let screen = d.wait_text("Q");
    assert!(line_of(&screen, "w40").contains('Q'), "{screen}");
    assert!(selected(&screen).contains("no sessions here"), "{screen}");
    d.press("up", UP);
    d.press("up", UP);
    d.typed("R");
    let screen = d.wait_text("R");
    assert!(line_of(&screen, "w05").contains('R'), "{screen}");
    assert!(selected(&screen).contains("no sessions here"), "{screen}");
    d.capture("cursor-on-first-row");

    // Up on the first row belongs to the list. The cursor stays where it was, so typing
    // still lands there.
    d.press("up", UP);
    let screen = d.wait_for("the selection to leave its row", |s| {
        !s.contains("▌ no sessions here")
    });
    d.capture("list-moved");
    d.typed("S");
    let screen_typed = d.wait_text("RS");
    assert!(
        line_of(&screen_typed, "w05").contains("RS"),
        "{screen_typed}"
    );
    // Down from the first row goes back into the draft, not down the list.
    d.press("down", DOWN);
    d.typed("T");
    let after = d.wait_text("T");
    assert!(line_of(&after, "w20").contains('T'), "{after}");
    assert!(!after.contains("▌ no sessions here"), "{after}");
    assert_eq!(
        screen.lines().position(|l| l.contains('▌')),
        after.lines().position(|l| l.contains('▌')),
        "down moved the list:\n{after}"
    );
    d.capture("back-in-the-draft");
    d.quit();
}

/// How many frames followed input, from the debug log's timing summaries, and the median
/// pump of a viewer with no output, in milliseconds, from the traced samples.
fn input_frames_and_pump_ms(log: &str) -> (u64, f64) {
    let (mut frames, mut pumps) = (0, Vec::new());
    for record in records(log) {
        let data = &record["data"];
        if record["event"] == "timing.summary" {
            frames += data["phases"]["input_to_draw"]["count"]
                .as_u64()
                .unwrap_or(0);
        } else if record["event"] == "timing"
            && data["phase"] == "viewer_pump"
            && data["output"] == false
        {
            pumps.push(data["duration_ms"].as_f64().unwrap());
        }
    }
    assert!(!pumps.is_empty(), "no idle viewer pump was traced");
    pumps.sort_by(f64::total_cmp);
    (frames, pumps[pumps.len() / 2])
}

#[test]
fn a_wheel_flood_reaches_the_pane_whole_in_a_few_frames() {
    const REPORTS: usize = 600;
    let mut d = Dashboard::new("claude-wheel-flood", &["claude"]);
    d.trace = true;
    d.install("claude", "fake_claude_fleet.py");
    fs::write(d.home().join("fake-attach-fullscreen"), "").unwrap();
    fs::write(d.home().join("fake-attach-mouse"), "").unwrap();
    let project = real_project(&d);
    let seeded: Vec<Seeded> = ["Rest a while", "Rest some more", "Rest again"]
        .iter()
        .enumerate()
        .map(|(i, title)| seed(&d, &project, i as u32 + 1, title, "idle", None))
        .collect();
    d.start();
    d.wait_text("fixture attached to");
    d.press("right", b"\x1b[C");
    d.wait_text("tab back · ctrl+\\ full screen");

    // A trackpad flick: hundreds of wheel reports over the pane, faster than any frame.
    d.burst("wheel flood", &b"\x1b[<64;100;10M".repeat(REPORTS));
    let forwarded = |text: &str| -> String {
        records(text)
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    };
    // A report split across two reads is dropped whole; every other one reaches the client.
    let split = || {
        fs::read_to_string(d.path("state/tui-debug.log"))
            .unwrap_or_default()
            .matches("input.split_mouse_report")
            .count()
    };
    let input = d.wait_file("home/attach-input.jsonl", |t| {
        forwarded(t).matches("\x1b[<64;").count() + split() >= REPORTS
    });
    d.keep("home/attach-input.jsonl");
    // In order and with nothing else among them.
    let input = forwarded(&input);
    let report = input.split_inclusive('M').next().unwrap().to_owned();
    assert!(report.starts_with("\x1b[<64;"), "{report:?}");
    assert_eq!(input, report.repeat(REPORTS - split()));
    d.press("tab", b"\t");
    d.quit();

    let log = fs::read_to_string(d.path("state/tui-debug.log")).unwrap();
    let (frames, pump_ms) = input_frames_and_pump_ms(&log);
    // One frame per batch of queued wheel reports, not one per report.
    assert!(
        frames < REPORTS as u64 / 4,
        "{frames} frames followed input"
    );
    // An idle viewer's pump returns at once instead of waiting for output that never comes.
    // A pump that waits sleeps at least PUMP_WAIT, 1ms, so the median is clear of runner
    // noise that moves a mean.
    assert!(
        pump_ms < 1.0,
        "the median idle viewer pump took {pump_ms:.2}ms"
    );
    for mut s in seeded {
        s.daemon.kill().unwrap();
        s.daemon.wait().unwrap();
    }
}

#[test]
fn left_unfolds_every_detail_of_a_session_under_its_row_and_folds_it_again() {
    let mut d = Dashboard::new("claude-details", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    let mut fix = seed(&d, &project, 1, "Fix the parser", "busy", None);
    let mut rest = seed(&d, &project, 2, "Rest a while", "idle", None);
    // A narrow pane leaves the list room for several detail columns.
    let yaml = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    fs::write(d.path("jobs.yaml"), format!("{yaml}pane:\n  ratio: 30\n")).unwrap();
    d.start();
    let screen = d.wait_text(&format!("fixture attached to {}", fix.short));
    assert!(selected(&screen).contains("Fix the parser"), "{screen}");
    assert!(
        !screen.contains(&fix.id),
        "folded rows show no id:\n{screen}"
    );

    d.press("left", b"\x1b[D");
    let screen = d.wait_text(&fix.id);
    d.capture("unfolded");
    let lines: Vec<&str> = screen.lines().collect();
    let (title, next) = (row(&screen, "Fix the parser"), row(&screen, "Rest a while"));
    let details = lines[title + 1..next].join("\n");
    for (key, value) in [
        ("id", fix.id.clone()),
        ("pid", fix.daemon.id().to_string()),
        ("harness", "claude bg".into()),
        ("state", "working".into()),
    ] {
        assert!(
            details.lines().any(|l| l
                .split('│')
                .next()
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>()
                .windows(2 + value.matches(' ').count())
                .any(|w| w[0] == key && w[1..].join(" ") == value)),
            "{key} {value} is not under the row:\n{screen}"
        );
    }
    for key in [
        "model",
        "effort",
        "context",
        "tokens",
        "cost",
        "folder",
        "branch",
        "bypass",
        "plan",
        "cpu",
        "memory",
        "children",
        "tasks",
        "activity",
        "started",
        "transcript",
        "last reply",
    ] {
        assert!(
            details.lines().any(|l| l
                .split_whitespace()
                .any(|w| w == key.split(' ').next().unwrap())),
            "{key} is not under the row:\n{screen}"
        );
    }
    // Each group names itself once, ahead of its first short value; values too wide to share
    // a line, here the start time, follow on their own.
    for (group, first) in [
        ("agent", "harness"),
        ("usage", "tokens"),
        ("time", "age"),
        ("process", "pid"),
        ("source", "forked"),
        ("reply", "last"),
    ] {
        assert!(
            details.lines().any(|l| l
                .split_whitespace()
                .collect::<Vec<_>>()
                .windows(2)
                .any(|w| w == [group, first])),
            "no {group} group led by {first}:\n{details}"
        );
    }
    // The start time takes its own line so the rest pack three to a line.
    assert!(
        details
            .lines()
            .any(|l| l.contains("harness") && l.contains(" state ") && l.contains(" model ")),
        "the list's room packs three details to a line:\n{details}"
    );
    // The second column starts at the same place in every group.
    let starts: Vec<_> = [("harness", "state"), ("tokens", "context"), ("pid", "cpu")]
        .map(|(first, second)| {
            let line = details
                .lines()
                .find(|l| l.contains(first))
                .unwrap_or_else(|| panic!("no {first} line:\n{details}"));
            line.find(&format!(" {second} "))
                .map(|i| line[..i].chars().count())
                .unwrap_or_else(|| panic!("{second} does not follow {first}:\n{details}"))
        })
        .to_vec();
    assert!(
        starts.windows(2).all(|w| w[0] == w[1]),
        "detail columns do not line up ({starts:?}):\n{details}"
    );
    let style = |text: &str| {
        let c = d.cell_of(text).unwrap();
        (c.bold(), c.dim())
    };
    assert_eq!(
        [style("agent"), style("harness"), style("claude bg")],
        [(true, false), (false, true), (false, false)],
        "group names are bold, keys dim and values at full brightness"
    );

    // Moving folds the details and still moves: down lands on the next session.
    d.press("down", DOWN);
    let screen = d.wait_for("the details folded", |s| !s.contains(&fix.id));
    assert!(selected(&screen).contains("Rest a while"), "{screen}");
    d.capture("folded");
    assert!(
        row(&screen, "Fix the parser") + 1 == row(&screen, "Rest a while"),
        "{screen}"
    );

    // Left again folds them in place.
    d.press("up", UP);
    d.wait_text(&format!("fixture attached to {}", fix.short));
    d.press("left", b"\x1b[D");
    d.wait_text(&fix.id);
    d.press("left", b"\x1b[D");
    let screen = d.wait_for("left folded the details", |s| !s.contains(&fix.id));
    assert!(selected(&screen).contains("Fix the parser"), "{screen}");
    d.quit();
    for s in [&mut fix, &mut rest] {
        s.daemon.kill().unwrap();
        s.daemon.wait().unwrap();
    }
}

#[test]
fn a_launch_into_an_untrusted_folder_trusts_that_folder_alone_and_starts_in_the_background() {
    let mut d = Dashboard::new("claude-trusts", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    let config = d.home().join(".claude/.claude.json");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let before = serde_json::json!({
        "numStartups": 3,
        "projects": {
            "/elsewhere": {"hasTrustDialogAccepted": false, "allowedTools": []},
            project.to_str().unwrap(): {"lastCost": 0.5},
        },
    });
    fs::write(&config, before.to_string()).unwrap();
    fs::write(d.home().join("fake-launch"), "untrusted").unwrap();
    d.start();
    d.wait_text("✻ claude › ");
    d.typed("count the flaky tests");
    d.press("enter", b"\r");
    let screen = d.wait_file("state/tui-debug.log", |t| {
        records(t).iter().any(|r| r["event"] == "launch.applied")
    });
    let applied = records(&screen)
        .into_iter()
        .find(|r| r["event"] == "launch.applied")
        .unwrap();
    assert_eq!(
        applied["data"]["outcome"], "awaiting_discovery",
        "{applied}"
    );

    // Only the launched folder's entry gained the accepted dialog; everything else is as it was.
    let after: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    let mut expected = before;
    expected["projects"][project.to_str().unwrap()]["hasTrustDialogAccepted"] = true.into();
    assert_eq!(after, expected);
    let launches: Vec<_> = calls(&d)
        .into_iter()
        .filter(|c| c.last().map(String::as_str) == Some("count the flaky tests"))
        .collect();
    assert_eq!(launches.len(), 1, "{launches:?}");
    assert_eq!(launches[0][0], "--bg", "{launches:?}");
    assert!(
        !d.home().join("trusted").exists(),
        "Claude's own question was asked"
    );
    d.quit();
}
