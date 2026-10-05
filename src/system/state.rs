//! The pages' view of this machine.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SYSTEM_PROFILE: &str = "/nix/var/nix/profiles/system";
const CURRENT_SYSTEM: &str = "/run/current-system";
const BOOTED_SYSTEM: &str = "/run/booted-system";
const FEATURES_FILE: &str = "/etc/nixos/features.nix";
const SERVICES_FILE: &str = "/etc/nixos/server-services.nix";
const FLAKE_LOCK: &str = "/etc/nixos/flake.lock";

/// Files the justfile edits for the next rebuild to pick up. One changed after the
/// running system was activated means there are changes waiting for a rebuild.
const REBUILD_INPUTS: [&str; 4] = [
    FEATURES_FILE,
    SERVICES_FILE,
    "/etc/nixos/storage-remote.nix",
    "/etc/nixos/storage-pool.nix",
];

#[derive(Debug, Clone, Default)]
pub struct SystemState {
    pub hostname: Option<String>,
    /// The generation number the `system` profile points at.
    pub generation: Option<u32>,
    /// True when the running kernel and system differ from the activated one, i.e. a
    /// rebuild has landed that a reboot has not picked up yet.
    pub reboot_pending: bool,
    /// True when a file the next rebuild reads changed after the last activation.
    pub rebuild_needed: bool,
    /// `option = value` settings from features.nix and server-services.nix.
    pub settings: Settings,
    /// How long since the newest flake input was updated.
    pub lock_age: Option<Duration>,
}

impl SystemState {
    pub fn read() -> Self {
        let mut settings = Settings::default();
        for file in [FEATURES_FILE, SERVICES_FILE] {
            match std::fs::read_to_string(file) {
                Ok(contents) => settings.merge(&contents),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => log::warn!("could not read {file}: {e}"),
            }
        }
        Self {
            hostname: read_hostname(),
            generation: read_generation(),
            reboot_pending: reboot_pending(),
            rebuild_needed: rebuild_needed(),
            settings,
            lock_age: read_lock_age(),
        }
    }

    /// "3 days", "6 hours" — for the overview's flake-freshness line.
    pub fn lock_age_label(&self) -> Option<String> {
        let age = self.lock_age?;
        let hours = age.as_secs() / 3600;
        Some(match hours {
            0 => "under an hour ago".to_string(),
            1 => "1 hour ago".to_string(),
            2..=23 => format!("{hours} hours ago"),
            _ => {
                let days = hours / 24;
                if days == 1 {
                    "1 day ago".to_string()
                } else {
                    format!("{days} days ago")
                }
            }
        })
    }
}

/// The assignments in VexOS's generated Nix files.
///
/// features.nix and server-services.nix are written by the justfile one
/// `vexos.<path> = <value>;` per line, which is exactly what the justfile itself greps
/// for — so a line match is enough, and avoids pulling in a Nix parser.
#[derive(Debug, Clone, Default)]
pub struct Settings(BTreeMap<String, String>);

impl Settings {
    pub fn merge(&mut self, contents: &str) {
        for line in contents.lines() {
            // Drop a trailing comment first: `vexos.features.vpn.enable = true; # PIA`
            // is enabled, however the comment is worded.
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if !key.starts_with("vexos.") {
                continue;
            }
            let value = value.trim().trim_end_matches(';').trim().trim_matches('"');
            self.0.insert(key.to_string(), value.to_string());
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    fn is_true(&self, key: &str) -> Option<bool> {
        self.get(key).map(|v| v == "true")
    }

    /// Whether a feature is on, and whether that is only the module default.
    pub fn feature(&self, name: &str, default_on: bool) -> (bool, bool) {
        match self.is_true(&format!("vexos.features.{name}.enable")) {
            Some(enabled) => (enabled, false),
            None => (default_on, true),
        }
    }

    pub fn service_enabled(&self, name: &str) -> bool {
        self.is_true(&format!("vexos.server.{name}.enable"))
            .unwrap_or(false)
    }

    pub fn plex_pass(&self) -> bool {
        self.is_true("vexos.server.plex.plexPass").unwrap_or(false)
    }

    /// The desktop environment `just switch` last wrote, if any.
    pub fn desktop_environment(&self) -> Option<&str> {
        self.get("vexos.desktop.environment")
    }

    pub fn vm_platform(&self) -> Option<&str> {
        self.get("vexos.vm.platform")
    }
}

fn read_hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
}

/// The profile symlink is `system-<n>-link`, which gives the generation number without
/// taking the profile lock that `nix-env --list-generations` needs root for.
fn read_generation() -> Option<u32> {
    let target = std::fs::read_link(SYSTEM_PROFILE).ok()?;
    let name = target.file_name()?.to_str()?;
    name.strip_prefix("system-")?
        .strip_suffix("-link")?
        .parse()
        .ok()
}

fn reboot_pending() -> bool {
    let (Ok(booted), Ok(current)) = (
        std::fs::read_link(BOOTED_SYSTEM),
        std::fs::read_link(CURRENT_SYSTEM),
    ) else {
        return false;
    };
    booted != current
}

/// `/run/current-system` is replaced on every activation, so its own modification
/// time (the link's, not its target's) is when this system was last switched to.
fn rebuild_needed() -> bool {
    let Ok(activated) = std::fs::symlink_metadata(CURRENT_SYSTEM).and_then(|m| m.modified()) else {
        return false;
    };
    REBUILD_INPUTS
        .iter()
        .filter_map(|file| std::fs::metadata(Path::new(file)).ok()?.modified().ok())
        .any(|edited| edited > activated)
}

/// The newest `lastModified` across all locked inputs — how current the flake is.
fn read_lock_age() -> Option<Duration> {
    let contents = std::fs::read_to_string(FLAKE_LOCK).ok()?;
    let lock: serde_json::Value = serde_json::from_str(&contents).ok()?;
    let newest = lock
        .get("nodes")?
        .as_object()?
        .values()
        .filter_map(|node| node.get("locked")?.get("lastModified")?.as_u64())
        .max()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(now.saturating_sub(newest)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_age_reads_naturally() {
        let with = |secs| SystemState {
            lock_age: Some(Duration::from_secs(secs)),
            ..Default::default()
        };
        assert_eq!(with(60).lock_age_label().unwrap(), "under an hour ago");
        assert_eq!(with(3600).lock_age_label().unwrap(), "1 hour ago");
        assert_eq!(with(3600 * 5).lock_age_label().unwrap(), "5 hours ago");
        assert_eq!(with(3600 * 24).lock_age_label().unwrap(), "1 day ago");
        assert_eq!(with(3600 * 24 * 9).lock_age_label().unwrap(), "9 days ago");
    }

    #[test]
    fn no_lock_means_no_label() {
        assert!(SystemState::default().lock_age_label().is_none());
    }

    #[test]
    fn reads_this_host_without_erroring() {
        // Every field is optional by design: VexPortal must still start on a machine
        // that is missing any of these files.
        let _ = SystemState::read();
    }

    fn settings(contents: &str) -> Settings {
        let mut settings = Settings::default();
        settings.merge(contents);
        settings
    }

    #[test]
    fn features_are_explicit_or_defaulted() {
        let s = settings(
            "{\n  vexos.features.gaming.enable = true;\n\
             # vexos.features.print3d.enable = true;\n\
             vexos.features.sunshine.enable = false;\n}\n",
        );
        assert_eq!(s.feature("gaming", false), (true, false));
        assert_eq!(s.feature("print3d", false), (false, true));
        assert_eq!(s.feature("sunshine", true), (false, false));
        assert_eq!(s.feature("vpn", false), (false, true));
    }

    #[test]
    fn a_trailing_comment_does_not_hide_an_enabled_feature() {
        // L8.
        let s = settings("  vexos.features.vpn.enable = true; # PIA\n");
        assert_eq!(s.feature("vpn", false), (true, false));
    }

    #[test]
    fn services_and_plex_pass() {
        let s = settings(
            "  vexos.server.plex.enable = true;\n  vexos.server.plex.plexPass = true;\n\
             vexos.server.jellyfin.enable = false;\n",
        );
        assert!(s.service_enabled("plex"));
        assert!(!s.service_enabled("jellyfin"));
        assert!(!s.service_enabled("immich"));
        assert!(s.plex_pass());
    }

    #[test]
    fn quoted_values_lose_their_quotes() {
        let s = settings("  vexos.desktop.environment = \"cosmic\";\n");
        assert_eq!(s.desktop_environment(), Some("cosmic"));
        assert_eq!(s.vm_platform(), None);
    }
}
