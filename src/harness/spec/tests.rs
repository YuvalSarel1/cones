use super::*;
use crate::{harness, history};
use std::{
    fs,
    os::unix::{ffi::OsStringExt, fs::PermissionsExt},
    time::{Duration, Instant},
};

const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

#[test]
fn every_registered_harness_has_a_valid_definition_and_explicit_capabilities() {
    assert_eq!(
        known(),
        [HarnessKind::Claude, HarnessKind::Codex, HarnessKind::Pi]
    );
    for &(kind, yaml) in BUILTINS {
        let definition = HarnessSpec::parse(yaml).unwrap();
        assert_eq!(definition.kind, kind);
        assert_eq!(by_name(&definition.name).unwrap().kind, kind);
    }
    let claude = spec(HarnessKind::Claude);
    let codex = spec(HarnessKind::Codex);
    let pi = spec(HarnessKind::Pi);
    assert_eq!(claude.session(Some("interactive")).join, Join::Unavailable);
    assert_eq!(claude.session(Some("bg")).stop, Stop::Remove);
    assert_eq!(codex.session(Some("daemon")).join, Join::CodexRemote);
    assert_eq!(codex.session(None).join, Join::Unavailable);
    assert_eq!(pi.session(None).join, Join::Unavailable);
    assert_eq!(pi.commands.resume_handler, Resume::Transcript);
    assert_eq!(codex.execution.enforcement, Support::Unknown);
    assert_eq!(pi.execution.enforcement, Support::Unsupported);
    assert!(harness::adapter(HarnessKind::Claude).is_ok());
    assert!(harness::adapter(HarnessKind::Codex).is_err());
    assert!(harness::adapter(HarnessKind::Pi).is_err());
}

#[test]
fn invalid_definitions_fail_before_a_command_or_discovery_runs() {
    use serde_json::json;
    for (kind, pointer, value) in [
        (0, "/unknown", json!(true)),
        (0, "/version", json!(1)),
        (0, "/name", json!("claude")),
        (0, "/home/typo", json!(true)),
        (0, "/home/env", json!("1INVALID")),
        (0, "/transcript/roots/0/path", json!("../projects")),
        (
            0,
            "/transcript/statusline/cost_pointer",
            json!("cost.total"),
        ),
        (0, "/discovery/handler", json!("claude")),
        (0, "/transcript/handler", json!("claude")),
        (0, "/operations/launch/handler", json!("claude_background")),
        (0, "/operations/launch/prompt", json!(["--", "{promtp}"])),
        (
            0,
            "/operations/attach/args",
            json!(["attach", "prefix-{id}"]),
        ),
        (
            0,
            "/operations/sessions/default/lifetime",
            json!("terminal"),
        ),
        (1, "/execution", json!({"enforcement":"supported"})),
        (1, "/discovery/daemon", json!(null)),
        (1, "/discovery/daemon/pid", json!("/app-server.pid")),
        (0, "/discovery/daemon", json!({"pid":"a.pid", "locks":"b"})),
        (
            0,
            "/viewer/input",
            json!({"return_to_list":[{"key":"tab","when":"always"},{"key":"tab","when":"always"}]}),
        ),
        (
            0,
            "/viewer/input",
            json!({"return_to_list":[{"key":"ctr+z","when":"always"}]}),
        ),
        (
            0,
            "/viewer/input",
            json!({"return_to_list":[{"key":"left","when":"empty_prompt"}]}),
        ),
        (1, "/operations/rename", json!(true)),
        (1, "/operations/attach", json!(null)),
        (0, "/operations/launch/provider", json!("--provider")),
    ] {
        let mut document: serde_json::Value = serde_yaml::from_str(BUILTINS[kind].1).unwrap();
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        document.pointer_mut(parent).unwrap()[key] = value;
        let yaml = serde_yaml::to_string(&document).unwrap();
        assert!(
            HarnessSpec::parse(&yaml).is_err(),
            "accepted {pointer}:\n{yaml}"
        );
    }
}

#[test]
fn argv_substitutions_are_single_arguments_and_preserve_native_path_bytes() {
    let raw = OsString::from_vec(b"/tmp/native-\xff home".to_vec());
    let prompt = OsStr::new("--model surprise; $(touch nope)\nsecond line");
    let template = vec!["-C".into(), "{cwd}".into(), "--".into(), "{prompt}".into()];
    assert_eq!(
        args(&template, &[("cwd", &raw), ("prompt", prompt)]).unwrap(),
        [OsString::from("-C"), raw, "--".into(), prompt.to_owned()]
    );
    assert!(args(&template, &[("prompt", prompt)]).is_err());
}

#[test]
fn native_home_defaults_overrides_and_original_thread_homes_remain_distinct() {
    let root = Path::new("/isolated/.claude");
    let codex = spec(HarnessKind::Codex);
    let pi = spec(HarnessKind::Pi);
    assert_eq!(spec(HarnessKind::Claude).home.resolve(root), root);
    assert_eq!(
        codex.home.resolve_with(root, Path::new("/user"), None),
        Path::new("/isolated/.codex")
    );
    assert_eq!(
        pi.home
            .resolve_with(root, Path::new("/user"), Some(OsStr::new(""))),
        Path::new("/isolated/.pi/agent")
    );
    assert_eq!(
        codex
            .home
            .resolve_with(root, Path::new("/user"), Some(OsStr::new("relative-home"))),
        Path::new("relative-home")
    );
    let row: Session = serde_json::from_value(serde_json::json!({
        "session_id": A, "harness": "codex", "cwd": "/fixture", "state": "done",
        "transcript_path": "/isolated/.codex-region/sessions/2026/09/16/rollout.jsonl"
    }))
    .unwrap();
    assert_eq!(
        codex.session_home(root, &row),
        Path::new("/isolated/.codex-region")
    );
}

#[test]
fn probe_fixtures_preserve_success_requirements_and_reported_versions() {
    let claude = &spec(HarnessKind::Claude).launch.as_ref().unwrap().probe;
    assert!(claude.report(false, "supports --bg and attach").is_ok());
    assert!(claude.report(true, "supports --bg only").is_err());
    let codex = &spec(HarnessKind::Codex).launch.as_ref().unwrap().probe;
    assert!(codex.report(false, "{\"cliVersion\":\"0.154\"}").is_err());
    assert!(codex.report(true, "{\"cliVersion\":\"0.153\"}").is_err());
    assert!(codex.report(true, "no version").is_err());
    assert!(
        codex
            .report(true, "notice\n{\"cliVersion\":\"0.154\"}\n")
            .unwrap()
            .starts_with("codex 0.154:")
    );
    assert!(
        spec(HarnessKind::Pi)
            .launch
            .as_ref()
            .unwrap()
            .probe
            .report(true, "0.85.1\n")
            .unwrap()
            .starts_with("pi 0.85.1:")
    );
}

#[test]
fn discovery_and_history_do_not_require_launch_or_control_operations() {
    let mut document: serde_json::Value = serde_yaml::from_str(BUILTINS[0].1).unwrap();
    for field in ["operations", "viewer", "execution"] {
        document.as_object_mut().unwrap().remove(field);
    }
    let definition = HarnessSpec::parse(&serde_yaml::to_string(&document).unwrap()).unwrap();
    assert!(definition.launch.is_none());
    assert!(definition.commands.resume.is_empty());
    assert!(!definition.operations.rename);
    assert_eq!(definition.session(None).join, Join::Unavailable);
    assert_eq!(definition.execution.enforcement, Support::Unknown);
    assert_eq!(definition.transcript.live_root(), Path::new("projects"));
    assert!(
        harness::check_operation(&definition, &definition.operations.resume, "resume").is_err()
    );
}

#[test]
fn independent_home_bases_and_operation_probes_are_validated() {
    use serde_json::json;
    let mut document: serde_json::Value = serde_yaml::from_str(BUILTINS[1].1).unwrap();
    document["home"]["default"] = json!({"base":"user", "path":".local/share/native"});
    document["operations"]["resume"]["probe"] = document["operations"]["launch"]["probe"].clone();
    let definition = HarnessSpec::parse(&serde_yaml::to_string(&document).unwrap()).unwrap();
    assert_eq!(
        definition
            .home
            .resolve_with(Path::new("/provided/claude"), Path::new("/user"), None),
        Path::new("/user/.local/share/native")
    );
    assert_eq!(
        definition.home.resolve_with(
            Path::new("/provided/claude"),
            Path::new("/user"),
            Some(OsStr::new("/separate/config"))
        ),
        Path::new("/separate/config")
    );
    let probe = definition
        .operations
        .resume
        .as_ref()
        .unwrap()
        .probe
        .as_ref()
        .unwrap();
    assert!(probe.report(true, r#"{"cliVersion":"0.154.0"}"#).is_ok());
    assert!(probe.report(true, r#"{"cliVersion":"0.153.9"}"#).is_err());
    document["operations"]["resume"]["probe"]["args"] = json!([]);
    assert!(HarnessSpec::parse(&serde_yaml::to_string(&document).unwrap()).is_err());
}

#[test]
fn transcript_storage_can_live_outside_the_native_config_home() {
    let pi = spec(HarnessKind::Pi);
    let root = pi.transcript.live_scan_root();
    assert_eq!(root.env.as_deref(), Some("PI_CODING_AGENT_SESSION_DIR"));
    assert_eq!(
        root.resolve_with(Path::new("/config/pi"), None),
        Path::new("/config/pi/sessions")
    );
    assert_eq!(
        root.resolve_with(Path::new("/config/pi"), Some(Path::new("/data/sessions"))),
        Path::new("/data/sessions")
    );
    assert_eq!(
        root.resolve_with(Path::new("/config/pi"), Some(Path::new(""))),
        Path::new("/config/pi/sessions")
    );
}

#[test]
fn statusline_cost_pointer_is_optional_and_validated() {
    use serde_json::json;
    let source = spec(HarnessKind::Claude)
        .transcript
        .statusline
        .as_ref()
        .unwrap();
    let report = json!({"cost": {"total_cost_usd": 0.125}});
    assert_eq!(
        report
            .pointer(source.cost_pointer.as_deref().unwrap())
            .and_then(serde_json::Value::as_f64),
        Some(0.125)
    );
    let mut document: serde_json::Value = serde_yaml::from_str(BUILTINS[0].1).unwrap();
    document["transcript"]["statusline"]
        .as_object_mut()
        .unwrap()
        .remove("cost_pointer");
    let absent = HarnessSpec::parse(&serde_yaml::to_string(&document).unwrap()).unwrap();
    assert!(absent.transcript.statusline.unwrap().cost_pointer.is_none());
    for invalid in ["cost.total_cost_usd", "/cost/~bad"] {
        document["transcript"]["statusline"]["cost_pointer"] = json!(invalid);
        assert!(HarnessSpec::parse(&serde_yaml::to_string(&document).unwrap()).is_err());
    }
}

#[test]
fn command_sequences_preserve_argv_environment_and_short_circuit_failures() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("fake harness");
    let capture = dir.path().join("argv");
    fs::write(
        &program,
        "#!/bin/sh\nprintf '%s\\0' \"$@\" >> \"$CAPTURE\"\n[ \"$1\" != fail ]\n",
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let after = || {
        let mut c = std::process::Command::new(&program);
        c.args(["attach", "keep '$HOME' literal"])
            .env("CAPTURE", &capture)
            .current_dir(dir.path());
        c
    };
    let first = vec![
        "resume".into(),
        "a b;$(touch should-not-exist)\nnext".into(),
    ];
    assert!(
        harness::then_exec(first, after())
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        fs::read(&capture).unwrap(),
        b"resume\0a b;$(touch should-not-exist)\nnext\0attach\0keep '$HOME' literal\0"
    );
    fs::write(&capture, []).unwrap();
    assert!(
        !harness::then_exec(vec!["fail".into()], after())
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(fs::read(capture).unwrap(), b"fail\0");
}

#[test]
fn all_native_transcript_fixtures_pass_through_the_same_history_contract() {
    let fixtures = [
        (
            HarnessKind::Claude,
            "projects/fixture",
            include_str!("../../../assets/harnesses/fixtures/claude.jsonl"),
            "Named Claude",
            15,
            5,
            15,
        ),
        (
            HarnessKind::Codex,
            "sessions/2026/09/16",
            include_str!("../../../assets/harnesses/fixtures/codex.jsonl"),
            "Inspect the fixture",
            40,
            6,
            9,
        ),
        (
            HarnessKind::Pi,
            "sessions/--fixture--",
            include_str!("../../../assets/harnesses/fixtures/pi.jsonl"),
            "Named pi",
            12,
            3,
            12,
        ),
    ];
    for (kind, folder, transcript, title, input, output, context) in fixtures {
        let preview = crate::transcript::parse(&kind.to_string(), transcript.as_bytes());
        assert_eq!(
            preview.messages.first().unwrap().role,
            crate::transcript::Role::User
        );
        assert_eq!(
            preview.messages.first().unwrap().text,
            "Inspect the fixture",
            "{kind}: injected instructions are not a user prompt"
        );
        let reply = match kind {
            HarnessKind::Claude => "Reading\n\nDone",
            HarnessKind::Codex => "Done\nAdditional detail",
            HarnessKind::Pi => "Earlier text\nDone",
        };
        assert_eq!(
            preview.messages.last().unwrap().text,
            reply,
            "{kind}: previews keep full text while columns use a headline"
        );
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join(folder);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join(format!("{A}.jsonl")), transcript).unwrap();
        let mut reader = history::Reader::new(vec![history::Source {
            harness: kind,
            home: dir.path().into(),
        }])
        .unwrap();
        reader
            .request(history::Query {
                hydrate: true,
                ..Default::default()
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let page = loop {
            if let Some(page) = reader.poll() {
                break page.unwrap();
            }
            assert!(Instant::now() < deadline, "history worker stalled");
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(page.entries.len(), 1, "{kind}");
        let entry = &page.entries[0];
        assert_eq!(entry.key.session_id, A);
        assert_eq!(entry.title.as_deref(), Some(title), "{kind}");
        let columns = entry.columns.as_ref().unwrap();
        assert_eq!(columns.tokens_in, Some(input), "{kind}");
        assert_eq!(columns.tokens_out, Some(output), "{kind}");
        assert_eq!(columns.context_tokens, Some(context), "{kind}");
        assert_eq!(columns.last.as_deref(), Some("Done"), "{kind}");
        if kind == HarnessKind::Pi {
            assert_eq!(columns.cost_usd, Some(0.003));
        } else {
            assert_eq!(columns.cost_usd, None, "unreported cost remains absent");
        }
    }
}

#[test]
fn native_event_guards_and_unknown_state_policy_are_declarative() {
    use serde_json::json;
    let codex = &spec(HarnessKind::Codex).state;
    assert_eq!(
        codex.read(&json!({"type":"event_msg","payload":{"type":"task_complete"}})),
        Some("done")
    );
    assert_eq!(
        codex.read(&json!({"type":"response_item","payload":{"type":"task_complete"}})),
        None
    );
    assert_eq!(
        codex.read(&json!({"type":"event_msg","payload":{"type":"token_count"}})),
        None,
        "unrelated events preserve prior state"
    );
    let pi = &spec(HarnessKind::Pi).state;
    assert_eq!(
        pi.read(&json!({"type":"message","message":{"role":"toolResult"}})),
        Some("active")
    );
    assert_eq!(
        pi.read(
            &json!({"type":"message","message":{"role":"assistant","stopReason":"new_reason"}})
        ),
        Some("-")
    );
    assert_eq!(
        pi.read(&json!({"type":"message","message":{"role":"assistant","stopReason":"stop"}})),
        Some("idle")
    );
    let mut changed = HarnessSpec::parse(BUILTINS[1].1).unwrap();
    let done = changed.state.rules[0]
        .values
        .remove("task_complete")
        .unwrap();
    changed.state.rules[0]
        .values
        .insert("turn_finished".into(), done);
    changed.validate().unwrap();
    assert_eq!(
        changed
            .state
            .read(&json!({"type":"event_msg","payload":{"type":"turn_finished"}})),
        Some("done"),
        "a native event rename needs only a definition change"
    );
}

#[test]
fn process_filters_change_without_reconfiguring_the_os_parser() {
    let ps = " 1 Sun Sep 13 15:19:19 2026 codex login\n 2 Sun Sep 13 15:19:19 2026 codex --remote unix:///s resume -- abc\n 3 Sun Sep 13 15:19:19 2026 pi\n 4 Sun Sep 13 15:19:19 2026 /bin/echo codex\n";
    assert_eq!(
        spec(HarnessKind::Codex)
            .discovery
            .processes(ps)
            .iter()
            .map(|p| p.pid)
            .collect::<Vec<_>>(),
        [2]
    );
    assert_eq!(
        spec(HarnessKind::Pi)
            .discovery
            .processes(ps)
            .iter()
            .map(|p| p.pid)
            .collect::<Vec<_>>(),
        [3]
    );
    let mut changed = HarnessSpec::parse(BUILTINS[1].1).unwrap();
    changed.discovery.process = Some("replacement".into());
    changed.validate().unwrap();
    let next = ps.replace("codex", "replacement");
    assert_eq!(
        changed
            .discovery
            .processes(&next)
            .iter()
            .map(|p| p.pid)
            .collect::<Vec<_>>(),
        [2]
    );
}
