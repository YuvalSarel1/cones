//! The divider between the session list and the pane, dragged with the mouse through the
//! real binary.
//!
//! A zsh terminal from the open folder fills the pane, so its pty reports the size it was
//! given. Covered: the pointer on the divider thickens only the three cells around it, and
//! the grip stays under the pointer through a drag; a left press there never reaches the
//! shell or a text selection; dragging moves the divider, the list redraws to its new width
//! and the shell's pty is resized to the pane's; a drag past the edge stops at 30 percent
//! for the list; each release saves the share as `pane.ratio` in jobs.yaml, and a restarted
//! dashboard draws the divider where the last drag left it. A wheel report split across two
//! reads, its Esc alone and then its text, never reaches the shell or the list's new-agent
//! input, while a typed Esc still reaches the shell. An OSC 8 link printed in the pane
//! reaches the outer terminal on the linked text only, through an SGR reset inside it and
//! with a separator in its target, while a target holding a space is dropped. One wheel report
//! over the list moves its selection one row, as far as one down arrow. No harness or
//! model is started.
use crate::dashboard::*;
use std::fs;

const ENTER: &[u8] = b"\r";
const CTRL_Z: &[u8] = b"\x1a";
const PROMPT: &str = "zsh-ready>";
/// The fourth screen row, inside the list's rows and the shell's pane.
const ROW: u16 = 4;

/// SGR mouse report: `button` and 0-based cell, pressed or released.
fn mouse(button: u8, column: u16, row: u16, release: bool) -> Vec<u8> {
    format!(
        "\x1b[<{button};{};{}{}",
        column + 1,
        row + 1,
        if release { 'm' } else { 'M' }
    )
    .into_bytes()
}

/// Where the divider is drawn: each column ruled on every screen row, with the rows its
/// heavy grip covers.
fn divider(screen: &str) -> Vec<(usize, Vec<usize>)> {
    let lines: Vec<Vec<char>> = screen.lines().map(|l| l.chars().collect()).collect();
    (0..COLS as usize)
        .filter(|&c| {
            lines.len() == ROWS as usize
                && lines
                    .iter()
                    .all(|l| matches!(l.get(c), Some('│') | Some('┃')))
        })
        .map(|c| {
            let grip = (0..lines.len()).filter(|&r| lines[r][c] == '┃').collect();
            (c, grip)
        })
        .collect()
}

/// The divider at `column`, its grip on the pointer's row and the rows either side.
fn gripped(column: usize) -> Vec<(usize, Vec<usize>)> {
    let row = ROW as usize;
    vec![(column, vec![row - 1, row, row + 1])]
}

/// The characters from `from` on, line by line.
fn right_of(screen: &str, from: usize) -> String {
    screen
        .lines()
        .map(|l| l.chars().skip(from).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The shell's pty size, as `stty size` reports it: rows, then columns.
fn pty_size(d: &mut Dashboard, file: &str) -> String {
    d.press("enter the shell", ENTER);
    d.wait_for("the focused shell", |s| s.contains("ctrl+z back"));
    d.typed(&format!("stty size > {file}\r"));
    let size = d.wait_file(&format!("project/{file}"), |s| s.ends_with('\n'));
    d.press("ctrl+z", CTRL_Z);
    d.wait_for("the list", |s| s.contains("tab pane"));
    size.trim().to_owned()
}

#[test]
fn dragging_the_divider_resizes_list_and_pane_and_persists() {
    let mut d = Dashboard::new("pane-divider-drag", &[]);
    fs::write(d.home().join(".zshrc"), format!("PROMPT='{PROMPT} '\n")).unwrap();
    d.start();
    d.press("enter", ENTER);
    d.wait_for("a shell in the pane", |s| s.contains(PROMPT));
    d.press("ctrl+z", CTRL_Z);
    let screen = d.wait_for("the list", |s| s.contains("tab pane"));
    d.capture("default-split");
    // Built-in 50 percent: the list takes 70 columns and the divider the next one.
    assert_eq!(divider(&screen), [(70, vec![])], "{screen}");
    assert_eq!(pty_size(&mut d, "before"), format!("{ROWS} 68"));

    d.press("hover the divider", &mouse(35, 70, ROW, false));
    d.wait_for("a grip under the pointer", |s| divider(s) == gripped(70));
    d.capture("hover");
    d.press("hover the list", &mouse(35, 20, ROW, false));
    d.wait_for("a thin divider", |s| divider(s) == [(70, vec![])]);

    // Press on the divider and drag it to column 49: the pane takes 65 percent.
    d.press("press the divider", &mouse(0, 70, ROW, false));
    d.press("drag left", &mouse(32, 60, ROW, false));
    d.press("drag further", &mouse(32, 49, ROW, false));
    d.wait_for("the divider at 49", |s| divider(s) == gripped(49));
    d.capture("dragging");
    d.press("release", &mouse(0, 49, ROW, true));
    let screen = d.wait_for("the saved width", |s| {
        s.contains("pane.ratio 65 saved") && divider(s) == gripped(49)
    });
    d.capture("released");
    // The list is drawn inside its new width and the shell starts right of the divider.
    assert!(right_of(&screen, 50).contains(PROMPT), "{screen}");
    assert!(
        !screen.contains("copied"),
        "the drag selected text:\n{screen}"
    );
    let jobs = d.wait_file("jobs.yaml", |s| s.contains("ratio: 65"));
    assert!(jobs.contains("pane:\n  at: right\n  ratio: 65\n"), "{jobs}");
    d.keep_as("jobs.yaml", "jobs-after-drag.yaml");
    // The press never reached the shell: its line holds no mouse report.
    assert!(!right_of(&screen, 50).contains("[<"), "{screen}");
    // 140 columns less the 49 the list keeps and the divider.
    assert_eq!(pty_size(&mut d, "after"), format!("{ROWS} 89"));

    // Past the left edge the list stops at 30 percent, 42 columns.
    d.press("press the divider", &mouse(0, 49, ROW, false));
    d.press("drag past the edge", &mouse(32, 2, ROW, false));
    d.press("release", &mouse(0, 2, ROW, true));
    d.press("hover the list", &mouse(35, 10, ROW, false));
    let screen = d.wait_for("the clamped divider", |s| {
        s.contains("pane.ratio 70 saved") && divider(s) == [(42, vec![])]
    });
    d.capture("clamped");
    let jobs = d.wait_file("jobs.yaml", |s| s.contains("ratio: 70"));
    assert!(jobs.contains("pane:\n  at: right\n  ratio: 70\n"), "{jobs}");
    assert!(jobs.contains("jobs: []"), "{jobs}");
    d.keep_as("jobs.yaml", "jobs-after-clamp.yaml");
    assert!(right_of(&screen, 43).contains(PROMPT), "{screen}");
    d.quit();

    d.start();
    let screen = d.wait_for("the restarted list", |s| {
        s.contains("start terminal") && divider(s) == [(42, vec![])]
    });
    d.capture("restarted");
    assert!(screen.contains("zsh"), "the hosted shell's row:\n{screen}");
    d.quit();
}

/// The hold on a bare Esc, widened from 50ms: a loaded runner can stall the test between
/// the two halves of a report for longer than that.
const SPLIT_WINDOW: (&str, &str) = ("CONES_TEST_SPLIT_MS", "1000");

#[test]
fn a_split_wheel_report_never_reaches_the_pane() {
    let mut d = Dashboard::new("pane-split-wheel-report", &[]);
    d.env.push(SPLIT_WINDOW);
    fs::write(d.home().join(".zshrc"), format!("PROMPT='{PROMPT} '\n")).unwrap();
    d.start();
    d.press("enter", ENTER);
    d.wait_for("a shell in the pane", |s| s.contains(PROMPT));
    d.typed("cat -v > split.txt\r");
    // The Esc read alone, as crossterm reports a lone byte, and the rest a moment later.
    d.burst("wheel report, Esc", b"\x1b");
    // Long enough for a loaded dashboard to read the Esc on its own, well inside the window.
    // At 2ms a CI runner read both writes at once, as one whole mouse event.
    std::thread::sleep(std::time::Duration::from_millis(200));
    d.press("wheel report, text", b"[<65;80;5M");
    // A typed Esc waits out the window and still reaches the shell.
    d.press("esc", b"\x1b");
    std::thread::sleep(std::time::Duration::from_millis(1200));
    d.typed("ok\r");
    d.press("ctrl+d", b"\x04");
    let out = d.wait_file("project/split.txt", |s| s.ends_with('\n'));
    d.keep("project/split.txt");
    assert_eq!(out, "^[ok\n");
    // The report reached the dashboard split, not as one mouse event.
    d.wait_file("state/tui-debug.log", |s| {
        s.contains("input.split_mouse_report")
    });
    d.press("ctrl+z", CTRL_Z);
    d.wait_for("the list", |s| s.contains("tab pane"));
    d.quit();
}

#[test]
fn a_split_wheel_report_never_reaches_the_list_input() {
    let mut d = Dashboard::new("list-split-wheel-report", &[]);
    d.env.push(SPLIT_WINDOW);
    d.start();
    d.wait_text("Type a command");
    d.burst("wheel report, Esc", b"\x1b");
    // Long enough for a loaded dashboard to read the Esc on its own, well inside the window.
    // At 2ms a CI runner read both writes at once, as one whole mouse event.
    std::thread::sleep(std::time::Duration::from_millis(200));
    d.press("wheel report, text", b"[<65;80;5M");
    d.typed("ok");
    let screen = d.wait_text("› ok");
    d.capture("typed");
    assert!(
        !screen.contains("[<"),
        "the report reached the input:\n{screen}"
    );
    d.wait_file("state/tui-debug.log", |s| {
        s.contains("input.split_mouse_report")
    });
    d.quit();
}

#[test]
fn an_osc8_link_in_the_pane_reaches_the_terminal() {
    let mut d = Dashboard::new("pane-osc8-link", &[]);
    fs::write(d.home().join(".zshrc"), format!("PROMPT='{PROMPT} '\n")).unwrap();
    d.start();
    d.press("enter", ENTER);
    d.wait_for("a shell in the pane", |s| s.contains(PROMPT));
    d.typed(concat!(
        r"printf '\e]8;;https://example.com/a;b\e\\link\e[0med\e]8;;\e\\ plain ",
        // The escaped c keeps the echoed command from matching the printed word.
        r"\e]8;;https://x y\e\\spa\x63ed\e]8;;\e\\\n'",
        "\r"
    ));
    let screen = d.wait_for("the printed link", |s| s.contains("linked plain spaced"));
    d.capture("linked");
    let url = Some("https://example.com/a;b".to_owned());
    assert_eq!(d.link_of("linked plain"), url, "{screen}");
    assert_eq!(d.link_of("ed plain"), url, "after the SGR reset:\n{screen}");
    assert_eq!(d.link_of(" plain spaced"), None, "{screen}");
    assert_eq!(d.link_of("spaced"), None, "{screen}");
    d.press("ctrl+z", CTRL_Z);
    d.wait_for("the list", |s| s.contains("tab pane"));
    d.quit();
}

#[test]
fn one_wheel_report_over_the_list_moves_the_selection_one_row() {
    let mut d = Dashboard::new("list-wheel-one-row", &[]);
    let dirs: Vec<_> = ["a", "b", "c", "d"]
        .iter()
        .map(|rel| {
            fs::create_dir_all(d.path(rel)).unwrap();
            d.path(rel).canonicalize().unwrap()
        })
        .collect();
    d.open_folders(&dirs);
    d.start();
    // Every folder's row reads the same, so the selection is its screen line.
    let selected = |s: &str| s.lines().position(|l| l.starts_with('▌'));
    let first = selected(&d.wait_for("a selected folder", |s| selected(s).is_some()));
    d.press("wheel down", &mouse(65, 5, 12, false));
    let wheeled = selected(&d.wait_for("the selection moved", |s| selected(s) != first));
    d.capture("wheeled");
    d.press("up", b"\x1b[A");
    d.wait_for("the first row again", |s| selected(s) == first);
    d.press("down", b"\x1b[B");
    let stepped = selected(&d.wait_for("the selection moved", |s| selected(s) != first));
    assert_eq!(
        wheeled, stepped,
        "one wheel report moves as far as one down arrow"
    );
    d.quit();
}

#[test]
fn the_selected_folder_is_the_terminals_working_directory() {
    let mut d = Dashboard::new("pane-osc7-dir", &[]);
    let dirs: Vec<_> = ["a", "b c"]
        .iter()
        .map(|rel| {
            fs::create_dir_all(d.path(rel)).unwrap();
            d.path(rel).canonicalize().unwrap()
        })
        .collect();
    d.open_folders(&dirs);
    let url =
        |dir: &std::path::Path| format!("file://localhost{}", dir.display()).replace(' ', "%20");
    let urls: Vec<_> = dirs.iter().map(|dir| url(dir)).collect();
    d.start();
    let selected = |s: &str| s.lines().position(|l| l.starts_with('▌'));
    let first = selected(&d.wait_for("a selected folder", |s| selected(s).is_some()));
    let named = d.reported_dirs();
    assert_eq!(named.len(), 1, "{named:?}");
    assert!(urls.contains(&named[0]), "{named:?} names an open folder");
    d.press("down", b"\x1b[B");
    d.wait_for("the next folder", |s| selected(s) != first);
    d.capture("moved");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while d.reported_dirs().len() < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let named = d.reported_dirs();
    assert_eq!(named.len(), 2, "{named:?}");
    assert!(
        urls.contains(&named[1]) && named[1] != named[0],
        "{named:?}"
    );
    d.quit();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while d.reported_dirs().len() < 3 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let named = d.reported_dirs();
    let own = url(&d.project().canonicalize().unwrap());
    assert_eq!(
        named.last(),
        Some(&own),
        "quitting names cones' folder: {named:?}"
    );
}

#[test]
fn a_plain_path_in_the_pane_links_to_its_file() {
    let mut d = Dashboard::new("pane-path-link", &[]);
    fs::write(d.home().join(".zshrc"), format!("PROMPT='{PROMPT} '\n")).unwrap();
    let project = d.project();
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/main.rs"), "").unwrap();
    fs::write(project.join("notes.md"), "").unwrap();
    d.start();
    d.press("enter", ENTER);
    d.wait_for("a shell in the pane", |s| s.contains(PROMPT));
    // The escaped letters keep the echoed command from holding the printed paths.
    d.typed(concat!(
        r"printf 'see src/ma\x69n.rs:12:3, \x6eotes.md. and gone/\x6eope.rs\n'",
        "\r"
    ));
    let screen = d.wait_for("the printed paths", |s| s.contains("notes.md. and"));
    d.capture("linked");
    let main = Some(format!("file://{}/src/main.rs", project.display()));
    assert_eq!(d.link_of("src/main.rs:12:3,"), main, "{screen}");
    assert_eq!(
        d.link_of(":3, notes"),
        main,
        "the suffix is linked:\n{screen}"
    );
    assert_eq!(d.link_of(", notes"), None, "{screen}");
    let notes = Some(format!("file://{}/notes.md", project.display()));
    assert_eq!(d.link_of("notes.md. and"), notes, "{screen}");
    assert_eq!(
        d.link_of(". and"),
        None,
        "the trailing period stays out:\n{screen}"
    );
    assert_eq!(d.link_of("gone/nope.rs"), None, "{screen}");
    d.press("ctrl+z", CTRL_Z);
    d.wait_for("the list", |s| s.contains("tab pane"));
    d.quit();
}

#[test]
fn a_drag_over_the_list_copies_the_rows_it_covers() {
    let mut d = Dashboard::new("list-drag-copy", &[]);
    let dirs: Vec<_> = ["alpha", "bravo"]
        .iter()
        .map(|rel| {
            fs::create_dir_all(d.path(rel)).unwrap();
            d.path(rel).canonicalize().unwrap()
        })
        .collect();
    d.open_folders(&dirs);
    // A pbcopy on the front of PATH keeps the machine's clipboard out of the test.
    let bin = d.path("bin");
    fs::create_dir_all(&bin).unwrap();
    let clip = d.path("clipboard");
    let stub = bin.join("pbcopy");
    fs::write(&stub, format!("#!/bin/sh\ncat > '{}'\n", clip.display())).unwrap();
    fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    d.env.push(("PATH", Box::leak(path.into_boxed_str())));
    d.start();
    let screen = d.wait_for("both folders", |s| s.contains("alpha") && s.contains("bravo"));
    let row = |name: &str| screen.lines().position(|l| l.contains(name)).unwrap() as u16;
    let (top, bottom) = (row("alpha"), row("bravo"));
    d.press("press", &mouse(0, 0, top, false));
    d.press("drag", &mouse(32, 20, bottom, false));
    d.press("release", &mouse(0, 20, bottom, true));
    let screen = d.wait_text("copied ");
    d.capture("copied");
    let text = d.wait_file("clipboard", |t| !t.is_empty());
    assert!(text.contains("alpha") && text.contains("bravo"), "{text:?}\n{screen}");
    d.quit();
}
