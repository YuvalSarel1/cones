//! The divider between the session list and the pane, dragged with the mouse through the
//! real binary.
//!
//! A zsh terminal from the pinned folder fills the pane, so its pty reports the size it was
//! given. Covered: the pointer resting on the divider thickens it; a left press there never
//! reaches the shell or a text selection; dragging moves the divider, the list redraws to
//! its new width and the shell's pty is resized to the pane's; a drag past the edge stops at
//! 30 percent for the list; each release saves the share as `pane.ratio` in jobs.yaml, and a
//! restarted dashboard draws the divider where the last drag left it. No harness or model
//! is started.
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

/// The columns holding `symbol` on every screen row: where the divider is drawn.
fn divider(screen: &str, symbol: char) -> Vec<usize> {
    let lines: Vec<Vec<char>> = screen.lines().map(|l| l.chars().collect()).collect();
    (0..COLS as usize)
        .filter(|&c| {
            lines.len() == ROWS as usize && lines.iter().all(|l| l.get(c) == Some(&symbol))
        })
        .collect()
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
    assert_eq!(divider(&screen, '│'), [70], "{screen}");
    assert_eq!(pty_size(&mut d, "before"), format!("{ROWS} 69"));

    d.press("hover the divider", &mouse(35, 70, ROW, false));
    let screen = d.wait_for("a thick divider", |s| divider(s, '┃') == [70]);
    d.capture("hover");
    assert!(divider(&screen, '│').is_empty(), "{screen}");
    d.press("hover the list", &mouse(35, 20, ROW, false));
    d.wait_for("a thin divider", |s| divider(s, '│') == [70]);

    // Press on the divider and drag it to column 49: the pane takes 65 percent.
    d.press("press the divider", &mouse(0, 70, ROW, false));
    d.press("drag left", &mouse(32, 60, ROW, false));
    d.press("drag further", &mouse(32, 49, ROW, false));
    let screen = d.wait_for("the divider at 49", |s| divider(s, '┃') == [49]);
    d.capture("dragging");
    assert!(divider(&screen, '│').is_empty(), "{screen}");
    d.press("release", &mouse(0, 49, ROW, true));
    let screen = d.wait_for("the saved width", |s| {
        s.contains("pane.ratio 65 saved") && divider(s, '┃') == [49]
    });
    d.capture("released");
    // The list is drawn inside its new width and the shell starts right of the divider.
    assert!(
        screen.lines().all(|l| l.chars().nth(49) == Some('┃')),
        "{screen}"
    );
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
    assert_eq!(pty_size(&mut d, "after"), format!("{ROWS} 90"));

    // Past the left edge the list stops at 30 percent, 42 columns.
    d.press("press the divider", &mouse(0, 49, ROW, false));
    d.press("drag past the edge", &mouse(32, 2, ROW, false));
    d.press("release", &mouse(0, 2, ROW, true));
    d.press("hover the list", &mouse(35, 10, ROW, false));
    let screen = d.wait_for("the clamped divider", |s| {
        s.contains("pane.ratio 70 saved") && divider(s, '│') == [42]
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
        s.contains("start terminal") && divider(s, '│') == [42]
    });
    d.capture("restarted");
    assert!(screen.contains("zsh"), "the hosted shell's row:\n{screen}");
    d.quit();
}
