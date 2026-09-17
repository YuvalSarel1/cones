//! The user's interactive shell, run inside the dashboard's PTY.
use std::{
    ffi::{CStr, OsStr},
    io,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

// Restore the user's startup directory before loading their configuration. Install the
// widgets at the first prompt, after .zshrc has chosen its keymaps and completion widgets.
const ZSHENV: &str = r#"
if (( ${+CONES_ZDOTDIR} )); then
    export ZDOTDIR=$CONES_ZDOTDIR
else
    unset ZDOTDIR
fi
unset CONES_ZDOTDIR
if [[ -r ${ZDOTDIR-$HOME}/.zshenv ]]; then
    source "${ZDOTDIR-$HOME}/.zshenv"
fi

_cones_return_or_edit() {
    if [[ $CONTEXT == start && -z $BUFFER && -z $PREBUFFER ]]; then
        builtin print -rn -- $'\e]777;cones;return\a'
    else
        zle "_cones_original_${WIDGET#_cones_return_}" -- "$@"
    fi
}

_cones_install_bindings() {
    emulate -L zsh
    add-zsh-hook -d precmd _cones_install_bindings
    local keymap key widget target
    local -a binding
    local -i serial=0
    for keymap in emacs viins vicmd; do
        for key in $'\e[D' $'\eOD' $'\t'; do
            binding=(${(z)"$(bindkey -M "$keymap" "$key")"})
            widget=${(Q)binding[2]}
            # A key bound to a string macro keeps that macro.
            (( ${+widgets[$widget]} )) || continue
            (( ++serial ))
            target=${keymap}_${serial}
            zle -A "$widget" "_cones_original_$target"
            zle -N "_cones_return_$target" _cones_return_or_edit
            bindkey -M "$keymap" "$key" "_cones_return_$target"
        done
    done
    unfunction _cones_install_bindings
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd _cones_install_bindings
"#;

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

pub fn command(
    shell: &Path,
    dir: &Path,
    startup: &mut Option<tempfile::TempDir>,
) -> io::Result<Command> {
    let mut command = Command::new(shell);
    command
        .arg("-i")
        .current_dir(dir)
        .env("SHELL", shell)
        .env("TERM", "xterm-256color");
    if shell.file_name() == Some(OsStr::new("zsh")) {
        if startup.is_none() {
            let files = tempfile::Builder::new().prefix("cones-shell-").tempdir()?;
            std::fs::write(files.path().join(".zshenv"), ZSHENV)?;
            *startup = Some(files);
        }
        command
            .env("ZDOTDIR", startup.as_ref().unwrap().path())
            .env_remove("CONES_ZDOTDIR");
        if let Some(original) = std::env::var_os("ZDOTDIR") {
            command.env("CONES_ZDOTDIR", original);
        }
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewer::{Colors, Viewer};
    use std::time::{Duration, Instant};

    fn until(viewer: &mut Viewer, ready: impl Fn(&Viewer) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            viewer.pump().unwrap();
            if ready(viewer) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "shell did not become ready: {}",
                viewer.screen().contents()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn expect_return(viewer: &mut Viewer, key: &[u8]) {
        viewer.write(key);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            viewer.pump().unwrap();
            if viewer.take_return_to_list() {
                assert!(!viewer.take_return_to_list(), "requests are consumed once");
                return;
            }
            assert!(Instant::now() < deadline, "empty prompt kept {key:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn zsh_returns_from_empty_prompts_and_keeps_editing_completion_and_continuations() {
        let zsh = Path::new("/bin/zsh");
        if !zsh.is_file() {
            return;
        }
        for keymap in ["emacs", "viins"] {
            let d = tempfile::tempdir().unwrap();
            std::fs::write(
                d.path().join(".zshrc"),
                format!(
                    "bindkey -A {keymap} main\nprecmd() {{ (( ++prompt_count )); PS1=\"READY-$prompt_count: \"; }}\n"
                ),
            )
            .unwrap();
            std::fs::write(d.path().join("completion_target"), "").unwrap();
            let mut startup = None;
            let mut command = command(zsh, d.path(), &mut startup).unwrap();
            command.env("HOME", d.path()).env_remove("CONES_ZDOTDIR");
            let mut viewer =
                Viewer::spawn_terminal(command, 24, 100, None, Colors::default()).unwrap();
            until(&mut viewer, |v| v.screen().contents().contains("READY-1:"));
            for key in [b"\t".as_slice(), b"\x1b[D", b"\x1bOD"] {
                expect_return(&mut viewer, key);
            }

            viewer.write(b"printf '\\nEDIT:%s\\n' ac\x1b[Db\r");
            until(&mut viewer, |v| v.screen().contents().contains("EDIT:abc"));
            assert!(
                !viewer.take_return_to_list(),
                "Left must edit a typed command"
            );

            viewer.write(b"printf '\\nVALUE:%s\\n' comp\t\r");
            until(&mut viewer, |v| {
                v.screen().contents().contains("VALUE:completion_target")
            });
            assert!(
                !viewer.take_return_to_list(),
                "Tab must complete a typed command"
            );

            viewer.write(b"printf '\\nMULTI:%s\\n' '\r");
            until(&mut viewer, |v| v.screen().contents().contains("quote>"));
            viewer.write(b"\x1b[D\tcontinued'\r");
            until(&mut viewer, |v| v.screen().contents().contains("READY-4:"));
            assert!(
                !viewer.take_return_to_list(),
                "an empty continuation line belongs to the unfinished command"
            );

            viewer.write(b"sh -c 'printf \"\\nFOREGROUND-%s\\n\" READY; exec cat'\r");
            until(&mut viewer, |v| {
                v.screen()
                    .contents()
                    .lines()
                    .any(|line| line == "FOREGROUND-READY")
            });
            viewer.write(b"\x1b[D\tFOREGROUND\r");
            until(&mut viewer, |v| {
                v.screen().contents().contains("FOREGROUND")
            });
            assert!(
                !viewer.take_return_to_list(),
                "foreground programs own their keys"
            );
            viewer.write(b"\x03");
            until(&mut viewer, |v| v.screen().contents().contains("READY-5:"));
            expect_return(&mut viewer, b"\t");
        }
    }

    #[test]
    fn zsh_preserves_startup_directories_and_custom_widgets() {
        let zsh = Path::new("/bin/zsh");
        if !zsh.is_file() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let original = d.path().join("original");
        let redirected = d.path().join("redirected");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&redirected).unwrap();
        std::fs::write(
            original.join(".zshenv"),
            "export ZDOTDIR=\"$HOME/redirected\"\nexport ORIGINAL_ENV=loaded\n",
        )
        .unwrap();
        std::fs::write(
            redirected.join(".zshrc"),
            "bindkey -e\nPS1='CUSTOM: '\ncustom_tab() { BUFFER+='CUSTOM'; CURSOR=${#BUFFER}; }\nzle -N custom_tab\nbindkey '^I' custom_tab\n",
        )
        .unwrap();
        let mut startup = None;
        let mut command = command(zsh, d.path(), &mut startup).unwrap();
        command
            .env("HOME", d.path())
            .env("CONES_ZDOTDIR", &original);
        let mut viewer = Viewer::spawn_terminal(command, 24, 100, None, Colors::default()).unwrap();
        until(&mut viewer, |v| v.screen().contents().contains("CUSTOM:"));
        expect_return(&mut viewer, b"\t");
        viewer.write(b"printf '\\nWIDGET:%s\\n' \t\r");
        until(&mut viewer, |v| {
            v.screen().contents().contains("WIDGET:CUSTOM")
        });
        assert!(!viewer.take_return_to_list());
        viewer.write(b"printf '\\nENV:%s DIR:%s\\n' \"$ORIGINAL_ENV\" \"$ZDOTDIR\"\r");
        let expected = format!("ENV:loaded DIR:{}", redirected.display());
        until(&mut viewer, |v| v.screen().contents().contains(&expected));
    }

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
