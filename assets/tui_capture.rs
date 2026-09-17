//! README capture using the same App::draw and native Viewer as the dashboard.
//! The ignored entry point is driven by assets/tui.py; ordinary tests never start a harness.
use super::*;
use ratatui::{Terminal, backend::TestBackend};

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
        &fixture.join("discovery/.claude"),
    )?;
    app.cwd = PathBuf::from(input["cwd"].as_str().context("cwd")?);
    app.data.sessions = serde_json::from_value(input["sessions"].clone())?;
    app.rebuild();
    let selected = input["selected"].as_str().context("selected")?;
    app.cursor = app
        .visible
        .iter()
        .position(|&i| app.rows[i].kind.key() == Some(selected))
        .context("selected session in the cast")?;
    app.tick = 8;

    let mut terminal = Terminal::new(TestBackend::new(cols, rows))?;
    terminal.draw(|frame| app.draw(frame))?;
    let mut command = Command::new(input["command"][0].as_str().context("native binary")?);
    for arg in input["command"]
        .as_array()
        .context("command")?
        .iter()
        .skip(1)
    {
        command.arg(arg.as_str().context("command argument")?);
    }
    command.env_clear();
    for (key, value) in input["env"].as_object().context("env")? {
        command.env(key, value.as_str().context("environment value")?);
    }
    command.current_dir(&app.cwd);
    app.viewers.push(Open {
        key: selected.into(),
        what: "attach".into(),
        harness: Some(HarnessKind::Claude),
        viewer: Viewer::spawn(
            command,
            app.pane.height,
            app.pane.width,
            None,
            viewer::Colors::default(),
        )?,
        record: None,
        recorded: false,
        first_paint_logged: false,
        last_focused: Instant::now(),
        speculative: true,
        operation: None,
    });
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut ready_at = None;
    loop {
        app.viewers[0].viewer.pump()?;
        terminal.draw(|frame| app.draw(frame))?;
        let screen = app.viewers[0].viewer.screen().contents();
        if screen.contains("Claude Code") && screen.contains("All 4 tests passed") {
            let since = ready_at.get_or_insert_with(Instant::now);
            if since.elapsed() > Duration::from_secs(2) {
                break;
            }
        }
        anyhow::ensure!(
            Instant::now() < deadline && app.viewers[0].viewer.exited().is_none(),
            "native preview did not become ready:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::write(
        fixture.join("native.txt"),
        app.viewers[0].viewer.screen().contents(),
    )?;
    write_cells(&terminal, &fixture.join("cells.json"))
}

fn write_cells(terminal: &Terminal<TestBackend>, path: &Path) -> Result<()> {
    let buffer = terminal.backend().buffer();
    let cells: Vec<Value> = buffer
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
        .collect();
    std::fs::write(path, serde_json::to_vec(&cells)?)?;
    Ok(())
}

/// Config captures use an empty fixture and never start a viewer.
#[test]
#[ignore = "set CONES_CONFIG_CAPTURE to an output directory"]
fn capture_config() -> Result<()> {
    let output = PathBuf::from(std::env::var("CONES_CONFIG_CAPTURE")?);
    std::fs::create_dir_all(&output)?;
    let fixture = tempfile::tempdir()?;
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
    for (name, field, choice) in [
        ("cones", "activity.bucket", false),
        ("harnesses", "model", false),
        ("choices", "model", true),
        ("runs", "timeout_min", false),
    ] {
        let mut form = app.config_form();
        form.go(field_at(field));
        if choice {
            form.key(KeyCode::Enter, KeyModifiers::NONE);
        }
        app.mode = Mode::Config(form);
        let mut terminal = Terminal::new(TestBackend::new(60, 32))?;
        terminal.draw(|frame| app.draw(frame))?;
        write_cells(&terminal, &output.join(format!("{name}.json")))?;
    }
    Ok(())
}
