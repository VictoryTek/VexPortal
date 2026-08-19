//! The record of what the portal was asked to do.
//!
//! Everything lands in the journal under the daemon's unit, so `journalctl -u
//! vexportal-daemon` is the answer to "who rebuilt this machine at 3am". Secrets never
//! appear: [`Invocation::audit_line`] renders a stdin secret as a placeholder.

use log::{info, warn};
use vexportal_catalog::validate::Invocation;

pub fn started(job: &str, caller: &str, uid: Option<u32>, invocation: &Invocation) {
    info!(
        "job {job} started by {caller} (uid {}): {} [risk {:?}]",
        uid.map(|u| u.to_string()).unwrap_or_else(|| "?".into()),
        invocation.audit_line(),
        invocation.risk
    );
}

pub fn finished(job: &str, exit_code: i32) {
    if exit_code == 0 {
        info!("job {job} finished successfully");
    } else {
        warn!("job {job} failed with exit code {exit_code}");
    }
}

pub fn cancelled(job: &str) {
    warn!("job {job} cancelled");
}

/// A request that never reached `exec`. Worth a warning: a well-behaved GUI validates
/// before calling, so a rejection here means either a bug or something else on the bus.
pub fn rejected(caller: &str, recipe: &str, reason: &str) {
    warn!("rejected `{recipe}` from {caller}: {reason}");
}
