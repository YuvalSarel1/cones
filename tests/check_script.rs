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

    /// Runs a copy of the wrapper from this fixture's own checkout, sharing only `queue`.
    fn start(&self, queue: &std::path::Path) -> Running {
        // Distinct checkouts, sharing only the queue. No real Cargo or native CLI.
        let scripts = self.dir.path().join("checkout/scripts");
        fs::create_dir_all(&scripts).unwrap();
        for name in ["check", "check_queue.py"] {
            fs::copy(
                format!("{}/scripts/{name}", env!("CARGO_MANIFEST_DIR")),
                scripts.join(name),
            )
            .unwrap();
        }
        let log = fs::File::create(self.dir.path().join("console")).unwrap();
        let command = self.command();
        // Command's program cannot be replaced; run the copied wrapper via Bash.
        let mut copied = Command::new("/bin/bash");
        copied
            .arg(scripts.join("check"))
            .arg("test")
            .envs(command.get_envs().filter_map(|(k, v)| v.map(|v| (k, v))))
            .env("CONES_CHECK_STATE_DIR", queue)
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        // Keep the fixture cwd independent of whichever checkout starts the gate.
        copied.current_dir(self.dir.path());
        Running(copied.spawn().unwrap())
    }

    fn console(&self) -> String {
        fs::read_to_string(self.dir.path().join("console")).unwrap()
    }

    /// Waits until the console reports that this gate is queued behind another.
    fn until_waiting(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.console().contains("waiting for PID") {
            assert!(
                Instant::now() < deadline,
                "never queued:\n{}",
                self.console()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn has(&self, name: &str) -> bool {
        self.dir.path().join(name).exists()
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
        // The first gate builds, then holds its exclusive test run open. The
        // second gate's build must not start beside it.
        let first = Fixture::new(
            "case \" $* \" in *\" --no-run \"*) exit 0 ;; esac\necho $$ > \"$TMPDIR/cargo.pid\"\ntouch \"$TMPDIR/entered\"\nwhile [ ! -f \"$TMPDIR/release\" ]; do sleep 0.02; done\n",
        );
        let second = Fixture::new("touch \"$TMPDIR/entered\"\n");
        let mut a = first.start(queue.path());
        until(|| first.has("entered"));
        let mut b = second.start(queue.path());
        second.until_waiting();
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
fn two_builds_overlap_in_the_background_while_further_builds_and_test_runs_wait() {
    let cargo = r#"
case " $* " in
*" --no-run "*)
    python3 -c 'import os; print(os.getpriority(4, 0))' > "$TMPDIR/build.priority"
    touch "$TMPDIR/built"
    while [ ! -f "$TMPDIR/release" ]; do sleep 0.02; done ;;
*)
    python3 -c 'import os; print(os.getpriority(4, 0))' > "$TMPDIR/test.priority"
    touch "$TMPDIR/tested" ;;
esac
"#;
    let release = |fixture: &Fixture| fs::write(fixture.dir.path().join("release"), "").unwrap();
    let finish = |gates: Vec<(&Fixture, Running)>| {
        for (fixture, mut gate) in gates {
            assert!(gate.0.wait().unwrap().success(), "{}", fixture.console());
            let priority = |name| fs::read_to_string(fixture.dir.path().join(name)).unwrap();
            assert_eq!(
                priority("build.priority"),
                "1\n",
                "builds run in the background"
            );
            assert_eq!(
                priority("test.priority"),
                "0\n",
                "test runs keep normal priority"
            );
        }
    };

    let queue = tempfile::tempdir().unwrap();
    let [a, b, c] = [(); 3].map(|_| Fixture::new(cargo));
    let (first, second) = (a.start(queue.path()), b.start(queue.path()));
    until(|| a.has("built") && b.has("built"));
    let third = c.start(queue.path());
    c.until_waiting();
    assert!(!c.has("built"), "a third build waits for a slot");
    [&a, &b, &c].into_iter().for_each(release);
    finish(vec![(&a, first), (&b, second), (&c, third)]);

    let queue = tempfile::tempdir().unwrap();
    let [a, b, c] = [(); 3].map(|_| Fixture::new(cargo));
    let (first, second) = (a.start(queue.path()), b.start(queue.path()));
    until(|| a.has("built") && b.has("built"));
    release(&a);
    a.until_waiting();
    assert!(!a.has("tested"), "a test run waits for every build");
    // A slot is free, but the waiting test run holds the turnstile.
    let third = c.start(queue.path());
    c.until_waiting();
    assert!(
        !c.has("built"),
        "new builds cannot starve a waiting test run"
    );
    release(&b);
    until(|| c.has("built"));
    assert!(a.has("tested"), "the waiting test run went first");
    release(&c);
    finish(vec![(&a, first), (&b, second), (&c, third)]);
}

#[test]
fn a_cancelled_waiter_never_starts_and_native_overrides_do_not_reach_tests() {
    let fixture = Fixture::new(
        "touch \"$TMPDIR/ran\"\ntest -z \"${OPENCODE_DB+x}\" && test -z \"${CODEX_HOME+x}\" && test -z \"${PI_CODING_AGENT_DIR+x}\" || exit 92\n",
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
    assert!(!fixture.has("ran"), "a cancelled waiter never runs Cargo");
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
printf '%s\n' "$*" >> "$TMPDIR/$1.args"
if [ "$1" = test ] && [ "$2" != --no-run ]; then
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
        "test --no-run --all-targets\ntest --all-targets\n",
        "tests are compiled first, then run"
    );
    let stages: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("build: ") || l.starts_with("test: "))
        .collect();
    assert_eq!(stages.len(), 2, "{text}");
    for (line, stage) in stages.iter().zip(["build", "test"]) {
        let seconds = line
            .strip_prefix(&format!("{stage}: ok ("))
            .and_then(|rest| rest.strip_suffix("s)"))
            .unwrap_or_else(|| panic!("{stage} reports its duration: {text}"));
        seconds.parse::<u64>().unwrap();
    }
    let second = fixture.run(&["fmt"]);
    assert!(second.status.success());
    assert_ne!(fixture.logs(&second), logs);
    assert_eq!(fs::read_to_string(logs.join("test.log")).unwrap(), test_log);
}

#[test]
fn failures_preserve_status_and_logs_bound_output_and_stop_the_gate() {
    // Stages that need no slot run first, so their failures never queue. A failed
    // test build is reported as the build and never reaches the test run.
    for (stage, shown, next) in [
        ("fmt", "fmt", "reporting"),
        ("reporting", "reporting", "clippy"),
        ("clippy", "clippy", "test"),
        ("test", "build", "none"),
    ] {
        let fixture = Fixture::new(&format!(
            r#"
echo ran >> "$TMPDIR/$1.ran"
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
        assert!(
            text.contains(&format!("{shown}: FAILED (exit 37, ")),
            "{text}"
        );
        assert!(text.contains("final diagnostic"));
        if stage == "test" {
            let ran = fs::read_to_string(fixture.dir.path().join("test.ran")).unwrap();
            assert_eq!(ran, "ran\n", "the test run never starts");
        }
        let log = fixture.logs(&output).join(format!("{shown}.log"));
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
printf '%s\n' "$PWD" "$@" >> "$TMPDIR/args"
"#,
    );
    let output = fixture.run(&["test", "--lib", "filter with spaces", "--", "--exact"]);
    assert!(output.status.success());
    // The build takes the selection; test binary arguments go only to the run.
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("args")).unwrap(),
        format!(
            "{root}\ntest\n--no-run\n--lib\nfilter with spaces\n\
             {root}\ntest\n--lib\nfilter with spaces\n--\n--exact\n",
            root = env!("CARGO_MANIFEST_DIR")
        )
    );
    assert!(!fixture.logs(&output).join("fmt.log").exists());
    assert!(!fixture.logs(&output).join("clippy.log").exists());
}

#[test]
fn installing_head_builds_a_detached_worktree_in_the_background_and_removes_it() {
    let fixture = Fixture::new(
        r#"
printf '%s\n' "$@" > "$TMPDIR/args"
python3 -c 'import os; print("priority", os.getpriority(4, 0))' >> "$TMPDIR/args"
test -f "$3/Cargo.toml" && printf 'package\n' >> "$TMPDIR/args"
git -C "$3" rev-parse HEAD >> "$TMPDIR/args"
"#,
    );
    let root = fixture.dir.path().join("prefix");
    let state = fixture.dir.path().join("queue");
    let output = fixture
        .command()
        .env("CONES_INSTALL_ROOT", &root)
        .arg("install")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let worktree = state.join("install-worktree");
    let head = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let recorded = fs::read_to_string(fixture.dir.path().join("args")).unwrap();
    assert_eq!(
        recorded,
        format!(
            "install\n--path\n{}\n--force\n--root\n{}\n--target-dir\n{}\npriority 1\npackage\n{head}",
            worktree.display(),
            root.display(),
            state.join("install-target").display(),
        )
    );
    assert!(!worktree.exists(), "the worktree is cleaned up");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!(
            "Installed {} to {}/bin",
            &head[..7],
            root.display()
        )),
        "{output:?}"
    );
}

#[test]
fn a_worktree_without_a_target_gates_in_a_warm_one_cleaning_another_checkouts_crate() {
    let fixture = Fixture::new(
        r#"
printf '%s|%s\n' "${CARGO_TARGET_DIR-}" "$*" >> "$TMPDIR/args"
if [[ -n ${CARGO_TARGET_DIR-} ]]; then mkdir -p "$CARGO_TARGET_DIR"; fi
"#,
    );
    let gate = |name: &str| {
        let scripts = fixture.dir.path().join(name).join("scripts");
        fs::create_dir_all(&scripts).unwrap();
        // A linked worktree's .git is a file.
        fs::write(
            scripts.parent().unwrap().join(".git"),
            "gitdir: elsewhere\n",
        )
        .unwrap();
        for file in ["check", "check_queue.py"] {
            fs::copy(
                format!("{}/scripts/{file}", env!("CARGO_MANIFEST_DIR")),
                scripts.join(file),
            )
            .unwrap();
        }
        let command = fixture.command();
        let output = Command::new("/bin/bash")
            .arg(scripts.join("check"))
            .args(["test", "--lib"])
            .envs(command.get_envs().filter_map(|(k, v)| v.map(|v| (k, v))))
            .env_remove("CARGO_TARGET_DIR")
            .current_dir(fixture.dir.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let args = fixture.dir.path().join("args");
        let recorded = fs::read_to_string(&args).unwrap_or_default();
        let _ = fs::remove_file(&args);
        recorded
    };
    let queue = fixture.dir.path().join("queue");
    let built = |slot: usize| {
        let target = queue.join(format!("gate-target.{slot}"));
        format!(
            "{t}|test --no-run --lib\n{t}|test --lib\n",
            t = target.display()
        )
    };
    let cleaned = |slot: usize| {
        format!(
            "|clean --quiet -p cones -p vt100 --target-dir {}\n",
            queue.join(format!("gate-target.{slot}")).display()
        )
    };
    assert_eq!(gate("a"), built(0));
    // The same checkout keeps its slot and its incremental build.
    assert_eq!(gate("a"), built(0));
    // Another checkout takes the unused slot before evicting anyone.
    assert_eq!(gate("b"), built(1));
    // With every slot owned elsewhere, the workspace's own crates are rebuilt.
    assert_eq!(gate("c"), cleaned(0) + &built(0));
    assert_eq!(gate("a"), cleaned(0) + &built(0));
    // A checkout with its own target keeps it.
    fs::create_dir(fixture.dir.path().join("c/target")).unwrap();
    assert_eq!(gate("c"), "|test --no-run --lib\n|test --lib\n");
}
