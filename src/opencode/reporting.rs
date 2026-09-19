//! Native TUI reports for viewers cones launches. The temporary config adds one
//! observer and retains the user's explicit TUI settings and relative paths.
use super::{millis, valid_id};
use crate::{config::HarnessKind, cost, fleet::Session, harness};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read, Write},
    path::PathBuf,
    process::Command,
};

pub(crate) const ENABLE: &str = "CONES_OPENCODE_VIEWER";

pub(crate) struct Reporter {
    _directory: tempfile::TempDir,
    _config: tempfile::NamedTempFile,
    report: PathBuf,
}

impl Reporter {
    pub(crate) fn prepare(command: &mut Command) -> io::Result<Option<Self>> {
        if !command
            .get_envs()
            .any(|(key, value)| key == ENABLE && value.is_some())
        {
            return Ok(None);
        }
        command.env_remove(ENABLE);
        let directory = tempfile::Builder::new()
            .prefix("cones-opencode-")
            .tempdir()?;
        let plugin = directory.path().join("report.mjs");
        fs::write(
            &plugin,
            include_str!("../../assets/harnesses/opencode-report.mjs"),
        )?;
        let report = directory.path().join("session.json");
        let original = command
            .get_envs()
            .find(|(key, _)| *key == "OPENCODE_TUI_CONFIG")
            .map(|(_, value)| value.map(PathBuf::from))
            .unwrap_or_else(|| std::env::var_os("OPENCODE_TUI_CONFIG").map(PathBuf::from))
            .filter(|path| !path.as_os_str().is_empty());
        let cwd = std::env::current_dir()?.join(
            command
                .get_current_dir()
                .unwrap_or(std::path::Path::new(".")),
        );
        let original = original.map(|path| cwd.join(path));
        let mut settings = match original.as_ref().filter(|path| path.is_file()) {
            Some(path) => jsonc(&fs::read_to_string(path)?)?,
            None => json!({}),
        };
        let object = settings.as_object_mut().ok_or_else(|| {
            io::Error::other("OpenCode's explicit TUI configuration must be an object")
        })?;
        // Native TuiConfig normalizes a nested `tui` object. Preserve its plugins
        // when the explicit configuration uses that legacy form.
        let plugins = object
            .get("plugin")
            .or_else(|| object.get("tui").and_then(|tui| tui.get("plugin")))
            .cloned()
            .unwrap_or_else(|| json!([]));
        let mut plugins = plugins.as_array().cloned().ok_or_else(|| {
            io::Error::other("OpenCode's TUI plugin configuration must be an array")
        })?;
        plugins.push(json!([plugin, {"path": report}]));
        object.insert("plugin".into(), plugins.into());
        // Keep native file substitutions, sound paths and existing relative plugin
        // paths relative to the same directory as the user's explicit config.
        let parent = original
            .as_ref()
            .filter(|path| path.is_file())
            .and_then(|path| path.parent())
            .unwrap_or(directory.path());
        let mut config = tempfile::Builder::new()
            .prefix(".cones-tui-")
            .suffix(".json")
            .tempfile_in(parent)?;
        serde_json::to_writer(&mut config, &settings).map_err(io::Error::other)?;
        config.flush()?;
        command.env("OPENCODE_TUI_CONFIG", config.path());
        Ok(Some(Self {
            _directory: directory,
            _config: config,
            report,
        }))
    }

    pub(crate) fn read(&self, pid: u32) -> Option<Report> {
        let mut bytes = Vec::new();
        let file = fs::File::open(&self.report).ok()?;
        if file.metadata().ok()?.modified().ok()?.elapsed().ok()?
            > std::time::Duration::from_secs(5)
        {
            return None;
        }
        file.take(64 * 1024 + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() > 64 * 1024 {
            return None;
        }
        Report::parse(&bytes, pid)
    }
}

pub(crate) struct Report(Value);

impl Report {
    pub(crate) fn parse(bytes: &[u8], pid: u32) -> Option<Self> {
        let value: Value = serde_json::from_slice(bytes).ok()?;
        if value["version"] != 1 || value["pid"].as_u64() != Some(u64::from(pid)) {
            return None;
        }
        value.get("session")?;
        if !value["session"].is_null()
            && (!value["session"]["id"].as_str().is_some_and(valid_id)
                || !value["session"]["directory"]
                    .as_str()
                    .is_some_and(|s| std::path::Path::new(s).is_absolute()))
        {
            return None;
        }
        Some(Self(value))
    }

    pub(crate) fn apply(&self, row: &mut Session) {
        let session = &self.0["session"];
        row.session_id = session["id"].as_str().map_or_else(
            || format!("opencode-{}", row.pid.unwrap_or_default()),
            str::to_owned,
        );
        row.title = session["title"].as_str().and_then(crate::fleet::headline);
        if let Some(directory) = session["directory"].as_str() {
            row.cwd = directory.into();
        }
        row.started = millis(&session["time"]["created"]).or(row.started);
        row.last_activity = millis(&session["time"]["updated"]);
        row.model = session["model"]["id"].as_str().map(|model| {
            session["model"]["providerID"].as_str().map_or_else(
                || model.to_owned(),
                |provider| format!("{provider}/{model}"),
            )
        });
        row.cost_usd = session["cost"].as_f64().filter(|usd| cost::valid(*usd));
        row.cost_info = row.cost_usd.map(|_| cost::Info::reported());
        let tokens = &session["tokens"];
        row.tokens_in = sum(tokens, &["/input", "/cache/read", "/cache/write"]);
        row.tokens_out = tokens["output"].as_u64();
        row.context_tokens = sum(
            &self.0["context"],
            &[
                "/input",
                "/output",
                "/reasoning",
                "/cache/read",
                "/cache/write",
            ],
        );
        row.context_window = None;
        row.last = self.0["last"].as_str().and_then(crate::fleet::headline);
        // A startup --session argument can name a different conversation after a
        // native session switch. Never keep metadata tied to that old argument.
        row.transcript_path = None;
        let event = json!({
            "type": "session.status",
            "status": self.0["status"],
            "waiting": self.0["waiting"],
        });
        row.state = if session.is_null() {
            "-"
        } else {
            harness::spec(HarnessKind::Opencode)
                .state
                .read(&event)
                .unwrap_or("-")
        }
        .into();
    }
}

fn sum(value: &Value, pointers: &[&str]) -> Option<u64> {
    pointers.iter().try_fold(0_u64, |total, pointer| {
        total.checked_add(value.pointer(pointer)?.as_u64()?)
    })
}

/// JSONC permits comments and trailing commas, while retaining JSON string rules.
fn jsonc(text: &str) -> io::Result<Value> {
    let mut bytes = text.as_bytes().to_vec();
    let mut i = 0;
    let mut quoted = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if quoted => i += 1,
            b'"' => quoted = !quoted,
            b'/' if !quoted && bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    bytes[i] = b' ';
                    i += 1;
                }
                continue;
            }
            b'/' if !quoted && bytes.get(i + 1) == Some(&b'*') => {
                bytes[i..i + 2].fill(b' ');
                i += 2;
                while i + 1 < bytes.len() && &bytes[i..i + 2] != b"*/" {
                    bytes[i] = b' ';
                    i += 1;
                }
                if i + 1 >= bytes.len() {
                    return Err(io::Error::other("unterminated OpenCode TUI config comment"));
                }
                bytes[i..i + 2].fill(b' ');
                i += 2;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    quoted = false;
    i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if quoted => i += 1,
            b'"' => quoted = !quoted,
            b',' if !quoted
                && bytes[i + 1..]
                    .iter()
                    .find(|b| !b.is_ascii_whitespace())
                    .is_some_and(|b| matches!(b, b']' | b'}')) =>
            {
                bytes[i] = b' ';
            }
            _ => {}
        }
        i += 1;
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_reports_supply_identity_status_model_activity_and_cost() {
        let directory = tempfile::tempdir().unwrap();
        let process = crate::opencode::Process {
            pid: 42,
            started: chrono::Utc::now(),
            cwd: Some(directory.path().to_owned()),
            session_id: None,
        };
        let mut row = crate::opencode::rows(directory.path(), &[process])
            .unwrap()
            .remove(0);
        let mut value = json!({
            "version": 1,
            "pid": 42,
            "session": {
                "id": "ses_native",
                "directory": directory.path(),
                "title": "Native session",
                "model": {"id": "model", "providerID": "provider"},
                "time": {"created": 1789600000000_i64, "updated": 1789600001000_i64},
                "cost": 1.25,
                "tokens": {"input": 12, "output": 4, "cache": {"read": 7, "write": 3}}
            },
            "status": {"type": "busy"},
            "context": {"input": 12, "output": 4, "reasoning": 2, "cache": {"read": 7, "write": 3}}
        });
        let apply = |value: &Value, row: &mut Session| {
            Report::parse(&serde_json::to_vec(value).unwrap(), 42)
                .unwrap()
                .apply(row);
        };
        apply(&value, &mut row);
        assert_eq!(row.session_id, "ses_native");
        assert_eq!(row.state, "active");
        assert_eq!(row.model.as_deref(), Some("provider/model"));
        assert_eq!(row.cost_usd, Some(1.25));
        assert_eq!(row.tokens_in, Some(22));
        assert_eq!(row.tokens_out, Some(4));
        assert_eq!(row.context_tokens, Some(28));
        assert_eq!(
            row.last_activity.unwrap().timestamp_millis(),
            1789600001000_i64
        );
        value["waiting"] = "question".into();
        apply(&value, &mut row);
        assert_eq!(row.state, "blocked");
        value["waiting"] = "permission".into();
        apply(&value, &mut row);
        assert_eq!(row.state, "blocked");
        value["waiting"] = Value::Null;
        value["status"]["type"] = "idle".into();
        value["session"]["id"] = "ses_switched".into();
        value["session"]["cost"] = 0.into();
        apply(&value, &mut row);
        assert_eq!(row.session_id, "ses_switched");
        assert_eq!(row.state, "idle");
        assert_eq!(row.cost_usd, Some(0.0), "native zero is a reported cost");
        value["session"] = Value::Null;
        value["context"] = Value::Null;
        apply(&value, &mut row);
        assert_eq!(row.session_id, "opencode-42");
        assert_eq!(row.state, "-");
        assert!(row.model.is_none() && row.cost_usd.is_none() && row.last_activity.is_none());
        assert!(Report::parse(&serde_json::to_vec(&value).unwrap(), 43).is_none());
        assert!(Report::parse(br#"{"version":1,"pid":42}"#, 42).is_none());
    }

    #[test]
    fn viewer_report_config_preserves_native_settings_and_relative_plugins() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("custom.jsonc");
        let text = r#"{
            // Keep the user's settings and native variable substitutions.
            "theme": "custom",
            "value": "https://example.invalid/a/*b*/{env:NAME}",
            "tui": {"plugin": ["./existing.mjs",],},
            /* a block comment */ "keybinds": {"input_left": "left",},
        }"#;
        fs::write(&original, text).unwrap();
        let mut command = Command::new("opencode");
        command
            .env(ENABLE, "1")
            .env("OPENCODE_TUI_CONFIG", &original);
        let reporter = Reporter::prepare(&mut command).unwrap().unwrap();
        let config_path = reporter._config.path().to_owned();
        assert_eq!(config_path.parent(), original.parent());
        let settings: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        assert_eq!(settings["theme"], "custom");
        assert_eq!(settings["plugin"][0], "./existing.mjs");
        assert_eq!(settings["plugin"].as_array().unwrap().len(), 2);
        assert_eq!(settings["keybinds"]["input_left"], "left");
        assert_eq!(
            settings["value"],
            "https://example.invalid/a/*b*/{env:NAME}"
        );
        assert_eq!(fs::read_to_string(&original).unwrap(), text);
        drop(reporter);
        assert!(
            !config_path.exists(),
            "temporary native config is removed with the viewer"
        );
        assert!(
            Reporter::prepare(&mut Command::new("opencode"))
                .unwrap()
                .is_none()
        );
        assert!(jsonc("{ /* unfinished").is_err());
    }

    #[test]
    fn an_expired_or_removed_report_cannot_supply_live_state() {
        let mut command = Command::new("opencode");
        command.env(ENABLE, "1").env_remove("OPENCODE_TUI_CONFIG");
        let reporter = Reporter::prepare(&mut command).unwrap().unwrap();
        fs::write(
            &reporter.report,
            br#"{"version":1,"pid":42,"session":null}"#,
        )
        .unwrap();
        assert!(reporter.read(42).is_some());
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&reporter.report)
            .unwrap();
        file.set_times(
            fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60)),
        )
        .unwrap();
        assert!(reporter.read(42).is_none());
        fs::write(
            &reporter.report,
            br#"{"version":1,"pid":42,"session":null}"#,
        )
        .unwrap();
        assert!(reporter.read(42).is_some());
        fs::remove_file(&reporter.report).unwrap();
        assert!(reporter.read(42).is_none());
    }
}
