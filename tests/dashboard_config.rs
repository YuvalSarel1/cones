//! The config screen, driven through the real dashboard: opening it from the menu, moving
//! between group tabs with `[` and `]`, saving a choice (Right, Space) and a text setting
//! (Enter edits, Enter saves, Escape restores) into jobs.yaml while the file's comments and
//! other settings stay; the session column picker changing the live table and jobs.yaml,
//! with backspace restoring the defaults; adding a pinned folder, which `+ add folder`
//! offers until it is open and offers again once ctrl+x closes it; tab listing
//! folders on `+ add folder` and Enter twice creating a missing one; and a failed write or a validation error keeping the
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
    lines.windows(2).any(|w| {
        // The selected row starts with the cursor bar instead of two spaces.
        w[0].starts_with(&format!("{folder} "))
            && w[1]
                .trim_start_matches(['▌', ' '])
                .starts_with("no sessions here ")
    })
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

    // The worktree layout saves on its own, as the colour does.
    d.press("down", DOWN);
    d.press("right", RIGHT);
    d.wait_file("jobs.yaml", |t| t.contains("\nworktrees: flat\n"));
    let screen = d.capture("worktrees-flat");
    assert!(
        line(&screen, "worktree sessions").ends_with("‹ flat › *"),
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
    // Past skip permissions and use Bedrock to the AWS profile.
    for _ in 0..3 {
        d.press("down", DOWN);
    }
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
    d.wait_text("9 of 17 shown · defaults");
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
            "\ncolumns: [context, activity, model, age, last_active, folder, commands, last_reply]\n"
        ),
        "{yaml}"
    );
    d.wait_text("8 of 17 shown · custom");
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
    for _ in 0..9 {
        d.press("down", DOWN);
    }
    d.wait_text("› [ ]   harness");
    d.press("space", b" ");
    d.wait_file("jobs.yaml", |t| t.contains("last_reply, harness]"));
    d.press("[", b"[");
    let yaml = d.wait_file("jobs.yaml", |t| t.contains("harness, last_reply]"));
    assert!(
        yaml.contains(
            "\ncolumns: [context, activity, model, age, last_active, folder, commands, harness, last_reply]\n"
        ),
        "{yaml}"
    );
    d.wait_text("9 of 17 shown · custom");
    let screen = d.capture("harness-shown");
    assert!(
        line(&screen, "› [x]").contains("harness         08"),
        "{screen}"
    );

    // Backspace restores the table's defaults and removes the override from the file.
    d.press("backspace", BACKSPACE);
    d.wait_text("9 of 17 shown · defaults");
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

/// Press `key` until the selected list line, given with the line above it, satisfies `pick`.
fn select(d: &mut Dashboard, key: &[u8], what: &str, pick: impl Fn(&str, &str) -> bool) {
    for _ in 0..12 {
        let screen = d.wait_for("a settled list", |_| true);
        let lines: Vec<&str> = screen.lines().collect();
        if let Some(at) = lines.iter().position(|l| l.starts_with('▌'))
            && pick(lines[at], lines[at.saturating_sub(1)])
        {
            return;
        }
        d.press(if key == UP { "up" } else { "down" }, key);
    }
    panic!("never selected {what}:\n{}", d.screen());
}

#[test]
fn a_pinned_folder_is_offered_under_add_folder_until_it_is_open() {
    let mut d = Dashboard::new("config_pins", &["claude"]);
    let extra = d.path("extra");
    fs::create_dir_all(&extra).unwrap();
    // An opened folder is listed by its resolved path; the pin keeps the path as written.
    let real = extra.canonicalize().unwrap().display().to_string();
    let extra = extra.display().to_string();
    let project = d.project().display().to_string();
    d.start();
    let screen = d.wait_text("+ add folder");
    assert!(!screen.contains(&extra), "{screen}");

    open_config(&mut d);
    for _ in 0..3 {
        d.press("down", DOWN);
    }
    d.wait_text("› pinned folders");
    d.press("enter", b"\r");
    d.wait_text("↑↓ folder · enter add");
    let screen = d.capture("pins-open");
    assert!(line(&screen, "↑↓ folder").contains("1 / 1"), "{screen}");
    assert!(screen.contains("│ › + add folder"), "{screen}");

    // `+ add folder` types a new pin; Enter saves it.
    d.press("enter", b"\r");
    d.typed(&extra);
    d.wait_text("enter save · esc revert");
    d.capture("pin-typed");
    d.press("enter", b"\r");
    let yaml = d.wait_file("jobs.yaml", |t| t.contains(&extra));
    assert!(
        yaml.contains(&format!("\nfolders: [\"{extra}\"]\n")),
        "{yaml}"
    );
    d.wait_text("1 / 2");
    let screen = d.capture("pin-added");
    assert!(screen.contains(&format!("│ › {extra}")), "{screen}");
    d.keep("jobs.yaml");

    // A pin is not a row: the list shows only the open project folder.
    d.press("esc", ESC);
    d.wait_text("[ 1 folder ] *");
    d.press("esc", ESC);
    d.press("ctrl+z", CTRL_Z);
    let screen = d.wait_for("the list", |s| empty_folder_row(s, &project));
    d.capture("pin-not-a-row");
    assert!(!screen.contains(&extra), "{screen}");

    // `+ add folder` offers it, and Enter on the offer opens it.
    select(&mut d, DOWN, "+ add folder", |l, _| l.contains("+ "));
    let screen = d.wait_text(&extra);
    d.capture("offered");
    assert_eq!(screen.matches(&extra).count(), 1, "{screen}");
    select(&mut d, DOWN, "the offer", |l, _| l.contains(&extra));
    d.press("enter", b"\r");
    let screen = d.wait_for("the opened folder's row", |s| empty_folder_row(s, &real));
    d.capture("opened");
    assert!(empty_folder_row(&screen, &project), "{screen}");
    d.wait_file("state/open-folders.json", |t| t.contains(&real));
    d.keep("state/open-folders.json");

    // An open folder is not offered again.
    select(&mut d, DOWN, "+ add folder", |l, _| l.contains("+ "));
    let screen = d.capture("open-not-offered");
    assert_eq!(screen.matches(&extra).count(), 1, "{screen}");

    // ctrl+x closes the folder and keeps the pin, which is offered again.
    select(&mut d, UP, "the opened folder", |_, above| {
        above.starts_with(&real)
    });
    d.press("ctrl+x", CTRL_X);
    d.press("ctrl+x", CTRL_X);
    d.wait_for("the folder row to leave", |s| !empty_folder_row(s, &real));
    d.wait_file("state/open-folders.json", |t| !t.contains(&real));
    let yaml = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    assert!(
        yaml.contains(&format!("\nfolders: [\"{extra}\"]\n")),
        "{yaml}"
    );
    select(&mut d, DOWN, "+ add folder", |l, _| l.contains("+ "));
    let screen = d.wait_text(&extra);
    d.capture("offered-again");
    assert!(empty_folder_row(&screen, &project), "{screen}");
    d.quit();
}

/// Make the fixture root, which holds jobs.yaml, read-only or writable again.
#[test]
fn add_folder_completes_like_a_shell_and_creates_a_missing_folder_after_asking() {
    let mut d = Dashboard::new("config_add_folder", &["claude"]);
    for name in ["alpha", "alps"] {
        fs::create_dir_all(d.project().join(name)).unwrap();
    }
    d.start();
    d.wait_text("+ add folder");
    select(&mut d, DOWN, "+ add folder", |l, _| l.contains("+ "));

    // The first tab grows to the shared prefix and lists both folders.
    d.typed("a");
    d.press("tab", b"\t");
    let screen = d.wait_text("alpha/  alps/");
    d.capture("listed");
    assert!(screen.contains("+ alp"), "{screen}");
    for _ in 0..3 {
        d.press("backspace", BACKSPACE);
    }

    // A missing folder asks; another key cancels, and enter twice creates it.
    let made = d.project().join("made/deep");
    d.typed("made/deep");
    d.press("enter", b"\r");
    d.wait_text("no such folder · enter creates");
    d.capture("asked");
    assert!(!made.exists());
    d.press("right", RIGHT);
    d.press("enter", b"\r");
    d.wait_text("no such folder · enter creates");
    assert!(!made.exists(), "a key between the presses asks again");
    d.press("enter", b"\r");
    let real = made.canonicalize().unwrap().display().to_string();
    let screen = d.wait_for("the created folder's row", |s| empty_folder_row(s, &real));
    d.capture("created");
    assert!(screen.contains("added"), "{screen}");
    d.wait_file("state/open-folders.json", |t| t.contains(&real));
}

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
    // Past skip permissions and use Bedrock to the AWS profile.
    for _ in 0..3 {
        d.press("down", DOWN);
    }
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
fn a_failed_write_can_be_retried_once_the_cause_is_fixed() {
    let mut d = Dashboard::new("config_retry", &["claude"]);
    d.start();
    open_config(&mut d);
    d.press("]", b"]");
    d.wait_text("› connectivity");
    // Past skip permissions and use Bedrock to the AWS profile.
    for _ in 0..3 {
        d.press("down", DOWN);
    }
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
