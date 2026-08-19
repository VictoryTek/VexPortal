//! Parsing `/etc/nixos/vexos-variant`.

use std::fmt;
use std::path::Path;
use vexportal_catalog::Role;

pub const VARIANT_FILE: &str = "/etc/nixos/vexos-variant";

/// Lets a developer point the GUI at another role to check what it renders, without
/// building that role. It only affects which recipes are listed; the daemon decides
/// what may actually run.
pub const OVERRIDE_ENV: &str = "VEXPORTAL_VARIANT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub role: Role,
    /// The GPU part of the variant: `amd`, `nvidia`, `nvidia-legacy535`, `intel`, `vm`.
    pub gpu: String,
    /// The string as written in the file, e.g. `vexos-desktop-nvidia`.
    pub raw: String,
}

impl fmt::Display for Variant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VariantError {
    #[error("{VARIANT_FILE} does not exist — this host has not been built by vexos-nix yet")]
    NotBuilt,
    #[error("could not read {VARIANT_FILE}: {0}")]
    Unreadable(#[from] std::io::Error),
    #[error("`{0}` is not a variant VexPortal recognises")]
    Unrecognised(String),
}

impl Variant {
    pub fn detect() -> Result<Self, VariantError> {
        if let Ok(override_value) = std::env::var(OVERRIDE_ENV) {
            return Self::parse(override_value.trim());
        }
        if !Path::new(VARIANT_FILE).exists() {
            return Err(VariantError::NotBuilt);
        }
        Self::parse(std::fs::read_to_string(VARIANT_FILE)?.trim())
    }

    /// `vexos-<role>-<gpu>`, where role may itself contain a hyphen
    /// (`headless-server`), so the role is matched by longest prefix rather than by
    /// splitting on hyphens.
    pub fn parse(raw: &str) -> Result<Self, VariantError> {
        let rest = raw
            .strip_prefix("vexos-")
            .ok_or_else(|| VariantError::Unrecognised(raw.to_string()))?;

        // Longest first, so `headless-server` is not shadowed by `server`.
        let mut roles = Role::ALL;
        roles.sort_by_key(|r| std::cmp::Reverse(r.as_str().len()));

        for role in roles {
            if let Some(gpu) = rest.strip_prefix(role.as_str()) {
                let gpu = gpu.strip_prefix('-').unwrap_or(gpu);
                return Ok(Variant {
                    role,
                    gpu: gpu.to_string(),
                    raw: raw.to_string(),
                });
            }
        }
        Err(VariantError::Unrecognised(raw.to_string()))
    }

    /// How the GPU reads in the dashboard badge.
    pub fn gpu_label(&self) -> &str {
        match self.gpu.as_str() {
            "amd" => "AMD",
            "nvidia" => "NVIDIA",
            "nvidia-legacy535" => "NVIDIA (legacy 535)",
            "intel" => "Intel",
            "vm" => "Virtual machine",
            "" => "no GPU variant",
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_this_host() {
        let variant = Variant::parse("vexos-desktop-nvidia").unwrap();
        assert_eq!(variant.role, Role::Desktop);
        assert_eq!(variant.gpu, "nvidia");
    }

    #[test]
    fn a_hyphenated_role_is_not_confused_with_its_suffix() {
        let variant = Variant::parse("vexos-headless-server-amd").unwrap();
        assert_eq!(variant.role, Role::HeadlessServer);
        assert_eq!(variant.gpu, "amd");
    }

    #[test]
    fn parses_every_role_and_gpu_combination() {
        for role in Role::ALL {
            for gpu in ["amd", "nvidia", "nvidia-legacy535", "intel", "vm"] {
                let raw = format!("vexos-{}-{gpu}", role.as_str());
                let variant = Variant::parse(&raw).unwrap();
                assert_eq!(variant.role, role, "{raw}");
                assert_eq!(variant.gpu, gpu, "{raw}");
            }
        }
    }

    #[test]
    fn rejects_something_that_is_not_a_variant() {
        assert!(Variant::parse("ubuntu").is_err());
        assert!(Variant::parse("vexos-toaster-amd").is_err());
    }
}
