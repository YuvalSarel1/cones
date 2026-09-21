//! README capture using the same App::draw and native Viewer as the dashboard.
//! The ignored entry point is driven by assets/tui.py; ordinary tests never start a harness.
//! src/tui.rs includes this file with #[path] under cfg(test), so an unformatted line here fails
//! `cargo fmt --check` for the whole crate and rustfmt reports it against src/tui.rs.
use super::*;
use ratatui::{Terminal, backend::TestBackend};

const NAVIGATION_MS: u64 = 150;

#[test]
#[ignore = "run through python3 assets/tui.py"]
fn capture() -> Result<()> {
    let fixture = PathBuf::from(std::env::var("CONES_README_FIXTURE")?);
    let input: Value = serde_json::from_slice(&std::fs::read(fixture.join("capture.json"))?)?;
    let cols = input["cols"].as_u64().context("cols")? as u16;
    let rows = input["rows"].as_u64().context("rows")? as u16;
    let mut app = App::new(
        Path::new("cones"),
        &fixture.join("jobs.yaml"),
        &fixture.join("state"),
        &fixture.join(".claude"),
    )?;
    app.colors = viewer::Colors {
        fg: input["colors"]["fg"].as_str().context("foreground")?.into(),
        bg: input["colors"]["bg"].as_str().context("background")?.into(),
    };
    app.data.sessions.clear();
    app.data
        .folders
        .retain(|dir| dir.starts_with(fixture.join("projects")));
    app.cwd = PathBuf::from(input["cwd"].as_str().context("cwd")?);
    app.rebuild();
    let mut terminal = Terminal::new(TestBackend::new(cols, rows))?;
    terminal.draw(|frame| app.draw(frame))?;
    for spec in input["viewers"].as_array().context("viewers")? {
        spawn_viewer(&mut app, spec)?;
    }
    let mut recording = Recording::new(&fixture)?;
    for title in ["Retry failed webhooks", "Keyboard navigation"] {
        recording.until(&mut app, &mut terminal, "initial change completed", |app| {
            reported_state(app, title) == Some("idle")
        })?;
        let id = app
            .data
            .sessions
            .iter()
            .find(|session| session.title.as_deref() == Some(title))
            .context("initial change session")?
            .session_id
            .clone();
        let open = app
            .viewers
            .iter_mut()
            .find(|open| open.key == id)
            .context("initial change viewer")?;
        open.viewer.write(b"Run the regression checks.\r");
    }
    recording.until(
        &mut app,
        &mut terminal,
        "initial sessions running and question waiting",
        |app| {
            [
                "api/.ready-retry",
                "api/.ready-events",
                "web/.ready-keyboard",
            ]
            .iter()
            .all(|path| fixture.join("projects").join(path).is_file())
                && input["viewers"].as_array().unwrap().iter().all(|spec| {
                    reported_state(app, spec["title"].as_str().unwrap())
                        == spec["initial_state"].as_str()
                })
                && app.data.sessions.len() == input["viewers"].as_array().unwrap().len()
        },
    )?;
    recording.dump(&app)?;
    let mut browse = [
        "Retry failed webhooks",
        "Deduplicate events",
        "Keyboard navigation",
    ]
    .map(|title| Ok((title_index(&app, title)?, title)))
    .into_iter()
    .collect::<Result<Vec<_>>>()?;
    browse.sort_by_key(|(index, _)| *index);
    recording.select_title(&mut app, &mut terminal, browse[0].1)?;
    terminal.draw(|frame| app.draw(frame))?;
    write_cells(&terminal, &fixture.join("cells.json"))?;
    std::fs::write(fixture.join("rolling"), b"")?;
    recording.saving = true;
    recording.scene = "browse";
    for (_, title) in browse {
        recording.select_title(&mut app, &mut terminal, title)?;
        anyhow::ensure!(
            reported_state(&app, title) == Some("active"),
            "{title} finished before browsing"
        );
        let shown = app
            .shown()
            .context(format!("{title} has no native preview"))?;
        let screen = app.viewers[shown].viewer.screen().contents();
        anyhow::ensure!(
            screen.contains("Edited 1 file") || screen.contains("event ordering"),
            "{title} preview is not populated: {screen}"
        );
        if title != "Deduplicate events" {
            anyhow::ensure!(
                !screen.contains("Update("),
                "Claude edit diff is expanded: {screen}"
            );
        }
        recording.hold(&mut app, &mut terminal, 1800)?;
    }
    recording.scene = "input";
    recording.select_title(&mut app, &mut terminal, "Session timeout policy")?;
    anyhow::ensure!(
        reported_state(&app, "Session timeout policy") == Some("blocked"),
        "question is not waiting"
    );
    recording.hold(&mut app, &mut terminal, 1800)?;
    app.key(KeyCode::Enter, KeyModifiers::NONE)?;
    anyhow::ensure!(app.focus.is_some(), "Enter did not focus the question");
    recording.hold(&mut app, &mut terminal, 700)?;
    app.key(KeyCode::Enter, KeyModifiers::NONE)?;
    recording.hold(&mut app, &mut terminal, 700)?;
    app.key(KeyCode::Enter, KeyModifiers::NONE)?;
    recording.scene = "reply";
    recording.until(&mut app, &mut terminal, "answer resumes work", |app| {
        reported_state(app, "Session timeout policy") == Some("active")
    })?;
    recording.hold(&mut app, &mut terminal, 2200)?;
    app.key(KeyCode::Char('z'), KeyModifiers::CONTROL)?;
    anyhow::ensure!(app.focus.is_none(), "Ctrl+Z did not return to list");
    recording.hold(&mut app, &mut terminal, 300)?;
    recording.scene = "folder";
    for _ in 0..app.rows.len() {
        if matches!(app.selected().map(|row| &row.kind), Some(Kind::NewFolder)) {
            break;
        }
        app.key(KeyCode::Down, KeyModifiers::NONE)?;
        recording.hold(&mut app, &mut terminal, NAVIGATION_MS)?;
    }
    anyhow::ensure!(
        matches!(app.selected().map(|row| &row.kind), Some(Kind::NewFolder)),
        "the add folder row was not reached"
    );
    recording.hold(&mut app, &mut terminal, 1200)?;
    recording.type_text(&mut app, &mut terminal, "~/projects/docs", 120)?;
    recording.hold(&mut app, &mut terminal, 700)?;
    app.key(KeyCode::Enter, KeyModifiers::NONE)?;
    anyhow::ensure!(
        app.target_dir() == fixture.join("projects/docs"),
        "new folder was not selected"
    );
    recording.hold(&mut app, &mut terminal, 1200)?;
    app.key(KeyCode::BackTab, KeyModifiers::SHIFT)?;
    anyhow::ensure!(
        harness::launchable()[app.harness] == HarnessKind::Codex,
        "Codex was not selected"
    );
    recording.hold(&mut app, &mut terminal, 700)?;
    recording.type_text(&mut app, &mut terminal, "Write a quick-start guide.", 90)?;
    recording.hold(&mut app, &mut terminal, 800)?;
    recording.scene = "launch";
    app.key(KeyCode::Enter, KeyModifiers::NONE)?;
    recording.until(
        &mut app,
        &mut terminal,
        "new session writes the guide",
        |app| {
            fixture.join("projects/docs/QUICKSTART.md").is_file()
                && app.data.sessions.iter().any(|session| {
                    session.cwd == fixture.join("projects/docs") && session.state == "done"
                })
        },
    )?;
    recording.hold(&mut app, &mut terminal, 2200)?;
    recording.dump(&app)?;
    Ok(())
}

fn reported_state<'a>(app: &'a App, title: &str) -> Option<&'a str> {
    app.data
        .sessions
        .iter()
        .find(|session| session.title.as_deref() == Some(title))
        .map(|session| session.state.as_str())
}

fn title_index(app: &App, title: &str) -> Result<usize> {
    let id = app
        .data
        .sessions
        .iter()
        .find(|session| session.title.as_deref() == Some(title))
        .context(format!("session {title}"))?
        .session_id
        .clone();
    app.visible
        .iter()
        .position(|&row| app.rows[row].kind.key() == Some(&id))
        .context("visible session")
}

fn spawn_viewer(app: &mut App, spec: &Value) -> Result<()> {
    let mut command = Command::new(spec["command"][0].as_str().context("native binary")?);
    for arg in spec["command"]
        .as_array()
        .context("command")?
        .iter()
        .skip(1)
    {
        command.arg(arg.as_str().context("command argument")?);
    }
    command.env_clear();
    for (key, value) in spec["env"].as_object().context("env")? {
        command.env(key, value.as_str().context("environment value")?);
    }
    command.current_dir(spec["cwd"].as_str().context("viewer cwd")?);
    app.viewers.push(Open {
        key: spec["session"].as_str().context("session")?.into(),
        what: "attach".into(),
        harness: Some(serde_json::from_value(spec["harness"].clone())?),
        viewer: Viewer::spawn(
            command,
            app.pane.height,
            app.pane.width,
            None,
            app.colors.clone(),
        )?,
        record: None,
        recorded: false,
        fork: None,
        first_paint_logged: false,
        last_focused: Instant::now(),
        speculative: true,
        operation: None,
    });
    Ok(())
}

fn cells(terminal: &Terminal<TestBackend>) -> Vec<Value> {
    let buffer = terminal.backend().buffer();
    buffer
        .content
        .iter()
        .map(|cell| {
            json!({
                "text": cell.symbol(),
                "fg": format!("{:?}", cell.fg),
                "bg": format!("{:?}", cell.bg),
                "bold": cell.modifier.contains(Modifier::BOLD),
                "dim": cell.modifier.contains(Modifier::DIM),
                "reverse": cell.modifier.contains(Modifier::REVERSED),
                "underline": cell.modifier.contains(Modifier::UNDERLINED),
            })
        })
        .collect()
}

fn write_cells(terminal: &Terminal<TestBackend>, path: &Path) -> Result<()> {
    std::fs::write(path, serde_json::to_vec(&cells(terminal))?)?;
    Ok(())
}

struct Recording {
    fixture: PathBuf,
    initial_titles: HashMap<String, String>,
    frame: usize,
    saving: bool,
    scene: &'static str,
}

impl Recording {
    fn new(fixture: &Path) -> Result<Self> {
        std::fs::create_dir_all(fixture.join("frames"))?;
        let input: Value = serde_json::from_slice(&std::fs::read(fixture.join("capture.json"))?)?;
        let initial_titles = input["viewers"]
            .as_array()
            .context("viewers")?
            .iter()
            .map(|spec| {
                Ok((
                    spec["session"].as_str().context("session")?.into(),
                    spec["title"].as_str().context("title")?.into(),
                ))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            fixture: fixture.into(),
            initial_titles,
            frame: 0,
            saving: false,
            scene: "warmup",
        })
    }

    /// Record each arrow press so intermediate rows and native previews remain visible.
    fn select_title(
        &mut self,
        app: &mut App,
        terminal: &mut Terminal<TestBackend>,
        title: &str,
    ) -> Result<()> {
        for _ in 0..app.visible.len() {
            let target = title_index(app, title)?;
            if app.cursor == target {
                return Ok(());
            }
            app.key(
                if app.cursor < target {
                    KeyCode::Down
                } else {
                    KeyCode::Up
                },
                KeyModifiers::NONE,
            )?;
            if self.saving {
                self.hold(app, terminal, NAVIGATION_MS)?;
            }
        }
        anyhow::bail!("could not select {title}")
    }

    fn tick(
        &mut self,
        app: &mut App,
        terminal: &mut Terminal<TestBackend>,
        millis: u64,
    ) -> Result<()> {
        let started = Instant::now();
        app.tick += 1;
        if app.refreshed.elapsed() >= Duration::from_millis(250) {
            app.reload();
        }
        // Keep native discovery and launch reconciliation, filtering before App sees any rows.
        if let Some(receiver) = app.loading.take() {
            match receiver.try_recv() {
                Ok(mut result) => {
                    if let Ok(data) = &mut result {
                        data.folders
                            .retain(|dir| dir.starts_with(self.fixture.join("projects")));
                        let thread_ids: Vec<String> =
                            std::fs::read_dir(self.fixture.join("threads"))
                                .into_iter()
                                .flatten()
                                .filter_map(|entry| {
                                    let path = entry.ok()?.path();
                                    if path.extension()?.to_str()? != "json" {
                                        return None;
                                    }
                                    let bytes = std::fs::read(path).ok()?;
                                    let value: Value = serde_json::from_slice(&bytes).ok()?;
                                    value["id"].as_str().map(str::to_owned)
                                })
                                .collect();
                        data.sessions.retain(|session| {
                            session.cwd.starts_with(self.fixture.join("projects"))
                                && (session.harness != "codex"
                                    || thread_ids.contains(&session.session_id))
                        });
                        for open in &mut app.viewers {
                            let matches: Vec<_> = data
                                .sessions
                                .iter()
                                .filter(|session| {
                                    session.pid == Some(open.viewer.pid())
                                        || self.initial_titles.get(&open.key).is_some_and(|title| {
                                            session.title.as_ref() == Some(title)
                                        })
                                })
                                .collect();
                            if let [session] = matches.as_slice() {
                                open.key = session.session_id.clone();
                            }
                        }
                    }
                    let (sender, receiver) = mpsc::channel();
                    sender.send(result)?;
                    app.loading = Some(receiver);
                    app.poll();
                }
                Err(mpsc::TryRecvError::Empty) => app.loading = Some(receiver),
                Err(error) => anyhow::bail!("discovery failed: {error}"),
            }
        }
        app.pump();
        app.poll_opening();
        terminal.draw(|frame| app.draw(frame))?;
        if self.saving {
            std::fs::write(
                self.fixture
                    .join("frames")
                    .join(format!("{:05}.json", self.frame)),
                serde_json::to_vec(&json!({
                    "duration_ms": millis,
                    "scene": self.scene,
                    "cells": cells(terminal),
                    "sessions": app.data.sessions.iter().map(|session| json!({
                        "id": session.session_id, "title": session.title, "state": session.state,
                    })).collect::<Vec<_>>(),
                    "selected": app.selected().and_then(|row| row.kind.key()),
                }))?,
            )?;
            self.frame += 1;
        }
        std::thread::sleep(Duration::from_millis(millis).saturating_sub(started.elapsed()));
        Ok(())
    }

    fn hold(
        &mut self,
        app: &mut App,
        terminal: &mut Terminal<TestBackend>,
        millis: u64,
    ) -> Result<()> {
        let mut remaining = millis;
        while remaining > 0 {
            let duration = remaining.min(100);
            self.tick(app, terminal, duration)?;
            remaining -= duration;
        }
        Ok(())
    }

    fn type_text(
        &mut self,
        app: &mut App,
        terminal: &mut Terminal<TestBackend>,
        text: &str,
        delay_ms: u64,
    ) -> Result<()> {
        for ch in text.chars() {
            app.key(KeyCode::Char(ch), KeyModifiers::NONE)?;
            self.hold(app, terminal, delay_ms)?;
        }
        Ok(())
    }

    fn until(
        &mut self,
        app: &mut App,
        terminal: &mut Terminal<TestBackend>,
        label: &str,
        ready: impl Fn(&App) -> bool,
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(45);
        while !ready(app) {
            if app.status.contains(" failed:") {
                self.dump(app)?;
                anyhow::bail!("capture action failed: {}", app.status);
            }
            if self.fixture.join("provider-error.txt").is_file() {
                self.dump(app)?;
                anyhow::bail!(
                    "demo provider failed: {}",
                    std::fs::read_to_string(self.fixture.join("provider-error.txt"))?
                );
            }
            if Instant::now() >= deadline {
                self.dump(app)?;
                anyhow::bail!("timed out: {label}; see {}", self.fixture.display());
            }
            self.tick(app, terminal, 100)?;
        }
        Ok(())
    }

    fn dump(&self, app: &App) -> Result<()> {
        for (i, open) in app.viewers.iter().enumerate() {
            std::fs::write(
                self.fixture.join(format!("native-{i}.txt")),
                open.viewer.screen().contents(),
            )?;
        }
        std::fs::write(
            self.fixture.join("sessions.json"),
            serde_json::to_vec_pretty(&app.data.sessions)?,
        )?;
        std::fs::write(self.fixture.join("status.txt"), &app.status)?;
        Ok(())
    }
}

/// Config and Help captures use a disposable config and never start a viewer.
#[test]
#[ignore = "set CONES_CONFIG_CAPTURE to an output directory"]
fn capture_config() -> Result<()> {
    let output = PathBuf::from(std::env::var("CONES_CONFIG_CAPTURE")?);
    std::fs::create_dir_all(&output)?;
    let fixture = tempfile::tempdir()?;
    std::fs::write(
        fixture.path().join("jobs.yaml"),
        "version: 4\ndefaults:\n  model: opus\njobs: []\n",
    )?;
    let mut app = App::new(
        Path::new("cones"),
        &fixture.path().join("jobs.yaml"),
        &fixture.path().join("state"),
        &fixture.path().join(".claude"),
    )?;
    app.data.sessions.clear();
    app.data.runs.clear();
    app.rebuild();
    app.split = false;
    for (name, field, choice, width, height) in [
        ("cones", "activity.bucket", false, 60, 32),
        ("harnesses", "model", false, 60, 40),
        ("harnesses-narrow", "model", false, 40, 28),
        ("choices", "model", true, 60, 32),
        ("config-help", "bedrock", false, 60, 26),
        ("runs", "timeout_min", false, 60, 24),
    ] {
        let mut form = app.config_form();
        form.go(field_at(field));
        if choice {
            form.key(KeyCode::Enter, KeyModifiers::NONE);
        }
        if name == "config-help" {
            form.key(KeyCode::F(1), KeyModifiers::NONE);
        }
        app.mode = Mode::Config(form);
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| app.draw(frame))?;
        write_cells(&terminal, &output.join(format!("{name}.json")))?;
    }
    for (name, query, width, height) in [
        ("help", "", 60, 36),
        ("help-search", "config reset", 60, 24),
        ("help-empty", "not-a-shortcut", 60, 24),
        ("help-narrow", "viewer", 40, 28),
    ] {
        let origin = app.guide_origin();
        app.mode = Mode::Guide(Guide {
            find: Input::new(query),
            ..Guide::new(&origin)
        });
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| app.draw(frame))?;
        write_cells(&terminal, &output.join(format!("{name}.json")))?;
    }
    Ok(())
}
