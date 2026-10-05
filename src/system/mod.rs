//! Reading what this machine currently is.
//!
//! All of it comes from world-readable files and symlinks, or from CLIs that answer
//! without root, so the pages need neither the daemon nor a privileged subprocess:
//! `/etc/nixos/vexos-variant` names the role and GPU, the `system` profile symlink
//! names the generation, comparing `/run/booted-system` with `/run/current-system`
//! says whether a reboot is pending, and `vexos-vpn status --json` reports the VPN.

pub mod state;
pub mod variant;
pub mod vpn;

pub use state::SystemState;
pub use variant::Variant;
