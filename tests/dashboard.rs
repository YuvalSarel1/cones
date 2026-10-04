//! The real dashboard binary in a pty, read through a vt100 screen. No model is called.
//!
//! `Dashboard` owns a disposable HOME, state dir, jobs file and project folder, starts
//! `cones` on a pseudo terminal and keeps the screen it draws. Flows press keys and wait
//! for text, then `capture` the screen. Each test leaves its artifact under
//! `$CARGO_TARGET_TMPDIR/e2e/<test>/`: the numbered screens it asserted on, the keys it
//! pressed, and the files cones wrote, with the fixture root shown as `$ROOT` so a rerun
//! produces the same artifact. Flow tests live in `dashboard_*.rs` beside this file.
use std::{
    fmt::Write as _,
    fs,
    io::{Read, Write},
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn open_folders(root: &Path, dirs: &[PathBuf]) {
    fs::write(
        root.join("state/open-folders.json"),
        serde_json::to_vec(dirs).unwrap(),
    )
    .unwrap();
}

/// The tap formula's `url` line, the only line the update check reads.
pub fn formula(version: &str) -> String {
    format!(
        "class Cones < Formula\n  url \"https://github.com/YuvalSarel1/cones/archive/refs/tags/v{version}.tar.gz\"\nend\n"
    )
}

pub const ROWS: u16 = 35;
pub const COLS: u16 = 140;
const HARNESSES: [&str; 10] = [
    "claude", "codex", "pi", "opencode", "gemini", "cursor", "copilot", "amp", "droid", "kimi",
];

pub struct Dashboard {
    pub root: tempfile::TempDir,
    child: Option<Child>,
    master: Option<std::fs::File>,
    screen: Arc<Mutex<vt100::Parser>>,
    artifact: PathBuf,
    shots: usize,
    keys: String,
}

impl Dashboard {
    /// A fixture with every harness disabled except `enabled`. Nothing starts yet, so a test
    /// can write transcripts, registries or jobs first.
    pub fn new(test: &str, enabled: &[&str]) -> Self {
        let root = tempfile::Builder::new()
            .prefix("cones-e2e-")
            .tempdir_in("/tmp")
            .unwrap();
        for dir in ["state", "home/.local/bin", "project"] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        let mut jobs = String::from("version: 4\ndefaults:\n");
        for h in HARNESSES {
            let _ = writeln!(jobs, "  {h}_enabled: {}", enabled.contains(&h));
        }
        jobs.push_str("jobs: []\n");
        fs::write(root.path().join("jobs.yaml"), jobs).unwrap();
        let project = root.path().join("project");
        open_folders(root.path(), &[project]);
        // A fresh tap formula at this version, so no dashboard checks GitHub for updates.
        fs::write(
            root.path().join("state/formula.rb"),
            formula(env!("CARGO_PKG_VERSION")),
        )
        .unwrap();
        let artifact = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("e2e")
            .join(test);
        let _ = fs::remove_dir_all(&artifact);
        fs::create_dir_all(&artifact).unwrap();
        Self {
            root,
            child: None,
            master: None,
            screen: Arc::new(Mutex::new(vt100::Parser::new(ROWS, COLS, 0))),
            artifact,
            shots: 0,
            keys: String::new(),
        }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    pub fn home(&self) -> PathBuf {
        self.path("home")
    }

    /// Keep `dirs` open in the list, as `+ add folder` would, replacing the project folder.
    pub fn open_folders(&self, dirs: &[PathBuf]) {
        open_folders(self.root.path(), dirs);
    }

    pub fn project(&self) -> PathBuf {
        self.path("project")
    }

    /// Put `fixture` (a file under tests/fixtures) on the launch path as `name`.
    pub fn install(&self, name: &str, fixture: &str) {
        let target = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(fixture);
        std::os::unix::fs::symlink(target, self.path("home/.local/bin").join(name)).unwrap();
    }

    fn env(&self) -> Vec<(&'static str, PathBuf)> {
        let home = self.home();
        vec![
            ("HOME", home.clone()),
            ("CLAUDE_CONFIG_DIR", home.join(".claude")),
            ("CODEX_HOME", home.join(".codex")),
            ("PI_CODING_AGENT_DIR", home.join(".pi")),
            ("XDG_DATA_HOME", home.join(".local/share")),
            ("SHELL", PathBuf::from("/bin/zsh")),
            ("TERM", PathBuf::from("xterm-256color")),
        ]
    }

    /// Start the dashboard and wait for its first frame.
    pub fn start(&mut self) {
        let (mut master, mut slave) = (0, 0);
        let mut size = libc::winsize {
            ws_row: ROWS,
            ws_col: COLS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        assert_eq!(opened, 0, "openpty");
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        let mut command = Command::new(env!("CARGO_BIN_EXE_cones"));
        command
            .arg("--debug")
            .current_dir(self.project())
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        command.args([
            "--jobs".as_ref(),
            self.path("jobs.yaml").as_os_str(),
            "--state-dir".as_ref(),
            self.path("state").as_os_str(),
        ]);
        for name in [
            "ZDOTDIR",
            "CONES_ZDOTDIR",
            "OPENCODE_DB",
            "OPENCODE_TUI_CONFIG",
            "AWS_PROFILE",
            "CLAUDE_CODE_USE_BEDROCK",
        ] {
            command.env_remove(name);
        }
        command.envs(self.env()).env("CONES_TEST_FAST", "1");
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let master = unsafe { std::fs::File::from_raw_fd(master) };
        let mut reader = master.try_clone().unwrap();
        let screen = self.screen.clone();
        std::thread::spawn(move || {
            let mut buf = [0; 65536];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                screen.lock().unwrap().process(&buf[..n]);
            }
        });
        self.child = Some(child);
        self.master = Some(master);
        self.wait_for("first frame", |s| !s.trim().is_empty());
    }

    /// The screen as plain text, one line per row, trailing spaces trimmed.
    pub fn screen(&self) -> String {
        let parser = self.screen.lock().unwrap();
        let mut out = String::new();
        for row in parser.screen().rows(0, COLS) {
            out.push_str(row.trim_end());
            out.push('\n');
        }
        out
    }

    /// The foreground colour of the first cell of the first `text` on the screen.
    pub fn colour_of(&self, text: &str) -> Option<vt100::Color> {
        let parser = self.screen.lock().unwrap();
        let screen = parser.screen();
        for row in 0..ROWS {
            let mut line = String::new();
            let mut cols = Vec::new();
            for col in 0..COLS {
                if let Some(cell) = screen.cell(row, col)
                    && !cell.is_wide_continuation()
                {
                    let text = cell.contents();
                    let text = if text.is_empty() { " " } else { text };
                    cols.extend(std::iter::repeat_n(col, text.len()));
                    line.push_str(text);
                }
            }
            if let Some(at) = line.find(text) {
                return screen.cell(row, cols[at]).map(|c| c.fgcolor());
            }
        }
        None
    }

    /// Wait until `ready` holds on a settled screen, failing with the last screen. Settled
    /// means unchanged for 100ms: one action can draw several frames (a notice, then the
    /// reloaded table), and callers assert on more than what they waited for.
    pub fn wait_for(&self, what: &str, mut ready: impl FnMut(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut last = String::new();
        loop {
            let screen = self.screen();
            if ready(&screen) && screen == last {
                return screen;
            }
            if ready(&screen) {
                last = screen;
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            last.clear();
            if Instant::now() > deadline {
                panic!("timed out waiting for {what}; screen:\n{screen}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until the screen shows `text`.
    pub fn wait_text(&self, text: &str) -> String {
        self.wait_for(&format!("{text:?}"), |s| s.contains(text))
    }

    /// Wait until any file at `rel` satisfies `ready`, then return its contents.
    pub fn wait_file(&self, rel: &str, mut ready: impl FnMut(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let text = fs::read_to_string(self.path(rel)).unwrap_or_default();
            if ready(&text) {
                return text;
            }
            if Instant::now() > deadline {
                panic!(
                    "timed out waiting for {rel}:\n{text}\nscreen:\n{}",
                    self.screen()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Send raw bytes, recorded by `label` in the artifact's key log.
    pub fn press(&mut self, label: &str, bytes: &[u8]) {
        let _ = writeln!(self.keys, "{label}");
        let master = self.master.as_mut().expect("dashboard started");
        master.write_all(bytes).unwrap();
        master.flush().unwrap();
        // One crossterm event per write; a burst can be read as a paste.
        std::thread::sleep(Duration::from_millis(60));
    }

    pub fn typed(&mut self, text: &str) {
        let _ = writeln!(self.keys, "type {text:?}");
        for c in text.chars() {
            let mut buf = [0; 4];
            let master = self.master.as_mut().expect("dashboard started");
            master
                .write_all(c.encode_utf8(&mut buf).as_bytes())
                .unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(60));
    }

    /// Save the screen as the next numbered artifact and return it.
    pub fn capture(&mut self, label: &str) -> String {
        self.shots += 1;
        let screen = self.screen();
        fs::write(
            self.artifact.join(format!("{:02}-{label}.txt", self.shots)),
            self.rooted(&screen),
        )
        .unwrap();
        let _ = writeln!(self.keys, "# capture {:02}-{label}", self.shots);
        screen
    }

    /// Copy a file cones wrote into the artifact.
    pub fn keep(&self, rel: &str) {
        self.keep_as(rel, &rel.replace('/', "_"));
    }

    /// Copy a file cones wrote into the artifact under `name`, for a file kept at several steps.
    pub fn keep_as(&self, rel: &str, name: &str) {
        let text = fs::read_to_string(self.path(rel)).unwrap();
        fs::write(self.artifact.join(name), self.rooted(&text)).unwrap();
    }

    fn rooted(&self, text: &str) -> String {
        let root = self.root.path().display().to_string();
        // macOS reaches /tmp as /private/tmp.
        text.replace(&format!("/private{root}"), "$ROOT")
            .replace(&root, "$ROOT")
    }

    /// Ctrl+C twice, then wait for exit.
    pub fn quit(&mut self) {
        self.press("ctrl+c", b"\x03");
        self.press("ctrl+c", b"\x03");
        let child = self.child.as_mut().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "dashboard did not quit:\n{}",
                self.screen()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        let _ = fs::write(self.artifact.join("keys.txt"), &self.keys);
        eprintln!("e2e artifact: {}", self.artifact.display());
        if let Some(child) = self.child.as_mut()
            && child.try_wait().ok().flatten().is_none()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Stop every host this fixture started and every process its registries name.
        let terminals = self.path("state/terminals");
        for entry in fs::read_dir(terminals).into_iter().flatten().flatten() {
            if let Ok(record) = fs::read_to_string(entry.path())
                && let Ok(value) = serde_json::from_str::<serde_json::Value>(&record)
                && let Some(pid) = value["session"]["pid"].as_i64()
            {
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
        }
        let sessions = self.home().join(".claude/sessions");
        for entry in fs::read_dir(sessions).into_iter().flatten().flatten() {
            if let Ok(record) = fs::read_to_string(entry.path())
                && let Ok(value) = serde_json::from_str::<serde_json::Value>(&record)
                && let Some(pid) = value["pid"].as_i64()
            {
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
        }
    }
}

#[test]
fn smoke_the_dashboard_draws_the_open_folder() {
    let mut d = Dashboard::new("smoke", &[]);
    d.start();
    let screen = d.capture("first-frame");
    assert!(screen.contains("no sessions here"), "{screen}");
    assert!(screen.contains("terminal (zsh)"), "{screen}");
    d.quit();
}
