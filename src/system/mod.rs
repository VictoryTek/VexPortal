//! Reading what this machine currently is.
//!
//! All of it comes from world-readable files and symlinks, so the dashboard needs
//! neither the daemon nor a subprocess: `/etc/nixos/vexos-variant` names the role and
//! GPU, the `system` profile symlink names the generation, and comparing
//! `/run/booted-system` with `/run/current-system` says whether a reboot is pending.

pub mod state;
pub mod variant;

pub use state::SystemState;
pub use variant::Variant;
