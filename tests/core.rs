use chrono::Utc;
use cones::{
    config,
    harness::Outcome,
    launchd,
    ledger::{Ledger, Record, Status},
};
use std::{fs, io::Write};

#[test]
fn run_times_display_in_the_local_timezone_and_keep_utc_in_json() {
    use cones::config::HarnessKind;
    use std::process::Command;
    let d = tempfile::tempdir().unwrap();
    let jobs = d.path().join("jobs.yaml");
    fs::write(
        &jobs,
        "version: 3\nrun_columns: [started, ended]\njobs: []\n",
    )
    .unwrap();
    let ledger = Ledger::new(d.path()).unwrap();
    for (id, at) in [
        ("summer", "2026-09-17T22:30:00Z"),
        ("winter", "2026-01-17T22:30:00Z"),
    ] {
        let mut record = Record::new(id.into(), Status::Started);
        record.job = Some(id.into());
        record.fired_at = Some(at.parse().unwrap());
        record.harness = Some(HarnessKind::Claude);
        ledger.append(&record).unwrap();
        let mut end = Record::new(id.into(), Status::Ok);
        end.ended_at = record.fired_at.map(|at| at + chrono::Duration::minutes(5));
        ledger.append(&end).unwrap();
    }
    let run = |zone: &str, args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_cones"))
            .args([
                "--jobs",
                jobs.to_str().unwrap(),
                "--state-dir",
                d.path().to_str().unwrap(),
            ])
            .args(args)
            .env("TZ", zone)
            .env("HOME", d.path())
            .env("CLAUDE_CONFIG_DIR", d.path().join("claude"))
            .env("CODEX_HOME", d.path().join("missing-codex"))
            .env("PI_CODING_AGENT_DIR", d.path().join("missing-pi"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let shown = run("Asia/Jerusalem", &["__list"]);
    for time in [
        "09-18 01:30:00",
        "09-18 01:35:00",
        "01-18 00:30:00",
        "01-18 00:35:00",
    ] {
        assert!(shown.contains(time), "{time}: {shown}");
    }
    let utc = run("UTC", &["__list"]);
    assert!(
        utc.contains("09-17 22:30:00") && utc.contains("01-17 22:30:00"),
        "{utc}"
    );
    let ls = run("Asia/Jerusalem", &["ls"]);
    assert!(ls.contains("2026-09-18T01:30:00+03:00"), "{ls}");
    assert!(ls.contains("2026-01-18T00:30:00+02:00"), "{ls}");
    let json = run("Asia/Jerusalem", &["ls", "--json"]);
    assert!(
        json.contains("2026-09-17T22:30:00Z") && json.contains("2026-01-17T22:30:00Z"),
        "{json}"
    );
}

/// `--dir` is the coordinator's read: the folder it was launched in, plus the worktrees under it.
#[test]
fn ls_scopes_runs_and_sessions_to_a_folder_and_its_worktrees() {
    use cones::config::HarnessKind;
    use std::process::Command;
    let d = tempfile::tempdir().unwrap();
    let jobs = d.path().join("jobs.yaml");
    fs::write(&jobs, "version: 3\njobs: []\n").unwrap();
    let claude = d.path().join("claude");
    // A folder the coordinator owns, a worktree under it, and a sibling it must not report.
    let project = d.path().join("project");
    let worktree = project.join(".worktrees/feature");
    let sibling = d.path().join("other");
    for path in [&project, &worktree, &sibling] {
        fs::create_dir_all(path).unwrap();
    }
    // Rows carry the resolved path while --dir gets the raw one, so /var and /private/var must
    // still compare equal on macOS.
    let real = |p: &std::path::Path| p.canonicalize().unwrap();
    let ledger = Ledger::new(d.path()).unwrap();
    for (id, cwd) in [("mine", real(&project)), ("theirs", real(&sibling))] {
        let mut record = Record::new(id.into(), Status::Started);
        record.job = Some(id.into());
        record.fired_at = Some("2026-09-17T22:30:00Z".parse().unwrap());
        record.harness = Some(HarnessKind::Claude);
        record.cwd = Some(cwd);
        ledger.append(&record).unwrap();
        ledger.append(&Record::new(id.into(), Status::Ok)).unwrap();
    }
    let registry = claude.join("sessions");
    fs::create_dir_all(&registry).unwrap();
    for (name, cwd) in [
        ("in-worktree", real(&worktree)),
        ("elsewhere", real(&sibling)),
    ] {
        fs::write(
            registry.join(format!("{name}.json")),
            serde_json::json!({
                "pid": std::process::id(), "sessionId": name, "cwd": cwd,
                "kind": "interactive", "status": "idle"
            })
            .to_string(),
        )
        .unwrap();
    }
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_cones"))
            .args([
                "--jobs",
                jobs.to_str().unwrap(),
                "--state-dir",
                d.path().to_str().unwrap(),
            ])
            .args(args)
            .env("HOME", d.path())
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env("CODEX_HOME", d.path().join("missing-codex"))
            .env("PI_CODING_AGENT_DIR", d.path().join("missing-pi"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let all = run(&["ls"]);
    for row in ["mine", "theirs", "in-worktree", "elsewhere"] {
        assert!(all.contains(row), "unscoped read is missing {row}: {all}");
    }
    let scoped = run(&["ls", "--dir", project.to_str().unwrap()]);
    assert!(scoped.contains("mine"), "the folder's own run: {scoped}");
    assert!(
        scoped.contains("in-worktree"),
        "a session in a worktree under the folder: {scoped}"
    );
    assert!(
        !scoped.contains("theirs"),
        "a sibling folder's run leaked: {scoped}"
    );
    assert!(
        !scoped.contains("elsewhere"),
        "a sibling folder's session leaked: {scoped}"
    );
    let json = run(&["ls", "--dir", project.to_str().unwrap(), "--json"]);
    let kinds: Vec<String> = json
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["kind"].to_string())
        .collect();
    assert_eq!(kinds, ["\"run\"", "\"session\""], "row kinds: {json}");
}

#[test]
fn cron_preserves_day_or_weekday_semantics() {
    let rows = launchd::calendar_intervals("0 2 1 * 1").unwrap();
    assert_eq!(rows.len(), 1);
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
fn yaml_rejects_typos_duplicates_and_unknown_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.yaml");
    fs::write(&path, config_text("    budegt_usd: 1\n")).unwrap();
    assert!(
        config::read_jobs(&path)
            .unwrap_err()
            .to_string()
            .contains("invalid jobs")
    );
    fs::write(&path, config_text("    tools: [Read]\n")).unwrap();
    let error = config::read_jobs(&path).unwrap_err().to_string();
    assert!(
        error.contains("invalid jobs"),
        "the old allowlist is an unknown field: {error}"
    );
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
fn policy_inherits_defaults_and_write_decides_the_allowlist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.yaml");
    let text = config_text("").replace("jobs:\n", "defaults:\n  timeout_min: 5\njobs:\n");
    fs::write(&path, text).unwrap();
    let mut job = config::read_jobs(&path).unwrap().remove(0);
    assert_eq!(job.timeout_min, 5.0);
    assert_eq!(
        cones::harness::effective_tools(&job),
        ["Read", "Grep", "Glob"]
    );
    job.write = true;
    assert_eq!(
        cones::harness::effective_tools(&job),
        ["Read", "Grep", "Glob", "Edit", "Write", "Bash"]
    );
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
fn ledger_lock_cost_and_orphan_status() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::new(dir.path()).unwrap();
    let lock_id = uuid::Uuid::new_v4().to_string();
    let lock = ledger.run_lock(&lock_id).unwrap().unwrap();
    assert!(ledger.run_lock(&lock_id).unwrap().is_none());
    drop(lock);
    assert!(ledger.run_lock(&lock_id).unwrap().is_some());
    let mut start = Record::new("run".into(), Status::Started);
    start.job = Some("job".into());
    start.fired_at = Some(Utc::now() - chrono::Duration::minutes(5));
    start.timeout_s = Some(5.0);
    ledger.append(&start).unwrap();
    assert_eq!(ledger.runs().unwrap()[0].status(), "crashed");
    let mut terminal = Record::new("run".into(), Status::Ok);
    terminal.cost_usd = Some(0.25);
    terminal.ended_at = Some(Utc::now());
    ledger.append(&terminal).unwrap();
    let list = cones::tui::list(&dir.path().join("none.yaml"), dir.path(), dir.path()).unwrap();
    let row = list.lines().find(|l| l.starts_with("run\tok\t")).unwrap();
    assert!(
        row.contains("$0.25"),
        "the run row shows the ledger's dollars: {row}"
    );
    assert_eq!(cones::fleet::cost(0.004), "$0.0040");
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
fn overlap_allow_is_valid_for_writers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.yaml");
    fs::write(&path, config_text("    write: true\n    overlap: allow\n")).unwrap();
    let job = &config::read_jobs(&path).unwrap()[0];
    assert!(job.write);
    assert_eq!(job.overlap, config::Overlap::Allow);
}

fn registry(claude: &std::path::Path, name: &str, entry: serde_json::Value) {
    let dir = claude.join("sessions");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{name}.json")), entry.to_string()).unwrap();
}

#[test]
fn claude_session_cost_comes_only_from_the_saved_report_and_keeps_zero() {
    let dir = tempfile::tempdir().unwrap();
    let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    registry(
        dir.path(),
        id,
        serde_json::json!({
            "pid": std::process::id(), "sessionId": id, "cwd": dir.path(),
            "kind": "interactive", "status": "idle",
        }),
    );
    fs::create_dir(dir.path().join("statusline")).unwrap();
    let path = dir.path().join("statusline").join(format!("{id}.json"));
    let cost = || {
        cones::fleet::find(dir.path(), id)
            .unwrap()
            .unwrap()
            .cost_usd
    };
    assert_eq!(cost(), None);
    for (reported, expected) in [
        (serde_json::json!(0), Some(0.0)),
        (serde_json::json!(0.125), Some(0.125)),
        (serde_json::json!(-1), None),
        (serde_json::json!("0.125"), None),
        (serde_json::Value::Null, None),
    ] {
        fs::write(&path, serde_json::json!({"cost":{"total_cost_usd":reported},"context_window":{"context_window_size":200000}}).to_string()).unwrap();
        assert_eq!(cost(), expected);
    }
    fs::write(path, r#"{"usage":{"input_tokens":1000000}}"#).unwrap();
    assert_eq!(cost(), None, "token counts do not supply a price");
}
fn transcript(claude: &std::path::Path, cwd: &std::path::Path, id: &str, prompt: u64, text: &str) {
    let project = claude.join("projects").join(
        cwd.to_string_lossy()
            .replace(|c: char| !c.is_ascii_alphanumeric(), "-"),
    );
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join(format!("{id}.jsonl")),
        format!(
            "{{\"type\":\"ai-title\",\"aiTitle\":\"fix the widget\"}}\n{{\"type\":\"assistant\",\"timestamp\":\"2026-09-12T10:56:35.556Z\",\"message\":{{\"id\":\"m\",\"model\":\"claude-fable-5-1\",\"usage\":{{\"input_tokens\":{prompt},\"output_tokens\":300}},\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}\n"
        ),
    )
    .unwrap();
}
#[test]
fn doctor_spots_entries_left_by_the_removed_hook() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    assert!(
        !cones::fleet::stale_hook(&settings),
        "no file, nothing stale"
    );
    fs::write(
        &settings,
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"say done"}]}]}}"#,
    )
    .unwrap();
    assert!(
        !cones::fleet::stale_hook(&settings),
        "the user's own hooks are not ours"
    );
    fs::write(&settings, r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"say done"}]},{"hooks":[{"type":"command","command":"'/x/cones' --state-dir '/y' hook $PPID"}]}]}}"#).unwrap();
    assert!(cones::fleet::stale_hook(&settings));
    fs::write(&settings, "not json").unwrap();
    assert!(!cones::fleet::stale_hook(&settings));
}
#[test]
fn fleet_reads_claude_registry_and_counts_tokens_once_per_message() {
    let dir = tempfile::tempdir().unwrap();
    let claude = dir.path();
    let id = "0f1e2d3c-4b5a-4978-8a1b-2c3d4e5f6a7b";
    let cwd = "/tmp/re.po";
    let project = claude.join("projects/-tmp-re-po");
    fs::create_dir_all(&project).unwrap();
    let transcript = project.join(format!("{id}.jsonl"));
    // One streamed message in two blocks, then another message; the first timestamp is an attachment.
    let usage = |mid: &str, i: u64, o: u64, ts: &str| {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{mid}","model":"claude-fable-5-1","usage":{{"input_tokens":{i},"cache_read_input_tokens":10,"output_tokens":{o}}}}}}}"#
        )
    };
    fs::write(
        &transcript,
        format!(
            "{{\"type\":\"user\"}}\n{{\"type\":\"attachment\",\"timestamp\":\"2026-09-12T10:56:31.487Z\"}}\n{{\"type\":\"ai-title\",\"aiTitle\":\"fix the widget\"}}\n{}\n{}\nnot json\n{}\n{}\n",
            usage("m1", 100, 5, "2026-09-12T10:56:35.556Z"),
            usage("m1", 100, 5, "2026-09-12T10:56:35.556Z"),
            usage("m2", 200, 7, "2026-09-12T10:57:00.250Z"),
            r#"{"type":"assistant","timestamp":"2026-09-12T10:57:01.750Z","message":{"content":[{"type":"text","text":"\n**Running** the tests\nsecond line"}]}}"#
        ),
    )
    .unwrap();
    let me = std::process::id();
    let entry = |status: &str| {
        serde_json::json!({"pid": me, "sessionId": id, "cwd": cwd, "kind": "interactive",
            "status": status, "startedAt": 1757682871892i64, "updatedAt": 1757682900000i64, "name": "job a"})
    };
    registry(claude, id, entry("busy"));
    let get = || cones::fleet::find(claude, id).unwrap().unwrap();
    let s = get();
    assert_eq!(
        (s.pid, s.state.as_str(), s.kind.as_deref()),
        (Some(me), "active", Some("interactive"))
    );
    assert_eq!(s.cwd, std::path::Path::new(cwd));
    assert_eq!(s.transcript_path.as_deref(), Some(transcript.as_path()));
    assert_eq!(
        (s.title.as_deref(), s.last.as_deref()),
        (Some("fix the widget"), Some("Running the tests")),
        "title and last line come from the transcript, not the registry name"
    );
    assert_eq!((s.tokens_in, s.tokens_out), (Some(320), Some(12)));
    assert_eq!(
        (s.context_tokens, s.context_window, s.model.as_deref()),
        (Some(210), None, Some("claude-fable-5-1")),
        "the last message's prompt is the context in use and its model id is shown verbatim"
    );
    assert_eq!(
        cones::fleet::context(&s),
        "210",
        "no window until the harness states one"
    );
    // Only a saved statusLine payload can supply the window size.
    let sidecar = claude.join("statusline");
    fs::create_dir_all(&sidecar).unwrap();
    fs::write(
        sidecar.join(format!("{id}.json")),
        r#"{"session_id":"x","model":{"id":"claude-fable-5-1[1m]"},"context_window":{"context_window_size":200000,"used_percentage":0.1}}"#,
    )
    .unwrap();
    let s = get();
    assert_eq!(
        (s.context_window, cones::fleet::context(&s).as_str()),
        (Some(200_000), "210/200k")
    );
    let rfc = |t: Option<chrono::DateTime<chrono::Utc>>| t.map(|t| t.to_rfc3339());
    assert_eq!(
        (rfc(s.started), rfc(s.last_activity)),
        (
            Some("2026-09-12T10:56:31.487+00:00".into()),
            Some("2026-09-12T10:57:01.750+00:00".into())
        ),
        "start and last activity are the transcript's first and last timestamps, not the registry's"
    );
    // Model names and usage thresholds do not report a window. Grow the transcript to invalidate its cache.
    fs::write(claude.join("settings.json"), r#"{"model":"opus[1m]"}"#).unwrap();
    let mut t = fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    writeln!(t, "{}", usage("m3", 300_000, 1, "2026-09-12T11:00:00.100Z")).unwrap();
    // Claude's placeholder for a turn no model answered: model `<synthetic>`, all-zero usage.
    let synthetic = r#"{"type":"assistant","timestamp":"2026-09-12T11:00:05.500Z","message":{"id":"m4","model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0},"content":[{"type":"text","text":"No response requested."}]}}"#;
    writeln!(t, "{synthetic}").unwrap();
    let big = get();
    assert_eq!(
        (
            big.context_tokens,
            big.model.as_deref(),
            rfc(big.last_activity)
        ),
        (
            Some(300_010),
            Some("claude-fable-5-1"),
            Some("2026-09-12T11:00:05.500+00:00".into())
        ),
        "a <synthetic> line is activity but no model report: context and model keep the real one"
    );
    // Over the stated window shows as reported, never clamped or re-guessed.
    assert_eq!(cones::fleet::context(&big), "300k/200k");
    assert_eq!((big.tokens_in, big.tokens_out), (Some(300_330), Some(13)));
    for (status, state) in [
        ("idle", "idle"),
        ("shell", "active"),
        ("busy", "active"),
        ("waiting", "blocked"),
        ("parked", "parked"),
    ] {
        registry(claude, id, entry(status));
        assert_eq!(get().state, state, "{status}");
    }
    // Background jobs supply detail and a fallback transcript path, but timestamps still require a transcript.
    let other = "22222222-2222-4222-8222-222222222222";
    fs::create_dir_all(claude.join("jobs/aaaaaaaa")).unwrap();
    fs::write(
        claude.join("jobs/aaaaaaaa/state.json"),
        r#"{"state":"working","name":"install push clear","detail":"Inspecting job state files","updatedAt":"2026-09-12T13:14:31.892Z","linkScanPath":"/t/b.jsonl","cwd":"/src/launch"}"#,
    )
    .unwrap();
    let bg = |job: &str| {
        serde_json::json!({"pid": me, "sessionId": other, "cwd": "/src/b", "kind": "bg",
            "jobId": job, "status": "busy", "name": "aaaaaaaa", "startedAt": 1757682871892i64, "updatedAt": 1757682900000i64})
    };
    registry(claude, other, bg("aaaaaaaa"));
    let b = cones::fleet::find(claude, other).unwrap().unwrap();
    assert_eq!(
        (
            b.kind.as_deref(),
            b.title.as_deref(),
            b.last.as_deref(),
            b.transcript_path.as_deref().and_then(|p| p.to_str()),
            b.cwd.to_str(),
        ),
        (
            Some("bg"),
            Some("install push clear"),
            Some("Inspecting job state files"),
            Some("/t/b.jsonl"),
            Some("/src/launch"),
        ),
        "a job's row sits in its launch directory and carries the job's name, as in `claude \
         agents`, even after the session entered a worktree and the registry cwd moved and \
         while a claimed spare still holds its 8-hex id as the registry name"
    );
    assert_eq!(
        (
            b.started,
            b.last_activity,
            b.model,
            b.context_tokens,
            b.tokens_in
        ),
        (None, None, None, None, None),
        "the registry's startedAt and updatedAt never stand in for the transcript"
    );
    // Registry busy must override a stale finished-job state.
    for state in [
        r#"{"state":"done"}"#,
        r#"{"state":"failed"}"#,
        r#"{"state":"stopped"}"#,
        r#"{"state":"working","tempo":"blocked"}"#,
    ] {
        fs::write(claude.join("jobs/aaaaaaaa/state.json"), state).unwrap();
        let b = cones::fleet::find(claude, other).unwrap().unwrap();
        assert_eq!(b.state, "active", "{state}");
    }
    let mut idle = bg("aaaaaaaa");
    idle["status"] = "idle".into();
    registry(claude, other, idle.clone());
    for (state, expect) in [
        (r#"{"state":"done"}"#, "done"),
        (r#"{"state":"failed"}"#, "failed"),
        (r#"{"state":"stopped"}"#, "stopped"),
    ] {
        fs::write(claude.join("jobs/aaaaaaaa/state.json"), state).unwrap();
        let b = cones::fleet::find(claude, other).unwrap().unwrap();
        assert_eq!(b.state, expect, "{state}");
    }
    fs::write(
        claude.join("jobs/aaaaaaaa/state.json"),
        r#"{"state":"working","tempo":"blocked"}"#,
    )
    .unwrap();
    registry(claude, other, idle);
    let b = cones::fleet::find(claude, other).unwrap().unwrap();
    assert_eq!(
        b.state, "blocked",
        "a blocked tempo on an idle status is input"
    );
    registry(claude, other, bg("../x"));
    let b = cones::fleet::find(claude, other).unwrap().unwrap();
    assert_eq!(
        (b.last.as_deref(), b.transcript_path.as_deref(), b.started),
        (None, None, None),
        "an unsafe job id reads no file"
    );
    // Reject dead/reused pids, unsafe ids, spares and junk; registry timestamps are optional.
    registry(
        claude,
        "dead",
        serde_json::json!({"pid": 4_000_000, "sessionId": "33333333-3333-4333-8333-333333333333", "cwd": "/x", "status": "busy"}),
    );
    registry(
        claude,
        "reused",
        serde_json::json!({"pid": me, "sessionId": "44444444-4444-4444-8444-444444444444", "cwd": "/x",
            "status": "busy", "startedAt": 1i64, "procStart": "Thu Jan  1 00:00:00 1970"}),
    );
    registry(
        claude,
        "stampless",
        serde_json::json!({"pid": me, "sessionId": "55555555-5555-4555-8555-555555555555", "cwd": "/x", "status": "busy"}),
    );
    let raw = "66666666-6666-4666-8666-666666666666";
    registry(
        claude,
        "raw",
        serde_json::json!({"pid": me, "sessionId": raw, "cwd": "/x", "status": "compacting", "startedAt": 2i64}),
    );
    assert_eq!(
        cones::fleet::find(claude, raw).unwrap().unwrap().state,
        "compacting",
        "an unknown registry status is shown as Claude's word, not as active"
    );
    fs::remove_file(claude.join("sessions/raw.json")).unwrap();
    registry(
        claude,
        "escape",
        serde_json::json!({"pid": me, "sessionId": "../escape", "cwd": "/x", "status": "busy"}),
    );
    registry(
        claude,
        "spare",
        serde_json::json!({"pid": me, "sessionId": "77777777-7777-4777-8777-777777777777", "cwd": "/x",
            "status": "idle", "kind": "bg", "spare": true}),
    );
    fs::write(claude.join("sessions/junk.json"), "not json").unwrap();
    fs::write(claude.join("sessions/1.abc.key"), "k").unwrap();
    let ids: Vec<String> = cones::fleet::sessions(claude)
        .unwrap()
        .into_iter()
        .map(|s| s.session_id)
        .collect();
    let stampless = "55555555-5555-4555-8555-555555555555";
    assert_eq!(
        ids,
        [id, other, stampless],
        "a reported start sorts first; sessions whose transcript reports none follow, by id"
    );
    assert!(
        cones::fleet::sessions(&claude.join("nowhere"))
            .unwrap()
            .is_empty()
    );
}
fn plain(s: &str) -> String {
    let mut out = String::new();
    let mut skip = false;
    for c in s.chars() {
        match c {
            '\x1b' => skip = true,
            'm' if skip => skip = false,
            _ if !skip => out.push(c),
            _ => {}
        }
    }
    out
}

#[test]
fn fleet_view_lists_live_sessions_and_collapses_cones_runs() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::new(dir.path()).unwrap();
    let owned = "11111111-1111-4111-8111-111111111111";
    let mut started = Record::new("run-1".into(), Status::Started);
    started.session_id = Some(owned.into());
    ledger.append(&started).unwrap();
    let cwd = dirs::home_dir().unwrap().join("src/repo");
    let me = std::process::id();
    let session = |id: &str, pid: u32, status: &str| {
        serde_json::json!({"pid": pid, "sessionId": id, "cwd": cwd, "kind": "interactive",
            "status": status, "name": "fix the widget", "startedAt": 1757682871892i64})
    };
    let live = "22222222-2222-4222-8222-222222222222";
    registry(dir.path(), live, session(live, me, "idle"));
    transcript(dir.path(), &cwd, live, 100_000, "Running the tests");
    registry(dir.path(), owned, session(owned, me, "busy"));
    let dead = "33333333-3333-4333-8333-333333333333";
    registry(dir.path(), dead, session(dead, 4_000_000, "busy"));
    let rows = cones::tui::fleet_rows(
        dir.path(),
        dir.path(),
        &ledger.runs().unwrap(),
        &cones::config::Policy::default(),
    )
    .unwrap();
    assert_eq!(
        rows.iter()
            .map(|s| s.session_id.as_str())
            .collect::<Vec<_>>(),
        [live],
        "the cones-owned session collapses into its run row and the dead pid is stale"
    );
    let json = serde_json::to_value(&rows[0]).unwrap();
    assert_eq!(
        (
            json["model"].as_str(),
            json["started"].as_str(),
            json["last_activity"].as_str(),
            json["context_tokens"].as_u64(),
            json.get("context_window"),
            json.get("updated"),
        ),
        (
            Some("claude-fable-5-1"),
            Some("2026-09-12T10:56:35.556Z"),
            Some("2026-09-12T10:56:35.556Z"),
            Some(100_000),
            None,
            None,
        ),
        "{json}"
    );
    let list = cones::tui::list(&dir.path().join("none.yaml"), dir.path(), dir.path()).unwrap();
    let row = list.lines().find(|l| l.starts_with(live)).unwrap();
    assert!(row.starts_with(&format!("{live}\tidle\t")), "{row}");
    for s in [
        "✻  ",
        "idle  ",
        "fix the widget",
        "Fable 5.1",
        "100k  ",
        "Running the tests",
    ] {
        assert!(row.contains(s), "{row}");
    }
    let lines: Vec<&str> = list.lines().collect();
    assert!(
        lines[..3].iter().all(|l| l.starts_with("hdr\t-\t"))
            && ["0 working", "0 input", "1 idle"]
                .iter()
                .all(|s| plain(lines[1]).contains(s)),
        "three pinned header lines carry the summary"
    );
    let names = lines.iter().find(|l| l.contains("context")).unwrap();
    assert!(
        names.starts_with("hdr\t-\t")
            && ["title", "model", "age", "last"]
                .iter()
                .all(|n| names.contains(n)),
        "an unselectable row names the session columns: {names}"
    );
    let jobs = dir.path().join("jobs.yaml");
    fs::write(&jobs, "version: 1\ncolumns: [tokens]\njobs: []\n").unwrap();
    let list = cones::tui::list(&jobs, dir.path(), dir.path()).unwrap();
    assert!(
        list.contains("tokens in/out") && !list.contains("100k  "),
        "columns: in jobs.yaml picks the session columns"
    );
    fs::write(&jobs, "version: 1\ncolumns: [speed]\njobs: []\n").unwrap();
    assert!(
        config::read_jobs(&jobs)
            .unwrap_err()
            .to_string()
            .contains("unknown column"),
        "a column cones cannot show is a validation error"
    );
    assert!(lines[3] == "hdr\t-\t", "a blank row separates sections");
    assert!(
        row.contains("▁"),
        "an idle row rests on the lowest bar: {row}"
    );
    assert!(
        lines.iter().any(|l| l.contains("~/src/repo")),
        "sessions are grouped by directory"
    );
    assert!(lines.iter().any(|l| l.starts_with("run-1\tstarted")));
    let data =
        cones::tui::Data::load(&dir.path().join("none.yaml"), dir.path(), dir.path()).unwrap();
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
    let pane = data.details(&cones::tui::Kind::Session(live.into(), "idle".into()), 1);
    let local = "2026-09-12T10:56:35Z"
        .parse::<chrono::DateTime<Utc>>()
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%m-%d %H:%M:%S")
        .to_string();
    assert!(
        pane[0] == "~/src/repo"
            && [
                "idle interactive",
                "Fable 5.1",
                &format!("started {local}"),
                &format!("last activity {local}"),
                "100k context",
            ]
            .iter()
            .all(|s| pane[1].contains(s)),
        "{pane:?}"
    );
}
#[test]
fn adhoc_job_borrows_policy_or_defaults_to_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let plain = cones::config::adhoc(None, "fix it", dir.path()).unwrap();
    assert!(plain.name.starts_with("adhoc-") && !plain.write && plain.enabled);
    assert_eq!(
        cones::harness::effective_tools(&plain),
        ["Read", "Grep", "Glob"]
    );
    let mut template = plain.clone();
    template.write = true;
    template.timeout_min = 9.0;
    let borrowed = cones::config::adhoc(Some(&template), "ship it", dir.path()).unwrap();
    assert!(borrowed.write && borrowed.timeout_min == 9.0 && borrowed.name != template.name);
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
        registry(
            dir.path(),
            id,
            serde_json::json!({"pid": std::process::id(), "sessionId": id, "cwd": dir.path().join(cwd),
                "status": "idle", "name": title, "startedAt": 1757682871892i64}),
        );
    }
    let data =
        cones::tui::Data::load(&dir.path().join("none.yaml"), dir.path(), dir.path()).unwrap();
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
    assert_eq!(
        cones::fleet::exchanges(&t, 2),
        [
            "> old prompt",
            "",
            "old reply",
            "",
            "> fix it",
            "> please",
            "",
            "Fixed it",
            "second line",
            "",
            "done"
        ]
    );
    assert_eq!(
        cones::fleet::exchanges(&t, 9),
        cones::fleet::exchanges(&t, 2),
        "asking for more than the transcript holds gives the whole transcript"
    );
    assert_eq!(cones::fleet::exchanges(&t, 1), cones::fleet::exchange(&t));
    fs::write(
        &t,
        concat!(
            r#"{"type":"user","message":{"content":"quiet"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"loud"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"reply"}]}}"#,
            "\n",
        ),
    )
    .unwrap();
    assert_eq!(
        cones::fleet::exchanges(&t, 2),
        ["> quiet", "", "", "> loud", "", "reply"]
    );
}
#[test]
fn doctor_version_range_is_major_minor_up_to_next_major() {
    use cones::harness::{TESTED_CLAUDE_RANGE, claude_version_tested};
    assert_eq!(TESTED_CLAUDE_RANGE, ">=2.1, <3");
    assert_eq!(claude_version_tested("2.1.269 (Claude Code)"), Some(true));
    assert_eq!(claude_version_tested("2.8.0"), Some(true));
    assert_eq!(claude_version_tested("3.0.0 (Claude Code)"), Some(false));
    assert_eq!(claude_version_tested("1.99.0"), Some(false));
    assert_eq!(claude_version_tested("Claude Code 2.1.269"), None);
    assert_eq!(claude_version_tested(""), None);
}
#[test]
fn doctor_probes_only_the_switches_the_compiler_emits() {
    let argv: Vec<String> = [
        "--print",
        "--output-format",
        "stream-json",
        "--mcp-config",
        "{\"mcpServers\":{}}",
        "--setting-sources",
        "",
        "--",
        "--prompt-that-looks-like-a-flag",
    ]
    .map(String::from)
    .to_vec();
    assert_eq!(
        cones::harness::compiled_flags(&argv),
        [
            "--print",
            "--output-format",
            "--mcp-config",
            "--setting-sources"
        ]
    );
}

#[test]
fn coordinator_plugin_is_written_from_the_binary_with_its_helper_path_filled_in() {
    let state = tempfile::tempdir().unwrap();
    let plugin = cones::harness::coordinator_plugin(state.path()).unwrap();
    let skill = plugin.join("skills/start-orchestrator");
    let text = std::fs::read_to_string(skill.join("SKILL.md")).unwrap();
    assert!(text.starts_with("---\nname: start-orchestrator\n"));
    assert!(text.contains(&format!("S=\"{}\"", skill.join("bin").display())));
    assert!(!text.contains("__CONES_"));
    for f in [
        "bin/self.sh",
        "bin/sweep.sh",
        "bin/status.py",
        "bin/codex.sh",
        "bin/codex.py",
        "bin/tick.sh",
    ] {
        assert!(skill.join(f).is_file(), "{f}");
    }
    let help = std::process::Command::new("python3")
        .arg(skill.join("bin/codex.py"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(
        help.status.success(),
        "{}",
        String::from_utf8_lossy(&help.stderr)
    );
    std::fs::write(skill.join("bin/codex.py"), "stale helper").unwrap();
    cones::harness::coordinator_plugin(state.path()).unwrap();
    assert_eq!(
        std::fs::read_to_string(skill.join("bin/codex.py")).unwrap(),
        include_str!("../assets/coordinator/skills/start-orchestrator/bin/codex.py")
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(plugin.join(".claude-plugin/plugin.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["name"], "cones");
}
