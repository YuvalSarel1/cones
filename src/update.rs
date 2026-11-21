//! The newest cones the Homebrew tap offers. The dashboard keeps the tap's formula in the
//! state dir and, while it runs, downloads it again in the background once it is a day old,
//! so drawing never waits on the network. A Homebrew install then upgrades itself with
//! `brew upgrade cones`, keeping the running version's files, and the next start uses the
//! new one. A newer version Homebrew installed on its own, such as through `brew autoupdate`,
//! is noticed from its Cellar folder within a minute. `start.update_check: false` turns all
//! of it off.
use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Mutex, Once, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

const FORMULA_URL: &str =
    "https://raw.githubusercontent.com/YuvalSarel1/cones/main/Formula/cones.rb";
const REFRESH_AFTER: Duration = Duration::from_secs(24 * 3600);
const PASS_EVERY: Duration = Duration::from_secs(3600);
const CELLAR_EVERY: Duration = Duration::from_secs(60);

static NEWER: Mutex<Option<String>> = Mutex::new(None);
static INSTALLED: Mutex<Option<String>> = Mutex::new(None);
static ENABLED: AtomicBool = AtomicBool::new(false);
static WATCH: Once = Once::new();
static PREFIX: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Called at startup and again when the config editor saves `start.update_check`.
pub fn init(state: &Path, enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
    if !enabled {
        *NEWER.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return;
    }
    homebrew();
    let path = state.join("formula.rb");
    remember(&path);
    WATCH.call_once(|| {
        std::thread::spawn(move || {
            let mut since_pass = PASS_EVERY;
            loop {
                if ENABLED.load(Ordering::Relaxed) {
                    if since_pass >= PASS_EVERY {
                        pass(&path);
                        since_pass = Duration::ZERO;
                    } else {
                        see_cellar();
                    }
                }
                std::thread::sleep(CELLAR_EVERY);
                since_pass += CELLAR_EVERY;
            }
        });
    });
}

/// Refreshes a day-old formula, then installs a newer release Homebrew has not installed yet.
/// A failed fetch or upgrade changes nothing, so the next pass tries again.
fn pass(path: &Path) {
    let age = fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok());
    if age.is_none_or(|a| a >= REFRESH_AFTER) {
        fetch(path);
    }
    see_cellar();
    let newer = NEWER.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let installed = INSTALLED.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let (Some(v), Some(prefix)) = (newer, homebrew())
        && installed.as_ref() != Some(&v)
        && upgrade(&prefix, &v)
    {
        *INSTALLED.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
    }
}

/// `brew upgrade cones` exits 0 when the tap is behind and nothing changed, so success is
/// the new version's Cellar folder. Homebrew refreshes its taps before an upgrade at most once
/// a day, or never under `HOMEBREW_NO_AUTO_UPDATE`, so its copy of the tap can lag the formula
/// read from GitHub for a day; `brew update` first brings it level. Cleanup is off: the running
/// dashboard and the terminal hosts it starts still execute the old version's binary.
fn upgrade(prefix: &Path, v: &str) -> bool {
    let brew = |args: &[&str]| {
        Command::new(prefix.join("bin/brew"))
            .args(args)
            .env("HOMEBREW_NO_INSTALL_CLEANUP", "1")
            .env("HOMEBREW_NO_ENV_HINTS", "1")
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    // A failed update can still leave a tap that has the release, so the upgrade runs anyway.
    brew(&["update", "--quiet"]);
    brew(&["upgrade", "cones"]) && prefix.join("Cellar/cones").join(v).is_dir()
}

/// A newer cones Homebrew installed without this dashboard, which `brew autoupdate` does
/// hourly, is a release already installed: the notice asks to reopen, and nothing upgrades.
fn see_cellar() {
    let Some(prefix) = homebrew() else { return };
    let newest = fs::read_dir(prefix.join("Cellar/cones"))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|v| parts(v).len() == 3)
        .max_by_key(|v| parts(v));
    let Some(v) = newest.filter(|v| parts(v) > parts(env!("CARGO_PKG_VERSION"))) else {
        return;
    };
    let mut newer = NEWER.lock().unwrap_or_else(|e| e.into_inner());
    if newer.as_ref().is_none_or(|n| parts(n) <= parts(&v)) {
        *newer = Some(v.clone());
        *INSTALLED.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
    }
}

/// The Homebrew prefix when Homebrew installed this binary, as `<prefix>/Cellar/cones/<v>/bin`.
/// Read once: an upgrade's cleanup can delete the running version's folder.
fn homebrew() -> Option<PathBuf> {
    PREFIX.get_or_init(prefix).clone()
}

fn prefix() -> Option<PathBuf> {
    let exe = std::env::current_exe()
        .and_then(|e| e.canonicalize())
        .ok()?;
    let exe = exe.to_string_lossy();
    exe.split_once("/Cellar/cones/")
        .map(|(p, _)| PathBuf::from(p))
}

fn fetch(path: &Path) {
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
    // An offline check leaves the old formula, so the next pass tries again.
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
                .open(path)
                .and_then(|mut f| f.write_all(&out.stdout))
                .is_ok();
        if saved {
            remember(path);
        }
    }
}

/// `cones 0.2.3 installed · reopen cones` once Homebrew has it, otherwise `cones 0.2.3
/// available`, with Homebrew's command when Homebrew installed this binary.
pub fn notice() -> Option<String> {
    let v = NEWER.lock().unwrap_or_else(|e| e.into_inner()).clone()?;
    if INSTALLED.lock().unwrap_or_else(|e| e.into_inner()).as_ref() == Some(&v) {
        return Some(format!("cones {v} installed · reopen cones"));
    }
    Some(if homebrew().is_some() {
        format!("cones {v} available · brew upgrade cones")
    } else {
        format!("cones {v} available")
    })
}

/// The newer release Homebrew has installed, which this dashboard runs only once reopened.
pub fn installed() -> Option<String> {
    let v = NEWER.lock().unwrap_or_else(|e| e.into_inner()).clone()?;
    (INSTALLED.lock().unwrap_or_else(|e| e.into_inner()).as_ref() == Some(&v)).then_some(v)
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
