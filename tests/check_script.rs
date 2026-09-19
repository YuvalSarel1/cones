use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new(cargo: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cargo");
        fs::write(&path, format!("#!/bin/bash\n{cargo}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let node = dir.path().join("node");
        fs::write(
            &node,
            format!("#!/bin/bash\nset -- reporting \"$@\"\n{cargo}\n"),
        )
        .unwrap();
        fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/check"));
        command
            .current_dir(self.dir.path())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.dir.path().display()),
            )
            .env("TMPDIR", self.dir.path())
            .env("CONES_CHECK_STATE_DIR", self.dir.path().join("queue"));
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn logs(&self, output: &Output) -> std::path::PathBuf {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("Logs: "))
            .unwrap()
            .into()
    }
}

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "fixture did not reach its barrier"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn checkouts_share_one_slot_and_cancellation_or_supervisor_death_cannot_overlap_cargo() {
    use fs2::FileExt;
    for end in ["success", "cancel", "crash"] {
        let queue = tempfile::tempdir().unwrap();
        let first = Fixture::new(
            "echo $$ > \"$TMPDIR/cargo.pid\"\ntouch \"$TMPDIR/entered\"\nwhile [ ! -f \"$TMPDIR/release\" ]; do sleep 0.02; done\n",
        );
        let second = Fixture::new("touch \"$TMPDIR/entered\"\n");
        let start = |fixture: &Fixture| {
            // Distinct checkouts, sharing only the queue. No real Cargo or native CLI.
            let scripts = fixture.dir.path().join("checkout/scripts");
            fs::create_dir_all(&scripts).unwrap();
            for name in ["check", "check_queue.py"] {
                fs::copy(
                    format!("{}/scripts/{name}", env!("CARGO_MANIFEST_DIR")),
                    scripts.join(name),
                )
                .unwrap();
            }
            let log = fs::File::create(fixture.dir.path().join("console")).unwrap();
            let command = fixture.command();
            // Command's program cannot be replaced; run the copied wrapper via Bash.
            let mut copied = Command::new("/bin/bash");
            copied
                .arg(scripts.join("check"))
                .arg("test")
                .envs(command.get_envs().filter_map(|(k, v)| v.map(|v| (k, v))))
                .env("CONES_CHECK_STATE_DIR", queue.path())
                .stdout(log.try_clone().unwrap())
                .stderr(log);
            // Keep the fixture cwd independent of whichever checkout starts the gate.
            copied.current_dir(fixture.dir.path());
            Running(copied.spawn().unwrap())
        };
        let mut a = start(&first);
        until(|| first.dir.path().join("entered").exists());
        let mut b = start(&second);
        until(|| {
            fs::read_to_string(second.dir.path().join("console"))
                .unwrap()
                .contains("waiting for PID")
        });
        assert!(!second.dir.path().join("entered").exists());
        let cargo: i32 = fs::read_to_string(first.dir.path().join("cargo.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        match end {
            "success" => fs::write(first.dir.path().join("release"), "").unwrap(),
            "cancel" => {
                unsafe { libc::kill(a.0.id() as i32, libc::SIGTERM) };
                assert_eq!(
                    a.0.wait().unwrap().code(),
                    Some(143),
                    "{}",
                    fs::read_to_string(first.dir.path().join("console")).unwrap()
                );
                until(|| unsafe { libc::kill(cargo, 0) } != 0);
            }
            "crash" => {
                a.0.kill().unwrap();
                a.0.wait().unwrap();
                assert_eq!(unsafe { libc::kill(cargo, 0) }, 0);
                let lease = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(queue.path().join("check.lock"))
                    .unwrap();
                assert_eq!(
                    lease.try_lock_exclusive().unwrap_err().kind(),
                    std::io::ErrorKind::WouldBlock,
                    "the child must retain the slot after its supervisor dies"
                );
                assert!(b.0.try_wait().unwrap().is_none());
                assert!(!second.dir.path().join("entered").exists());
                fs::write(first.dir.path().join("release"), "").unwrap();
            }
            _ => unreachable!(),
        }
        until(|| second.dir.path().join("entered").exists());
        assert!(b.0.wait().unwrap().success(), "{end}");
        if end == "success" {
            assert!(a.0.wait().unwrap().success());
        }
        until(|| unsafe { libc::kill(cargo, 0) } != 0);
    }
}

#[test]
fn a_cancelled_waiter_never_starts_and_native_overrides_do_not_reach_tests() {
    let fixture = Fixture::new(
        "test -z \"${OPENCODE_DB+x}\" && test -z \"${CODEX_HOME+x}\" && test -z \"${PI_CODING_AGENT_DIR+x}\" || exit 92\n",
    );
    let queue = fixture.dir.path().join("queue");
    fs::create_dir(&queue).unwrap();
    let lease = fs::File::create(queue.join("check.lock")).unwrap();
    use fs2::FileExt;
    lease.lock_exclusive().unwrap();
    let console = fs::File::create(fixture.dir.path().join("console")).unwrap();
    let mut waiter = Running(
        fixture
            .command()
            .arg("test")
            .stdout(console)
            .spawn()
            .unwrap(),
    );
    until(|| {
        fs::read_to_string(fixture.dir.path().join("console"))
            .unwrap()
            .contains("waiting")
    });
    unsafe { libc::kill(waiter.0.id() as i32, libc::SIGINT) };
    assert_eq!(waiter.0.wait().unwrap().code(), Some(130));
    assert!(
        !fs::read_to_string(fixture.dir.path().join("console"))
            .unwrap()
            .contains("Logs:")
    );
    drop(lease);
    let output = fixture
        .command()
        .arg("test")
        .env("OPENCODE_DB", ":memory:")
        .env("CODEX_HOME", "/host/codex")
        .env("PI_CODING_AGENT_DIR", "/host/pi")
        .output()
        .unwrap();
    assert!(output.status.success());
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
    for (stage, next) in [
        ("fmt", "clippy"),
        ("clippy", "test"),
        ("test", "reporting"),
        ("reporting", "none"),
    ] {
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
