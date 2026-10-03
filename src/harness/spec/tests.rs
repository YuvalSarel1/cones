use super::*;
use crate::{harness, history};
use std::{
    fs,
    os::unix::{ffi::OsStringExt, fs::PermissionsExt},
    time::{Duration, Instant},
};

const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

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
        (2, "/operations/rename", json!(true)),
        (1, "/operations/attach", json!(null)),
        (0, "/operations/launch/provider", json!("--provider")),
        (0, "/operations/launch/effort", json!("effort high")),
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
fn a_launch_names_a_home_only_when_it_differs_from_the_one_the_harness_would_pick() {
    let user = dirs::home_dir().unwrap();
    let override_of = |home: &Home, path: &Path| {
        let mut command = std::process::Command::new("true");
        home.set_command_home(&mut command, path);
        command
            .get_envs()
            .find(|(name, _)| *name == OsStr::new(&home.env))
            .map(|(_, value)| value.unwrap().to_owned())
    };
    let native = Home {
        env: "CONES_TEST_NATIVE_HOME".into(),
        default: HomeDefault::User {
            path: ".native".into(),
        },
        siblings: None,
    };
    assert_eq!(override_of(&native, &user.join(".native")), None);
    assert_eq!(
        override_of(&native, Path::new("/isolated/.native")),
        Some(OsString::from("/isolated/.native"))
    );
    let xdg = Home {
        env: "CONES_TEST_XDG_HOME".into(),
        default: HomeDefault::XdgData {
            path: "native".into(),
        },
        siblings: None,
    };
    assert_eq!(override_of(&xdg, &user.join(".local/share/native")), None);
    assert_eq!(
        override_of(&xdg, Path::new("/isolated/share/native")),
        Some(OsString::from("/isolated/share"))
    );
    // A fork or a resume against the default Claude home must leave CLAUDE_CONFIG_DIR exactly as
    // the machine has it: naming it moves the global configuration into that directory and the
    // native session starts with onboarding instead of the user's settings.
    let claude = &spec(HarnessKind::Claude).home;
    let ambient = std::env::var_os(&claude.env).filter(|v| !v.is_empty());
    let default = user.join(crate::fleet::CLAUDE_DIR);
    assert_eq!(
        override_of(claude, &default),
        ambient.map(|_| default.clone().into_os_string())
    );
    assert_eq!(
        override_of(claude, Path::new("/isolated/.claude")),
        Some(OsString::from("/isolated/.claude"))
    );
}

#[test]
fn probe_fixtures_preserve_success_requirements_and_reported_versions() {
    let claude = &spec(HarnessKind::Claude).launch.as_ref().unwrap().probe;
    assert!(claude.report(false, "supports --bg and attach").is_ok());
    assert!(claude.report(true, "supports --bg only").is_err());
    let codex = &spec(HarnessKind::Codex).launch.as_ref().unwrap().probe;
    // The probe must not need a daemon already running, or a cold machine reports no daemon.
    assert_eq!(codex.args, ["app-server", "daemon", "start"]);
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
            HarnessKind::Opencode => unreachable!("SQLite fixtures have their own tests"),
            _ => unreachable!("this fixture tests the original JSONL readers"),
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

#[test]
fn interpreter_entrypoints_and_aliases_do_not_match_agent_names_inside_prompts() {
    let dir = tempfile::tempdir().unwrap();
    let native = dir.path().join("cursor-agent");
    fs::write(&native, "fixture").unwrap();
    let alias = dir.path().join("agent");
    std::os::unix::fs::symlink(&native, &alias).unwrap();
    let alias_command = format!("{} -- hello", alias.display());
    let commands = [
        "/usr/bin/node /opt/node_modules/@google/gemini-cli/bundle/gemini.js --prompt-interactive=hello",
        "/usr/bin/python3 /tools/bin/kimi --prompt=hello",
        &alias_command,
        "/bin/echo gemini --prompt-interactive=hello",
        "/usr/bin/node /tools/other.js /opt/node_modules/@google/gemini-cli/bundle/gemini.js",
        "/tools/bin/gemini mcp list",
        "/tools/bin/agent other-service",
        "/usr/bin/python-backup /tools/bin/kimi",
        "/Library/Frameworks/Python.framework/Versions/3.14/Resources/Python.app/Contents/MacOS/Python /tools/bin/kimi",
    ];
    let table = commands
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{} Sun Sep 13 15:19:19 2026 {c}\n", i + 1))
        .collect::<String>();
    for (kind, expected) in [
        (HarnessKind::Gemini, vec![1]),
        (HarnessKind::Kimi, vec![2, 9]),
        (HarnessKind::Cursor, vec![3]),
    ] {
        assert_eq!(
            spec(kind)
                .discovery
                .processes(&table)
                .iter()
                .map(|p| p.pid)
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn kimi_process_titles_are_exact_and_cannot_be_claimed_by_an_argument() {
    let table = " 1 Sun Sep 13 15:19:19 2026 Kimi Code\n 2 Sun Sep 13 15:19:19 2026 /bin/echo Kimi Code\n 3 Sun Sep 13 15:19:19 2026 Kimi Code other\n";
    assert_eq!(
        spec(HarnessKind::Kimi)
            .discovery
            .processes(table)
            .iter()
            .map(|p| p.pid)
            .collect::<Vec<_>>(),
        [1]
    );
}
