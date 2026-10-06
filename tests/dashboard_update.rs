//! The version and the update notice, read from the tap formula the dashboard keeps in its
//! state dir. The fixture's formula is fresh, so no test reaches GitHub.
//!
//! Covered: Help's title names this binary's version; a formula at a newer release puts
//! `cones <version> available` in the Navigation row and in Help's title, and the row
//! drops keys other than help to keep it when a pane narrows the bar; a formula at
//! this binary's own version shows no notice; turning "check for updates" off in the config
//! screen saves `start.update_check: false` and takes the notice away at once, and a
//! dashboard started with it off shows none. Homebrew's `brew upgrade cones` is offered
//! only to a binary under a Homebrew Cellar, which a test binary never is.
use crate::dashboard::*;
use std::fs;

const CTRL_G: &[u8] = b"\x07";
const ESC: &[u8] = b"\x1b";
const ENTER: &[u8] = b"\r";
const UP: &[u8] = b"\x1b[A";
const DOWN: &[u8] = b"\x1b[B";
const RIGHT: &[u8] = b"\x1b[C";
const CTRL_Z: &[u8] = b"\x1a";
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[test]
fn a_newer_tap_release_is_announced_in_the_footer_and_help() {
    let mut d = Dashboard::new("update-newer", &[]);
    fs::write(d.path("state/formula.rb"), formula("99.0.0")).unwrap();
    d.start();
    let list = d.wait_text("cones 99.0.0 available");
    let navigation = list.lines().find(|l| l.starts_with("Navigation")).unwrap();
    assert!(
        navigation.contains("ctrl+g help · cones 99.0.0 available"),
        "{list}"
    );
    assert!(!list.contains("brew upgrade"), "{list}");
    d.capture("list");
    d.press("ctrl+g", CTRL_G);
    let help = d.wait_text(&format!("help  cones {VERSION} · cones 99.0.0 available"));
    d.capture("help");
    assert!(!help.contains("brew upgrade"), "{help}");
    d.press("esc", ESC);
    d.wait_text("ctrl+g help");
    // A shell beside the list halves the bar; the notice keeps its place over the keys.
    d.press("enter the shell", ENTER);
    d.wait_text("ctrl+z back");
    d.press("ctrl+z", CTRL_Z);
    let paned = d.wait_text("tab pane");
    d.capture("beside-a-pane");
    let navigation = paned.lines().find(|l| l.starts_with("Navigation")).unwrap();
    assert!(
        navigation.contains("ctrl+g help · cones 99.0.0 available"),
        "{paned}"
    );
    d.quit();
}

#[test]
fn the_current_release_shows_only_the_version() {
    let mut d = Dashboard::new("update-current", &[]);
    d.start();
    let list = d.wait_text("ctrl+g help");
    d.capture("list");
    d.press("ctrl+g", CTRL_G);
    let help = d.wait_text(&format!("help  cones {VERSION}"));
    d.capture("help");
    for screen in [&list, &help] {
        assert!(!screen.contains("available"), "{screen}");
    }
    d.press("esc", ESC);
    d.wait_text("ctrl+g help");
    d.quit();
}

#[test]
fn turning_the_check_off_hides_the_notice() {
    let mut d = Dashboard::new("update-off", &["claude"]);
    fs::write(d.path("state/formula.rb"), formula("99.0.0")).unwrap();
    d.start();
    d.wait_text("cones 99.0.0 available");
    // The menu is the row above the folder; its jobs button comes first, config next.
    d.press("up", UP);
    d.wait_text("▌  jobs   config   help");
    d.press("right", RIGHT);
    d.wait_text("job defaults and dashboard settings");
    d.press("enter", ENTER);
    d.wait_text("←→ group · ↓ fields");
    d.press("down", DOWN);
    d.wait_text("↑↓ field");
    // From ctrl+x armed: highlight, worktrees, folders, then the start rows down to the check.
    for _ in 0..8 {
        d.press("down", DOWN);
    }
    d.wait_for("check for updates selected", |s| {
        s.lines()
            .any(|l| l.contains("›     check for updates     ‹ on ›"))
    });
    d.press("right", RIGHT);
    d.wait_file("jobs.yaml", |t| {
        t.contains("start:\n") && t.contains("  update_check: false\n")
    });
    d.keep("jobs.yaml");
    d.capture("check-off");
    d.press("esc", ESC);
    let list = d.wait_text("ctrl+g help");
    d.capture("list-after");
    assert!(!list.contains("available"), "{list}");
    d.quit();

    let mut d = Dashboard::new("update-off-restart", &[]);
    fs::write(d.path("state/formula.rb"), formula("99.0.0")).unwrap();
    let jobs = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    fs::write(
        d.path("jobs.yaml"),
        format!("{jobs}start:\n  update_check: false\n"),
    )
    .unwrap();
    d.start();
    let list = d.wait_text("ctrl+g help");
    d.capture("list");
    assert!(!list.contains("available"), "{list}");
    d.press("ctrl+g", CTRL_G);
    let help = d.wait_text(&format!("help  cones {VERSION}"));
    assert!(!help.contains("available"), "{help}");
    d.press("esc", ESC);
    d.wait_text("ctrl+g help");
    d.quit();
}
