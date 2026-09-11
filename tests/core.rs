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
fn backend_rejects_nonlocal_endpoints() {
    for url in [
        "https://example.org:7878",
        "http://example.org:7878",
        "http://127.0.0.1:7878/path",
        "http://user:pass@localhost:7878",
    ] {
        assert!(cones::control::AgentConsole::new(url).is_err());
    }
    assert!(cones::control::AgentConsole::new("http://[::1]:7878").is_ok());
}

#[test]
fn health_api_accepts_patch_versions_but_not_unverified_minor_versions() {
    for version in ["0.3.0", "0.3.1", "0.3.99", "0.3.1+build"] {
        assert!(cones::control::compatible_health_version(version));
    }
    for version in ["0.2.9", "0.4.0", "1.0.0", "0.3.1-rc.1", "unknown"] {
        assert!(!cones::control::compatible_health_version(version));
    }
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
fn healthy_console_cannot_silently_drop_execution_policy() {
    use cones::control::{AgentConsole, ControlPlane};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buf = [0; 4096];
        let n = socket.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
        let body = r#"{"ok":true,"version":"0.3.0","auth":"token"}"#;
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
        request
    });
    let backend = AgentConsole::new(&format!("http://{address}")).unwrap();
    let error = backend
        .spawn(std::path::Path::new("/unused/cones"), "unused-run")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("policy-aware spawn is unavailable")
    );
    assert!(server.join().unwrap().starts_with("GET /api/health "));
}
