//! The Jobs screen and its wizard, driven through the real dashboard.
//!
//! Covers: opening Jobs from the menu; the wizard's `once` answer starting a supervised run with
//! the first job's policy and writing no job; a scheduled job created through every question
//! (what, where, when, at, name), shown as a Jobs row and written as its own block while the
//! file's comment and other jobs stay byte for byte; `ctrl+e` reopening it with its answers,
//! and an edit of the task, schedule and run settings replacing the block; `ctrl+x` arming and
//! a second `ctrl+x` deleting it; and `enter` on a job running it, recorded in the ledger.
//!
//! The harness is `fake_claude.py`, copied as a real executable into the fixture's
//! `~/.local/bin`, and its `--model` picks the outcome. No model is called.
//!
//! Saving and deleting a job also installs schedules through `cones __install`, and that
//! reaches the machine: the plists go under `$HOME/Library/LaunchAgents`, which the fixture
//! HOME contains, but `launchctl print`, `bootstrap` and `bootout` address the real
//! `gui/<uid>` domain, which no HOME redirection contains. So every fixture here makes
//! `$HOME/Library` a plain file: the install fails creating the agents directory, before any
//! `launchctl` call, and startup's signature relearn finds no agents to touch. The tests assert
//! that the install was attempted and stopped there; whether launchd loads a job is not
//! exercised.
use crate::dashboard::*;
use serde_json::Value;
use std::{fs, os::unix::fs::PermissionsExt};

const UP: &[u8] = b"\x1b[A";
const DOWN: &[u8] = b"\x1b[B";
const RIGHT: &[u8] = b"\x1b[C";
const ENTER: &[u8] = b"\r";
const CTRL_E: &[u8] = b"\x05";
const CTRL_X: &[u8] = b"\x18";

/// The comment and job a user already has; saves and deletes must leave both as written.
const KEPT: &str = concat!(
    "  # The nightly triage stays as written.\n",
    "  - name: triage\n",
    "    schedule: '0 3 * * *'\n",
    "    cwd: project\n",
    "    prompt: triage the failures\n",
    "    model: success\n",
    "    timeout_min: 5\n",
);

/// Only clock-free columns, so a rerun captures the same screens.
const COLUMNS: &str = "\
job_columns: [status, schedule, model, folder]
run_columns: [status, model, folder, reason]
";

/// A fixture with the fake Claude on the launch path, `jobs` under `jobs:`, and no
/// LaunchAgents directory for an install to reach.
fn fixture(test: &str, jobs: &str) -> Dashboard {
    let d = Dashboard::new(test, &["claude"]);
    fs::write(
        d.home().join("Library"),
        "a file, so no LaunchAgent can be installed\n",
    )
    .unwrap();
    let claude = d.home().join(".local/bin/claude");
    fs::write(&claude, include_bytes!("fixtures/fake_claude.py")).unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o700)).unwrap();
    let file = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    let list = if jobs.is_empty() {
        "jobs: []\n".to_owned()
    } else {
        format!("jobs:\n{jobs}")
    };
    fs::write(
        d.path("jobs.yaml"),
        file.replace("jobs: []\n", &format!("{list}{COLUMNS}")),
    )
    .unwrap();
    d
}

/// From the first list row, up to the menu's `jobs` button and into the Jobs screen.
fn open_jobs(d: &mut Dashboard) -> String {
    d.press("up", UP);
    d.wait_text("Menu        enter jobs");
    d.press("enter", ENTER);
    d.wait_text("+ new job · a task once or on a schedule")
}

/// Move the Jobs screen's selection onto the row of job `name`.
fn select_job(d: &mut Dashboard, name: &str) {
    let selected = |s: &str| {
        s.lines().any(|l| {
            let pane = l.rsplit('│').next().unwrap_or(l);
            pane.contains("▌") && pane.contains(&format!("  {name}  "))
        })
    };
    for _ in 0..8 {
        if selected(&d.screen()) {
            return;
        }
        d.press("up", UP);
    }
    panic!("no job row {name}:\n{}", d.screen());
}

/// `$HOME/Library` is still the fixture's file: nothing was installed.
fn assert_no_agents(d: &Dashboard) {
    assert!(d.home().join("Library").is_file());
}

fn records(d: &Dashboard) -> Vec<Value> {
    fs::read_to_string(d.path("state/runs.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Wait until the ledger holds a started and a finished record, and return both.
fn finished_run(d: &Dashboard) -> (Value, Value) {
    d.wait_file("state/runs.jsonl", |t| t.lines().count() >= 2);
    let records = records(d);
    assert_eq!(records.len(), 2, "{records:#?}");
    (records[0].clone(), records[1].clone())
}

#[test]
fn a_task_run_once_from_the_wizard_starts_a_run_under_the_first_jobs_policy_and_writes_no_job() {
    let mut d = fixture("jobs-run-once", KEPT);
    let notes = d.project().join("notes");
    fs::create_dir(&notes).unwrap();
    d.start();
    let screen = open_jobs(&mut d);
    assert!(screen.contains("triage  0 3 * * *  success"), "{screen}");
    // Startup may move pinned folders into the file, so compare against it from here.
    let before = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    d.capture("jobs");

    d.press("down", DOWN);
    d.press("enter", ENTER);
    d.wait_text("what › the task, as you would type it to the harness");
    d.typed("summarize the TODOs");
    d.press("enter", ENTER);
    let screen = d.wait_text("where › a folder; empty takes the one shown");
    // The folder starts as the selection's, the dashboard's own project.
    let project = d.project().canonicalize().unwrap();
    assert!(
        screen.contains(&format!("where  {}\n", project.display())),
        "{screen}"
    );
    d.typed(&notes.display().to_string());
    d.press("enter", ENTER);
    let screen = d.wait_text("enter run now");
    assert!(
        screen.contains("when   [once] hourly  daily  weekdays  weekly  cron"),
        "{screen}"
    );
    assert!(screen.contains("what   summarize the TODOs"), "{screen}");
    assert!(screen.contains("/project/notes"), "{screen}");
    // `once` asks for no time, name or settings.
    assert!(!screen.contains("name   "), "{screen}");
    d.capture("wizard-once");

    d.press("enter", ENTER);
    d.wait_text("started a run in");
    let (started, ended) = finished_run(&d);
    let screen = d.wait_text("  ok  ");
    d.capture("run-finished");

    let job = started["job"].as_str().unwrap();
    assert!(
        job.starts_with("adhoc-") && job.len() == "adhoc-".len() + 8,
        "{started}"
    );
    assert_eq!(started["status"], "started");
    assert_eq!(started["trigger"], "manual");
    assert_eq!(started["harness"], "claude");
    assert!(
        started["cwd"].as_str().unwrap().ends_with("/project/notes"),
        "{started}"
    );
    // The first job's policy: its model and its five minutes.
    assert_eq!(started["policy"]["model"], "success");
    assert_eq!(started["timeout_s"], 300.0);
    assert_eq!(
        started["policy"]["program"].as_str().unwrap(),
        d.home().join(".local/bin/claude").display().to_string()
    );
    assert_eq!(ended["run_id"], started["run_id"]);
    assert_eq!(ended["status"], "ok", "{ended}");
    assert!(screen.contains(job), "{screen}");

    // A task run once is not a job.
    assert_eq!(fs::read_to_string(d.path("jobs.yaml")).unwrap(), before);
    assert_no_agents(&d);
    d.keep("jobs.yaml");
    d.keep("state/runs.jsonl");
    d.quit();
}

#[test]
fn a_scheduled_job_is_created_edited_and_deleted_leaving_the_rest_of_the_file_as_written() {
    let mut d = fixture("jobs-create-edit-delete", KEPT);
    d.start();
    open_jobs(&mut d);
    // Startup may move pinned folders into the file, so compare against it from here.
    let before = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    d.keep_as("jobs.yaml", "jobs-0-before.yaml");

    d.press("down", DOWN);
    d.press("enter", ENTER);
    d.wait_text("what › the task");
    d.typed("write release notes");
    d.press("enter", ENTER);
    d.wait_text("where › a folder");
    d.press("enter", ENTER);
    d.wait_text("when › once runs it now");
    d.press("right", RIGHT);
    d.press("right", RIGHT);
    let screen = d.wait_text("[daily]");
    assert!(screen.contains("at     09:00"), "{screen}");
    d.press("enter", ENTER);
    d.typed("7.30");
    d.press("enter", ENTER);
    let screen = d.wait_text("a local time as HH:MM, not \"7.30\"");
    d.capture("wizard-bad-time");
    assert!(!screen.contains("name ›"), "{screen}");
    for _ in 0..4 {
        d.press("backspace", b"\x7f");
    }
    d.typed("07:30");
    d.press("enter", ENTER);
    let screen = d.wait_text("name › the job's name");
    // The name follows the task until it is typed.
    assert!(screen.contains("name   write-release-notes"), "{screen}");
    d.typed("triage");
    d.press("enter", ENTER);
    d.wait_text("duplicate job name: triage");
    d.capture("wizard-duplicate-name");
    assert_eq!(fs::read_to_string(d.path("jobs.yaml")).unwrap(), before);
    for _ in 0.."triage".len() {
        d.press("backspace", b"\x7f");
    }
    d.typed("release-notes");
    let screen = d.wait_text("name   release-notes");
    assert!(screen.contains("what   write release notes"), "{screen}");
    assert!(
        screen.contains("when    once  hourly [daily] weekdays  weekly  cron"),
        "{screen}"
    );
    assert!(screen.contains("at     07:30"), "{screen}");
    d.capture("wizard-daily");

    d.press("enter", ENTER);
    // The notice can draw a frame before the job table reloads.
    d.wait_for("the saved job and its row", |s| {
        s.contains("job release-notes saved · install failed")
            && s.contains("release-notes  30 7 * * *  -")
    });
    d.capture("saved");
    let cwd = d.project().canonicalize().unwrap();
    let block = format!(
        "  - name: release-notes\n    schedule: 30 7 * * *\n    cwd: {}\n    prompt: write release notes\n",
        cwd.display()
    );
    let saved = fs::read_to_string(d.path("jobs.yaml")).unwrap();
    assert_eq!(saved, before.replace(KEPT, &format!("{KEPT}{block}")));
    d.keep_as("jobs.yaml", "jobs-1-saved.yaml");
    assert_no_agents(&d);

    select_job(&mut d, "release-notes");
    d.press("ctrl+e", CTRL_E);
    let screen = d.wait_text("what › the task");
    assert!(screen.contains("what   write release notes"), "{screen}");
    assert!(screen.contains("[daily]"), "{screen}");
    assert!(screen.contains("at     07:30"), "{screen}");
    assert!(screen.contains("name   release-notes"), "{screen}");
    d.capture("edit-opened");
    d.typed(" for v2");
    d.press("down", DOWN);
    d.press("down", DOWN);
    d.press("right", RIGHT);
    d.wait_text("[weekdays]");
    d.press("down", DOWN);
    d.typed("18:15");
    d.press("down", DOWN);
    d.press("down", DOWN);
    d.press("enter", ENTER);
    d.wait_text("enabled › on its schedule");
    for _ in 0..3 {
        d.press("down", DOWN);
    }
    d.press("right", RIGHT);
    d.press("down", DOWN);
    d.press("right", RIGHT);
    let screen = d.wait_text("[allow]");
    assert!(screen.contains("timeout_min         ‹ 35 ›"), "{screen}");
    assert!(
        screen.contains("what   write release notes for v2"),
        "{screen}"
    );
    assert!(screen.contains("at     18:15"), "{screen}");
    d.capture("edit-settings");
    d.press("enter", ENTER);
    d.wait_file("jobs.yaml", |t| t.contains("for v2"));
    let screen = d.wait_text("job release-notes saved · install failed");
    assert!(
        screen.contains("release-notes  15 18 * * 1-5  -"),
        "{screen}"
    );
    // The block is replaced in place; the timeout is written as the float the field holds.
    let edited_block = format!(
        "  - name: release-notes\n    schedule: 15 18 * * 1-5\n    cwd: {}\n    prompt: write release notes for v2\n    timeout_min: 35.0\n    overlap: allow\n",
        cwd.display()
    );
    assert_eq!(
        fs::read_to_string(d.path("jobs.yaml")).unwrap(),
        before.replace(KEPT, &format!("{KEPT}{edited_block}"))
    );
    d.capture("edited");
    d.keep_as("jobs.yaml", "jobs-2-edited.yaml");

    select_job(&mut d, "release-notes");
    d.press("ctrl+x", CTRL_X);
    d.wait_text("ctrl+x again to delete job release-notes");
    d.capture("delete-armed");
    assert!(
        fs::read_to_string(d.path("jobs.yaml"))
            .unwrap()
            .contains("release-notes")
    );
    d.press("ctrl+x", CTRL_X);
    let screen = d.wait_text("job release-notes deleted · install failed");
    assert!(!screen.contains("release-notes  "), "{screen}");
    // The job column keeps the width the longer name gave it.
    assert!(
        screen.contains("◆  ✻  -       triage         0 3 * * *      success"),
        "{screen}"
    );
    d.capture("deleted");
    assert_eq!(fs::read_to_string(d.path("jobs.yaml")).unwrap(), before);
    d.keep_as("jobs.yaml", "jobs-3-deleted.yaml");
    assert_no_agents(&d);
    d.quit();
}

#[test]
fn enter_on_a_job_runs_it_with_its_own_policy_and_the_row_shows_the_outcome() {
    let mut d = fixture("jobs-run-job", KEPT);
    d.start();
    let screen = open_jobs(&mut d);
    assert!(screen.contains("▌ ◆  ✻  -       triage"), "{screen}");
    // A job row offers its own keys first.
    assert!(
        screen.contains("enter start job · ctrl+e edit · ctrl+x delete"),
        "{screen}"
    );
    d.capture("jobs");
    d.press("enter", ENTER);
    d.wait_text("run triage requested");
    let (started, ended) = finished_run(&d);
    let screen = d.wait_for("the job row to show ok", |s| {
        let squashed = s
            .split(' ')
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        squashed.contains("▌ ◆ ✻ ok triage") && squashed.contains("✓ ✻ ok ✉ triage")
    });
    // Column widths follow the widest status, so compare with runs of spaces collapsed.
    let squashed = screen
        .split(' ')
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        squashed.contains("▌ ◆ ✻ ok triage 0 3 * * * success"),
        "{screen}"
    );
    // The run joins the session list's runs, unread (✉) whether or not a reload caught it
    // running.
    assert!(squashed.contains("✓ ✻ ok ✉ triage"), "{screen}");
    assert!(screen.contains("● 1 unread"), "{screen}");
    d.capture("ran");
    assert_eq!(started["job"], "triage");
    assert_eq!(started["trigger"], "manual");
    assert_eq!(started["harness"], "claude");
    assert_eq!(
        started["cwd"].as_str().unwrap(),
        d.project().canonicalize().unwrap().display().to_string()
    );
    assert_eq!(started["policy"]["model"], "success");
    assert_eq!(started["timeout_s"], 300.0);
    assert_eq!(ended["run_id"], started["run_id"]);
    assert_eq!(ended["status"], "ok", "{ended}");
    d.keep("state/runs.jsonl");
    d.quit();
}
