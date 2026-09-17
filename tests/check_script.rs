use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new(cargo: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cargo");
        fs::write(&path, format!("#!/bin/bash\n{cargo}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/check"))
            .args(args)
            .current_dir(self.dir.path())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.dir.path().display()),
            )
            .env("TMPDIR", self.dir.path())
            .output()
            .unwrap()
    }

    fn logs(&self, output: &Output) -> std::path::PathBuf {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("Logs: "))
            .unwrap()
            .into()
    }
}

#[test]
fn successful_checks_keep_full_logs_and_only_print_suite_summaries() {
    let fixture = Fixture::new(
        r#"
printf '%s\n' "$*" > "$TMPDIR/$1.args"
if [ "$1" = test ]; then
    echo '     Running unittests src/lib.rs (target/debug/deps/cones-123)' >&2
    for ((i = 0; i < 10000; i++)); do echo "test case_$i ... ok"; done
    echo 'test result: ok. 10000 passed; 0 failed'
    echo '     Running tests/core.rs (target/debug/deps/core-456)' >&2
    echo 'test result: ok. 2 passed; 0 failed'
fi
"#,
    );
    let output = fixture.run(&[]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.len() < 1000, "{text}");
    assert!(text.contains("unittests src/lib.rs: test result: ok. 10000 passed"));
    assert!(text.contains("tests/core.rs: test result: ok. 2 passed"));
    assert!(!text.contains("case_"));
    let logs = fixture.logs(&output);
    let test_log = fs::read_to_string(logs.join("test.log")).unwrap();
    assert!(test_log.contains("test case_9999 ... ok"));
    assert!(test_log.contains("Running tests/core.rs"));
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("clippy.args")).unwrap(),
        "clippy --all-targets -- -D warnings\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("test.args")).unwrap(),
        "test --all-targets\n"
    );
    let second = fixture.run(&["fmt"]);
    assert!(second.status.success());
    assert_ne!(fixture.logs(&second), logs);
    assert_eq!(fs::read_to_string(logs.join("test.log")).unwrap(), test_log);
}

#[test]
fn failures_preserve_status_and_logs_bound_output_and_stop_the_gate() {
    for (stage, next) in [("fmt", "clippy"), ("clippy", "test"), ("test", "none")] {
        let fixture = Fixture::new(&format!(
            r#"
echo ran > "$TMPDIR/$1.ran"
if [ "$1" = {stage} ]; then
    echo 'early diagnostic' >&2
    for ((i = 0; i < 20000; i++)); do printf 'verbose output '; done
    printf '\nfinal diagnostic\n' >&2
    exit 37
fi
"#,
        ));
        let output = fixture.run(&[]);
        assert_eq!(output.status.code(), Some(37));
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(text.len() < 13000);
        assert!(text.contains(&format!("{stage}: FAILED (exit 37)")));
        assert!(text.contains("final diagnostic"));
        let log = fixture.logs(&output).join(format!("{stage}.log"));
        let saved = fs::read_to_string(log).unwrap();
        assert!(saved.starts_with("early diagnostic"));
        assert!(saved.ends_with("final diagnostic\n"));
        assert!(saved.len() > 200000);
        assert!(!fixture.dir.path().join(format!("{next}.ran")).exists());
    }
}

#[test]
fn focused_tests_preserve_arguments_and_run_from_the_checkout() {
    let fixture = Fixture::new(
        r#"
printf '%s\n' "$PWD" "$@" > "$TMPDIR/args"
"#,
    );
    let output = fixture.run(&["test", "--lib", "filter with spaces", "--", "--exact"]);
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("args")).unwrap(),
        format!(
            "{}\ntest\n--lib\nfilter with spaces\n--\n--exact\n",
            env!("CARGO_MANIFEST_DIR")
        )
    );
    assert!(!fixture.logs(&output).join("fmt.log").exists());
    assert!(!fixture.logs(&output).join("clippy.log").exists());
}
