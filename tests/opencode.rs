//! Token-free fixtures of OpenCode's native SQLite tables and process argv.
use cones::{
    config::{HarnessKind, Policy},
    harness,
    history::{self, Query, Source},
    opencode,
    transcript::{self, Target},
};
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn sql(db: &Path, sql: &str) {
    let mut child = Command::new("sqlite3")
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(sql.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("opencode.db");
    sql(
        &db,
        include_str!("../assets/harnesses/fixtures/opencode.sql"),
    );
    (dir, db)
}

fn page(reader: &mut history::Reader, query: Query) -> history::Page {
    assert!(reader.request(query).unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(page) = reader.poll() {
            return page.unwrap();
        }
        assert!(Instant::now() < deadline, "history worker timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn preview(reader: &mut transcript::Reader, db: &Path, id: &str) -> transcript::Response {
    assert!(
        reader
            .request(Target {
                key: format!("opencode:{id}"),
                harness: "opencode".into(),
                path: db.to_owned(),
                session_id: Some(id.into()),
            })
            .unwrap()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(response) = reader.poll() {
            return response.unwrap();
        }
        assert!(Instant::now() < deadline, "transcript worker timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn history_pages_one_database_without_mixing_conversations_or_inventing_usage() {
    let (dir, db) = fixture();
    let before = fs::read(&db).unwrap();
    let mut reader = history::Reader::new(vec![Source {
        harness: HarnessKind::Opencode,
        home: dir.path().into(),
    }])
    .unwrap();
    let query = || Query {
        hydrate: true,
        ..Query::default()
    };
    let first = page(&mut reader, query());
    assert_eq!(
        first.total, 2,
        "archived, child and directory-less sessions are excluded"
    );
    assert_eq!(first.entries[0].title.as_deref(), Some("Named OpenCode"));
    let columns = first.entries[0].columns.as_ref().unwrap();
    assert_eq!(
        columns.model.as_deref(),
        Some("fixture-provider/fixture-model")
    );
    assert_eq!((columns.tokens_in, columns.tokens_out), (Some(43), Some(5)));
    assert_eq!(columns.context_tokens, Some(36));
    assert_eq!(columns.context_window, None);
    assert_eq!(columns.cost_usd, Some(0.3));
    assert_eq!(columns.last.as_deref(), Some("Done"));
    assert_eq!(
        first.entries[1].columns.as_ref().unwrap().last.as_deref(),
        Some("Other answer")
    );
    let cached = page(&mut reader, query());
    assert_eq!(cached.stats.column_cache_hits, 2);
    let archived = page(
        &mut reader,
        Query {
            include_archived: true,
            ..Query::default()
        },
    );
    assert_eq!(archived.total, 3);
    assert!(archived.entries.last().unwrap().archived);
    assert_eq!(
        fs::read(&db).unwrap(),
        before,
        "browsing writes no native data"
    );
}

#[test]
fn previews_select_text_exclude_injected_parts_and_keep_cache_identity() {
    let (_dir, db) = fixture();
    let mut reader = transcript::Reader::new().unwrap();
    let first = preview(&mut reader, &db, "ses_fixture").result.unwrap();
    assert_eq!(first.messages.len(), 3);
    assert_eq!(
        first.messages[0].text,
        "Inspect the fixture\nand explain it"
    );
    assert_eq!(first.messages[2].text, "Done\nAdditional detail");
    assert!(first.messages[0].at.is_some());
    let other = preview(&mut reader, &db, "ses_second").result.unwrap();
    assert_eq!(other.messages[0].text, "Other answer");
    assert!(preview(&mut reader, &db, "ses_fixture").cache_hit);
    assert!(preview(&mut reader, &db, "ses_deleted").result.is_err());
    assert!(opencode::require_session(&db, "ses_x'; DROP TABLE session;--").is_err());
    opencode::require_session(&db, "ses_fixture").unwrap();
}

#[test]
fn wal_only_updates_invalidate_history_columns_and_previews() {
    let (dir, db) = fixture();
    let alias = dir.path().join("alias.sqlite");
    std::os::unix::fs::symlink(&db, &alias).unwrap();
    // Keep the connection open so SQLite leaves committed writes in its WAL.
    let mut writer = Command::new("sqlite3")
        .arg(&db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let input = writer.stdin.as_mut().unwrap();
    writeln!(
        input,
        "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; BEGIN; SELECT 1 FROM session LIMIT 1;"
    )
    .unwrap();
    input.flush().unwrap();
    let mut output = BufReader::new(writer.stdout.take().unwrap());
    for expected in ["wal\n", "0\n", "1\n"] {
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        assert_eq!(line, expected);
    }
    let mut history = history::Reader::new(vec![Source {
        harness: HarnessKind::Opencode,
        home: dir.path().into(),
    }])
    .unwrap();
    let first = page(
        &mut history,
        Query {
            hydrate: true,
            ..Query::default()
        },
    );
    let mut reader = transcript::Reader::new().unwrap();
    preview(&mut reader, &alias, "ses_fixture").result.unwrap();
    let before = fs::read(&db).unwrap();
    sql(&db, "UPDATE session SET title='Changed title', time_updated=time_updated+1 WHERE id='ses_fixture';
        UPDATE part SET data='{\"type\":\"text\",\"text\":\"Changed answer\"}' WHERE id='prt_b2';");
    assert!(
        fs::read(&db).unwrap() == before,
        "fixture really changed only the WAL"
    );
    let next = page(
        &mut history,
        Query {
            refresh: true,
            hydrate: true,
            ..Query::default()
        },
    );
    assert!(next.generation > first.generation);
    assert_eq!(next.entries[0].title.as_deref(), Some("Changed title"));
    assert_eq!(
        next.entries[0].columns.as_ref().unwrap().last.as_deref(),
        Some("Changed answer")
    );
    let next = preview(&mut reader, &alias, "ses_fixture");
    assert!(!next.cache_hit);
    assert_eq!(
        next.result.unwrap().messages.last().unwrap().text,
        "Changed answer"
    );
    drop(writer.stdin.take());
    assert!(writer.wait().unwrap().success());
}

#[test]
fn live_rows_require_native_identity_and_do_not_guess_state_from_finished_messages() {
    let (dir, _) = fixture();
    let ps = "\
11 Thu Sep 17 10:00:00 2026 /tmp/opencode --session ses_fixture
12 Thu Sep 17 10:00:00 2026 opencode
13 Thu Sep 17 10:00:00 2026 opencode serve --port 4096
14 Thu Sep 17 10:00:00 2026 opencode db path
15 Thu Sep 17 10:00:00 2026 vim opencode
16 Thu Sep 17 10:00:00 2026 opencode --prompt=explain --session ses_fixture
17 Thu Sep 17 10:00:00 2026 opencode attach http://localhost:4096 --session ses_fixture
";
    let mut procs = opencode::processes(ps);
    assert_eq!(
        procs.iter().map(|p| p.pid).collect::<Vec<_>>(),
        [11, 12, 16, 17]
    );
    for p in &mut procs {
        p.cwd = Some("/fixture".into());
    }
    let rows = opencode::rows(dir.path(), &procs).unwrap();
    assert_eq!(rows[0].session_id, "ses_fixture");
    assert_eq!(rows[0].state, "-");
    assert_eq!(rows[0].title.as_deref(), Some("Named OpenCode"));
    assert_eq!(rows[0].context_tokens, Some(36));
    assert!(rows.iter().all(|row| row.own_terminal()));
    for row in &rows[1..] {
        assert!(row.session_id.starts_with("opencode-"));
        assert_eq!(row.model, None);
        assert_eq!(row.tokens_in, None);
    }
}

#[test]
fn launch_preserves_prompt_and_native_permissions_and_home_overrides() {
    let spec = harness::spec(HarnessKind::Opencode);
    assert!(harness::launchable().contains(&HarnessKind::Opencode));
    assert!(harness::adapter(HarnessKind::Opencode).is_err());
    let prompt = "--auto; $(touch nope)\n--session ses_other";
    let args = harness::session_args(
        HarnessKind::Opencode,
        None,
        prompt,
        &Policy {
            opencode_model: Some("provider/model".into()),
            ..Policy::default()
        },
    )
    .unwrap();
    assert_eq!(
        args,
        ["--model", "provider/model", &format!("--prompt={prompt}")]
    );
    let root = Path::new("/fixture/.claude");
    assert_eq!(
        spec.home.resolve_with(root, Path::new("/user"), None),
        Path::new("/user/.local/share/opencode")
    );
    assert_eq!(
        spec.home
            .resolve_with(root, Path::new("/user"), Some("/data".as_ref())),
        Path::new("/data/opencode")
    );
    let mut command = Command::new("opencode");
    spec.home
        .set_command_home(&mut command, Path::new("/original/opencode"));
    assert_eq!(
        command.get_envs().next(),
        Some(("XDG_DATA_HOME".as_ref(), Some("/original".as_ref())))
    );
    let policy: Policy = serde_yaml::from_str("opencode_model: provider/model").unwrap();
    let value: Value = serde_json::to_value(policy).unwrap();
    assert_eq!(value["opencode_model"], "provider/model");
}

#[test]
fn checkpointed_databases_with_uri_characters_remain_read_only() {
    let (dir, db) = fixture();
    sql(&db, "PRAGMA journal_mode=WAL;");
    let renamed = dir.path().join("history #?%.db");
    fs::rename(&db, &renamed).unwrap();
    let before = fs::read(&renamed).unwrap();
    let mut reader = transcript::Reader::new().unwrap();
    assert_eq!(
        preview(&mut reader, &renamed, "ses_fixture")
            .result
            .unwrap()
            .messages
            .last()
            .unwrap()
            .text,
        "Done\nAdditional detail"
    );
    assert!(fs::read(&renamed).unwrap() == before);
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        1,
        "readers create no WAL or SHM"
    );
}

#[test]
fn native_launch_and_resume_preserve_the_database_and_probe_stderr() {
    const CHILD: &str = "CONES_OPENCODE_COMMAND_FIXTURE";
    if let Some(home) = std::env::var_os(CHILD) {
        let home = PathBuf::from(home);
        let start = harness::start(
            HarnessKind::Opencode,
            &home,
            "--auto literal",
            &Policy::default(),
        )
        .unwrap();
        let harness::Start::Foreground(command) = start else {
            panic!("OpenCode is a terminal client")
        };
        assert_eq!(command.get_program(), home.join(".opencode/bin/opencode"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["--prompt=--auto literal"]
        );
        assert_eq!(command.get_current_dir(), Some(home.as_path()));
        let entry = history::Entry {
            key: history::Key {
                harness: "opencode".into(),
                home: home.join("native/opencode"),
                session_id: "ses_fixture".into(),
            },
            cwd: home.clone(),
            transcript: home.join("opencode.db"),
            archived: false,
            started: None,
            last_activity: None,
            title: None,
            columns: None,
        };
        let command = harness::resume_history(&entry).unwrap();
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["--session", "ses_fixture"]
        );
        let env: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert_eq!(
            env[std::ffi::OsStr::new("OPENCODE_DB")],
            Some(entry.transcript.as_os_str())
        );
        assert_eq!(
            env[std::ffi::OsStr::new("XDG_DATA_HOME")],
            Some(home.join("native").as_os_str())
        );
        return;
    }
    let (dir, _) = fixture();
    let bin = dir.path().join(".opencode/bin");
    fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("opencode");
    fs::write(&fake, "#!/bin/sh\n[ \"$1\" = --help ] || exit 91\nprintf '%s\\n' '--prompt --session --model' >&2\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(fake, fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "native_launch_and_resume_preserve_the_database_and_probe_stderr",
            "--nocapture",
        ])
        .env(CHILD, dir.path())
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn large_messages_keep_a_bounded_preview_and_the_original_reply_headline() {
    let (dir, db) = fixture();
    sql(
        &db,
        "UPDATE part SET data=json_object('type','text','text',
        'First line' || char(10) || replace(hex(zeroblob(40000)), '00', 'x')
        || char(27) || '[31mEnd') WHERE id='prt_b2';",
    );
    let mut reader = transcript::Reader::new().unwrap();
    let doc = preview(&mut reader, &db, "ses_fixture").result.unwrap();
    assert!(doc.earlier);
    assert!(doc.messages.last().unwrap().text.ends_with("End"));
    assert!(!doc.messages.last().unwrap().text.contains('\x1b'));
    assert!(doc.bytes_read < 256 * 1024);
    let mut reader = history::Reader::new(vec![Source {
        harness: HarnessKind::Opencode,
        home: dir.path().into(),
    }])
    .unwrap();
    let page = page(
        &mut reader,
        Query {
            hydrate: true,
            ..Query::default()
        },
    );
    assert_eq!(
        page.entries[0].columns.as_ref().unwrap().last.as_deref(),
        Some("First line")
    );
}
