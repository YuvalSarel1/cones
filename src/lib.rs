pub mod agents;
mod attention;
pub mod codex;
pub mod config;
pub mod context;
pub mod coordinator;
pub mod copy;
pub mod cost;
pub mod fleet;
pub mod forks;
pub mod harness;
pub mod history;
pub mod launch;
pub mod launchd;
pub mod ledger;
pub mod mcp;
pub mod observe;
pub mod opencode;
pub mod output;
pub mod pi;
#[cfg(target_os = "macos")]
pub(crate) mod process_info;
pub mod runner;
pub mod search;
pub mod show;
pub mod sqlite;
pub mod stop;
pub mod terminal;
pub mod terminal_host;
pub mod transcript;
pub mod tui;
mod viewer;

use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        bail!("refusing symlink directory: {}", path.display());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub fn private_file(path: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(f)
}

pub fn expand_path(path: &Path, base: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" {
        return dirs::home_dir().context("cannot locate home directory");
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return Ok(dirs::home_dir()
            .context("cannot locate home directory")?
            .join(rest));
    }
    if text.starts_with('~') {
        bail!("~user paths are unsupported; use an absolute path");
    }
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
}
