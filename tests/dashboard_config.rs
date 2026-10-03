//! The config screen, driven through the real dashboard: opening it from the menu, moving
//! between group tabs with `[` and `]`, saving a choice (Right, Space) and a text setting
//! (Enter edits, Enter saves, Escape restores) into jobs.yaml while the file's comments and
//! other settings stay; the session column picker changing the live table and jobs.yaml,
//! with backspace restoring the defaults; adding a pinned folder that appears as a folder
//! row and removing it with ctrl+x; and a failed write or a validation error keeping the
//! typed value on screen with the reason. Connectivity checks are not run: they probe the
//! machine's installed CLIs.
use crate::dashboard::*;
use std::{fs, os::unix::fs::PermissionsExt};

const UP: &[u8] = b"\x1b[A";
const DOWN: &[u8] = b"\x1b[B";
const RIGHT: &[u8] = b"\x1b[C";
const LEFT: &[u8] = b"\x1b[D";
const ESC: &[u8] = b"\x1b";
const BACKSPACE: &[u8] = b"\x7f";
const CTRL_X: &[u8] = b"\x18";
const CTRL_Z: &[u8] = b"\x1a";

/// The menu row with focus, as the list draws it.
const MENU: &str = "▌  jobs   config   help";
/// The menu's description of the config button.
const CONFIG: &str = "job defaults and dashboard settings";

/// Reach the menu, choose config, enter it and step from the tabs into the fields.
fn open_config(d: &mut Dashboard) {
    for _ in 0..8 {
        if d.screen().contains(MENU) {
            break;
        }
        d.press("up", UP);
    }
    d.wait_text(MENU);
    for _ in 0..3 {
        if d.screen().contains(CONFIG) {
            break;
        }
        d.press("left", LEFT);
    }
    if !d.screen().contains(CONFIG) {
        d.press("right", RIGHT);
    }
    d.wait_text(CONFIG);
    d.press("enter", b"\r");
    d.wait_text("←→ group · ↓ fields");
    d.press("down", DOWN);
    d.wait_text("↑↓ field");
}

/// The line of `screen` holding `text`, failing with the screen.
fn line<'a>(screen: &'a str, text: &str) -> &'a str {
    screen
        .lines()
        .find(|l| l.contains(text))
        .unwrap_or_else(|| panic!("no line with {text:?}:\n{screen}"))
}

/// Whether the list draws `folder` as an empty folder row.
fn empty_folder_row(screen: &str, folder: &str) -> bool {
    let lines: Vec<&str> = screen.lines().collect();
    lines
        .windows(2)
        .any(|w| w[0].starts_with(&format!("{folder} ")) && w[1].starts_with("  no sessions here "))
}

/// A live Claude background session in the project, held by a disposable `sleep` the
/// fixture's Drop kills through the registry.
fn session(d: &Dashboard) {
    let pid = std::process::Command::new("/bin/sleep")
        .arg("600")
        .spawn()
        .unwrap()
        .id();
    let dir = d.home().join(".claude/sessions");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("e2e.json"),
        serde_json::json!({
            "pid": pid, "sessionId": "11111111-2222-3333-4444-555555555555",
            "cwd": d.project(), "kind": "bg", "status": "idle", "jobId": "11111111",
            "startedAt": 1_700_000_000_000u64, "name": "config fixture",
        })
        .to_string(),
    )
    .unwrap();
}

const COMMENTED: &str = "# e2e: my notes survive every save
version: 4
defaults:
  claude_enabled: true
  codex_enabled: false
  pi_enabled: false
  opencode_enabled: false
  timeout_min: 45
jobs: [] # none yet
";

#[test]
fn config_saves_choice_and_text_settings_into_jobs_yaml() {
    let mut d = Dashboard::new("config_settings", &["claude"]);
    fs::write(d.path("jobs.yaml"), COMMENTED).unwrap();
    d.start();
    open_config(&mut d);
    let screen = d.capture("cones-group");
    assert!(
        screen.contains(" cones   harnesses   runs   columns"),
        "{screen}"
    );
    assert!(line(&screen, "ctrl+x armed (s)").contains("› ctrl+x armed (s)"));
    assert!(line(&screen, "highlight colour").ends_with("‹ magenta ›"));

    // A choice: Right cycles and saves.
    d.press("down", DOWN);
    d.press("right", RIGHT);
    d.wait_file("jobs.yaml", |t| t.contains("\nhighlight: cyan\n"));
    d.wait_text("config saved to");
    let screen = d.capture("highlight-cyan");
    assert!(
        line(&screen, "highlight colour").ends_with("› highlight colour          ‹ cyan › *"),
        "{screen}"
    );

    // Space cycles and saves too.
    for _ in 0..3 {
        d.press("down", DOWN);
    }
    d.press("space", b" ");
    let yaml = d.wait_file("jobs.yaml", |t| t.contains("start:\n"));
    d.wait_text("‹ on › *");
    let screen = d.capture("notify-on");
    assert!(
        line(&screen, "desktop notifications").ends_with("›     desktop notifications ‹ on › *"),
        "{screen}"
    );
    assert!(
        yaml.contains("start:\n  harness: claude\n  pane: true\n  notify: true\n"),
        "{yaml}"
    );

    // `]` moves to the harnesses group; a text field edits on Enter and saves on Enter.
    d.press("]", b"]");
    d.wait_text("› connectivity");
    d.press("down", DOWN);
    d.press("down", DOWN);
    d.press("enter", b"\r");
    d.typed("e2e-profile");
    d.wait_text("enter keep · esc revert");
    let screen = d.capture("profile-typing");
    assert!(
        line(&screen, "e2e-profile").ends_with("[ e2e-profile\u{a0} ]"),
        "{screen}"
    );
    assert!(
        !fs::read_to_string(d.path("jobs.yaml"))
            .unwrap()
            .contains("aws_profile")
    );
    d.press("enter", b"\r");
    let saved = d.wait_file("jobs.yaml", |t| t.contains("  aws_profile: e2e-profile\n"));
    d.wait_text("[ e2e-profile ] *");
    d.capture("profile-saved");

    // Escape restores the saved value and writes nothing.
    d.press("enter", b"\r");
    d.typed("-more");
    d.wait_text("[ e2e-profile-more\u{a0} ]");
    d.capture("profile-editing-again");
    d.press("esc", ESC);
    d.wait_text("enter type · bksp reset");
    let screen = d.capture("profile-restored");
    assert!(
        line(&screen, "AWS profile").ends_with("[ e2e-profile ] *"),
        "{screen}"
    );
    assert!(!screen.contains("e2e-profile-more"), "{screen}");
    assert_eq!(fs::read_to_string(d.path("jobs.yaml")).unwrap(), saved);

    // The runs group shows the file's own value; `[` back to cones keeps its last field.
    d.press("]", b"]");
    d.wait_text("time limit (min)");
    let screen = d.capture("runs-group");
    assert!(
        line(&screen, "time limit (min)").ends_with("‹ 45 › *"),
        "{screen}"
    );
    d.press("[", b"[");
    d.press("[", b"[");
    d.wait_text("composer starts on");
    let screen = d.capture("cones-group-again");
    assert!(
        line(&screen, "desktop notifications").contains("›     desktop"),
        "{screen}"
    );
    assert!(!screen.contains("› ctrl+x armed"), "{screen}");

    // The comments and the unrelated settings are still there.
    let yaml = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    for kept in [
        "# e2e: my notes survive every save\nversion: 4\ndefaults:\n",
        "  timeout_min: 45\n",
        "  claude_enabled: true\n",
        "  codex_enabled: false\n",
        "  pi_enabled: false\n",
        "  opencode_enabled: false\n",
        "  aws_profile: e2e-profile\n",
        "jobs: [] # none yet\n",
        "\nhighlight: cyan\n",
    ] {
        assert!(yaml.contains(kept), "lost {kept:?}:\n{yaml}");
    }
    d.keep("jobs.yaml");
    d.press("esc", ESC);
    d.quit();
}

#[test]
fn config_save_keeps_every_harness_switch() {
    let mut d = Dashboard::new("config_harness_switches", &["claude"]);
    d.start();
    open_config(&mut d);
    d.press("down", DOWN);
    d.press("right", RIGHT);
    let yaml = d.wait_file("jobs.yaml", |t| t.contains("\nhighlight: cyan\n"));
    d.keep("jobs.yaml");
    // Every switch the file had is still there, so the six harnesses stay off.
    for (harness, on) in [
        ("claude", true),
        ("codex", false),
        ("pi", false),
        ("opencode", false),
        ("gemini", false),
        ("cursor", false),
        ("copilot", false),
        ("amp", false),
        ("droid", false),
        ("kimi", false),
    ] {
        let kept = format!("  {harness}_enabled: {on}\n");
        assert!(yaml.contains(&kept), "lost {kept:?}:\n{yaml}");
    }
    // Reopened from the file, the harnesses group still shows them configured off.
    d.press("esc", ESC);
    open_config(&mut d);
    d.press("]", b"]");
    d.wait_text("› connectivity");
    let screen = d.capture("harnesses-reopened");
    for label in ["gemini  ", "cursor-agent  ", "copilot  ", "amp  "] {
        assert!(
            line(&screen, label).ends_with("‹ off › *"),
            "{label}:\n{screen}"
        );
    }
    d.press("esc", ESC);
    d.quit();
}

#[test]
fn column_picker_changes_the_live_session_table_and_jobs_yaml() {
    let mut d = Dashboard::new("config_columns", &["claude"]);
    session(&d);
    d.start();
    let screen = d.wait_text("config fixture");
    assert!(
        line(&screen, "context  activity").contains("state  title"),
        "{screen}"
    );
    assert!(
        line(&screen, "config fixture").contains("idle   config fixture"),
        "{screen}"
    );
    d.capture("default-table");

    open_config(&mut d);
    for _ in 0..3 {
        d.press("]", b"]");
    }
    d.wait_text("whole columns only");
    d.press("down", DOWN);
    d.wait_text("› sessions");
    let screen = d.capture("columns-group");
    assert!(
        line(&screen, "› sessions").contains("[ state, context, activity, model, age, last… ]"),
        "{screen}"
    );
    d.press("enter", b"\r");
    d.wait_text("8 of 15 shown · defaults");
    let screen = d.capture("picker-defaults");
    assert!(
        screen.contains(" sessions   runs   jobs   history"),
        "{screen}"
    );
    assert!(
        line(&screen, "› [x]").contains("state           01"),
        "{screen}"
    );
    assert!(
        line(&screen, "[ ]   harness").contains("[ ]   harness"),
        "{screen}"
    );

    // Space hides the selected column: the live table and the file both lose it.
    d.press("space", b" ");
    let yaml = d.wait_file("jobs.yaml", |t| t.contains("\ncolumns:"));
    assert!(
        yaml.contains(
            "\ncolumns: [context, activity, model, age, last_active, folder, last_reply]\n"
        ),
        "{yaml}"
    );
    d.wait_text("7 of 15 shown · custom");
    let screen = d.capture("state-hidden");
    assert!(line(&screen, "› [ ]").contains("state"), "{screen}");
    assert!(
        line(&screen, "context  activity").contains("title           context"),
        "{screen}"
    );
    assert!(
        !line(&screen, "context  activity").contains("state"),
        "{screen}"
    );
    assert!(
        !line(&screen, "config fixture").contains("idle"),
        "{screen}"
    );
    assert!(screen.contains("columns saved"), "{screen}");

    // Showing an optional column appends it; `[` moves it earlier.
    for _ in 0..8 {
        d.press("down", DOWN);
    }
    d.wait_text("› [ ]   harness");
    d.press("space", b" ");
    d.wait_file("jobs.yaml", |t| t.contains("last_reply, harness]"));
    d.press("[", b"[");
    let yaml = d.wait_file("jobs.yaml", |t| t.contains("harness, last_reply]"));
    assert!(
        yaml.contains(
            "\ncolumns: [context, activity, model, age, last_active, folder, harness, last_reply]\n"
        ),
        "{yaml}"
    );
    d.wait_text("8 of 15 shown · custom");
    let screen = d.capture("harness-shown");
    assert!(
        line(&screen, "› [x]").contains("harness         07"),
        "{screen}"
    );

    // Backspace restores the table's defaults and removes the override from the file.
    d.press("backspace", BACKSPACE);
    d.wait_text("8 of 15 shown · defaults");
    let yaml = d.wait_file("jobs.yaml", |t| !t.contains("columns:"));
    let screen = d.capture("defaults-restored");
    assert!(
        line(&screen, "context  activity").contains("state  title"),
        "{screen}"
    );
    assert!(
        line(&screen, "config fixture").contains("idle   config fixture"),
        "{screen}"
    );
    assert!(
        yaml.contains("defaults:\n  claude_enabled: true\n"),
        "{yaml}"
    );
    d.keep("jobs.yaml");

    // Escape returns to Config, Ctrl+Z to the list.
    d.press("esc", ESC);
    d.wait_text("› sessions");
    d.press("ctrl+z", CTRL_Z);
    d.quit();
}

#[test]
fn pinned_folders_add_a_folder_row_and_ctrl_x_removes_it() {
    let mut d = Dashboard::new("config_pins", &["claude"]);
    let extra = d.path("extra");
    fs::create_dir_all(&extra).unwrap();
    let extra = extra.display().to_string();
    let project = d.project().display().to_string();
    d.start();
    let screen = d.wait_text("+ add folder");
    assert!(!screen.contains(&extra), "{screen}");

    open_config(&mut d);
    d.press("down", DOWN);
    d.press("down", DOWN);
    d.wait_text("› pinned folders");
    d.press("enter", b"\r");
    d.wait_text("↑↓ folder · enter edit · ctrl+x remove");
    let screen = d.capture("pins-open");
    assert!(line(&screen, "↑↓ folder").contains("1 / 2"), "{screen}");
    assert!(screen.contains(&format!("│ › {project}")), "{screen}");
    assert!(screen.contains("│   + add folder"), "{screen}");

    // `+ add folder` types a new pin; Enter saves it.
    d.press("down", DOWN);
    d.press("enter", b"\r");
    d.typed(&extra);
    d.wait_text("enter save · esc revert");
    d.capture("pin-typed");
    d.press("enter", b"\r");
    let yaml = d.wait_file("jobs.yaml", |t| t.contains(&extra));
    assert!(
        yaml.contains(&format!("\nfolders: [\"{project}\", \"{extra}\"]\n")),
        "{yaml}"
    );
    d.wait_text("2 / 3");
    let screen = d.capture("pin-added");
    assert!(screen.contains(&format!("│ › {extra}")), "{screen}");
    d.keep("jobs.yaml");

    // The list draws the new pin as an empty folder row.
    d.press("esc", ESC);
    d.wait_text("[ 2 folders ] *");
    d.press("esc", ESC);
    d.press("ctrl+z", CTRL_Z);
    let screen = d.wait_for("the pinned folder's row", |s| empty_folder_row(s, &extra));
    d.capture("folder-row");
    assert!(empty_folder_row(&screen, &project), "{screen}");

    // ctrl+x on the pin removes it from the file and the list.
    open_config(&mut d);
    d.press("down", DOWN);
    d.press("down", DOWN);
    d.press("enter", b"\r");
    d.press("down", DOWN);
    d.wait_text(&format!("│ › {extra}"));
    d.press("ctrl+x", CTRL_X);
    let yaml = d.wait_file("jobs.yaml", |t| !t.contains(&extra));
    assert!(
        yaml.contains(&format!("\nfolders: [\"{project}\"]\n")),
        "{yaml}"
    );
    d.wait_text("2 / 2");
    let screen = d.capture("pin-removed");
    assert!(!screen.contains(&extra), "{screen}");
    assert!(screen.contains(&format!("│   {project}\n")), "{screen}");
    assert!(screen.contains("│ › + add folder"), "{screen}");
    d.press("esc", ESC);
    d.press("esc", ESC);
    d.press("ctrl+z", CTRL_Z);
    d.wait_for("the folder row to leave", |s| !s.contains(&extra));
    let screen = d.capture("folder-row-gone");
    assert!(empty_folder_row(&screen, &project), "{screen}");
    d.quit();
}

/// Make the fixture root, which holds jobs.yaml, read-only or writable again.
fn writable(d: &Dashboard, yes: bool) {
    let mode = if yes { 0o755 } else { 0o555 };
    fs::set_permissions(d.root.path(), fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn failed_write_and_invalid_value_keep_the_typed_value_with_the_reason() {
    let mut d = Dashboard::new("config_failures", &["claude"]);
    d.start();
    let before = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    open_config(&mut d);

    // A write the file system refuses: the typed value stays, with the error.
    d.press("]", b"]");
    d.wait_text("› connectivity");
    d.press("down", DOWN);
    d.press("down", DOWN);
    writable(&d, false);
    d.press("enter", b"\r");
    d.typed("e2e-profile");
    d.press("enter", b"\r");
    let screen = d.wait_text("Permission denied (os error 13)");
    writable(&d, true);
    d.capture("write-failed");
    assert!(
        line(&screen, "AWS profile").ends_with("[ e2e-profile ] *"),
        "{screen}"
    );
    assert_eq!(fs::read_to_string(d.path("jobs.yaml")).unwrap(), before);
    assert!(!d.path("jobs.tmp").exists());

    // A value out of range: the field is named and nothing is written.
    d.press("[", b"[");
    d.wait_text("composer starts on");
    d.press("home", b"\x1b[H");
    d.wait_text("› ctrl+x armed (s)");
    d.press("enter", b"\r");
    d.press("backspace", BACKSPACE);
    d.typed("9999");
    d.press("enter", b"\r");
    let screen = d.wait_text("confirm_secs:");
    d.capture("invalid-value");
    assert!(line(&screen, "ctrl+x armed").contains("9999"), "{screen}");
    assert_eq!(fs::read_to_string(d.path("jobs.yaml")).unwrap(), before);
    d.keep("jobs.yaml");
    d.press("esc", ESC);
    d.press("esc", ESC);
    d.quit();
}

#[test]
#[ignore = "bug: after a failed write, Enter Enter on the retained value saves nothing, though the row shows it set"]
fn a_failed_write_can_be_retried_once_the_cause_is_fixed() {
    let mut d = Dashboard::new("config_retry", &["claude"]);
    d.start();
    open_config(&mut d);
    d.press("]", b"]");
    d.wait_text("› connectivity");
    d.press("down", DOWN);
    d.press("down", DOWN);
    writable(&d, false);
    d.press("enter", b"\r");
    d.typed("e2e-profile");
    d.press("enter", b"\r");
    d.wait_text("Permission denied (os error 13)");
    writable(&d, true);
    d.capture("write-failed");
    // Retry the retained value as it stands.
    d.press("enter", b"\r");
    d.press("enter", b"\r");
    d.capture("retried");
    let yaml = d.wait_file("jobs.yaml", |t| t.contains("aws_profile"));
    assert!(yaml.contains("  aws_profile: e2e-profile\n"), "{yaml}");
    d.keep("jobs.yaml");
    d.press("esc", ESC);
    d.quit();
}
