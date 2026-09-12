use chrono::Utc;
use cones::{
    config,
    harness::Outcome,
    launchd,
    ledger::{Ledger, Record, Status},
};
use std::{fs, io::Write};

#[test]
fn cron_preserves_day_or_weekday_semantics() {
    let rows = launchd::calendar_intervals("0 2 1 * 1").unwrap();
    assert_eq!(rows.len(), 1);
    // The native launchd dictionary defines Day + Weekday as OR.
    assert_eq!(rows[0].get("Day"), Some(&1));
    assert_eq!(rows[0].get("Weekday"), Some(&1));
}
#[test]
fn cron_handles_steps_ranges_sunday_and_wildcards() {
    let rows = launchd::calendar_intervals("*/15 9-10 * * 0,7").unwrap();
    assert_eq!(rows.len(), 8);
    assert!(
        rows.iter()
            .all(|r| r.get("Weekday") == Some(&0) && !r.contains_key("Day"))
    );
    assert!(launchd::calendar_intervals("* * * * *").unwrap()[0].is_empty());
    assert!(launchd::calendar_intervals("0 2 */2 * 1").is_err());
}
#[test]
fn cron_rejects_invalid_and_explosive_schedules() {
    for s in [
        "* * *",
        "0 24 * * *",
        "*/0 * * * *",
        "0 0 * * MON",
        "0 0 * 0 *",
        "0 0 * * 9",
        "0-59 0-23 1-31 1-12 0-6",
    ] {
        assert!(launchd::calendar_intervals(s).is_err(), "{s}");
    }
}
fn config_text(extra: &str) -> String {
    format!(
        "version: 1\njobs:\n  - name: sample\n    schedule: '0 2 * * *'\n    harness: claude\n    cwd: .\n    prompt: test\n{extra}"
    )
}
#[test]
fn yaml_rejects_typos_duplicates_and_unsupported_codex_tools() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.yaml");
    fs::write(&path, config_text("    budegt_usd: 1\n")).unwrap();
    assert!(
        config::read_jobs(&path)
            .unwrap_err()
            .to_string()
            .contains("invalid jobs")
    );
    fs::write(
        &path,
        config_text("    tools: []\n").replace("harness: claude", "harness: codex"),
    )
    .unwrap();
    let error = config::read_jobs(&path).unwrap_err().to_string();
    assert!(error.contains("no per-tool allowlist") && error.contains("write: false"));
    fs::write(&path, config_text("    timeout_min: .nan\n")).unwrap();
    assert!(config::read_jobs(&path).is_err());
    fs::write(&path, config_text("    env: [NODE_OPTIONS]\n")).unwrap();
    assert!(config::read_jobs(&path).is_err());
    let text = config_text("");
    let duplicate = format!("{text}{}", text.split("jobs:\n").nth(1).unwrap());
    fs::write(&path, duplicate).unwrap();
    assert!(
        config::read_jobs(&path)
            .unwrap_err()
            .to_string()
            .contains("duplicate")
    );
}
#[test]
fn policy_inherits_defaults_and_strips_write_tools() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.yaml");
    let text = config_text("    tools: [Read, Edit, Write, 'Bash(git status *)']\n").replace(
        "jobs:\n",
        "defaults:\n  budget_usd: 0.5\n  max_turns: 4\njobs:\n",
    );
    fs::write(&path, text).unwrap();
    let job = config::read_jobs(&path).unwrap().remove(0);
    assert_eq!(job.budget_usd, 0.5);
    assert_eq!(job.max_turns, Some(4));
    assert_eq!(cones::harness::effective_tools(&job).unwrap(), vec!["Read"]);
}

#[test]
fn scoped_bash_guarantees_are_rejected_without_intercepting_the_harness() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.yaml");
    fs::write(
        &path,
        config_text("    write: true\n    tools: ['Bash(git status *)']\n"),
    )
    .unwrap();
    let mut job = config::read_jobs(&path).unwrap().remove(0);
    let error = cones::harness::effective_tools(&job)
        .unwrap_err()
        .to_string();
    assert!(error.contains("pre-approvals"));
    job.tools = vec!["Bash".into()];
    assert_eq!(cones::harness::effective_tools(&job).unwrap(), vec!["Bash"]);
}
#[test]
fn plist_uses_argument_arrays_and_explicit_environment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.yaml");
    fs::write(&path, config_text("")).unwrap();
    let job = config::read_jobs(&path).unwrap().remove(0);
    let bytes = launchd::render(
        &job,
        std::path::Path::new("/tmp/a & b/cones"),
        &path,
        &dir.path().join("state"),
    )
    .unwrap();
    let value = plist::Value::from_reader_xml(bytes.as_slice()).unwrap();
    let d = value.as_dictionary().unwrap();
    let args = d["ProgramArguments"].as_array().unwrap();
    assert_eq!(args[0].as_string(), Some("/tmp/a & b/cones"));
    assert!(args.iter().any(|v| v.as_string() == Some("schedule")));
    let env = d["EnvironmentVariables"].as_dictionary().unwrap();
    assert!(
        env["PATH"]
            .as_string()
            .unwrap()
            .contains("/opt/homebrew/bin")
    );
    assert!(env["PATH"].as_string().unwrap().contains(".cargo/bin"));
    assert_eq!(d["RunAtLoad"].as_boolean(), Some(false));
}
#[test]
fn parses_claude_fixture_and_detects_permissions_without_exit_code() {
    let mut result = Outcome::default();
    for line in include_str!("fixtures/claude-success.jsonl").lines() {
        result
            .observe(line, "13c73aaf-43d6-4b2c-af51-05763e0c6834")
            .unwrap();
    }
    assert!(result.result_seen && !result.failed && !result.session_mismatch);
    assert_eq!(result.tokens_in, Some(61));
    assert_eq!(result.cost_usd, Some(0.012));
    let mut result = Outcome::default();
    result.observe(r#"{"type":"result","subtype":"success","is_error":false,"permission_denials":[{"tool_name":"Bash"}]}"#,"id").unwrap();
    assert!(result.permission_denied);
}
#[test]
fn ledger_repairs_only_torn_tail_and_never_hides_middle_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::new(dir.path()).unwrap();
    let r = Record::new("test".into(), Status::Started);
    ledger.append(&r).unwrap();
    let path = dir.path().join("runs.jsonl");
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"v\":1")
        .unwrap();
    assert_eq!(ledger.runs().unwrap().len(), 1);
    ledger
        .append(&Record::new("test".into(), Status::Ok))
        .unwrap();
    assert_eq!(
        ledger.runs().unwrap()[0].terminal.as_ref().unwrap().status,
        Status::Ok
    );
    let content = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("broken\n{content}")).unwrap();
    assert!(ledger.runs().is_err());
    assert!(ledger.append(&r).is_err());
}
#[test]
fn workspace_lock_wait_blocks_until_writer_releases() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::new(dir.path()).unwrap();
    let held = ledger.workspace_lock(dir.path()).unwrap().unwrap();
    let state = dir.path().to_owned();
    let waiter = std::thread::spawn(move || {
        let ledger = Ledger::new(&state).unwrap();
        let _f = ledger.workspace_lock_wait(&state).unwrap();
        std::time::Instant::now()
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    let released = std::time::Instant::now();
    drop(held);
    assert!(waiter.join().unwrap() >= released);
    assert!(ledger.workspace_lock(dir.path()).unwrap().is_some());
}
#[test]
fn ledger_lock_budget_and_orphan_status() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::new(dir.path()).unwrap();
    let lock_id = uuid::Uuid::new_v4().to_string();
    let lock = ledger.run_lock(&lock_id).unwrap().unwrap();
    assert!(ledger.run_lock(&lock_id).unwrap().is_none());
    drop(lock);
    assert!(ledger.run_lock(&lock_id).unwrap().is_some());
    let mut start = Record::new("run".into(), Status::Started);
    start.job = Some("job".into());
    start.budget_usd = Some(2.0);
    start.fired_at = Some(Utc::now() - chrono::Duration::minutes(5));
    start.timeout_s = Some(5.0);
    ledger.append(&start).unwrap();
    assert_eq!(ledger.runs().unwrap()[0].status(), "crashed");
    assert_eq!(ledger.reserved_spend("job").unwrap(), 2.0);
    let mut terminal = Record::new("run".into(), Status::Ok);
    terminal.cost_usd = Some(0.25);
    terminal.ended_at = Some(Utc::now());
    ledger.append(&terminal).unwrap();
    assert_eq!(ledger.reserved_spend("job").unwrap(), 0.25);
}
#[test]
fn permission_words_in_read_output_are_data_not_denials() {
    let mut result = Outcome::default();
    for line in include_str!("fixtures/claude-read-permissions.jsonl").lines() {
        result
            .observe(line, "13c73aaf-43d6-4b2c-af51-05763e0c6834")
            .unwrap();
    }
    assert!(result.result_seen && !result.failed && !result.permission_denied);
}

#[test]
fn budget_stop_preserves_reported_per_model_usage() {
    let mut result = Outcome::default();
    result
        .observe(
            include_str!("fixtures/claude-budget-exhausted.jsonl").trim(),
            "1dca15df-bb7c-41bb-9086-b85bd6bd6b83",
        )
        .unwrap();
    assert!(result.failed && !result.permission_denied);
    assert_eq!(result.tokens_in, Some(4781));
    assert_eq!(result.tokens_out, Some(271));
    assert_eq!(result.cost_usd, Some(0.010907));
}

#[test]
fn os_permission_errors_require_a_known_failing_bash_call() {
    for (tool, error, text, expected) in [
        ("Read", true, "cat: file: Permission denied", false),
        (
            "Read",
            false,
            "permission denied, denied, not allowed",
            false,
        ),
        ("Bash", false, "touch: file: Operation not permitted", false),
        (
            "Bash",
            true,
            "Guide: Permission denied is an example.",
            false,
        ),
        (
            "Bash",
            true,
            "Exit code 1\ntouch: file: Operation not permitted",
            true,
        ),
        ("Bash", true, "cat: file: Permission denied", true),
        ("Bash", true, "touch: file: Read-only file system", true),
    ] {
        let mut result = Outcome::default();
        result
            .observe(
                &serde_json::json!({"type":"assistant","message":{"content":[
                    {"type":"tool_use","id":"call","name":tool,"input":{}}
                ]}})
                .to_string(),
                "session",
            )
            .unwrap();
        result
            .observe(
                &serde_json::json!({"type":"user","message":{"content":[
                    {"type":"tool_result","tool_use_id":"call","is_error":error,"content":text}
                ]}})
                .to_string(),
                "session",
            )
            .unwrap();
        assert_eq!(result.permission_denied, expected, "{tool} {error} {text}");
    }
    let mut result = Outcome::default();
    result.observe(r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"unknown","is_error":true,"content":"touch: file: Permission denied"}]}}"#, "session").unwrap();
    assert!(!result.permission_denied);
    result
        .observe(
            r#"{"type":"system","subtype":"permission_denied"}"#,
            "session",
        )
        .unwrap();
    assert!(result.permission_denied);
}

#[test]
fn overlap_allow_requires_read_only_and_workspace_locks_follow_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.yaml");
    fs::write(&path, config_text("    write: true\n    overlap: allow\n")).unwrap();
    assert!(
        config::read_jobs(&path)
            .unwrap_err()
            .to_string()
            .contains("worktree-per-run")
    );
    fs::write(&path, config_text("    overlap: allow\n")).unwrap();
    assert_eq!(
        config::read_jobs(&path).unwrap()[0].overlap,
        config::Overlap::Allow
    );
    let cwd = dir.path().join("repo");
    fs::create_dir(&cwd).unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&cwd, &alias).unwrap();
    let ledger = Ledger::new(&dir.path().join("state")).unwrap();
    let lock = ledger.workspace_lock(&cwd).unwrap().unwrap();
    assert!(ledger.workspace_lock(&alias).unwrap().is_none());
    drop(lock);
    assert!(ledger.workspace_lock(&alias).unwrap().is_some());
}

#[test]
fn fleet_hook_records_sessions_and_counts_tokens_once_per_message() {
    let dir = tempfile::tempdir().unwrap();
    let id = "0f1e2d3c-4b5a-4978-8a1b-2c3d4e5f6a7b";
    let transcript = dir.path().join("t.jsonl");
    // Two streamed content blocks of one message, then a second message.
    let usage = |mid: &str, i: u64, o: u64| {
        format!(
            r#"{{"type":"assistant","message":{{"id":"{mid}","usage":{{"input_tokens":{i},"cache_read_input_tokens":10,"output_tokens":{o}}}}}}}"#
        )
    };
    fs::write(
        &transcript,
        format!(
            "{{\"type\":\"user\"}}\n{{\"type\":\"ai-title\",\"aiTitle\":\"fix the widget\"}}\n{}\n{}\nnot json\n{}\n{}\n",
            usage("m1", 100, 5),
            usage("m1", 100, 5),
            usage("m2", 200, 7),
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"\n**Running** the tests\nsecond line"}]}}"#
        ),
    )
    .unwrap();
    let payload = |event: &str| {
        serde_json::json!({"session_id": id, "hook_event_name": event, "cwd": "/tmp/repo",
            "transcript_path": transcript, "tool_name": "Bash"})
    };
    cones::fleet::record(dir.path(), 42, &payload("SessionStart")).unwrap();
    let get = || cones::fleet::find(dir.path(), id).unwrap().unwrap();
    let first = get();
    assert_eq!(
        (first.pid, first.state.as_str(), first.tokens_in),
        (Some(42), "idle", None)
    );
    assert_eq!(first.cwd, std::path::Path::new("/tmp/repo"));
    assert_eq!(
        (first.title.as_deref(), first.last.as_deref()),
        (Some("fix the widget"), Some("Running the tests"))
    );
    cones::fleet::record(dir.path(), 42, &payload("PostToolUse")).unwrap();
    assert_eq!(get().tool.as_deref(), Some("Bash"));
    let notify = |kind: &str| {
        let mut p = payload("Notification");
        p["notification_type"] = kind.into();
        p
    };
    cones::fleet::record(dir.path(), 42, &notify("auth_success")).unwrap();
    assert_eq!(get().state, "active");
    cones::fleet::record(dir.path(), 42, &notify("permission_prompt")).unwrap();
    assert_eq!(get().state, "blocked");
    cones::fleet::record(dir.path(), 42, &notify("idle_prompt")).unwrap();
    assert_eq!(get().state, "idle");
    cones::fleet::record(dir.path(), 42, &payload("Stop")).unwrap();
    let idle = get();
    assert_eq!(
        (idle.state.as_str(), idle.tokens_in, idle.tokens_out),
        ("idle", Some(320), Some(12))
    );
    cones::fleet::record(dir.path(), 42, &payload("SessionEnd")).unwrap();
    assert_eq!(get().state, "exited");
    let bad = serde_json::json!({"session_id": "../escape", "hook_event_name": "Stop"});
    assert!(cones::fleet::record(dir.path(), 1, &bad).is_err());
    assert_eq!(cones::fleet::sessions(dir.path()).unwrap().len(), 1);
}
#[test]
fn fleet_hook_install_merges_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    fs::write(&settings, r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"say done"}]}]}}"#).unwrap();
    assert!(!cones::fleet::installed(&settings));
    let cmd = cones::fleet::hook_command(std::path::Path::new("/opt/cones"), dir.path());
    assert!(cmd.ends_with(" hook $PPID"));
    cones::fleet::install(&settings, &cmd).unwrap();
    cones::fleet::install(&settings, "'/moved/cones' --state-dir '/x' hook $PPID").unwrap();
    let root: serde_json::Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert_eq!(root["model"], "opus");
    let stop = root["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 2, "user's own Stop hook kept, one cones entry");
    assert_eq!(
        stop[1]["hooks"][0]["command"],
        "'/moved/cones' --state-dir '/x' hook $PPID"
    );
    assert!(cones::fleet::installed(&settings));
    for event in cones::fleet::EVENTS {
        assert_eq!(
            root["hooks"][event].as_array().unwrap().len(),
            if event == "Stop" { 2 } else { 1 }
        );
    }
}
#[test]
fn fleet_view_lists_live_sessions_and_collapses_cones_runs() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::new(dir.path()).unwrap();
    let owned = "11111111-1111-4111-8111-111111111111";
    let mut started = Record::new("run-1".into(), Status::Started);
    started.session_id = Some(owned.into());
    ledger.append(&started).unwrap();
    let session = |id: &str, pid: u32, state: &str| cones::fleet::Session {
        v: 1,
        session_id: id.into(),
        harness: "claude".into(),
        cwd: dirs::home_dir().unwrap().join("src/repo"),
        state: state.into(),
        updated: Utc::now(),
        event: Some("Stop".into()),
        tool: None,
        pid: Some(pid),
        transcript_path: None,
        tokens_in: Some(12_500),
        tokens_out: Some(300),
        cost_usd: Some(0.42),
        title: Some("fix the widget".into()),
        last: Some("Running the tests".into()),
    };
    let live = "22222222-2222-4222-8222-222222222222";
    cones::fleet::write(dir.path(), &session(live, std::process::id(), "idle")).unwrap();
    cones::fleet::write(dir.path(), &session(owned, std::process::id(), "active")).unwrap();
    cones::fleet::write(
        dir.path(),
        &session("33333333-3333-4333-8333-333333333333", 4_000_000, "active"),
    )
    .unwrap();
    let rows = cones::tui::fleet_rows(dir.path(), &ledger.runs().unwrap()).unwrap();
    assert_eq!(
        rows.iter()
            .map(|s| s.session_id.as_str())
            .collect::<Vec<_>>(),
        [live],
        "the cones-owned session collapses into its run row and the dead pid is stale"
    );
    let list = cones::tui::list(&dir.path().join("none.yaml"), dir.path()).unwrap();
    let row = list.lines().find(|l| l.starts_with(live)).unwrap();
    assert!(row.starts_with(&format!("{live}\tidle\t")), "{row}");
    for s in ["✻ claude", "fix the widget", "12k/300", "Running the tests"] {
        assert!(row.contains(s), "{row}");
    }
    let lines: Vec<&str> = list.lines().collect();
    assert!(
        lines[..3].iter().all(|l| l.starts_with("hdr\t-\t"))
            && lines[1].contains("0 working · 0 need input · 1 idle"),
        "three pinned header lines carry the summary"
    );
    assert!(lines[3] == "hdr\t-\t", "a blank row separates sections");
    assert!(row.contains("△"), "{row}");
    assert!(
        lines.iter().any(|l| l.contains("~/src/repo")),
        "sessions are grouped by directory"
    );
    assert!(lines.iter().any(|l| l.starts_with("run-1\tstarted")));
    let data = cones::tui::Data::load(&dir.path().join("none.yaml"), dir.path()).unwrap();
    let by_state = data.rows(true);
    let headers: Vec<String> = by_state
        .iter()
        .filter(|r| r.kind == cones::tui::Kind::Header)
        .map(|r| r.cells[0].0.clone())
        .collect();
    assert_eq!(
        headers,
        ["idle", "runs"],
        "grouping by state names the state"
    );
    let pane = data.details(&cones::tui::Kind::Session(live.into(), "idle".into()));
    assert!(
        pane[0] == "~/src/repo" && pane[1].contains("idle Stop"),
        "{pane:?}"
    );
}
#[test]
fn adhoc_job_borrows_policy_or_defaults_to_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let plain = cones::config::adhoc(None, "fix it", dir.path()).unwrap();
    assert!(plain.name.starts_with("adhoc-") && !plain.write && plain.enabled);
    assert_eq!(plain.tools, ["Read", "Grep", "Glob"]);
    let mut template = plain.clone();
    template.write = true;
    template.budget_usd = 9.0;
    let borrowed = cones::config::adhoc(Some(&template), "ship it", dir.path()).unwrap();
    assert!(borrowed.write && borrowed.budget_usd == 9.0 && borrowed.name != template.name);
    assert_eq!(borrowed.prompt, "ship it");
    assert!(cones::config::adhoc(None, "  ", dir.path()).is_err());
}
#[test]
fn session_columns_align_across_directory_groups() {
    let dir = tempfile::tempdir().unwrap();
    for (id, cwd, title) in [
        ("44444444-4444-4444-8444-444444444444", "a", "short"),
        (
            "55555555-5555-4555-8555-555555555555",
            "b",
            "a much longer session title",
        ),
    ] {
        cones::fleet::write(
            dir.path(),
            &cones::fleet::Session {
                v: 1,
                session_id: id.into(),
                harness: "claude".into(),
                cwd: dir.path().join(cwd),
                state: "idle".into(),
                updated: Utc::now(),
                event: None,
                tool: None,
                pid: Some(std::process::id()),
                transcript_path: None,
                tokens_in: None,
                tokens_out: None,
                cost_usd: None,
                title: Some(title.into()),
                last: None,
            },
        )
        .unwrap();
    }
    let data = cones::tui::Data::load(&dir.path().join("none.yaml"), dir.path()).unwrap();
    let widths: Vec<usize> = data
        .rows(false)
        .iter()
        .filter(|r| matches!(r.kind, cones::tui::Kind::Session(..)))
        .map(|r| r.cells[2].0.chars().count())
        .collect();
    assert_eq!(widths.len(), 2);
    assert_eq!(
        widths[0], widths[1],
        "title column padded to one width across groups"
    );
}

#[test]
fn fleet_exchange_is_the_last_prompt_and_the_full_reply() {
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().join("t.jsonl");
    fs::write(
        &t,
        concat!(
            r#"{"type":"user","message":{"content":"old prompt"}}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"old reply"}]}}"#, "\n",
            r#"{"type":"user","message":{"content":[{"type":"text","text":"fix it\nplease"}]}}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#, "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"**Fixed** it\nsecond line"}]}}"#, "\n",
            "not json\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#, "\n",
        ),
    )
    .unwrap();
    assert_eq!(
        cones::fleet::exchange(&t),
        [
            "> fix it",
            "> please",
            "",
            "Fixed it",
            "second line",
            "",
            "done"
        ]
    );
    assert!(cones::fleet::exchange(&dir.path().join("missing.jsonl")).is_empty());
}
