use crate::{
    config::{Backend, Config},
    harness::Harness,
    ledger::Run,
};
use anyhow::{Result, bail, ensure};
use semver::{Version, VersionReq};
use serde::Deserialize;
use std::{
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

pub trait ControlPlane {
    fn spawn(&self, executable: &Path, run_id: &str) -> Result<Child>;
    fn attach(&self, run: &Run, harness: &dyn Harness) -> Result<Command>;
    fn status(&self) -> Result<String>;
    fn annotate(&self, run_id: &str, name: &str) -> Result<()>;
}

pub struct Headless;
impl ControlPlane for Headless {
    fn spawn(&self, executable: &Path, run_id: &str) -> Result<Child> {
        Ok(Command::new(executable)
            .args(["__worker", "--run-id", run_id])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?)
    }
    fn attach(&self, run: &Run, harness: &dyn Harness) -> Result<Command> {
        ensure!(
            run.terminal.is_some(),
            "run is still active; headless runs can be resumed after they finish"
        );
        harness.resume(
            run.started
                .session_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("run has no session ID"))?,
            run.started
                .cwd
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("run has no working directory"))?,
        )
    }
    fn status(&self) -> Result<String> {
        Ok("headless; native resume after completion".into())
    }
    fn annotate(&self, _run_id: &str, _name: &str) -> Result<()> {
        Ok(())
    }
}

pub struct AgentConsole {
    pub url: String,
}
pub const HEALTH_VERSION_RANGE: &str = ">=0.3.0, <0.4.0";

pub fn compatible_health_version(version: &str) -> bool {
    Version::parse(version).is_ok_and(|v| {
        VersionReq::parse(HEALTH_VERSION_RANGE)
            .expect("constant version range")
            .matches(&v)
    })
}
#[derive(Deserialize)]
struct Health {
    ok: bool,
    version: String,
}
impl AgentConsole {
    pub fn new(url: &str) -> Result<Self> {
        // A local execution backend must stay local. No redirects, credentials or arbitrary paths.
        let authority = url
            .strip_prefix("http://")
            .ok_or_else(|| anyhow::anyhow!("agent-console URL must be local http://"))?;
        let authority = authority.strip_suffix('/').unwrap_or(authority);
        let (host, port) = authority
            .rsplit_once(':')
            .ok_or_else(|| anyhow::anyhow!("agent-console URL requires an explicit port"))?;
        ensure!(
            matches!(host, "127.0.0.1" | "localhost" | "[::1]")
                && port.parse::<u16>().is_ok_and(|p| p > 0),
            "agent-console URL must use a loopback host and port"
        );
        Ok(Self {
            url: format!("http://{authority}"),
        })
    }
}
impl ControlPlane for AgentConsole {
    fn spawn(&self, _executable: &Path, _run_id: &str) -> Result<Child> {
        let status = self.status()?;
        bail!(
            "{status}; policy-aware spawn is unavailable (API cannot carry argv, session ID, budget or headless mode)"
        )
    }
    fn attach(&self, _run: &Run, _harness: &dyn Harness) -> Result<Command> {
        bail!("agent-console live attach is unavailable until policy-aware spawn is supported")
    }
    fn status(&self) -> Result<String> {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(2)))
            .max_redirects(0)
            .build()
            .new_agent();
        let health: Health = agent
            .get(format!("{}/api/health", self.url))
            .call()
            .map_err(|e| anyhow::anyhow!("agent-console unreachable at {}: {e}", self.url))?
            .body_mut()
            .with_config()
            .limit(16384)
            .read_json()?;
        ensure!(health.ok, "agent-console health is not OK");
        ensure!(
            compatible_health_version(&health.version),
            "agent-console {} is outside health API compatibility range {}; using headless fallback",
            health.version,
            HEALTH_VERSION_RANGE
        );
        Ok(format!("agent-console {}", health.version))
    }
    fn annotate(&self, _run_id: &str, _name: &str) -> Result<()> {
        bail!("no policy-governed agent-console session exists to annotate")
    }
}
pub fn backend(config: &Config) -> Result<Box<dyn ControlPlane>> {
    match config.control_plane {
        Backend::None => Ok(Box::new(Headless)),
        Backend::AgentConsole => Ok(Box::new(AgentConsole::new(&config.agent_console_url)?)),
    }
}
