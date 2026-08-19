//! Stopping a running recipe.
//!
//! A recipe is a bash script that shells out to `nix`, `nixos-rebuild` and `sudo`, so
//! signalling the `just` process alone would leave the real work running. Every job
//! gets its own process group and cancellation signals the whole group.

use log::{debug, warn};
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

pub struct JobHandle {
    pub job_id: String,
    pub recipe: String,
    pub cancel: CancellationToken,
    pub task: tokio::task::JoinHandle<()>,
}

impl JobHandle {
    pub fn is_running(&self) -> bool {
        !self.task.is_finished()
    }

    /// Request cancellation. Returns false if the job had already finished.
    pub fn request_cancel(&self) -> bool {
        if self.task.is_finished() {
            return false;
        }
        self.cancel.cancel();
        true
    }
}

fn signal_group(child: &Child, signal: i32, name: &str) {
    let Some(pid) = child.id() else {
        debug!("no pid to signal — the child has already been reaped");
        return;
    };
    // Negative pid targets the process group, which `process_group(0)` made equal to
    // the child's own pid.
    let result = unsafe { libc::kill(-(pid as i32), signal) };
    if result != 0 {
        warn!(
            "failed to send {name} to process group {pid}: {}",
            std::io::Error::last_os_error()
        );
    }
}

pub fn terminate(child: &Child) {
    signal_group(child, libc::SIGTERM, "SIGTERM");
}

pub fn kill(child: &Child) {
    signal_group(child, libc::SIGKILL, "SIGKILL");
}
