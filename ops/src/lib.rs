//! Native VexOS operations.
//!
//! This crate is what VexPortal does instead of running `just`. Each operation is a
//! typed function that edits the same files and calls the same tools a person would on
//! a Linux box without `just`, one `argv` at a time, never through a shell.
//!
//! It has no D-Bus and no GTK, so everything here is tested against a temporary
//! directory and a scripted [`runner::testing::FakeRunner`]. The daemon supplies the real
//! runner and the privilege; this crate decides what to do.
//!
//! Operations arrive as an id and a set of named, already-validated answers
//! ([`Ctx::args`]). The daemon has checked the id against the catalog and each answer
//! against its format before anything here runs; operations still treat
//! destructive confirmations and disk choices as their own job, because a caller on the
//! bus is not trusted to have asked the GUI first.

pub mod activate;
pub mod beszel;
pub mod bootloader;
pub mod features;
pub mod fsutil;
pub mod gitrepo;
pub mod nixedit;
pub mod rebuild;
pub mod runner;
pub mod services;
pub mod sidefile;
pub mod sops;
pub mod system;
pub mod tailscale;
pub mod upgrade;

use runner::{Cmd, RunError, Runner, Sink};
use std::collections::BTreeMap;
use std::path::Path;

/// Named answers, keyed by the parameter names in the catalog.
pub type Args = BTreeMap<String, String>;

/// A secret answer. `Debug` hides it, so a stray `{:?}` in an operation's log line or
/// error cannot leak it; reading it takes a deliberate call to [`Secret::expose`].
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The operations this crate implements. The daemon sends these here and every other
/// action id to the legacy recipe runner until that is removed. Each phase adds its
/// operations here and to [`run`].
pub const NATIVE_OPERATIONS: &[&str] = &[
    "variant",
    "reboot",
    "shutdown",
    "set-hostname",
    "rollback",
    "rollforward",
    "rebuild",
    "switch",
    "build",
    "upgrade-analysis",
    "bootloader-switch",
    "bootloader-cleanup",
    "feature-list",
    "feature-enable",
    "feature-disable",
    "fix-flake",
    "tailscale-operator",
    "service-enable",
    "service-disable",
    "service-set-url",
    "service-restart",
    "plex-pass-enable",
    "plex-pass-disable",
    "beszel-setup",
    "beszel-status",
    "secrets-init",
];

/// Read-only operations that answer with JSON instead of starting a job.
pub const QUERIES: &[&str] = &[];

pub fn is_native(op_id: &str) -> bool {
    NATIVE_OPERATIONS.contains(&op_id)
}

pub fn is_query(op_id: &str) -> bool {
    QUERIES.contains(&op_id)
}

/// Everything an operation needs from the daemon.
pub struct Ctx<'a, R: Runner> {
    pub runner: &'a R,
    /// Where the host's configuration lives: `/etc/nixos`, or a temporary directory in
    /// tests. Operations never build a path from anything the caller sent.
    pub nixos_dir: &'a Path,
    /// The filesystem root: `/`, or a temporary directory in tests. The fixed system
    /// paths (`/run/current-system`, the system profile, `/sys/firmware/efi`) hang off
    /// it, so a test can build a miniature machine.
    pub root: &'a Path,
    pub sink: &'a dyn Sink,
    /// The validated, non-secret answers.
    pub args: &'a Args,
    /// Secret answers. They go straight to a 0600 file or a child's stdin and are never
    /// logged, which is why they are kept apart from `args`.
    pub secrets: &'a BTreeMap<String, Secret>,
    /// The uid that asked, for the few operations that act on that user's account.
    pub caller_uid: Option<u32>,
}

/// Run a native operation and return its exit code, or `None` if `op_id` is not one.
///
/// When a [`runner::Runner`] call returns [`runner::RunError::Cancelled`] the job was
/// cancelled: the operation must stop there and return a non-zero code, not carry on
/// to its next step.
pub async fn run<R: Runner>(op_id: &str, ctx: &Ctx<'_, R>) -> Option<i32> {
    Some(match op_id {
        "variant" => system::variant(ctx),
        "reboot" => system::reboot(ctx).await,
        "shutdown" => system::shutdown(ctx).await,
        "set-hostname" => system::set_hostname(ctx).await,
        "rollback" => system::rollback(ctx).await,
        "rollforward" => system::rollforward(ctx).await,
        "rebuild" => rebuild::rebuild(ctx).await,
        "switch" => rebuild::switch(ctx).await,
        "build" => rebuild::build(ctx).await,
        "upgrade-analysis" => upgrade::analyse(ctx).await,
        "bootloader-switch" => bootloader::switch(ctx).await,
        "bootloader-cleanup" => bootloader::cleanup(ctx).await,
        "feature-list" => features::list(ctx).await,
        "feature-enable" => features::enable(ctx).await,
        "feature-disable" => features::disable(ctx).await,
        "fix-flake" => features::fix_flake(ctx).await,
        "tailscale-operator" => tailscale::operator(ctx).await,
        "service-enable" => services::enable(ctx).await,
        "service-disable" => services::disable(ctx).await,
        "service-set-url" => services::set_url(ctx).await,
        "service-restart" => services::restart(ctx).await,
        "plex-pass-enable" => services::plex_pass(ctx, true).await,
        "plex-pass-disable" => services::plex_pass(ctx, false).await,
        "beszel-setup" => beszel::setup(ctx).await,
        "beszel-status" => beszel::status(ctx).await,
        "secrets-init" => sops::init(ctx).await,
        _ => {
            debug_assert!(!is_native(op_id), "`{op_id}` is listed but has no handler");
            return None;
        }
    })
}

/// Report a failure to the job log and give the exit code that goes with it.
pub(crate) fn fail(sink: &dyn Sink, message: impl std::fmt::Display) -> i32 {
    sink.error(&format!("error: {message}"));
    1
}

/// Run a long command with its output in the job log. `Err` is the exit code to stop
/// with: the command could not start, or the job was cancelled.
pub(crate) async fn stream<R: Runner>(ctx: &Ctx<'_, R>, cmd: &Cmd) -> Result<i32, i32> {
    ctx.sink.info(&format!("$ {}", cmd.display()));
    ctx.runner
        .stream(cmd, ctx.sink)
        .await
        .map_err(|e| run_error(ctx.sink, e))
}

/// Run a short command and capture it. `Err` as for [`stream`].
pub(crate) async fn capture<R: Runner>(ctx: &Ctx<'_, R>, cmd: &Cmd) -> Result<runner::Output, i32> {
    ctx.runner
        .run(cmd)
        .await
        .map_err(|e| run_error(ctx.sink, e))
}

fn run_error(sink: &dyn Sink, error: RunError) -> i32 {
    match error {
        RunError::Cancelled => {
            sink.error("Cancelled.");
            1
        }
        other => fail(sink, other),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("`{0}` is not a query VexPortal knows about")]
    Unknown(String),
    #[error("{0}")]
    Failed(String),
}

/// Answer a read-only query with JSON.
pub fn query(op_id: &str, nixos_dir: &Path, args: &Args) -> Result<String, QueryError> {
    let _ = (nixos_dir, args);
    Err(QueryError::Unknown(op_id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_is_hidden_from_debug_until_exposed() {
        let secret = Secret::new("hunter2");
        assert!(!format!("{secret:?}").contains("hunter2"));
        assert_eq!(secret.expose(), "hunter2");
        let secrets: BTreeMap<String, Secret> =
            [("password".to_string(), secret)].into_iter().collect();
        assert!(!format!("{secrets:?}").contains("hunter2"));
    }

    #[test]
    fn the_system_and_feature_operations_are_native_and_the_rest_are_not_yet() {
        assert!(is_native("switch"));
        assert!(is_native("feature-enable"));
        assert!(is_native("service-restart"));
        assert!(!is_native("backup-now"));
        assert!(!is_query("feature-list"));
    }

    #[test]
    fn an_unknown_query_is_an_error_not_an_empty_answer() {
        let err = query("nope", Path::new("/nonexistent"), &Args::new()).unwrap_err();
        assert!(matches!(err, QueryError::Unknown(_)));
    }
}

/// A miniature machine for the operations' tests.
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use crate::fsutil::testing::TempDir;
    use crate::runner::testing::CollectSink;
    use std::path::PathBuf;

    pub struct Kit {
        pub root: TempDir,
        pub nixos: PathBuf,
        pub sink: CollectSink,
        pub args: Args,
        pub secrets: BTreeMap<String, Secret>,
        pub caller: Option<u32>,
    }

    impl Kit {
        pub fn new() -> Self {
            let root = TempDir::new();
            let nixos = root.join("etc/nixos");
            std::fs::create_dir_all(&nixos).unwrap();
            std::fs::create_dir_all(root.join("run")).unwrap();
            Self {
                root,
                nixos,
                sink: CollectSink::new(),
                args: Args::new(),
                secrets: BTreeMap::new(),
                caller: None,
            }
        }

        pub fn with_args(mut self, pairs: &[(&str, &str)]) -> Self {
            self.args = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            self
        }

        /// Write a file under the fake `/etc/nixos`.
        pub fn nixos_file(&self, name: &str, text: &str) -> PathBuf {
            let path = self.nixos.join(name);
            std::fs::write(&path, text).unwrap();
            path
        }

        pub fn ctx<'a, R: Runner>(&'a self, runner: &'a R) -> Ctx<'a, R> {
            Ctx {
                runner,
                nixos_dir: &self.nixos,
                root: self.root.path(),
                sink: &self.sink,
                args: &self.args,
                secrets: &self.secrets,
                caller_uid: self.caller,
            }
        }
    }
}
