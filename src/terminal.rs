//! The user's interactive shell, run inside the dashboard's PTY.
use std::{
    ffi::{CStr, OsStr},
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

pub fn default_shell() -> PathBuf {
    choose_shell(std::env::var_os("SHELL").as_deref(), account_shell)
}

fn choose_shell(environment: Option<&OsStr>, account: impl FnOnce() -> Option<PathBuf>) -> PathBuf {
    let executable = |path: &Path| {
        path.is_absolute()
            && path
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    environment
        .map(PathBuf::from)
        .filter(|p| executable(p))
        .or_else(|| account().filter(|p| executable(p)))
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// Use the reentrant account lookup: fleet discovery also runs on worker threads.
fn account_shell() -> Option<PathBuf> {
    let mut buffer = vec![0u8; 4096];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let status = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer.len() < 1024 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        let shell = unsafe { (*result).pw_shell };
        if shell.is_null() {
            return None;
        }
        let bytes = unsafe { CStr::from_ptr(shell) }.to_bytes();
        return Some(PathBuf::from(OsStr::from_bytes(bytes)));
    }
}

pub fn command(shell: &Path, dir: &Path) -> Command {
    let mut command = Command::new(shell);
    command
        .arg("-i")
        .current_dir(dir)
        .env("SHELL", shell)
        .env("TERM", "xterm-256color");
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_shell_wins_without_an_account_lookup() {
        assert_eq!(
            choose_shell(Some(OsStr::new("/bin/sh")), || panic!("unneeded lookup")),
            PathBuf::from("/bin/sh")
        );
    }

    #[test]
    fn missing_or_unusable_shells_fall_back_to_the_account_then_sh() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("not-executable");
        std::fs::write(&file, "no executable bit").unwrap();
        let account = d.path().join("account shell");
        std::fs::write(&account, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&account, std::fs::Permissions::from_mode(0o755)).unwrap();
        for candidate in [
            None,
            Some(OsStr::new("")),
            Some(OsStr::new("relative-shell")),
            Some(OsStr::new("/missing/shell")),
            Some(d.path().as_os_str()),
            Some(file.as_os_str()),
        ] {
            assert_eq!(choose_shell(candidate, || Some(account.clone())), account);
            assert_eq!(choose_shell(candidate, || None), PathBuf::from("/bin/sh"));
        }
        assert_eq!(
            choose_shell(None, || Some("/missing/account-shell".into())),
            PathBuf::from("/bin/sh")
        );
    }
}
