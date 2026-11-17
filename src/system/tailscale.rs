//! Tailscale's state, from `tailscale status --json`.
//!
//! Once an account is the operator (the daemon's `tailscale-operator`), `tailscale
//! status` and `tailscale up` run as that account with no privilege. Both run as async
//! subprocesses; the window never waits on them. The parsing is `vexportal_ops::tailscale`,
//! which is tested without a Tailscale.

use std::ffi::OsStr;
use std::time::Duration;
use vexportal_ops::tailscale::{parse_status, Status};

const CLI: &str = "tailscale";

/// A `tailscale status` that has not answered by now is not going to. It is stopped so a
/// hung daemon cannot hold the page, or a sign-in in progress, waiting forever.
const STATUS_TIMEOUT: Duration = Duration::from_secs(8);

pub fn is_installed() -> bool {
    glib::find_program_in_path(CLI).is_some()
}

/// What the Tailscale card can show.
#[derive(Debug, Clone)]
pub enum State {
    NotInstalled,
    /// `tailscaled` is not running, did not answer in time, or answered with something
    /// unreadable.
    Unavailable(String),
    Ready(Status),
}

pub async fn status() -> State {
    if !is_installed() {
        return State::NotInstalled;
    }
    let argv = [OsStr::new(CLI), OsStr::new("status"), OsStr::new("--json")];
    let process = match gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE,
    ) {
        Ok(process) => process,
        Err(e) => return State::Unavailable(e.to_string()),
    };

    // If nothing has answered by the deadline the process is killed, which closes its pipes
    // and ends the wait below. `finished` stops a late timer from touching anything.
    let timed_out = std::rc::Rc::new(std::cell::Cell::new(false));
    let finished = std::rc::Rc::new(std::cell::Cell::new(false));
    glib::spawn_future_local({
        let process = process.clone();
        let timed_out = timed_out.clone();
        let finished = finished.clone();
        async move {
            glib::timeout_future(STATUS_TIMEOUT).await;
            if !finished.get() {
                timed_out.set(true);
                process.force_exit();
            }
        }
    });

    let answered = process.communicate_utf8_future(None).await;
    finished.set(true);
    let (stdout, stderr) = match answered {
        Ok(output) => output,
        Err(e) => return State::Unavailable(e.to_string()),
    };
    if timed_out.get() {
        return State::Unavailable("tailscale did not answer".to_string());
    }
    // A stopped or signed-out Tailscale still prints its state, sometimes with a failing
    // exit status, so the document is read whatever the status was.
    if let Some(status) = stdout.as_deref().and_then(parse_status) {
        return State::Ready(status);
    }
    let message = stderr.map(|s| s.trim().to_string()).unwrap_or_default();
    State::Unavailable(if message.is_empty() {
        "tailscale did not answer".to_string()
    } else {
        message
    })
}

/// Start `tailscale up`, which signs this account in and keeps running until the person
/// has finished in the browser. Its own output is not read: the sign-in link comes from
/// `status`, which carries it while a sign-in is waiting.
pub fn start_up() -> Result<gio::Subprocess, glib::Error> {
    let argv = [OsStr::new(CLI), OsStr::new("up")];
    gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_SILENCE,
    )
}
