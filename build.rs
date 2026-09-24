//! Record the commit a release binary was built from, so a debug log names it. Only release
//! builds (`cargo install`) ask Git: a debug build would relink every test binary on each commit.
//! A source without its own Git history, such as a release tarball, leaves the commit unset.
use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }
    // A tarball unpacked inside some other repository must not report that repository's commit.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let ours = git(&["rev-parse", "--show-toplevel"]).is_some_and(|top| {
        Path::new(&top).canonicalize().ok() == Path::new(&manifest).canonicalize().ok()
    });
    if !ours {
        return;
    }
    // Rebuild when HEAD moves: a checkout changes HEAD, a commit on a branch changes its ref.
    let mut watched = vec!["HEAD".to_owned(), "packed-refs".to_owned()];
    watched.extend(git(&["symbolic-ref", "-q", "HEAD"]));
    for path in watched {
        if let Some(file) = git(&["rev-parse", "--path-format=absolute", "--git-path", &path]) {
            println!("cargo:rerun-if-changed={file}");
        }
    }
    if let Some(commit) = git(&["rev-parse", "--short=12", "HEAD"]) {
        let dirty =
            git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
        let mark = if dirty { "-dirty" } else { "" };
        println!("cargo:rustc-env=CONES_COMMIT={commit}{mark}");
    }
}
