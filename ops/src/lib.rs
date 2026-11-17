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

pub mod fsutil;
pub mod gitrepo;
pub mod nixedit;
pub mod runner;

use runner::{Runner, Sink};
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
pub const NATIVE_OPERATIONS: &[&str] = &[];

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
    let _ = ctx;
    debug_assert!(!is_native(op_id), "`{op_id}` is listed but has no handler");
    None
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
    fn nothing_is_native_until_a_phase_adds_it() {
        assert!(!is_native("switch"));
        assert!(!is_query("feature-list"));
    }

    #[test]
    fn an_unknown_query_is_an_error_not_an_empty_answer() {
        let err = query("nope", Path::new("/nonexistent"), &Args::new()).unwrap_err();
        assert!(matches!(err, QueryError::Unknown(_)));
    }
}
