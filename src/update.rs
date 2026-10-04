//! The newest cones the Homebrew tap offers. The dashboard keeps the tap's formula in the
//! state dir and downloads it again in the background when it is a day old, so drawing
//! never waits on the network. `CONES_NO_UPDATE_CHECK=1` turns the check off.
use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    process::{Command, Stdio},
    sync::Mutex,
    time::{Duration, SystemTime},
};

const FORMULA_URL: &str =
    "https://raw.githubusercontent.com/YuvalSarel1/cones/main/Formula/cones.rb";
const REFRESH_AFTER: Duration = Duration::from_secs(24 * 3600);

static NEWER: Mutex<Option<String>> = Mutex::new(None);

pub fn init(state: &Path) {
    if std::env::var_os("CONES_NO_UPDATE_CHECK").is_some_and(|v| v != "0") {
        return;
    }
    let path = state.join("formula.rb");
    remember(&path);
    let age = fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok());
    if age.is_some_and(|a| a < REFRESH_AFTER) {
        return;
    }
    std::thread::spawn(move || {
        let fetched = Command::new("/usr/bin/curl")
            .args([
                "--fail",
                "--silent",
                "--proto",
                "=https",
                "--connect-timeout",
                "5",
                "--max-time",
                "20",
                "--max-filesize",
                "65536",
                FORMULA_URL,
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        // An offline check leaves the old formula, so it runs again next start.
        if let Some(out) = fetched
            .ok()
            .filter(|o| o.status.success() && version(&o.stdout).is_some())
        {
            let dir = path.parent().unwrap_or(Path::new("."));
            let saved = crate::private_dir(dir).is_ok()
                && fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&path)
                    .and_then(|mut f| f.write_all(&out.stdout))
                    .is_ok();
            if saved {
                remember(&path);
            }
        }
    });
}

/// `cones 0.2.3 available`, with Homebrew's command when Homebrew installed this binary.
pub fn notice() -> Option<String> {
    let v = NEWER.lock().unwrap_or_else(|e| e.into_inner()).clone()?;
    let brew = std::env::current_exe()
        .and_then(|e| e.canonicalize())
        .is_ok_and(|e| e.to_string_lossy().contains("/Cellar/cones/"));
    Some(if brew {
        format!("cones {v} available · brew upgrade cones")
    } else {
        format!("cones {v} available")
    })
}

fn remember(path: &Path) {
    let latest = fs::read(path).ok().and_then(|f| version(&f));
    let newer = latest.filter(|v| parts(v) > parts(env!("CARGO_PKG_VERSION")));
    *NEWER.lock().unwrap_or_else(|e| e.into_inner()) = newer;
}

/// The release the formula's `url` downloads: `.../tags/v0.2.2.tar.gz` is `0.2.2`.
fn version(formula: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(formula).ok()?;
    let url = text.lines().find_map(|l| l.trim().strip_prefix("url "))?;
    let tag = url.trim_matches('"').rsplit("/v").next()?;
    let v = tag.strip_suffix(".tar.gz")?;
    (parts(v).len() == 3).then(|| v.to_owned())
}

fn parts(v: &str) -> Vec<u64> {
    v.split('.').map_while(|p| p.parse().ok()).collect()
}
