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
    let claude = BUILTINS[0].1;
    let codex = BUILTINS[1].1;
    for bad in [
        format!("{claude}\nunknown: true\n"),
        claude.replace("version: 1", "version: 2"),
        claude.replace("name: claude", "name: another"),
        claude.replace(
            "env: CLAUDE_CONFIG_DIR",
            "env: CLAUDE_CONFIG_DIR\n  typo: true",
        ),
        claude.replace("env: CLAUDE_CONFIG_DIR", "env: 1INVALID"),
        claude.replace("path: projects", "path: ../projects"),
        claude.replace("handler: claude\n", "handler: pi\n"),
        claude.replace("{prompt}", "{promtp}"),
        claude.replace("{short_id}", "prefix-{id}"),
        claude.replace("lifetime: daemon", "lifetime: terminal"),
        codex.replace("enforcement: unknown", "enforcement: supported"),
        claude.replace("key: ctrl+z", "key: tab"),
        claude.replace("key: ctrl+z", "key: ctr+z"),
        claude.replace("when: always", "when: empty_prompt"),
    ] {
        assert!(
            HarnessSpec::parse(&bad).is_err(),
            "accepted invalid definition:\n{bad}"
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
        codex.home.resolve_with(root, None).unwrap(),
        Path::new("/isolated/.codex")
    );
    assert_eq!(
        pi.home.resolve_with(root, Some(OsStr::new(""))).unwrap(),
        Path::new("/isolated/.pi/agent")
    );
    assert_eq!(
        codex
            .home
            .resolve_with(root, Some(OsStr::new("relative-home")))
            .unwrap(),
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
    let claude = &spec(HarnessKind::Claude).probe;
    assert!(claude.report(false, "supports --bg and attach").is_ok());
    assert!(claude.report(true, "supports --bg only").is_err());
    let codex = &spec(HarnessKind::Codex).probe;
    assert!(codex.report(false, "{\"cliVersion\":\"0.154\"}").is_err());
    assert!(
        codex
            .report(true, "notice\n{\"cliVersion\":\"0.154\"}\n")
            .unwrap()
            .starts_with("codex 0.154:")
    );
    assert!(
        spec(HarnessKind::Pi)
            .probe
            .report(true, "0.85.1\n")
            .unwrap()
            .starts_with("pi 0.85.1:")
    );
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
