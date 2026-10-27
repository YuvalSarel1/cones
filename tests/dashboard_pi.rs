//! A terminal-client harness in the native viewer, driven through the real dashboard.
//!
//! The client is `tests/fixtures/fake_pi.py` on the fixture HOME's launch path, never the
//! machine's pi: a python3 stand-in that draws pi's bordered editor, echoes keys into it,
//! publishes the private report cones' extension would and records its argv and every
//! submitted line under `$HOME/pi-fixture/<pid>.json`. The flows cover choosing pi with
//! shift+tab, launching it with an instruction from the composer, the pane showing the
//! client's own screen, typing into it, returning with ctrl+z and Left, Enter re-entering the
//! same process, ctrl+\ for fullscreen and for the pane, a pi outliving the dashboard and
//! keeping its draft, and ctrl+x twice stopping it until the process is gone.
use crate::dashboard::*;
use serde_json::Value;
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

const LIST_FOOTER: &str = "Session     enter return · ctrl+x stop · ctrl+n rename · ctrl+t pin";
const PANE_FOOTER: &str = "tab back · ctrl+\\ full screen";
/// With a draft in pi's editor Tab and Left stay native, and the footer stops offering them.
const DRAFT_FOOTER: &str = "ctrl+z back · ctrl+\\ full screen";
const FULLSCREEN_STRIP: &str = "tab back · ctrl+\\ split";
const EMPTY_COMPOSER: &str = "π pi › Type to start a new agent…";

/// A dashboard with only pi enabled and the fixture client installed as `pi`. The open
/// folder is written canonically, as `+ add folder` would, so the launch lands under its heading.
fn pi_dashboard(test: &str) -> Dashboard {
    let d = Dashboard::new(test, &["pi"]);
    d.install("pi", "fake_pi.py");
    fs::create_dir_all(d.home().join(".pi")).unwrap();
    let project = d.project().canonicalize().unwrap();
    d.open_folders(&[project]);
    d
}

fn json_files(dir: &Path) -> Vec<Value> {
    let mut out: Vec<_> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .map(|p| serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap())
        .collect();
    out.sort_by_key(|v: &Value| v["pid"].as_u64().or(v["session"]["pid"].as_u64()));
    out
}

/// The host records cones keeps for the processes it owns.
fn hosted(d: &Dashboard) -> Vec<Value> {
    json_files(&d.path("state/terminals"))
}

/// What each fixture pi that ever started recorded about itself.
fn clients(d: &Dashboard) -> Vec<Value> {
    json_files(&d.home().join("pi-fixture"))
}

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

/// The selected row in the list: the one carrying the `▌` mark.
fn selected(screen: &str) -> String {
    screen
        .lines()
        .find(|l| l.contains("▌ "))
        .unwrap_or_default()
        .to_owned()
}

/// The list line for the session titled `title`.
fn row<'a>(screen: &'a str, title: &str) -> Option<&'a str> {
    screen.lines().find(|l| {
        let words: Vec<&str> = l.split_whitespace().collect();
        words.contains(&"π") && words.contains(&"idle") && l.contains(&format!("  {title}  "))
    })
}

/// Type `prompt` into the pi composer unless it is drafted already, and submit it; wait for the client's first screen and
/// its idle row, and return the pid cones holds after checking it is this fixture's.
fn launch(d: &mut Dashboard, prompt: &str) -> u32 {
    if !d.screen().contains(&format!("π pi › {prompt}")) {
        d.typed(prompt);
        d.wait_text(&format!("π pi › {prompt}"));
    }
    d.press("enter", b"\r");
    d.wait_text(&format!("│ pi fixture ready: {prompt}"));
    d.wait_for("the idle pi row", |s| row(s, prompt).is_some());
    wait_until("the host record", || hosted(d).len() == 1);
    let record = hosted(d).remove(0);
    let pid = record["session"]["pid"].as_u64().unwrap() as u32;
    let project = d.project().canonicalize().unwrap();
    assert_eq!(record["session"]["harness"], "pi");
    assert_eq!(record["session"]["title"], prompt);
    assert_eq!(record["session"]["cwd"], project.to_str().unwrap());
    assert_eq!(
        record["what"],
        format!("pi in {}", project.display()),
        "{record}"
    );
    wait_until("the fixture's own record", || {
        clients(d).iter().any(|c| c["pid"] == pid)
    });
    let client = clients(d).into_iter().find(|c| c["pid"] == pid).unwrap();
    let argv: Vec<&str> = client["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(
        argv[0],
        d.home().join(".local/bin/pi").to_str().unwrap(),
        "the fixture on the fixture HOME's launch path ran, not the machine's pi"
    );
    assert_eq!(argv[1], "--extension", "cones loads its private reporter");
    assert!(argv[2].ends_with("/report.mjs"), "{argv:?}");
    assert_eq!(&argv[3..], ["--", prompt], "the instruction is pi's prompt");
    assert_eq!(client["cwd"], project.to_str().unwrap());
    assert!(client["report"].as_str().is_some(), "{client}");
    let ps = std::process::Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let command = String::from_utf8_lossy(&ps.stdout);
    assert!(
        command.contains(&format!("Python {} --extension", argv[0])),
        "pid {pid} is the fixture: {command}"
    );
    pid
}

/// Launch, type into the viewer, return with ctrl+z, and Enter on the row reaches the same
/// process: no second client starts, and the pane still holds what the first one drew.
#[test]
fn a_composer_pi_runs_in_its_viewer_and_enter_on_its_row_returns_to_the_same_process() {
    let mut d = pi_dashboard("pi_viewer_returns");
    d.start();
    let screen = d.capture("first-frame");
    assert!(screen.contains(EMPTY_COMPOSER), "{screen}");
    assert!(screen.contains("no sessions here"), "{screen}");

    d.press("shift+tab", b"\x1b[Z");
    let screen = d.wait_text("terminal (zsh) › Type a command, or Enter to open");
    assert!(!screen.contains("π pi ›"), "{screen}");
    d.capture("shift-tab-terminal");
    d.press("shift+tab", b"\x1b[Z");
    d.wait_text(EMPTY_COMPOSER);
    let screen = d.capture("shift-tab-pi");
    assert!(!screen.contains("terminal (zsh) ›"), "{screen}");

    d.typed("fix the tests");
    let screen = d.wait_text("enter start pi");
    assert!(screen.contains("π pi › fix the tests"), "{screen}");
    d.capture("drafted");
    let pid = launch(&mut d, "fix the tests");
    let screen = d.wait_text(LIST_FOOTER);
    d.capture("launched");
    // Starting leaves the list selected; the composer is empty again and the summary counts
    // the idle state pi's report gave.
    assert!(screen.contains(EMPTY_COMPOSER), "{screen}");
    assert!(screen.contains("▁ 1 idle"), "{screen}");
    let line = row(&screen, "fix the tests").unwrap();
    assert!(line.starts_with("▌"), "the new row is selected: {line}");
    assert_eq!(
        screen
            .lines()
            .filter(|l| row(l, "fix the tests").is_some())
            .count(),
        1,
        "discovery and the launch are one row: {screen}"
    );
    assert!(!screen.contains("no sessions here"), "{screen}");

    d.press("enter", b"\r");
    d.wait_text(PANE_FOOTER);
    d.typed("hello pi");
    let screen = d.wait_text("│  hello pi");
    assert!(
        screen.contains(EMPTY_COMPOSER),
        "keys went to pi, not the composer: {screen}"
    );
    d.capture("typing-in-viewer");
    d.press("enter", b"\r");
    let screen = d.wait_text("│ pi heard: hello pi");
    d.capture("submitted-in-viewer");
    assert!(
        !screen.contains("│  hello pi"),
        "pi took the line: {screen}"
    );
    wait_until("the client to record the line", || {
        clients(&d)[0]["submitted"] == serde_json::json!(["hello pi"])
    });

    d.press("ctrl+z", b"\x1a");
    let screen = d.wait_text(LIST_FOOTER);
    d.capture("back-to-list");
    assert!(row(&screen, "fix the tests").unwrap().starts_with("▌"));
    assert!(
        screen.contains("│ pi heard: hello pi"),
        "the pane keeps the selected session's screen: {screen}"
    );

    d.press("enter", b"\r");
    let screen = d.wait_text(PANE_FOOTER);
    d.capture("reentered");
    assert!(screen.contains("│ pi heard: hello pi"), "{screen}");
    d.typed("again");
    d.press("enter", b"\r");
    d.wait_text("│ pi heard: again");
    wait_until("the same client to take the second line", || {
        clients(&d)[0]["submitted"] == serde_json::json!(["hello pi", "again"])
    });
    let started = clients(&d);
    assert_eq!(started.len(), 1, "Enter started no second pi: {started:?}");
    assert_eq!(started[0]["pid"], pid);
    let records = hosted(&d);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["session"]["pid"], pid);
    assert!(alive(pid));
    d.capture("same-process");

    let launches = d.wait_file("state/launches.jsonl", |t| !t.is_empty());
    let lines: Vec<Value> = launches
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "one launch, however often it was entered");
    let project = d.project().canonicalize().unwrap();
    assert_eq!(lines[0]["event"], "launch.submitted");
    assert_eq!(lines[0]["level"], "recovery");
    assert_eq!(lines[0]["v"], 1);
    assert_eq!(lines[0]["data"]["harness"], "pi");
    assert_eq!(lines[0]["data"]["prompt"], "fix the tests");
    assert_eq!(lines[0]["data"]["cwd"], project.to_str().unwrap());
    assert!(
        lines[0]["data"]["operation_id"]
            .as_str()
            .unwrap()
            .starts_with("pi:start:"),
        "{launches}"
    );
    d.keep("state/launches.jsonl");
    let record = fs::read_dir(d.path("state/terminals"))
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().is_some_and(|x| x == "json"))
        .unwrap()
        .file_name();
    d.keep(&format!("state/terminals/{}", record.to_str().unwrap()));
    d.keep(&format!("home/pi-fixture/{pid}.json"));

    d.press("ctrl+z", b"\x1a");
    d.wait_text(LIST_FOOTER);
    d.quit();
}

/// ctrl+\ switches a focused viewer between split and fullscreen, and the client is resized
/// to each. Left and Tab return only from pi's empty editor. From the list, ctrl+\ hides
/// and shows the pane.
#[test]
fn ctrl_backslash_switches_fullscreen_and_the_pane_and_left_or_tab_return_from_an_empty_editor() {
    let mut d = pi_dashboard("pi_viewer_layout");
    d.start();
    let pid = launch(&mut d, "lay it out");
    d.press("enter", b"\r");
    d.wait_text(PANE_FOOTER);

    d.press("ctrl+\\", b"\x1c");
    let screen = d.wait_text(FULLSCREEN_STRIP);
    d.capture("fullscreen");
    let lines: Vec<&str> = screen.lines().collect();
    assert_eq!(lines[0], "pi fixture ready: lay it out", "{screen}");
    let strip = lines[ROWS as usize - 1];
    assert!(strip.starts_with("▲ cones · lay it out · "), "{strip}");
    assert!(strip.contains("▁ 1 idle"), "{strip}");
    let full = "─".repeat(COLS as usize);
    assert_eq!(
        lines.iter().filter(|l| **l == full).count(),
        2,
        "pi redrew its editor at the full width: {screen}"
    );
    assert!(!screen.contains("jobs   config   help"), "{screen}");

    d.press("ctrl+\\", b"\x1c");
    let screen = d.wait_text(PANE_FOOTER);
    d.capture("split-again");
    assert!(
        screen.contains("│ pi fixture ready: lay it out"),
        "{screen}"
    );
    assert!(screen.contains("jobs   config   help"), "{screen}");

    // A draft keeps Left and Tab in pi.
    d.typed("draft");
    d.wait_text("│  draft");
    d.press("left", b"\x1b[D");
    d.press("tab", b"\t");
    let screen = d.capture("draft-keeps-keys");
    assert!(screen.contains(DRAFT_FOOTER), "{screen}");
    assert!(!screen.contains(PANE_FOOTER), "{screen}");
    assert!(screen.contains("│  draft"), "{screen}");
    for _ in 0.."draft".len() {
        d.press("backspace", b"\x7f");
    }
    d.wait_for("the empty editor", |s| !s.contains("│  draft"));
    d.press("tab", b"\t");
    let screen = d.wait_text(LIST_FOOTER);
    d.capture("tab-returns");
    assert!(row(&screen, "lay it out").unwrap().starts_with("▌"));
    d.press("enter", b"\r");
    d.wait_text(PANE_FOOTER);
    d.press("left", b"\x1b[D");
    d.wait_text(LIST_FOOTER);
    d.capture("left-returns");

    d.press("ctrl+\\", b"\x1c");
    let screen = d.wait_for("the pane to close", |s| !s.contains("│ pi fixture"));
    d.capture("pane-off");
    assert!(
        screen.lines().any(|l| l.contains("last reply")),
        "the list takes the pane's columns: {screen}"
    );
    assert!(row(&screen, "lay it out").unwrap().starts_with("▌"));
    d.press("ctrl+\\", b"\x1c");
    let screen = d.wait_text("│ pi fixture ready: lay it out");
    d.capture("pane-on");
    assert!(
        !screen.lines().any(|l| l.contains("last reply")),
        "{screen}"
    );

    assert_eq!(clients(&d).len(), 1, "no layout change starts a client");
    assert!(alive(pid));
    d.quit();
}

/// The pi a dashboard started outlives it. A new dashboard on the same state lists it, and
/// Enter reconnects to that process with the draft it was holding.
#[test]
fn a_pi_outlives_the_dashboard_and_enter_reconnects_to_it_with_its_draft() {
    let mut d = pi_dashboard("pi_viewer_reconnects");
    d.start();
    let pid = launch(&mut d, "survive me");
    d.press("enter", b"\r");
    d.wait_text(PANE_FOOTER);
    d.typed("half typed");
    d.wait_text("│  half typed");
    d.press("ctrl+z", b"\x1a");
    d.wait_text(LIST_FOOTER);
    d.capture("before-quit");
    d.quit();
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(pid), "quitting the dashboard left pi running");
    assert_eq!(hosted(&d).len(), 1);

    d.start();
    let screen = d.wait_for("the surviving row", |s| row(s, "survive me").is_some());
    d.capture("restarted");
    assert!(screen.contains("▁ 1 idle"), "{screen}");
    d.wait_for("the row to be selected", |s| {
        row(s, "survive me").is_some_and(|l| l.starts_with("▌"))
    });
    d.press("enter", b"\r");
    let screen = d.wait_text("│  half typed");
    d.capture("reconnected-with-draft");
    assert!(screen.contains(DRAFT_FOOTER), "{screen}");
    assert!(
        screen.contains("│ pi fixture ready: survive me"),
        "{screen}"
    );
    let started = clients(&d);
    assert_eq!(started.len(), 1, "reconnecting started no client");
    assert_eq!(started[0]["pid"], pid);
    assert_eq!(hosted(&d)[0]["session"]["pid"], pid);
    d.keep(&format!("home/pi-fixture/{pid}.json"));
    d.press("ctrl+z", b"\x1a");
    d.wait_text(LIST_FOOTER);
    d.quit();
}

/// ctrl+x arms the stop, another key keeps the session, and ctrl+x twice ends the native
/// process and its host record; the row leaves the list.
#[test]
fn ctrl_x_twice_stops_the_pi_and_its_process_exits() {
    let mut d = pi_dashboard("pi_viewer_stops");
    d.start();
    let pid = launch(&mut d, "stop me");
    d.wait_text(LIST_FOOTER);

    d.press("ctrl+x", b"\x18");
    let screen = d.wait_text("ctrl+x again to stop · any other key keeps it");
    d.capture("armed");
    assert!(row(&screen, "stop me").is_some());
    d.press("esc", b"\x1b");
    let screen = d.wait_text("Session     kept");
    d.capture("kept");
    assert!(row(&screen, "stop me").is_some());
    assert!(alive(pid));
    assert_eq!(hosted(&d).len(), 1);

    d.press("ctrl+x", b"\x18");
    d.wait_text("ctrl+x again to stop");
    d.press("ctrl+x", b"\x18");
    wait_until("the fixture pi to exit", || !alive(pid));
    wait_until("the host record to go", || hosted(&d).is_empty());
    let screen = d.wait_for("the row to leave", |s| {
        row(s, "stop me").is_none() && s.contains("no sessions here")
    });
    d.capture("stopped");
    assert!(screen.contains("▁ 0 idle"), "{screen}");
    assert!(!screen.contains("stop me"), "{screen}");
    assert!(!screen.contains("│ pi fixture"), "{screen}");
    assert_eq!(clients(&d).len(), 1, "stopping started nothing");
    d.keep("state/launches.jsonl");
    d.quit();
}

/// A session an agent starts with `cones launch` is listed beneath that agent, though it works
/// in another folder, badged ⚙, and names that folder after its title since the heading no
/// longer does. Its folder does not open, so it leaves no empty folder behind when it ends, and
/// an agent started from its row starts in the launcher's folder.
/// `launched_under_parent: false` lists it in its own folder again.
#[test]
fn a_launch_from_an_agent_is_listed_beneath_it() {
    let mut d = pi_dashboard("pi_launch_under_parent");
    let project = d.project().canonicalize().unwrap();
    let other = d.path("other");
    fs::create_dir_all(&other).unwrap();
    let other = other.canonicalize().unwrap();
    d.open_folders(std::slice::from_ref(&project));
    let launch = |dir: &Path, prompt: &str, env: &[(&str, &str)]| {
        let out = d.cli(
            &[
                "launch",
                "--dir",
                dir.to_str().unwrap(),
                "--harness",
                "pi",
                prompt,
            ],
            env,
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    // A fork recorded before launches joined forks in one file is kept beside them.
    let old_fork = serde_json::json!([{"harness": "pi", "home": "/old", "cwd": "/old",
        "parent": "old-parent", "child": "old-child"}]);
    fs::write(d.path("state/forks.json"), old_fork.to_string()).unwrap();
    let parent = launch(&project, "lead the work", &[]);
    // The launcher is found by the session id its harness exports, as a Codex thread's is.
    let child = launch(&other, "write the docs", &[("CODEX_THREAD_ID", &parent)]);
    let relations_path = d.path("state/relations.json");
    let relations = || -> Value {
        serde_json::from_str(&fs::read_to_string(&relations_path).unwrap()).unwrap()
    };
    let recorded = relations();
    assert_eq!(recorded.as_array().unwrap().len(), 2, "{recorded}");
    assert_eq!(recorded[0]["kind"], "fork");
    assert_eq!(recorded[0]["child"], "old-child");
    let pid = |id: &str| id.strip_prefix("pi-").unwrap().parse::<u32>().unwrap();
    let launched = &recorded[1];
    assert_eq!(launched["kind"], "launch");
    assert_eq!(launched["harness"], "pi");
    // Each end is named by the conversation id pi reported, not the process id it is listed
    // under while it has no session file.
    for end in ["parent", "child"] {
        let id = launched[end].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(id).is_ok(), "{launched}");
    }
    assert_ne!(launched["parent"], launched["child"]);
    assert_eq!(launched["parent_pid"], pid(&parent));
    assert_eq!(launched["child_pid"], pid(&child));
    assert_eq!(launched["cwd"], other.to_str().unwrap());

    // The launch finished a turn nobody has read yet.
    let unread = serde_json::json!({format!("pi:{child}:"): {
        "observation": {"key": format!("pi:{child}:"), "state": "idle", "revision": ""},
        "unread": true, "touched": 0,
    }});
    fs::write(d.path("state/attention.json"), unread.to_string()).unwrap();
    d.start();
    // Its mark takes the indentation, so the row keeps its place beneath its launcher.
    let elsewhere = "  ✉ ↳ ⚙ write the docs · in other  ";
    let screen = d.wait_for("the launch beneath its launcher", |s| {
        s.contains(elsewhere) && row(s, "lead the work").is_some()
    });
    d.capture("beneath-launcher");
    // The badge says another agent started it, in the dashboard's orange; the folder is a
    // dim aside to the title.
    assert_eq!(d.colour_of("⚙"), Some(vt100::Color::Idx(208)));
    assert!(!d.cell_of("write the docs").unwrap().dim());
    assert!(d.cell_of("· in other").unwrap().dim());
    let lines: Vec<&str> = screen.lines().collect();
    let at = |text: &str| lines.iter().position(|l| l.contains(text));
    assert_eq!(
        at(elsewhere),
        at("lead the work").map(|i| i + 1),
        "{screen}"
    );
    assert_eq!(
        at(&project.display().to_string()),
        at("lead the work").map(|i| i - 1),
        "{screen}"
    );
    // The folder it works in heads no row of its own, empty or not.
    assert_eq!(at(&other.display().to_string()), None, "{screen}");
    assert!(!screen.contains("no sessions here"), "{screen}");
    // An agent started from the launch's row starts in the folder it is listed under.
    for _ in 0..4 {
        if selected(&d.screen()).contains("write the docs") {
            break;
        }
        d.press("down", b"\x1b[B");
    }
    assert!(
        selected(&d.screen()).contains("write the docs"),
        "{}",
        d.screen()
    );
    let prompt = "start beside the launcher";
    d.typed(prompt);
    d.wait_text(&format!("π pi › {prompt}"));
    d.press("enter", b"\r");
    d.wait_text(&format!("│ pi fixture ready: {prompt}"));
    let started = || {
        clients(&d)
            .into_iter()
            .find(|c| c["argv"].as_array().unwrap().last() == Some(&prompt.into()))
    };
    wait_until("the new client's record", || started().is_some());
    assert_eq!(started().unwrap()["cwd"], project.to_str().unwrap());
    d.press("ctrl+z", b"\x1a");
    d.quit();
    let open = fs::read_to_string(d.path("state/open-folders.json")).unwrap();
    assert!(!open.contains(other.to_str().unwrap()), "{open}");

    // `cones launch` records the launcher just after the launch starts, so a dashboard can
    // list the launch in its own folder first. Moved beneath its launcher, it closes that
    // folder again.
    let recorded = fs::read_to_string(&relations_path).unwrap();
    fs::remove_file(&relations_path).unwrap();
    d.start();
    d.wait_for("the launch in its own folder", |s| {
        s.contains(&other.display().to_string()) && s.contains("write the docs")
    });
    fs::write(&relations_path, recorded).unwrap();
    d.wait_for("the launch moved beneath its launcher", |s| {
        s.contains(elsewhere) && !s.contains(&other.display().to_string())
    });
    d.quit();
    let open = fs::read_to_string(d.path("state/open-folders.json")).unwrap();
    assert!(!open.contains(other.to_str().unwrap()), "{open}");

    // pi is listed by its process until it writes a session file, and then by its own id.
    // A record naming either session by an id it is no longer listed under still finds both
    // by their pids.
    let mut stale = relations();
    stale[1]["parent"] = "named-later".into();
    stale[1]["child"] = "also-named-later".into();
    fs::write(&relations_path, stale.to_string()).unwrap();
    d.start();
    d.wait_for("the launch beneath its renamed launcher", |s| {
        s.contains(elsewhere) && row(s, "lead the work").is_some()
    });
    d.capture("renamed-launcher");
    d.quit();

    let jobs = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    fs::write(
        d.path("jobs.yaml"),
        format!("{jobs}launched_under_parent: false\n"),
    )
    .unwrap();
    d.start();
    let screen = d.wait_for("the launch in its own folder", |s| {
        s.contains("   ✉ write the docs  ") && row(s, "lead the work").is_some()
    });
    d.capture("own-folder");
    assert!(!screen.contains("↳"), "{screen}");
    assert!(!screen.contains("· in "), "{screen}");
    let lines: Vec<&str> = screen.lines().collect();
    let at = |text: &str| lines.iter().position(|l| l.contains(text)).unwrap();
    assert_eq!(
        at(&other.display().to_string()),
        at("write the docs") - 1,
        "{screen}"
    );
}
