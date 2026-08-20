//! The dashboard's view of this machine.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SYSTEM_PROFILE: &str = "/nix/var/nix/profiles/system";
const CURRENT_SYSTEM: &str = "/run/current-system";
const BOOTED_SYSTEM: &str = "/run/booted-system";
const FEATURES_FILE: &str = "/etc/nixos/features.nix";
const FLAKE_LOCK: &str = "/etc/nixos/flake.lock";

#[derive(Debug, Clone, Default)]
pub struct SystemState {
    pub hostname: Option<String>,
    /// The generation number the `system` profile points at.
    pub generation: Option<u32>,
    /// True when the running kernel and system differ from the activated one, i.e. a
    /// rebuild has landed that a reboot has not picked up yet.
    pub reboot_pending: bool,
    /// Feature name → enabled, read from `/etc/nixos/features.nix`.
    pub features: Vec<(String, bool)>,
    /// How long since the newest flake input was updated.
    pub lock_age: Option<Duration>,
}

impl SystemState {
    pub fn read() -> Self {
        Self {
            hostname: read_hostname(),
            generation: read_generation(),
            reboot_pending: reboot_pending(),
            features: read_features(),
            lock_age: read_lock_age(),
        }
    }

    /// "3 days", "6 hours" — for the dashboard's flake-freshness line.
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

/// `features.nix` is a generated file that `just enable-feature` rewrites, so a line
/// match is enough and avoids pulling in a Nix parser for five booleans.
fn read_features() -> Vec<(String, bool)> {
    let Ok(contents) = std::fs::read_to_string(FEATURES_FILE) else {
        return Vec::new();
    };
    let mut features = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("vexos.features.") else {
            continue;
        };
        let Some((name, value)) = rest.split_once(".enable") else {
            continue;
        };
        let enabled = value
            .trim_start_matches([' ', '='])
            .trim_end_matches(';')
            .trim()
            == "true";
        features.push((name.to_string(), enabled));
    }
    features
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
}
