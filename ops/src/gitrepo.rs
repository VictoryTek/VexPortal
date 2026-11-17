//! Keeping `/etc/nixos` a local git repository for the rebuild.
//!
//! A `git+file:///etc/nixos` flake reference only sees files git tracks, which is what
//! keeps `secrets/` out of the world-readable Nix store, and it leaves out untracked
//! side files, which would drop every enabled service from the build. `vexos-update`
//! maintains the repository; this does the same, step for step, so the two stay
//! interchangeable. The repository has no remote and nothing here ever pushes.

use crate::fsutil;
use crate::runner::{Cmd, RunError, Runner, Sink};
use std::path::Path;

/// What `.gitignore` holds in a repository this creates. Same lines as `vexos-update`.
pub const GITIGNORE: &str = "secrets/\n*.bak\nvexos-variant\n";

/// The host-local files that are force-added so git tracks them even when they were
/// created after the first `git init`. The same list `vexos-update` carries.
pub const TRACKED_FILES: [&str; 15] = [
    "hardware-configuration.nix",
    "kernel-install-override.nix",
    "stateless-user-override.nix",
    "server-services.nix",
    "features.nix",
    "bootloader.nix",
    "hardware-local.nix",
    "host.nix",
    "hostname.nix",
    "vm-platform.nix",
    "user-override.nix",
    "local.nix",
    "storage-pool.nix",
    "storage-remote.nix",
    "zfs-pools.nix",
];

/// `flake.nix` is tracked too, but is added with the rest only when it exists. Kept
/// separate so the list above stays the one the installer's recipe documents.
const WRAPPER: &str = "flake.nix";

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git {step} failed: {detail}")]
    Failed { step: &'static str, detail: String },
    #[error(transparent)]
    Run(#[from] RunError),
    #[error("could not write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// What [`prepare`] had to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Prepared {
    pub initialised: bool,
    pub committed: bool,
}

fn git(dir: &Path) -> Cmd {
    // `-C` rather than a working directory, and the commit identity set per command so
    // nothing is written to anyone's git configuration. Like `vexos-update`, git still
    // reads root's own config, so a `commit.gpgsign` there would make the commit fail.
    Cmd::new("git").arg("-C").arg(dir.display().to_string())
}

fn commit(dir: &Path, message: &str) -> Cmd {
    git(dir).args([
        "-c",
        "user.email=vexos@localhost",
        "-c",
        "user.name=VexOS",
        "commit",
        "-q",
        "-m",
        message,
    ])
}

async fn check<R: Runner>(
    runner: &R,
    cmd: Cmd,
    step: &'static str,
) -> Result<crate::runner::Output, GitError> {
    let out = runner.run(&cmd).await?;
    if out.success() {
        Ok(out)
    } else {
        Err(GitError::Failed {
            step,
            detail: out.stderr.trim().to_string(),
        })
    }
}

/// Make `dir` ready for a `git+file://` build: a repository exists, every host-local
/// file is tracked, and what is tracked is committed.
pub async fn prepare<R: Runner>(
    runner: &R,
    dir: &Path,
    sink: &dyn Sink,
) -> Result<Prepared, GitError> {
    let mut done = Prepared::default();

    let probe = runner
        .run(&git(dir).args(["rev-parse", "--git-dir"]))
        .await?;
    if !probe.success() {
        sink.info("Initialising /etc/nixos as a git repository (one-time setup)…");
        let ignore = dir.join(".gitignore");
        fsutil::write_atomic(&ignore, GITIGNORE.as_bytes(), 0o644).map_err(|source| {
            GitError::Write {
                path: ignore.display().to_string(),
                source,
            }
        })?;
        check(runner, git(dir).args(["init", "-q"]), "init").await?;
        check(runner, git(dir).args(["add", "."]), "add").await?;
        check(
            runner,
            commit(dir, "chore: track /etc/nixos configuration"),
            "commit",
        )
        .await?;
        done.initialised = true;
        done.committed = true;
    }

    for name in TRACKED_FILES.iter().copied().chain([WRAPPER]) {
        if !dir.join(name).is_file() {
            continue;
        }
        // A file git refuses to add is missing from the build. `vexos-update` ignores
        // that; here it is reported, and the rest are still added.
        let added = runner
            .run(&git(dir).args(["add", "-f", "--", name]))
            .await?;
        if !added.success() {
            sink.error(&format!(
                "warning: could not track {name}: {}",
                added.stderr.trim()
            ));
        }
    }

    // Exit 1 means something is staged; 0 means nothing is.
    let staged = runner
        .run(&git(dir).args(["diff", "--cached", "--quiet"]))
        .await?;
    match staged.code {
        0 => {}
        1 => {
            check(
                runner,
                commit(dir, "chore: track /etc/nixos config files"),
                "commit",
            )
            .await?;
            done.committed = true;
        }
        _ => {
            return Err(GitError::Failed {
                step: "diff",
                detail: staged.stderr.trim().to_string(),
            })
        }
    }

    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testing::TempDir;
    use crate::runner::testing::{block_on, CollectSink, FakeRunner};

    #[test]
    fn a_new_directory_is_initialised_ignored_and_committed() {
        let dir = TempDir::new();
        dir.write("hardware-configuration.nix", "{ }");
        let d = dir.path().display().to_string();
        let runner = FakeRunner::new()
            .failing("git", &["-C", &d, "rev-parse"], 128, "not a git repository")
            .ok("git", &["-C", &d, "init"])
            .ok("git", &["-C", &d, "add"])
            .ok("git", &["-C", &d, "-c"])
            .on("git", &["-C", &d, "diff"], crate::runner::Output::default());

        let done = block_on(prepare(&runner, dir.path(), &CollectSink::new())).unwrap();

        assert_eq!(
            done,
            Prepared {
                initialised: true,
                committed: true
            }
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
            GITIGNORE
        );
        let lines = runner.lines();
        assert!(lines[1].ends_with("init -q"), "{lines:?}");
        assert!(lines
            .iter()
            .any(|l| l.contains("add -f -- hardware-configuration.nix")));
        // Identity comes from the command line, never from anyone's git config.
        assert!(lines
            .iter()
            .any(|l| l.contains("user.email=vexos@localhost") && l.contains("user.name=VexOS")));
    }

    #[test]
    fn an_existing_repository_is_not_reinitialised_and_only_present_files_are_added() {
        let dir = TempDir::new();
        dir.write("features.nix", "{ }");
        dir.write("flake.nix", "{ }");
        let d = dir.path().display().to_string();
        let runner = FakeRunner::new()
            .ok("git", &["-C", &d, "rev-parse"])
            .ok("git", &["-C", &d, "add"])
            .ok("git", &["-C", &d, "diff"]);

        let done = block_on(prepare(&runner, dir.path(), &CollectSink::new())).unwrap();

        assert_eq!(done, Prepared::default());
        let lines = runner.lines();
        assert!(!lines.iter().any(|l| l.contains(" init")));
        assert!(
            !dir.join(".gitignore").exists(),
            "an existing repo keeps its ignore file"
        );
        let adds: Vec<_> = lines.iter().filter(|l| l.contains("add -f")).collect();
        assert_eq!(adds.len(), 2, "{adds:?}");
        assert!(!lines.iter().any(|l| l.contains(" commit")));
    }

    #[test]
    fn staged_changes_are_committed() {
        let dir = TempDir::new();
        dir.write("server-services.nix", "{ }");
        let d = dir.path().display().to_string();
        let runner = FakeRunner::new()
            .ok("git", &["-C", &d, "rev-parse"])
            .ok("git", &["-C", &d, "add"])
            .failing("git", &["-C", &d, "diff"], 1, "")
            .ok("git", &["-C", &d, "-c"]);

        let done = block_on(prepare(&runner, dir.path(), &CollectSink::new())).unwrap();

        assert_eq!(
            done,
            Prepared {
                initialised: false,
                committed: true
            }
        );
    }

    #[test]
    fn a_file_git_refuses_is_reported_and_the_rest_continue() {
        let dir = TempDir::new();
        dir.write("features.nix", "{ }");
        dir.write("local.nix", "{ }");
        let d = dir.path().display().to_string();
        let runner = FakeRunner::new()
            .ok("git", &["-C", &d, "rev-parse"])
            .failing(
                "git",
                &["-C", &d, "add", "-f", "--", "features.nix"],
                1,
                "ignored",
            )
            .ok("git", &["-C", &d, "add"])
            .ok("git", &["-C", &d, "diff"]);
        let sink = CollectSink::new();

        block_on(prepare(&runner, dir.path(), &sink)).unwrap();

        assert!(sink.text().contains("could not track features.nix"));
        assert!(runner.lines().iter().any(|l| l.contains("local.nix")));
    }

    #[test]
    fn a_real_git_failure_stops_the_run() {
        let dir = TempDir::new();
        let d = dir.path().display().to_string();
        let runner = FakeRunner::new()
            .ok("git", &["-C", &d, "rev-parse"])
            .failing("git", &["-C", &d, "diff"], 128, "fatal: corrupt index");

        let err = block_on(prepare(&runner, dir.path(), &CollectSink::new())).unwrap_err();

        assert!(err.to_string().contains("corrupt index"), "{err}");
    }

    #[test]
    fn the_tracked_list_matches_vexos_update() {
        // Copied from the `for _f in …` line in vexos-nix `pkgs/vexos-update`, with
        // `flake.nix` (its last entry) carried separately as WRAPPER.
        let from_vexos_update = "hardware-configuration.nix kernel-install-override.nix             stateless-user-override.nix server-services.nix features.nix bootloader.nix             hardware-local.nix host.nix hostname.nix vm-platform.nix user-override.nix             local.nix storage-pool.nix storage-remote.nix zfs-pools.nix";
        assert_eq!(
            TRACKED_FILES.to_vec(),
            from_vexos_update.split_whitespace().collect::<Vec<_>>()
        );
        assert_eq!(WRAPPER, "flake.nix");
    }
}
