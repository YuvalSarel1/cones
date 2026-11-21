//! The version and the update notice, read from the tap formula the dashboard keeps in its
//! state dir. The fixture's formula is fresh, so no test reaches GitHub.
//!
//! Covered: Help's title names this binary's version; a formula at a newer release puts
//! `cones <version> available` in the footer after `? shortcuts` and in Help's title, and
//! the footer drops the selection's keys to keep it when a pane narrows the bar; `?` on an
//! empty composer lists the list's keys under it and closes on `?`, ctrl+g opens Help, and
//! `?` inside a draft is typed, with the footer offering
//! `ctrl+g help` instead; a formula at
//! this binary's own version shows no notice; turning "check for updates" off in the config
//! screen saves `start.update_check: false` and takes the notice away at once, and a
//! dashboard started with it off shows none. Homebrew's `brew upgrade cones` is offered
//! only to a binary under a Homebrew Cellar. A copy of the binary in a fixture Cellar runs
//! the prefix's `brew update`, then `brew upgrade cones` with cleanup off, so a tap Homebrew
//! has not refreshed yet still upgrades; once the new version's Cellar folder exists the
//! notice asks to reopen cones, and when brew fails it offers the command. A newer version
//! Homebrew installed on its own, while the kept formula still names this binary's release,
//! asks to reopen cones without running brew.
use crate::dashboard::*;
use std::{fs, os::unix::fs::PermissionsExt};

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
    let navigation = list.lines().find(|l| l.contains("? shortcuts")).unwrap();
    assert!(
        navigation.contains("? shortcuts · cones 99.0.0 available"),
        "{list}"
    );
    assert!(!list.contains("brew upgrade"), "{list}");
    d.capture("list");
    d.press("ctrl+g", CTRL_G);
    let help = d.wait_text(&format!("help  cones {VERSION} · cones 99.0.0 available"));
    d.capture("help");
    assert!(!help.contains("brew upgrade"), "{help}");
    d.press("esc", ESC);
    d.wait_text("? shortcuts");
    // A shell beside the list halves the bar; the notice keeps its place over the keys.
    d.press("enter the shell", ENTER);
    d.wait_text("ctrl+z back");
    d.press("ctrl+z", CTRL_Z);
    let paned = d.wait_for("the list beside the shell", |s| {
        !s.contains("ctrl+z back") && s.contains("? shortcuts")
    });
    d.capture("beside-a-pane");
    let navigation = paned.lines().find(|l| l.contains("? shortcuts")).unwrap();
    assert!(
        navigation.contains("? shortcuts · cones 99.0.0 available"),
        "{paned}"
    );
    d.quit();
}

#[test]
fn the_current_release_shows_only_the_version() {
    let mut d = Dashboard::new("update-current", &[]);
    d.start();
    let list = d.wait_text("? shortcuts");
    d.capture("list");
    // `?` lists the list's keys under the composer, as Claude Code does; `?` again closes it.
    d.press("?", b"?");
    let keys = d.wait_text("ctrl+g all of Help · esc close");
    d.capture("shortcuts");
    assert!(
        keys.contains("Fork the selected conversation.") && !keys.contains("? shortcuts"),
        "{keys}"
    );
    d.press("?", b"?");
    d.wait_text("? shortcuts");
    d.press("ctrl+g", CTRL_G);
    let help = d.wait_text(&format!("help  cones {VERSION}"));
    d.capture("help");
    for screen in [&list, &help] {
        assert!(!screen.contains("available"), "{screen}");
    }
    d.press("esc", ESC);
    d.wait_text("? shortcuts");
    // In a draft `?` is text, so the footer names the key that still opens Help.
    d.typed("why?");
    let draft = d.wait_text("ctrl+g help");
    d.capture("draft");
    assert!(
        draft.contains("why?") && !draft.contains("? shortcuts"),
        "{draft}"
    );
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
    // From ctrl+x armed: highlight, worktrees, sort, folders, then the start rows down to the
    // check.
    for _ in 0..9 {
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
    let list = d.wait_text("? shortcuts");
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
    let list = d.wait_text("? shortcuts");
    d.capture("list");
    assert!(!list.contains("available"), "{list}");
    d.press("ctrl+g", CTRL_G);
    let help = d.wait_text(&format!("help  cones {VERSION}"));
    assert!(!help.contains("available"), "{help}");
    d.press("esc", ESC);
    d.wait_text("? shortcuts");
    d.quit();
}

/// Runs the dashboard from `<root>/brew/Cellar/cones/0.0.1/bin/cones` beside a fake
/// `<root>/brew/bin/brew` that logs its call and runs `script`.
fn from_homebrew(d: &mut Dashboard, script: &str) {
    let prefix = d.path("brew");
    let bin = prefix.join("Cellar/cones/0.0.1/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(prefix.join("bin")).unwrap();
    let exe = bin.join("cones");
    if fs::hard_link(&d.exe, &exe).is_err() {
        fs::copy(&d.exe, &exe).unwrap();
    }
    d.exe = exe;
    let brew = prefix.join("bin/brew");
    fs::write(
        &brew,
        format!(
            "#!/bin/sh\necho \"$* cleanup=$HOMEBREW_NO_INSTALL_CLEANUP\" >> {}\n{script}\n",
            d.path("brew.log").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&brew, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(d.path("state/formula.rb"), formula("99.0.0")).unwrap();
}

#[test]
fn a_homebrew_install_upgrades_itself_and_asks_to_reopen() {
    let mut d = Dashboard::new("update-homebrew", &[]);
    let cellar = d.path("brew/Cellar/cones/99.0.0");
    let tap = d.path("brew/tap-updated");
    // Homebrew's own copy of the tap is a day behind: only `brew update` lets the upgrade work.
    from_homebrew(
        &mut d,
        &format!(
            "case $1 in update) touch {tap};; upgrade) [ -f {tap} ] && mkdir -p {cellar};; esac",
            tap = tap.display(),
            cellar = cellar.display()
        ),
    );
    d.start();
    let list = d.wait_text("cones 99.0.0 installed · reopen cones");
    d.capture("list");
    // The header leads with it too, since the update runs only after a restart.
    assert!(
        list.lines()
            .take(5)
            .any(|l| l.contains("cones 99.0.0 installed · quit and reopen cones")),
        "{list}"
    );
    let navigation = list.lines().find(|l| l.contains("? shortcuts")).unwrap();
    assert!(
        navigation.contains("? shortcuts · cones 99.0.0 installed · reopen cones"),
        "{list}"
    );
    let log = d.wait_file("brew.log", |t| t.contains("upgrade"));
    d.keep("brew.log");
    assert_eq!(log, "update --quiet cleanup=1\nupgrade cones cleanup=1\n");
    d.press("ctrl+g", CTRL_G);
    d.wait_text(&format!(
        "help  cones {VERSION} · cones 99.0.0 installed · reopen cones"
    ));
    d.capture("help");
    d.press("esc", ESC);
    d.wait_text("? shortcuts");
    d.quit();
}

#[test]
fn a_failed_homebrew_upgrade_offers_the_command() {
    let mut d = Dashboard::new("update-homebrew-failed", &[]);
    from_homebrew(&mut d, "exit 1");
    d.start();
    d.wait_file("brew.log", |t| !t.is_empty());
    d.keep("brew.log");
    // Beside the pane the bar has room only for the version; Help's title has the command.
    let list = d.wait_text("? shortcuts · cones 99.0.0 available");
    d.capture("list");
    assert!(!list.contains("installed"), "{list}");
    d.press("ctrl+g", CTRL_G);
    d.wait_text(&format!(
        "help  cones {VERSION} · cones 99.0.0 available · brew upgrade cones"
    ));
    d.capture("help");
    d.press("esc", ESC);
    d.wait_text("? shortcuts");
    d.quit();
}

#[test]
fn a_release_homebrew_installed_on_its_own_asks_to_reopen() {
    let mut d = Dashboard::new("update-homebrew-autoupdate", &[]);
    from_homebrew(&mut d, "");
    // `brew autoupdate` installed 99.0.0; the formula cones kept is a day old and still current.
    fs::write(d.path("state/formula.rb"), formula(VERSION)).unwrap();
    fs::create_dir_all(d.path("brew/Cellar/cones/99.0.0")).unwrap();
    d.start();
    let list = d.wait_text("cones 99.0.0 installed · reopen cones");
    d.capture("list");
    assert!(
        list.lines()
            .take(5)
            .any(|l| l.contains("cones 99.0.0 installed · quit and reopen cones")),
        "{list}"
    );
    assert!(!d.path("brew.log").exists(), "brew ran");
    d.quit();
}
