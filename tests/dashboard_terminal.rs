//! Shell terminals started from the dashboard composer, driven through the real binary.
//!
//! Each flow opens zsh from a folder row with the fixture's own `.zshrc` (a fixed prompt, no
//! user configuration) and asserts on the dashboard screen, the files the shell writes in the
//! fixture folders and the host records cones keeps under `state/terminals/`. Covered: Enter
//! on an empty folder opens a shell in that folder, ctrl+z returns to a `zsh` row, Enter on
//! that row reconnects to the same process and `exit` removes the row; a drafted command
//! opens a second shell, the first ctrl+x only arms the stop and the second stops that shell
//! alone, its process and record gone; a composer command runs in the selected folder, keeps
//! its draft apart from the agent instruction and leaves the shell open; a draft typed in the
//! native shell survives leaving, returning and a dashboard restart, and Tab returns from the
//! shell only once its command line is empty. No harness or model is started.
use crate::dashboard::*;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const ENTER: &[u8] = b"\r";
const CTRL_Z: &[u8] = b"\x1a";
const CTRL_X: &[u8] = b"\x18";
const UP: &[u8] = b"\x1b[A";
const DOWN: &[u8] = b"\x1b[B";
const SHIFT_TAB: &[u8] = b"\x1b[Z";
const PROMPT: &str = "zsh-ready>";
/// The width of the list; the pane starts after its border.
const LIST: usize = 70;
const SHELL_ROW: &str = "–  $  -      zsh";

/// A fixture whose pinned folders are `folders` (relative to the root, created here), in
/// canonical form so the dashboard's `/private/tmp` cwd and the pins name the same folder.
fn fixture(test: &str, enabled: &[&str], folders: &[&str]) -> (Dashboard, Vec<PathBuf>) {
    let d = Dashboard::new(test, enabled);
    fs::write(d.home().join(".zshrc"), format!("PROMPT='{PROMPT} '\n")).unwrap();
    let dirs: Vec<PathBuf> = folders
        .iter()
        .map(|rel| {
            fs::create_dir_all(d.path(rel)).unwrap();
            d.path(rel).canonicalize().unwrap()
        })
        .collect();
    let pins: String = dirs.iter().map(|p| format!("{}\n", p.display())).collect();
    fs::write(d.path("state/folders"), pins).unwrap();
    (d, dirs)
}

/// The list half of the screen, without the pane.
fn list(screen: &str) -> String {
    screen
        .lines()
        .map(|line| {
            line.chars()
                .take(LIST)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The pane half of the screen.
fn pane(screen: &str) -> String {
    screen
        .lines()
        .map(|line| line.chars().skip(LIST + 1).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn shell_rows(screen: &str) -> usize {
    list(screen).matches(SHELL_ROW).count()
}

/// The list lines from the heading for `dir` to the next blank line.
fn group(screen: &str, dir: &Path) -> Vec<String> {
    let heading = dir.display().to_string();
    let list = list(screen);
    let mut lines = list.lines().skip_while(|line| line.trim() != heading);
    assert!(lines.next().is_some(), "no heading {heading}:\n{screen}");
    lines
        .take_while(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

/// The host records cones keeps for its terminals.
fn records(d: &Dashboard) -> Vec<Value> {
    let mut found: Vec<Value> = fs::read_dir(d.path("state/terminals"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_str(&fs::read_to_string(e.path()).ok()?).ok())
        .collect();
    found.sort_by_key(|r| r["session"]["started"].as_str().unwrap_or("").to_owned());
    found
}

fn pid(record: &Value) -> i32 {
    record["session"]["pid"].as_i64().expect("record has a pid") as i32
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn until(what: &str, d: &Dashboard, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; screen:\n{}",
            d.screen()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Assert every field of a terminal's host record.
fn assert_record(record: &Value, dir: &Path) {
    let session = &record["session"];
    assert_eq!(session["harness"], "terminal", "{record}");
    assert_eq!(session["title"], "zsh", "{record}");
    assert_eq!(record["what"], "zsh", "{record}");
    assert_eq!(session["state"], "-", "{record}");
    assert_eq!(
        Path::new(session["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        dir,
        "{record}"
    );
    assert!(
        session["session_id"]
            .as_str()
            .unwrap()
            .starts_with("terminal:"),
        "{record}"
    );
    let id = record["id"].as_str().unwrap();
    assert!(uuid::Uuid::parse_str(id).is_ok(), "{record}");
    assert!(record["socket"].as_str().unwrap().ends_with(id), "{record}");
    assert!(alive(pid(record)), "{record}");
}

/// The list is back in front: the composer hint offers a new terminal and Tab enters the pane.
fn wait_list(d: &Dashboard) -> String {
    d.wait_for("the list", |s| {
        s.contains("New terminal  enter start terminal") && s.contains("tab pane")
    })
}

/// A shell has focus: its footer offers the way back.
fn wait_shell(d: &Dashboard) -> String {
    d.wait_for("a focused shell", |s| {
        s.contains("ctrl+z back · ctrl+c interrupt") && pane(s).contains(PROMPT)
    })
}

#[test]
fn a_shell_from_the_folder_row_works_in_the_folder_and_returns_to_its_row() {
    let (mut d, dirs) = fixture("terminal-folder-shell", &[], &["project"]);
    let project = &dirs[0];
    d.start();
    let screen = d.capture("empty-folder");
    assert_eq!(group(&screen, project), ["▌ no sessions here"], "{screen}");
    assert!(
        screen.contains("terminal (zsh) › Type a command, or Enter to open"),
        "{screen}"
    );
    assert!(screen.contains("enter start terminal"), "{screen}");
    assert!(records(&d).is_empty());

    d.press("enter", ENTER);
    wait_shell(&d);
    d.typed("print -r -- $PWD > made\r");
    let made = d.wait_file("project/made", |s| !s.is_empty());
    assert_eq!(made, format!("{}\n", project.display()));
    let screen = d.wait_for("the command and a new prompt", |s| {
        pane(s).contains(&format!("{PROMPT} print -r -- $PWD > made\n{PROMPT}"))
    });
    d.capture("shell-ran-command");
    assert_eq!(shell_rows(&screen), 1, "{screen}");
    let opened = records(&d);
    assert_eq!(opened.len(), 1);
    assert_record(&opened[0], project);
    let shell = pid(&opened[0]);

    d.press("ctrl+z", CTRL_Z);
    let screen = wait_list(&d);
    d.capture("list-with-shell-row");
    let rows = group(&screen, project);
    assert_eq!(rows.len(), 1, "{screen}");
    assert!(rows[0].starts_with(&format!("▌ {SHELL_ROW}")), "{screen}");
    assert!(
        list(&screen).contains("state  title  context  activity          model  age"),
        "{screen}"
    );
    assert!(!screen.contains("no sessions here"), "{screen}");
    assert!(alive(shell), "returning to the list keeps the shell");

    // Enter on the row with an empty composer reconnects to the same process.
    d.press("enter", ENTER);
    wait_shell(&d);
    d.typed("print -r -- $$ > pid\r");
    let reconnected = d.wait_file("project/pid", |s| s.ends_with('\n'));
    assert_eq!(reconnected.trim(), shell.to_string());
    assert_eq!(records(&d).len(), 1, "reconnecting opens no new shell");

    d.typed("exit\r");
    let screen = d.wait_for("the shell's row to leave", |s| {
        s.contains("no sessions here") && shell_rows(s) == 0
    });
    d.capture("shell-exited");
    let rows = group(&screen, project);
    assert_eq!(rows.len(), 1, "{screen}");
    assert_eq!(
        rows[0].trim_start_matches(['▌', ' ']),
        "no sessions here",
        "{screen}"
    );
    assert!(screen.contains("back from zsh"), "{screen}");
    until("the exited shell", &d, || {
        !alive(shell) && records(&d).is_empty()
    });
    d.keep("state/folders");
    d.keep("project/made");
    d.quit();
}

#[test]
fn a_second_terminal_opens_beside_the_first_and_ctrl_x_twice_stops_only_one() {
    let (mut d, dirs) = fixture("terminal-two-and-stop", &[], &["project"]);
    let project = &dirs[0];
    d.start();
    d.press("enter", ENTER);
    wait_shell(&d);
    d.press("ctrl+z", CTRL_Z);
    wait_list(&d);

    // A drafted command opens a new shell even with a shell row selected.
    d.typed("print -r -- second > second");
    d.press("enter", ENTER);
    d.wait_file("project/second", |s| s == "second\n");
    wait_shell(&d);
    d.press("ctrl+z", CTRL_Z);
    let screen = d.wait_for("two shell rows", |s| shell_rows(s) == 2);
    d.capture("two-shells");
    let rows = group(&screen, project);
    assert_eq!(rows.len(), 2, "{screen}");
    assert!(
        rows.iter()
            .all(|r| r.ends_with(SHELL_ROW) || r.contains(SHELL_ROW)),
        "{screen}"
    );
    // The new shell stays selected and the pane follows the selection.
    let selected = rows.iter().position(|r| r.starts_with('▌')).unwrap();
    assert!(pane(&screen).contains("print -r -- second"), "{screen}");
    let (away, back) = if selected == 0 {
        (DOWN, UP)
    } else {
        (UP, DOWN)
    };
    d.press("to the first shell", away);
    let screen = d.wait_for("the first shell's pane", |s| {
        pane(s).contains(PROMPT) && !pane(s).contains("second")
    });
    d.capture("first-shell-selected");
    assert!(
        group(&screen, project)[1 - selected].starts_with('▌'),
        "{screen}"
    );
    d.press("back to the new shell", back);
    d.wait_for("the new shell's pane", |s| {
        pane(s).contains("print -r -- second")
    });
    let both = records(&d);
    assert_eq!(both.len(), 2);
    for record in &both {
        assert_record(record, project);
    }
    assert_ne!(both[0]["id"], both[1]["id"]);
    assert_ne!(
        both[0]["session"]["session_id"],
        both[1]["session"]["session_id"]
    );
    let (kept, stopped) = (pid(&both[0]), pid(&both[1]));
    assert_ne!(kept, stopped);

    d.press("ctrl+x", CTRL_X);
    let screen = d.wait_text("ctrl+x again to stop · any other key keeps it");
    d.capture("stop-armed");
    assert_eq!(
        shell_rows(&screen),
        2,
        "the first ctrl+x only arms:\n{screen}"
    );
    assert!(alive(stopped));
    d.press("ctrl+x", CTRL_X);
    d.wait_for("one shell row", |s| shell_rows(s) == 1);
    until("the stopped shell to exit", &d, || {
        !alive(stopped) && records(&d).len() == 1
    });
    let screen = d.capture("one-stopped");
    let rows = group(&screen, project);
    assert_eq!(rows.len(), 1, "{screen}");
    assert!(
        rows[0].ends_with(SHELL_ROW) || rows[0].contains(SHELL_ROW),
        "{screen}"
    );
    let left = records(&d);
    assert_eq!(left[0]["id"], both[0]["id"]);
    assert!(alive(kept), "stopping one shell keeps the other");

    // The remaining shell still takes input in the same process. Where the cursor lands after
    // the stop depends on the unsorted terminal order (see terminal_rows_list_oldest_first).
    for _ in 0..3 {
        if d.screen()
            .lines()
            .any(|l| l.starts_with('▌') && l.contains(SHELL_ROW))
        {
            break;
        }
        d.press("up", b"\x1b[A");
    }
    d.press("enter", ENTER);
    wait_shell(&d);
    d.typed("print -r -- $$ > kept\r");
    let written = d.wait_file("project/kept", |s| s.ends_with('\n'));
    assert_eq!(written.trim(), kept.to_string());
    d.press("ctrl+z", CTRL_Z);
    wait_list(&d);
    d.press("ctrl+x", CTRL_X);
    d.press("ctrl+x", CTRL_X);
    let screen = d.wait_for("no shell rows", |s| {
        shell_rows(s) == 0 && s.contains("no sessions here")
    });
    d.capture("both-stopped");
    let rows = group(&screen, project);
    assert_eq!(rows.len(), 1, "{screen}");
    assert_eq!(
        rows[0].trim_start_matches(['▌', ' ']),
        "no sessions here",
        "{screen}"
    );
    until("the last shell to exit", &d, || {
        !alive(kept) && records(&d).is_empty()
    });
    d.keep("project/second");
    d.quit();
}

#[test]
fn a_composer_command_runs_in_the_selected_folder_and_keeps_its_own_draft() {
    let (mut d, dirs) = fixture(
        "terminal-command-folder",
        &["pi"],
        &["project", "other dir"],
    );
    let (project, other) = (&dirs[0], &dirs[1]);
    d.start();
    // Pinned folders list in path order: `other dir` first, selected.
    let screen = d.wait_text("pi ›");
    assert_eq!(group(&screen, other), ["▌ no sessions here"], "{screen}");
    assert_eq!(group(&screen, project), ["  no sessions here"], "{screen}");
    d.typed("an agent instruction");
    d.press("shift+tab", SHIFT_TAB);
    let screen = d.wait_text("terminal (zsh) › Type a command, or Enter to open");
    assert!(!screen.contains("an agent instruction"), "{screen}");
    d.typed("discard me");
    d.press("esc", b"\x1b");
    d.wait_text("terminal (zsh) › Type a command, or Enter to open");

    d.typed("print -r -- $PWD > 'here'");
    let screen = d.capture("command-drafted");
    assert!(
        screen.contains("terminal (zsh) › print -r -- $PWD > 'here'"),
        "{screen}"
    );
    d.press("enter", ENTER);
    let here = d.wait_file("other dir/here", |s| !s.is_empty());
    assert_eq!(here, format!("{}\n", other.display()));
    assert!(
        !project.join("here").exists(),
        "ran in the selected folder only"
    );
    let screen = wait_shell(&d);
    assert!(
        screen.contains("terminal (zsh) › Type a command, or Enter to open"),
        "the command left the composer:\n{screen}"
    );
    d.capture("command-ran");

    // The shell that ran the command stays open for more.
    d.typed("print -r -- $PWD > again\r");
    let again = d.wait_file("other dir/again", |s| !s.is_empty());
    assert_eq!(again, format!("{}\n", other.display()));
    let opened = records(&d);
    assert_eq!(opened.len(), 1);
    assert_record(&opened[0], other);

    d.press("ctrl+z", CTRL_Z);
    let screen = wait_list(&d);
    d.capture("row-in-selected-folder");
    let rows = group(&screen, other);
    assert_eq!(rows.len(), 1, "{screen}");
    assert!(rows[0].starts_with(&format!("▌ {SHELL_ROW}")), "{screen}");
    assert_eq!(group(&screen, project), ["  no sessions here"], "{screen}");

    // The agent instruction waited in its own draft.
    d.press("shift+tab", SHIFT_TAB);
    let screen = d.wait_text("pi › an agent instruction");
    d.capture("agent-draft-kept");
    assert!(!screen.contains("terminal (zsh) ›"), "{screen}");
    d.keep("other dir/here");
    d.keep("state/folders");
    d.quit();
}

#[test]
fn a_draft_in_the_native_shell_survives_leaving_returning_and_a_restart() {
    let (mut d, dirs) = fixture("terminal-native-draft", &[], &["project"]);
    let project = &dirs[0];
    let draft = "print -r -- DRAFT_KEPT > draft";
    d.start();
    d.press("enter", ENTER);
    wait_shell(&d);
    d.typed(draft);
    d.wait_for("the draft in the shell", |s| {
        pane(s).contains(&format!("{PROMPT} {draft}"))
    });
    // With a draft, Tab and Left stay in the shell.
    d.press("left", b"\x1b[D");
    let screen = d.capture("draft-typed");
    assert!(
        screen.contains("ctrl+z back"),
        "Left stayed native:\n{screen}"
    );
    d.press("ctrl+e", b"\x05");
    let shell = pid(&records(&d)[0]);

    d.press("ctrl+z", CTRL_Z);
    wait_list(&d);
    d.press("down", DOWN);
    d.wait_text("+ add folder");
    d.press("up", UP);
    d.wait_for("the shell row selected", |s| {
        group(s, project)
            .first()
            .is_some_and(|row| row.starts_with(&format!("▌ {SHELL_ROW}")))
    });
    d.capture("left-with-draft");
    assert!(!project.join("draft").exists(), "leaving never submits");
    d.press("enter", ENTER);
    let screen = wait_shell(&d);
    assert!(
        pane(&screen).contains(&format!("{PROMPT} {draft}")),
        "{screen}"
    );
    d.capture("returned-to-draft");

    d.press("ctrl+z", CTRL_Z);
    wait_list(&d);
    d.quit();
    assert!(alive(shell), "quitting the dashboard keeps the shell");
    assert!(!project.join("draft").exists(), "quitting never submits");

    d.start();
    let screen = d.wait_for("the shell row after restart", |s| shell_rows(s) == 1);
    assert_eq!(group(&screen, project).len(), 1, "{screen}");
    d.press("enter", ENTER);
    let screen = d.wait_for("the draft after restart", |s| {
        s.contains("ctrl+z back") && pane(s).contains(&format!("{PROMPT} {draft}"))
    });
    d.capture("draft-after-restart");
    assert_eq!(records(&d).len(), 1, "{screen}");
    assert_eq!(pid(&records(&d)[0]), shell, "the same process");
    d.press("enter", ENTER);
    let written = d.wait_file("project/draft", |s| !s.is_empty());
    assert_eq!(written, "DRAFT_KEPT\n");

    // On an empty command line, Tab returns to the list and the shell keeps running.
    d.wait_for("a fresh prompt", |s| {
        pane(s).contains(&format!("{draft}\n{PROMPT}"))
    });
    d.press("tab", b"\t");
    wait_list(&d);
    d.capture("tab-returned");
    assert!(alive(shell));
    d.keep("project/draft");
    d.quit();
}

#[test]
fn terminal_rows_list_oldest_first() {
    let (mut d, dirs) = fixture("terminal-row-order", &[], &["project"]);
    let project = &dirs[0];
    d.start();
    let labels = ["SHELL_ONE", "SHELL_TWO", "SHELL_THREE", "SHELL_FOUR"];
    for (n, label) in labels.iter().enumerate() {
        d.typed(&format!("print -r -- {label} > {label}"));
        d.press("enter", ENTER);
        d.wait_file(&format!("project/{label}"), |s| !s.is_empty());
        wait_shell(&d);
        d.press("ctrl+z", CTRL_Z);
        d.wait_for("one more shell row", |s| shell_rows(s) == n + 1);
        // Distinct start times, as the docs order rows by them.
        std::thread::sleep(Duration::from_millis(50));
    }
    let started: Vec<String> = records(&d)
        .iter()
        .map(|r| r["session"]["started"].as_str().unwrap().to_owned())
        .collect();
    assert!(started.windows(2).all(|w| w[0] < w[1]), "{started:?}");
    // Walk the rows top to bottom; the pane follows the selection and names the shell.
    let mut screen = d.screen();
    while group(&screen, project)
        .first()
        .is_some_and(|row| !row.starts_with('▌'))
    {
        d.press("up", UP);
        screen = d.screen();
    }
    let mut order = Vec::new();
    for row in 0..labels.len() {
        let screen = d.wait_for("a selected shell's pane", |s| {
            group(s, project)[row].starts_with('▌')
                && labels.iter().any(|l| pane(s).contains(&format!("> {l}")))
        });
        let label = labels
            .iter()
            .find(|l| pane(&screen).contains(&format!("> {l}")))
            .unwrap();
        order.push(*label);
        d.press("down", DOWN);
    }
    d.capture("rows-top-to-bottom");
    assert_eq!(order, labels, "rows oldest first by reported start");
    d.quit();
}
