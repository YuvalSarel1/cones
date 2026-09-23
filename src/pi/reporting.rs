//! Private native Pi UI reports for clients started by cones.
use crate::fleet::Session;
use serde_json::Value;
use std::{
    fs,
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

pub(crate) const ENABLE: &str = "CONES_PI_VIEWER";

pub(crate) struct Reporter {
    _directory: tempfile::TempDir,
    report: PathBuf,
}

impl Reporter {
    // Called before the viewer installs stdio or pre-exec hooks. Pi launch,
    // resume and fork commands carry only arguments, cwd and environment here.
    pub(crate) fn prepare(command: &mut Command) -> io::Result<Option<Self>> {
        if !command
            .get_envs()
            .any(|(key, value)| key == ENABLE && value.is_some())
        {
            return Ok(None);
        }
        let directory = tempfile::Builder::new().prefix("cones-pi-").tempdir()?;
        let plugin = directory.path().join("report.mjs");
        fs::write(
            &plugin,
            include_str!("../../assets/harnesses/pi-report.mjs"),
        )?;
        let report = directory.path().join("session.json");
        let mut prepared = Command::new(command.get_program());
        prepared
            .arg("--extension")
            .arg(&plugin)
            .args(command.get_args());
        if let Some(cwd) = command.get_current_dir() {
            prepared.current_dir(cwd);
        }
        for (key, value) in command.get_envs() {
            if let Some(value) = value {
                prepared.env(key, value);
            } else {
                prepared.env_remove(key);
            }
        }
        prepared.env_remove(ENABLE).env("CONES_PI_REPORT", &report);
        *command = prepared;
        Ok(Some(Self {
            _directory: directory,
            report,
        }))
    }

    pub(crate) fn read(&self, pid: u32) -> Option<Report> {
        let file = fs::File::open(&self.report).ok()?;
        if file.metadata().ok()?.modified().ok()?.elapsed().ok()? > Duration::from_secs(5) {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(64 * 1024 + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() > 64 * 1024 {
            return None;
        }
        Report::parse(&bytes, pid)
    }
}

#[derive(Clone)]
pub(crate) struct Report(pub(crate) Value);

impl Report {
    pub(crate) fn parse(bytes: &[u8], pid: u32) -> Option<Self> {
        let value: Value = serde_json::from_slice(bytes).ok()?;
        if value["version"] != 1
            || value["pid"].as_u64() != Some(u64::from(pid))
            || uuid::Uuid::parse_str(value["session"]["id"].as_str()?).is_err()
            || !Path::new(value["session"]["directory"].as_str()?).is_absolute()
            || !value["waiting"].is_boolean()
            || !value["idle"].is_boolean()
        {
            return None;
        }
        if !value["session"]["file"].is_null()
            && !Path::new(value["session"]["file"].as_str()?).is_absolute()
        {
            return None;
        }
        Some(Self(value))
    }

    pub(crate) fn apply(&self, row: &mut Session) {
        let (Some(pid), Some(started)) = (row.pid, row.started) else {
            return;
        };
        if row.harness != "pi" || self.0["pid"].as_u64() != Some(u64::from(pid)) {
            return;
        }
        let id = self.0["session"]["id"]
            .as_str()
            .expect("validated native id");
        let cwd = PathBuf::from(
            self.0["session"]["directory"]
                .as_str()
                .expect("validated cwd"),
        );
        let process = super::Process {
            pid,
            started,
            cwd: Some(cwd.clone()),
        };
        let file = self.0["session"]["file"].as_str().and_then(|path| {
            let mut line = String::new();
            BufReader::new(fs::File::open(path).ok()?.take(64 * 1024))
                .read_line(&mut line)
                .ok()?;
            let meta = super::meta(&line)?;
            (meta.session_id == id && meta.cwd == cwd).then(|| (PathBuf::from(path), meta))
        });
        let mut native = super::row(&process, file);
        native.session_id = id.to_owned();
        native.state = if self.0["waiting"] == true {
            "blocked"
        } else if self.0["idle"] == true {
            "idle"
        } else {
            "active"
        }
        .into();
        native.usage = row.usage;
        native.coordinator = row.coordinator;
        if row.native() == id || uuid::Uuid::parse_str(row.native()).is_err() {
            native.forked_from = row.forked_from.clone();
            if native.title.is_none() && row.transcript_path.is_none() {
                native.title = row.title.clone();
            }
        }
        *row = native;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs::FileTimes;

    fn value(root: &Path) -> Value {
        json!({"version":1,"pid":42,"session":{"id":"aaaaaaaa-1111-4111-8111-111111111111","directory":root,"file":null},"waiting":true,"idle":true})
    }

    #[test]
    fn native_dialog_state_keeps_identity_and_clears_when_answered_or_switched() {
        let root = tempfile::tempdir().unwrap();
        let mut row = crate::tui::placeholder(
            crate::config::HarnessKind::Pi,
            "launch-pi",
            root.path(),
            "task",
        );
        row.pid = Some(42);
        let mut native = value(root.path());
        let apply = |value: &Value, row: &mut Session| {
            Report::parse(&serde_json::to_vec(value).unwrap(), 42)
                .unwrap()
                .apply(row)
        };
        apply(&native, &mut row);
        assert_eq!(row.state, "blocked");
        assert_eq!(row.session_id, "aaaaaaaa-1111-4111-8111-111111111111");
        assert_eq!(row.pid, Some(42));
        assert_eq!(row.cwd, root.path());
        native["waiting"] = false.into();
        native["idle"] = false.into();
        apply(&native, &mut row);
        assert_eq!(row.state, "active");
        native["idle"] = true.into();
        apply(&native, &mut row);
        assert_eq!(row.state, "idle");
        row.title = Some("old title".into());
        row.model = Some("old model".into());
        row.forked_from = Some("old parent".into());
        native["session"]["id"] = "bbbbbbbb-2222-4222-8222-222222222222".into();
        apply(&native, &mut row);
        assert_eq!(row.session_id, "bbbbbbbb-2222-4222-8222-222222222222");
        assert!(row.title.is_none() && row.model.is_none() && row.forked_from.is_none());
        assert!(Report::parse(&serde_json::to_vec(&native).unwrap(), 43).is_none());
        native["waiting"] = "maybe".into();
        assert!(Report::parse(&serde_json::to_vec(&native).unwrap(), 42).is_none());
    }

    #[test]
    fn private_extension_preserves_native_flags_and_config_and_withdraws_stale_reports() {
        let root = tempfile::tempdir().unwrap();
        let mut command = Command::new("pi");
        command
            .args([
                "--no-extensions",
                "--extension",
                "my-extension.ts",
                "--",
                "--a prompt",
            ])
            .current_dir(root.path())
            .env("PI_CODING_AGENT_DIR", root.path())
            .env(ENABLE, "1");
        let reporter = Reporter::prepare(&mut command).unwrap().unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "--extension");
        assert!(Path::new(&args[1]).is_file());
        assert_eq!(
            &args[2..],
            [
                "--no-extensions",
                "--extension",
                "my-extension.ts",
                "--",
                "--a prompt"
            ]
        );
        assert_eq!(command.get_current_dir(), Some(root.path()));
        assert!(
            command
                .get_envs()
                .any(|(k, v)| k == "PI_CODING_AGENT_DIR" && v == Some(root.path().as_os_str()))
        );
        assert!(command.get_envs().any(|(k, v)| k == ENABLE && v.is_none()));
        assert!(
            Reporter::prepare(&mut Command::new("pi"))
                .unwrap()
                .is_none()
        );
        fs::write(
            &reporter.report,
            serde_json::to_vec(&value(root.path())).unwrap(),
        )
        .unwrap();
        assert!(reporter.read(42).is_some());
        assert!(reporter.read(43).is_none());
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&reporter.report)
            .unwrap();
        file.set_times(
            FileTimes::new().set_modified(std::time::SystemTime::now() - Duration::from_secs(60)),
        )
        .unwrap();
        assert!(reporter.read(42).is_none());
        fs::write(&reporter.report, b"{").unwrap();
        assert!(reporter.read(42).is_none());
        let plugin = PathBuf::from(&args[1]);
        drop(reporter);
        assert!(!plugin.exists());
    }
}
