//! Records the source commit so exported data can name the build that made it.
use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn git(arguments: &[&str]) -> Option<String> {
    let output = Command::new("git").args(arguments).output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (output.status.success() && !text.is_empty()).then(|| text.to_owned())
}

fn main() {
    // Dependency sources and staging can change without moving HEAD. Never
    // attribute an uncommitted build to that earlier source revision.
    for path in [
        "..",
        "../../Cargo.toml",
        "../../Cargo.lock",
        "../../locks",
        "../../proto",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    // Rebuild when the checkout moves to another commit, not on every build.
    // The branch directory is watched as a whole: a branch listed only among
    // the packed references gets a file of its own with its next commit.
    let mut watched: Vec<PathBuf> = ["HEAD", "refs/heads", "packed-refs", "reftable"]
        .iter()
        .filter_map(|name| git(&["rev-parse", "--git-path", name]))
        .map(PathBuf::from)
        .collect();
    // A linked worktree keeps the shared branches in the common directory.
    if let Some(common) = git(&["rev-parse", "--git-common-dir"]) {
        watched.push(Path::new(&common).join("reftable"));
    }
    for path in watched.iter().filter(|path| path.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    let clean = Command::new("git")
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=normal",
        ])
        .output()
        .is_ok_and(|output| output.status.success() && output.stdout.is_empty());
    let commit = clean
        .then(|| git(&["rev-parse", "--verify", "HEAD"]))
        .flatten()
        .unwrap_or_default();
    println!("cargo:rustc-env=KANADE_COMMIT={commit}");
}
