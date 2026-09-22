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
        "version: 4\nrun_columns: [started, ended]\njobs: []\n",
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
    fs::write(&jobs, "version: 4\njobs: []\n").unwrap();
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
fn policy_inherits_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.yaml");
    let text = config_text("").replace("jobs:\n", "defaults:\n  timeout_min: 5\njobs:\n");
    fs::write(&path, text).unwrap();
    let job = config::read_jobs(&path).unwrap().remove(0);
    assert_eq!(job.timeout_min, 5.0);
}
#[test]
fn a_job_launches_the_agent_with_the_prompt_and_no_policy_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.yaml");
    fs::write(&path, config_text("    model: opus\n")).unwrap();
    let job = config::read_jobs(&path).unwrap().remove(0);
    let id = "13c73aaf-43d6-4b2c-af51-05763e0c6834";
    let argv = cones::harness::adapter(job.harness)
        .unwrap()
        .compile(&job, id)
        .unwrap()
        .args;
    assert_eq!(
        argv,
        [
            "--print",
            "--output-format",
            "stream-json",
            "--verbose",
            "--dangerously-skip-permissions",
            "--session-id",
            id,
            "--name",
            "sample",
            "--model",
            "opus",
            "--",
            "test",
        ],
        "a job is the agent the owner runs by hand, on a schedule"
    );
    for gone in [
        "--tools",
        "--allowedTools",
        "--settings",
        "--mcp-config",
        "--strict-mcp-config",
        "--setting-sources",
        "--disable-slash-commands",
        "--safe-mode",
        "--restricted",
        "--permission-mode",
        "--permission-prompts",
    ] {
        assert!(!argv.iter().any(|a| a == gone), "{gone} is still compiled");
    }
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
fn a_run_answering_its_own_background_task_totals_every_result() {
    // What Claude actually streams when a background task finishes after the first answer:
    // a second init and a second result on the same session. Both answers are the run's.
    let id = "95fbd320-db69-420e-a16b-cdf050eb8830";
    let result_line = |cost: f64, tokens_in: u64, tokens_out: u64| {
        serde_json::json!({"type":"result","subtype":"success","is_error":false,"session_id":id,
            "total_cost_usd":cost,
            "usage":{"input_tokens":tokens_in,"cache_read_input_tokens":0,"output_tokens":tokens_out}})
        .to_string()
    };
    let mut result = Outcome::default();
    for line in [
        result_line(0.88, 100, 10),
        serde_json::json!({"type":"system","subtype":"init","session_id":id}).to_string(),
        result_line(1.29, 200, 20),
    ] {
        result.observe(&line, id).unwrap();
    }
    assert!(result.result_seen && !result.failed && !result.session_mismatch);
    assert_eq!(result.cost_usd, Some(2.17));
    assert_eq!(result.tokens_in, Some(300));
    assert_eq!(result.tokens_out, Some(30));

    // The last result is the verdict: an error after a success fails the run.
    let mut result = Outcome::default();
    result.observe(&result_line(0.88, 100, 10), id).unwrap();
    result
        .observe(
            &serde_json::json!({"type":"result","subtype":"error_max_turns","is_error":true,"session_id":id})
                .to_string(),
            id,
        )
        .unwrap();
    assert!(result.failed);
    assert_eq!(result.reason.as_deref(), Some("error_max_turns"));
    assert_eq!(result.cost_usd, Some(0.88));
}

#[test]
fn overlap_allow_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.yaml");
    fs::write(&path, config_text("    overlap: allow\n")).unwrap();
    let job = &config::read_jobs(&path).unwrap()[0];
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
    let mut rows = cones::tui::fleet_rows(
        dir.path(),
        dir.path(),
        &ledger.runs().unwrap(),
        &cones::config::Policy::default(),
    )
    .unwrap();
    // Keep every fixture identity so a failure to collapse or reject one still
    // fails the assertion, while unrelated machine-wide clients stay out.
    rows.retain(|s| [live, owned, dead].contains(&s.session_id.as_str()));
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
    // The busy session the run owns has no row of its own, but it is still an agent on this
    // machine: the summary counts the fleet, not the list.
    assert!(
        lines[..3].iter().all(|l| l.starts_with("hdr\t-\t"))
            && ["1 working", "0 input", "1 idle"]
                .iter()
                .all(|s| plain(lines[1]).contains(s)),
        "three pinned header lines count the run's agent with the loose one: {}",
        plain(lines[1])
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
    let mut data =
        cones::tui::Data::load(&dir.path().join("none.yaml"), dir.path(), dir.path()).unwrap();
    data.sessions
        .retain(|s| [live, owned, dead].contains(&s.session_id.as_str()));
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
fn adhoc_job_borrows_policy_or_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let plain = cones::config::adhoc(None, "fix it", dir.path()).unwrap();
    assert!(plain.name.starts_with("adhoc-") && plain.enabled);
    let mut template = plain.clone();
    template.timeout_min = 9.0;
    let borrowed = cones::config::adhoc(Some(&template), "ship it", dir.path()).unwrap();
    assert!(borrowed.timeout_min == 9.0 && borrowed.name != template.name);
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
    let mut data =
        cones::tui::Data::load(&dir.path().join("none.yaml"), dir.path(), dir.path()).unwrap();
    data.sessions.retain(|s| {
        [
            "44444444-4444-4444-8444-444444444444",
            "55555555-5555-4555-8555-555555555555",
        ]
        .contains(&s.session_id.as_str())
    });
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
fn coordinator_plugin_is_prose_and_nothing_else() {
    let state = tempfile::tempdir().unwrap();
    let plugin = cones::harness::coordinator_plugin(state.path()).unwrap();
    let skill = plugin.join("skills/start-coordinator");
    let text = std::fs::read_to_string(skill.join("SKILL.md")).unwrap();
    assert!(text.starts_with("---\nname: start-coordinator\n"));
    // The plumbing is `cones coordinator` now, so the skill ships no helpers and no path to
    // substitute into. A leftover from an older build would be a second, drifting runtime.
    assert!(!text.contains("__CONES_"));
    let stale = skill.join("bin/codex.py");
    std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
    std::fs::write(&stale, "yesterday's helper").unwrap();
    cones::harness::coordinator_plugin(state.path()).unwrap();
    assert!(
        !skill.join("bin").exists(),
        "an upgrade must take helpers away"
    );
    let files: Vec<String> = walk(&plugin)
        .iter()
        .map(|p| p.strip_prefix(&plugin).unwrap().display().to_string())
        .collect();
    assert_eq!(
        files,
        [
            ".claude-plugin/plugin.json",
            "skills/dispatch/SKILL.md",
            "skills/start-coordinator/SKILL.md"
        ]
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(plugin.join(".claude-plugin/plugin.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["name"], "cones");
}

#[test]
fn a_running_agent_reads_a_bundled_skill_without_starting_a_coordinator() {
    let cones = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_cones"))
            .args(args)
            .output()
            .unwrap()
    };
    let out = cones(&["skill"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "start-coordinator\ndispatch\n"
    );

    let out = cones(&["skill", "dispatch"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("---\nname: dispatch\n"), "{text}");
    // Printing is loading, because the skill is prose and ships no helper to substitute a path
    // into. Nothing hands a skill to a session that is already running.
    assert!(!text.contains("__CONES_"));
    // The commands the skill tells a dispatcher to run. Pinned so a stream landing a different
    // spelling than the walkthrough was written against shows up as a failure, not as prose
    // naming a command the binary does not have.
    let mut verbs: Vec<&str> = text
        .split("`cones ")
        .skip(1)
        .filter_map(|rest| rest.split([' ', '`']).next())
        .collect();
    verbs.sort_unstable();
    verbs.dedup();
    assert_eq!(verbs, ["comms", "launch", "ls", "show", "stop"]);

    let out = cones(&["skill", "orchestrate"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no bundled skill orchestrate"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        match entry.path().is_dir() {
            true => out.extend(walk(&entry.path())),
            false => out.push(entry.path()),
        }
    }
    out.sort();
    out
}

/// One coordinated folder with one worker already in it, and a way to run coordinator commands
/// against it. The worker's pid is the test's own, so the roster row is live and the parent
/// chain from a spawned `cones` reaches it, which is how a claim identifies itself.
struct Coordinated {
    dir: tempfile::TempDir,
    work: std::path::PathBuf,
    registry: std::path::PathBuf,
    jobs: std::path::PathBuf,
}

impl Coordinated {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("project");
        let registry = dir.path().join("claude/sessions");
        let jobs = dir.path().join("jobs.yaml");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&registry).unwrap();
        fs::write(&jobs, "version: 4\njobs: []\n").unwrap();
        let this = Self {
            dir,
            work,
            registry,
            jobs,
        };
        this.worker("worker-one");
        this
    }

    fn worker(&self, id: &str) {
        fs::write(
            self.registry.join(format!("{id}.json")),
            serde_json::json!({
                "pid": std::process::id(), "sessionId": id,
                "cwd": self.work.canonicalize().unwrap(), "kind": "interactive", "status": "idle"
            })
            .to_string(),
        )
        .unwrap();
    }

    /// A session registered under a status the harness would report, so the roster reaches the
    /// state a watch reacts to.
    fn worker_state(&self, id: &str, status: &str) {
        fs::write(
            self.registry.join(format!("{id}.json")),
            serde_json::json!({
                "pid": std::process::id(), "sessionId": id,
                "cwd": self.work.canonicalize().unwrap(), "kind": "interactive",
                "status": status
            })
            .to_string(),
        )
        .unwrap();
    }

    /// One cones invocation against this folder, under either spelling of the command group.
    fn spawn(&self, group: &str, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_cones"));
        command
            .args([
                "--jobs",
                self.jobs.to_str().unwrap(),
                "--state-dir",
                self.dir.path().to_str().unwrap(),
                group,
                "--dir",
                self.work.to_str().unwrap(),
            ])
            .args(args)
            .env("HOME", self.dir.path())
            .env("CLAUDE_CONFIG_DIR", self.dir.path().join("claude"))
            .env("CODEX_HOME", self.dir.path().join("missing-codex"))
            .env("PI_CODING_AGENT_DIR", self.dir.path().join("missing-pi"));
        command
    }

    fn at(&self, group: &str, args: &[&str]) -> std::process::Output {
        self.spawn(group, args).output().unwrap()
    }

    fn command(&self, args: &[&str]) -> std::process::Output {
        self.at("coordinator", args)
    }

    /// Block until a file the command under test writes appears, rather than sleeping for as
    /// long as it usually takes: five checkouts building at once make any such guess wrong.
    fn until(&self, path: &std::path::Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn run(&self, args: &[&str]) -> String {
        let out = self.command(args);
        assert!(
            out.status.success(),
            "cones coordinator {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// The folder's own directory, as the claim reports it.
    fn coordinator_dir(&self) -> std::path::PathBuf {
        let claimed = self.run(&["claim"]);
        let dir = claimed
            .lines()
            .next()
            .unwrap()
            .split("dir=")
            .nth(1)
            .unwrap();
        std::path::PathBuf::from(dir)
    }
}

/// The wake rule, which is the whole reason the watcher exists: a model call is earned by an
/// arrival that has not been shown and by a worker writing, and by nothing else. Gating on a
/// count the watcher kept per job instead woke the coordinator every ten seconds, forever, on
/// any folder whose inbox already had acknowledged history.
#[test]
fn the_coordinator_wakes_for_an_arrival_and_for_mail_and_for_nothing_else() {
    let f = Coordinated::new();
    let dir = f.coordinator_dir();
    let inbox = dir.join("inbox.jsonl");
    let quiet = |f: &Coordinated, why: &str| {
        let out = f.command(&["wait", "--timeout", "1"]);
        assert_eq!(out.status.code(), Some(2), "{why}");
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "timeout\n", "{why}");
    };
    // The first arm has the worker already on the roster: it was read before the watcher was
    // armed, so it is not an arrival.
    quiet(
        &f,
        "a worker present before the watcher was armed is not an arrival",
    );
    quiet(&f, "nothing moved");

    f.worker("worker-two");
    let woken = f.run(&["wait", "--timeout", "30"]);
    assert!(
        woken.starts_with("new: worker-two\tclaude\t"),
        "an arrival wakes the coordinator: {woken}"
    );
    quiet(&f, "an arrival already shown is not shown again");

    // A state change is a fact to read from a tick, not a reason to spend a model call.
    fs::write(
        f.registry.join("worker-two.json"),
        serde_json::json!({
            "pid": std::process::id(), "sessionId": "worker-two",
            "cwd": f.work.canonicalize().unwrap(), "kind": "interactive", "status": "busy"
        })
        .to_string(),
    )
    .unwrap();
    quiet(&f, "a state change is not worth a model call");

    // A departure is read from a tick too.
    fs::remove_file(f.registry.join("worker-two.json")).unwrap();
    quiet(&f, "a departure is not worth a model call");

    fs::write(&inbox, "{\"from\":\"codex:abc\",\"text\":\"done\"}\n").unwrap();
    let woken = f.run(&["wait", "--timeout", "30"]);
    assert!(
        woken.contains("mail:"),
        "mail wakes the coordinator: {woken}"
    );
    assert!(woken.contains("1\t{\"from\":\"codex:abc\""), "{woken}");
    quiet(
        &f,
        "one pending batch wakes the coordinator once, not every ten seconds",
    );
}

/// The claim is what marks a row as the coordinator, matched by process and folder and never
/// by a title. cones' own record holds it rather than one harness's home, so the role is not
/// Claude's to hold: the mark is applied where every harness's rows are already in hand.
#[test]
fn a_claim_marks_its_row_and_only_in_the_folder_it_claimed() {
    let f = Coordinated::new();
    // A second folder sharing this pid: the same process, somewhere it never claimed.
    let elsewhere = f.dir.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(
        f.registry.join("worker-elsewhere.json"),
        serde_json::json!({
            "pid": std::process::id(), "sessionId": "worker-elsewhere",
            "cwd": elsewhere.canonicalize().unwrap(), "kind": "interactive", "status": "idle"
        })
        .to_string(),
    )
    .unwrap();
    let marks = || {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_cones"))
            .args([
                "--jobs",
                f.jobs.to_str().unwrap(),
                "--state-dir",
                f.dir.path().to_str().unwrap(),
                "ls",
                "--json",
                "--dir",
                f.dir.path().to_str().unwrap(),
            ])
            .env("HOME", f.dir.path())
            .env("CLAUDE_CONFIG_DIR", f.dir.path().join("claude"))
            .env("CODEX_HOME", f.dir.path().join("missing-codex"))
            .env("PI_CODING_AGENT_DIR", f.dir.path().join("missing-pi"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut rows: Vec<(String, bool)> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .map(|v| {
                (
                    v["session"]["session_id"].as_str().unwrap().to_owned(),
                    v["session"]["coordinator"].as_bool().unwrap_or(false),
                )
            })
            .collect();
        rows.sort();
        rows
    };
    assert_eq!(
        marks(),
        [
            ("worker-elsewhere".to_owned(), false),
            ("worker-one".to_owned(), false)
        ]
    );
    f.run(&["claim"]);
    assert_eq!(
        marks(),
        [
            ("worker-elsewhere".to_owned(), false),
            ("worker-one".to_owned(), true)
        ],
        "a reused pid in a folder nobody claimed is not the coordinator"
    );
    f.run(&["claim", "--release"]);
    assert_eq!(
        marks(),
        [
            ("worker-elsewhere".to_owned(), false),
            ("worker-one".to_owned(), false)
        ],
        "releasing the folder gives the role up"
    );
}

/// Reading mail is not handling it. The position moves only on an explicit acknowledgement, so
/// a replaced coordinator still sees a reply the one before it read and never acted on.
#[test]
fn mail_stays_pending_until_it_is_acknowledged() {
    let f = Coordinated::new();
    let dir = f.coordinator_dir();
    fs::write(
        dir.join("inbox.jsonl"),
        "{\"text\":\"one\"}\n{\"text\":\"two\"}\n",
    )
    .unwrap();
    for _ in 0..2 {
        let pending = f.run(&["mail"]);
        assert!(pending.contains("1\t{\"text\":\"one\"}"), "{pending}");
        assert!(pending.contains("2\t{\"text\":\"two\"}"), "{pending}");
    }
    assert!(f.run(&["tick"]).contains("2\t{\"text\":\"two\"}"));
    assert!(f.run(&["mail", "--ack", "1"]).contains("through line 1"));
    let pending = f.run(&["mail"]);
    assert!(!pending.contains("\"one\""), "{pending}");
    assert!(pending.contains("2\t{\"text\":\"two\"}"), "{pending}");
    // Out of range in either direction: nothing handled, nothing beyond what has arrived.
    for n in ["1", "3"] {
        let out = f.command(&["mail", "--ack", n]);
        assert!(!out.status.success(), "--ack {n} should be refused");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("between 2 and 2"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(f.run(&["mail", "--ack", "2"]).contains("through line 2"));
    assert_eq!(f.run(&["mail"]), "mail: none pending\n");
}

/// Mail already in the folder when a coordinator claims it is history, not a backlog it was
/// asked to answer. It is counted as handled once, and the claim says so.
#[test]
fn a_claim_does_not_replay_the_mail_that_predates_it() {
    let f = Coordinated::new();
    let dir = cones::coordinator::directory(f.dir.path(), &f.work.canonicalize().unwrap());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("inbox.jsonl"), "{\"text\":\"old\"}\n").unwrap();
    assert!(
        f.run(&["claim"])
            .contains("1 inbox entries predate this claim")
    );
    assert_eq!(f.run(&["mail"]), "mail: none pending\n");
}

/// A second coordinator in a folder someone else holds is told to stand down. Without that the
/// workers get two sets of notes, each costing a turn, and the two can contradict each other.
#[test]
fn a_claim_is_refused_while_another_live_coordinator_holds_the_folder() {
    let f = Coordinated::new();
    let dir = f.coordinator_dir();
    let record = dir.join("status.json");
    let mine: serde_json::Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    assert_eq!(mine["pid"], std::process::id());
    assert_eq!(mine["session"], "worker-one");
    // Re-claiming your own folder is fine; it is how a coordinator recovers after a restart.
    f.run(&["claim"]);

    // launchd is pid 1 on macOS: alive, and certainly not in this process's parent chain.
    fs::write(
        &record,
        serde_json::json!({"cwd": f.work.canonicalize().unwrap(), "pid": 1, "session": "someone"})
            .to_string(),
    )
    .unwrap();
    for args in [["claim"], ["claim"]] {
        let out = f.command(&args);
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("another coordinator owns"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = f.command(&["claim", "--release"]);
    assert!(
        !out.status.success(),
        "a peer's claim is not yours to clear"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("held by pid 1"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(record.is_file());
}

/// A harness cones cannot write to says so. Faking delivery by typing into the session's
/// terminal would put the coordinator's words in the owner's own input line.
#[test]
fn a_note_is_refused_for_a_harness_with_no_delivery_command() {
    let f = Coordinated::new();
    f.run(&["claim"]);
    let out = f.command(&["send", "worker-one", "hello"]);
    assert!(!out.status.success());
    let error = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(error.contains("has no message operation"), "{error}");
    // An unknown recipient is refused before any harness is consulted.
    let out = f.command(&["send", "nobody", "hello"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not on this folder's roster"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Two coordinators can write the folder's record at once: a replacement overlapping the one it
/// takes over from, or a watcher left armed from an earlier arm. A shared temporary name means
/// the second writer's rename destroys the first writer's source, and that process dies.
#[test]
fn concurrent_claims_leave_one_valid_record_and_no_temporaries() {
    let f = Coordinated::new();
    let dir = f.coordinator_dir();
    let writers: Vec<_> = (0..6)
        .map(|_| {
            std::process::Command::new(env!("CARGO_BIN_EXE_cones"))
                .args([
                    "--jobs",
                    f.jobs.to_str().unwrap(),
                    "--state-dir",
                    f.dir.path().to_str().unwrap(),
                    "coordinator",
                    "--dir",
                    f.work.to_str().unwrap(),
                    "claim",
                ])
                .env("HOME", f.dir.path())
                .env("CLAUDE_CONFIG_DIR", f.dir.path().join("claude"))
                .env("CODEX_HOME", f.dir.path().join("missing-codex"))
                .env("PI_CODING_AGENT_DIR", f.dir.path().join("missing-pi"))
                .spawn()
                .unwrap()
        })
        .collect();
    for mut writer in writers {
        let status = writer.wait().unwrap();
        assert!(status.success(), "a concurrent claim died: {status}");
    }
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("status.json")).unwrap()).unwrap();
    assert_eq!(record["pid"], std::process::id());
    let leftovers: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporaries left behind: {leftovers:?}"
    );
}

/// Both spellings are one command group over one folder's state. A session that was started
/// before the rename still has the old skill loaded, so `coordinator send` and `comms send` have
/// to be the same command and not two that drift, and an acknowledgement under either name has
/// to be the acknowledgement the other one reads.
#[test]
fn comms_and_coordinator_are_one_implementation_over_one_folder_state() {
    let f = Coordinated::new();
    let dir = f.coordinator_dir();
    fs::write(
        dir.join("inbox.jsonl"),
        "{\"text\":\"one\"}\n{\"text\":\"two\"}\n",
    )
    .unwrap();
    let old = String::from_utf8(f.at("coordinator", &["mail"]).stdout).unwrap();
    let new = String::from_utf8(f.at("comms", &["mail"]).stdout).unwrap();
    assert_eq!(old, new);
    assert!(new.contains("1\t{\"text\":\"one\"}"), "{new}");

    let acked = f.at("comms", &["mail", "--ack", "1"]);
    assert!(acked.status.success());
    let old = String::from_utf8(f.at("coordinator", &["mail"]).stdout).unwrap();
    assert!(
        !old.contains("\"one\""),
        "one spelling's ack is the other's: {old}"
    );
    assert!(old.contains("2\t{\"text\":\"two\"}"), "{old}");
    assert!(
        f.at("coordinator", &["mail", "--ack", "2"])
            .status
            .success(),
        "the old spelling still moves the same cursor"
    );
    assert_eq!(f.at("comms", &["mail"]).status.code(), Some(0));

    // The wake gate and the refusals are shared too, not reimplemented under the new name.
    for group in ["comms", "coordinator"] {
        let out = f.at(group, &["wait", "--timeout", "1"]);
        assert_eq!(out.status.code(), Some(2), "{group}");
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "timeout\n");
        let out = f.at(group, &["send", "nobody", "hello"]);
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("not on this folder's roster"),
            "{group}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The first-arm race. A dispatcher reads its mail, decides nothing is pending, and arms the
/// watcher; a reply that lands in between is unacknowledged, which is the record of nobody
/// having acted on it. Snapshotting the inbox on the first arm swallowed exactly that reply and
/// left the dispatcher blocked on a worker that had already answered.
#[test]
fn a_reply_that_lands_before_the_first_wait_still_wakes_it() {
    let f = Coordinated::new();
    let dir = f.coordinator_dir();
    assert_eq!(f.at("comms", &["mail"]).stdout, b"mail: none pending\n");
    fs::write(
        dir.join("inbox.jsonl"),
        "{\"from\":\"stream:A\",\"text\":\"done\"}\n",
    )
    .unwrap();
    assert!(!dir.join("wait.json").exists(), "nothing has armed yet");

    let woken = f.at("comms", &["wait", "--timeout", "30"]);
    assert!(woken.status.success());
    let woken = String::from_utf8(woken.stdout).unwrap();
    assert!(woken.contains("1\t{\"from\":\"stream:A\""), "{woken}");
    // And still exactly once: a batch nobody acknowledged is not a wake every ten seconds.
    let out = f.at("comms", &["wait", "--timeout", "1"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// A replacement dispatcher inherits the unhandled replies, not the position the one before it
/// announced from. Reading mail is not handling it, so a reply the predecessor showed and never
/// acknowledged is still somebody's to act on, and the watcher must not start above it.
#[test]
fn a_claim_puts_unacknowledged_mail_back_in_front_of_the_watcher() {
    let f = Coordinated::new();
    let dir = f.coordinator_dir();
    fs::write(
        dir.join("inbox.jsonl"),
        "{\"text\":\"one\"}\n{\"text\":\"two\"}\n",
    )
    .unwrap();
    let woken = f.run(&["wait", "--timeout", "30"]);
    assert!(woken.contains("2\t{\"text\":\"two\"}"), "{woken}");
    f.run(&["mail", "--ack", "1"]);

    let reclaimed = f.run(&["claim"]);
    assert!(
        reclaimed.contains("unhandled mail from before this claim is pending again from line 2"),
        "{reclaimed}"
    );
    let woken = f.at("comms", &["wait", "--timeout", "30"]);
    assert!(
        woken.status.success(),
        "{}",
        String::from_utf8_lossy(&woken.stderr)
    );
    let woken = String::from_utf8(woken.stdout).unwrap();
    assert!(woken.contains("2\t{\"text\":\"two\"}"), "{woken}");
    assert!(!woken.contains("\"one\""), "line 1 was handled: {woken}");
}

/// A folder has one consumer. Acknowledgement is a single cursor and the watcher keeps a single
/// position, so a second reader either acts on a reply the first one owns or steps the cursor
/// past one it never saw. Sending is not restricted; consuming is.
#[test]
fn a_folder_rejects_a_second_inbox_consumer_and_a_second_watcher() {
    let f = Coordinated::new();
    let dir = f.coordinator_dir();
    fs::write(dir.join("inbox.jsonl"), "{\"text\":\"one\"}\n").unwrap();

    // A second watcher while the first is armed, told apart by the lease the first one writes.
    let watcher = dir.join("watcher.json");
    let mut armed = f
        .spawn("comms", &["wait", "--timeout", "60"])
        .spawn()
        .unwrap();
    f.until(&watcher);
    for group in ["comms", "coordinator"] {
        let out = f.at(group, &["wait", "--timeout", "1"]);
        // Its own exit code, because this is the refusal a caller may retry: an agent that
        // re-arms the instant its wait returns can race its own predecessor out of the folder.
        assert_eq!(out.status.code(), Some(3), "{group} took a second watcher");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("a folder has one watcher"),
            "{group}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(
        armed.wait().unwrap().success(),
        "the first watcher kept its wake"
    );
    assert!(
        !watcher.exists(),
        "the lease is released when the watch returns"
    );
    assert!(
        f.at("comms", &["wait", "--timeout", "1"]).status.code() == Some(2),
        "a released lease is free to take"
    );

    // A folder somebody else coordinates: consuming is refused, sending is not.
    // launchd is pid 1 on macOS: alive, and certainly not in this process's parent chain.
    fs::write(
        dir.join("status.json"),
        serde_json::json!({"cwd": f.work.canonicalize().unwrap(), "pid": 1, "session": "peer"})
            .to_string(),
    )
    .unwrap();
    for args in [
        vec!["mail"],
        vec!["mail", "--ack", "1"],
        vec!["wait", "--timeout", "1"],
    ] {
        let out = f.at("comms", &args);
        // Exit 1, not the retryable 3: a peer's claim does not clear by waiting for it.
        assert_eq!(
            out.status.code(),
            Some(1),
            "{args:?} consumed a peer's folder"
        );
        let error = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            error.contains("one agent consumes a folder's inbox"),
            "{args:?}: {error}"
        );
        assert!(error.contains("pid 1"), "{args:?}: {error}");
    }
    let error =
        String::from_utf8_lossy(&f.at("comms", &["send", "worker-one", "hi"]).stderr).into_owned();
    assert!(
        error.contains("has no message operation"),
        "sending into a coordinated folder is still allowed: {error}"
    );
}

/// Claiming has to be atomic across distinct agents. Reading the record and then replacing it is
/// not mutual exclusion: two agents both read the folder as free, both write, and both believe
/// they hold it, which is how workers end up taking notes from two coordinators at once.
#[test]
fn only_one_of_several_distinct_agents_wins_a_free_folder() {
    let f = Coordinated::new();
    let out = f.dir.path().join("race");
    fs::create_dir_all(&out).unwrap();
    let (go, done) = (out.join("go"), out.join("done"));
    // Each racer is its own shell, registered as its own session before it calls cones, so the
    // process chain a claim walks reaches a different roster row for every one of them. They
    // start together on `go` and stay alive until `done`, so the winner's claim is live for the
    // whole race rather than dying with the process that took it.
    let mut racers: Vec<_> = (0..6)
        .map(|n| {
            let script = format!(
                "printf '{{\"pid\":%s,\"sessionId\":\"racer-{n}\",\"cwd\":\"{cwd}\",\
                 \"kind\":\"interactive\",\"status\":\"idle\"}}' $$ > {reg}/racer-{n}.json\n\
                 while [ ! -f {go} ]; do sleep 0.01; done\n\
                 \"$@\" > {out}/{n}.out 2> {out}/{n}.err; echo $? > {out}/{n}.code\n\
                 while [ ! -f {done} ]; do sleep 0.01; done\n",
                cwd = f.work.canonicalize().unwrap().display(),
                reg = f.registry.display(),
                go = go.display(),
                done = done.display(),
                out = out.display(),
            );
            // "$@" in the script is the cones claim this racer runs, passed as arguments so
            // the folder's temporary path never has to survive a round through the shell.
            std::process::Command::new("/bin/sh")
                .args(["-c", &script, "sh", env!("CARGO_BIN_EXE_cones")])
                .args([
                    "--jobs",
                    f.jobs.to_str().unwrap(),
                    "--state-dir",
                    f.dir.path().to_str().unwrap(),
                    "coordinator",
                    "--dir",
                    f.work.to_str().unwrap(),
                    "claim",
                ])
                .env("HOME", f.dir.path())
                .env("CLAUDE_CONFIG_DIR", f.dir.path().join("claude"))
                .env("CODEX_HOME", f.dir.path().join("missing-codex"))
                .env("PI_CODING_AGENT_DIR", f.dir.path().join("missing-pi"))
                .spawn()
                .unwrap()
        })
        .collect();
    for n in 0..6 {
        f.until(&f.registry.join(format!("racer-{n}.json")));
    }
    fs::write(&go, "").unwrap();
    for n in 0..6 {
        f.until(&out.join(format!("{n}.code")));
    }
    fs::write(&done, "").unwrap();
    for racer in &mut racers {
        racer.wait().unwrap();
    }
    let results: Vec<(String, String)> = (0..6)
        .map(|n| {
            (
                fs::read_to_string(out.join(format!("{n}.code")))
                    .unwrap()
                    .trim()
                    .to_owned(),
                fs::read_to_string(out.join(format!("{n}.err"))).unwrap(),
            )
        })
        .collect();
    let won: Vec<_> = results.iter().filter(|(code, _)| code == "0").collect();
    assert_eq!(
        won.len(),
        1,
        "{} agents held one folder: {results:?}",
        won.len()
    );
    for (_, error) in results.iter().filter(|(code, _)| code != "0") {
        assert!(error.contains("another coordinator owns"), "{error}");
    }
}

/// Watching a named set of workers, which is what a dispatcher that launched them wants. An
/// arrival is somebody else's business; what earns a call is a worker that stopped moving on its
/// own, reported once, plus the reply that is how a worker actually reports finishing.
#[test]
fn a_watch_on_named_workers_reports_each_stall_once_and_is_not_completion() {
    let f = Coordinated::new();
    let dir = f.coordinator_dir();
    f.worker("worker-two");
    let watch = [
        "wait",
        "--id",
        "worker-one",
        "--id",
        "worker-two",
        "--timeout",
    ];
    let quiet = |why: &str| {
        let out = f.at("comms", &[watch.as_slice(), &["1"]].concat());
        assert_eq!(
            out.status.code(),
            Some(2),
            "{why}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    };
    let woken = |why: &str| {
        let out = f.at("comms", &[watch.as_slice(), &["30"]].concat());
        assert!(
            out.status.success(),
            "{why}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    quiet("two idle workers are not news");

    // An arrival is not this watch's business: it was told which workers are its own.
    f.worker("worker-three");
    quiet("an arrival outside the watched set is not a reason to wake");

    f.worker_state("worker-one", "waiting");
    let out = woken("a native input request is a reason to look at a worker");
    assert!(out.contains("worker: worker-one\tblocked\t"), "{out}");
    assert!(out.contains("asking for input natively"), "{out}");
    assert!(out.contains("not a completed task"), "{out}");
    assert!(!out.contains("worker-three"), "only the watched set: {out}");
    quiet("a worker still waiting for input is not announced again");

    f.worker_state("worker-two", "failed");
    let out = woken("a native failure is a reason to look at a worker");
    assert!(out.contains("worker: worker-two\tfailed\t"), "{out}");
    assert!(out.contains("reported a native failure"), "{out}");
    assert!(out.contains("not a completed task"), "{out}");
    quiet("a worker that is still failed is not announced again");

    fs::remove_file(f.registry.join("worker-two.json")).unwrap();
    let out = woken("a watched worker leaving the roster is worth a look");
    assert!(out.contains("worker: worker-two\tgone\t"), "{out}");
    assert!(out.contains("not a completed task"), "{out}");
    quiet("a worker that is still gone is not announced again");

    // A reply is how a worker reports, so it wakes a narrowed watch too.
    fs::write(
        dir.join("inbox.jsonl"),
        "{\"from\":\"claude:worker-one\",\"text\":\"done\"}\n",
    )
    .unwrap();
    let out = woken("a reply wakes a narrowed watch");
    assert!(out.contains("1\t{\"from\":\"claude:worker-one\""), "{out}");
    quiet("one pending batch wakes it once");

    // A worker cones cannot see is a typo, not a disappearance to report.
    let out = f.at("comms", &["wait", "--id", "nobody", "--timeout", "1"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("nobody is not on this folder's roster"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An arrival while the first arm is asleep is still an arrival. The first pass records the
/// folder as the coordinator already read it and keeps waiting, so the suppression has to end
/// with that pass rather than with the call: a worker that starts a second later is news.
#[test]
fn a_worker_that_arrives_while_the_first_arm_sleeps_still_wakes_it() {
    let f = Coordinated::new();
    let dir = f.coordinator_dir();
    let armed = dir.join("wait.json");
    let watch = f
        .spawn("comms", &["wait", "--timeout", "120"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // The first pass writes what it saw before it sleeps, which is the signal that it is armed.
    f.until(&armed);
    f.worker("worker-late");
    let out = watch.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let woken = String::from_utf8(out.stdout).unwrap();
    assert!(woken.starts_with("new: worker-late\tclaude\t"), "{woken}");
    assert!(
        !woken.contains("worker-one"),
        "already there before the arm: {woken}"
    );
}
