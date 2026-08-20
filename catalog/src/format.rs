//! Value validators.
//!
//! Arguments reach `just` through `argv`, never a shell, so shell metacharacters are
//! inert by construction. These checks exist anyway as a second line: they keep a
//! malformed value from reaching a recipe that *does* interpolate it into a `bash`
//! body (`{{service}}` inside a `#!/usr/bin/env bash` recipe is a real interpolation),
//! and they turn a typo into a clear error instead of a confusing recipe failure.

use crate::Format;

/// Longest value any parameter accepts. Comfortably above a real path or hostname.
const MAX_LEN: usize = 512;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FormatError {
    #[error("value is empty")]
    Empty,
    #[error("value is longer than {MAX_LEN} characters")]
    TooLong,
    #[error("value contains a control character")]
    ControlCharacter,
    #[error("expected {expected}, got `{got}`")]
    Malformed { expected: &'static str, got: String },
}

impl Format {
    /// Describe the accepted shape, for GUI help text and error messages.
    pub fn expectation(self) -> &'static str {
        match self {
            Format::Slug => "lowercase letters, digits, dots, dashes or underscores",
            Format::Hostname => "a hostname such as `vexos-office`",
            Format::SshTarget => "`user@host` or `host`",
            Format::AbsPath => "an absolute path",
            Format::NixosVersion => "a NixOS release such as `26.05`",
            Format::FlakeRef => "a path or flake reference such as `.` or `github:owner/repo`",
        }
    }

    pub fn validate(self, value: &str) -> Result<(), FormatError> {
        if value.is_empty() {
            return Err(FormatError::Empty);
        }
        if value.len() > MAX_LEN {
            return Err(FormatError::TooLong);
        }
        if value.chars().any(|c| c.is_control()) {
            return Err(FormatError::ControlCharacter);
        }

        let ok = match self {
            Format::Slug => is_slug(value),
            Format::Hostname => is_hostname(value),
            Format::SshTarget => is_ssh_target(value),
            Format::AbsPath => is_abs_path(value),
            Format::NixosVersion => is_nixos_version(value),
            Format::FlakeRef => is_flake_ref(value),
        };

        if ok {
            Ok(())
        } else {
            Err(FormatError::Malformed {
                expected: self.expectation(),
                got: value.to_string(),
            })
        }
    }
}

fn is_slug(v: &str) -> bool {
    v.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && v.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
}

fn is_hostname(v: &str) -> bool {
    // A single DNS label: RFC 1123, max 63 characters, no leading or trailing hyphen.
    v.len() <= 63
        && v.starts_with(|c: char| c.is_ascii_alphanumeric())
        && v.ends_with(|c: char| c.is_ascii_alphanumeric())
        && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn is_ssh_target(v: &str) -> bool {
    let (user, host) = match v.split_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, v),
    };
    if let Some(user) = user {
        let user_ok = !user.is_empty()
            && user.len() <= 32
            && user
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
        if !user_ok {
            return false;
        }
    }
    // Host is either a dotted name or an IP literal; both reduce to labels split on `.`.
    !host.is_empty()
        && host
            .split('.')
            .all(|label| !label.is_empty() && is_hostname(label))
}

fn is_abs_path(v: &str) -> bool {
    v.starts_with('/') && !v.split('/').any(|c| c == "..")
}

fn is_nixos_version(v: &str) -> bool {
    match v.split_once('.') {
        Some((year, month)) => {
            year.len() == 2
                && month.len() == 2
                && year.chars().all(|c| c.is_ascii_digit())
                && month.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

fn is_flake_ref(v: &str) -> bool {
    // Either a path (`.`, `./x`, `/etc/nixos`) or `scheme:rest` with a conservative charset.
    let path_like = v == "." || v.starts_with('/') || v.starts_with("./");
    let url_like = matches!(
        v.split_once(':'),
        Some((scheme, rest))
            if !rest.is_empty()
                && matches!(scheme, "github" | "gitlab" | "git+https" | "git+ssh" | "path" | "flake")
    );
    (path_like || url_like)
        && v.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '.' | '/' | '-' | '_' | ':' | '+' | '?' | '=' | '&' | '#')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert!(Format::Slug.validate("jellyfin").is_ok());
        assert!(Format::Slug.validate("nginx-proxy-manager").is_ok());
        assert!(Format::Slug.validate("matrix-conduit").is_ok());
        assert!(Format::Slug.validate("Jellyfin").is_err());
        assert!(Format::Slug.validate("-leading").is_err());
        assert!(Format::Slug.validate("").is_err());
    }

    #[test]
    fn shell_metacharacters_are_rejected_everywhere() {
        // The daemon never uses a shell, but a rejected value can never reach a
        // recipe body that interpolates `{{param}}` into bash either.
        for probe in [
            "; rm -rf /",
            "$(reboot)",
            "`id`",
            "a && b",
            "x | y",
            "foo\nbar",
            "../../etc/shadow",
        ] {
            for format in [
                Format::Slug,
                Format::Hostname,
                Format::SshTarget,
                Format::AbsPath,
                Format::NixosVersion,
                Format::FlakeRef,
            ] {
                assert!(
                    format.validate(probe).is_err(),
                    "{format:?} accepted {probe:?}"
                );
            }
        }
    }

    #[test]
    fn hostnames() {
        assert!(Format::Hostname.validate("vexos-office").is_ok());
        assert!(Format::Hostname.validate("a").is_ok());
        assert!(Format::Hostname.validate("-nope").is_err());
        assert!(Format::Hostname.validate("nope-").is_err());
        assert!(Format::Hostname.validate(&"a".repeat(64)).is_err());
    }

    #[test]
    fn ssh_targets() {
        assert!(Format::SshTarget.validate("nimda@10.35.1.50").is_ok());
        assert!(Format::SshTarget.validate("vexos-office").is_ok());
        assert!(Format::SshTarget
            .validate("nimda@vexos-vmc.tailbd686.ts.net")
            .is_ok());
        assert!(Format::SshTarget.validate("@host").is_err());
        assert!(Format::SshTarget.validate("user@").is_err());
    }

    #[test]
    fn absolute_paths() {
        assert!(Format::AbsPath.validate("/var/lib/plex.tar.gz").is_ok());
        assert!(Format::AbsPath
            .validate("/home/nimda/My Backup.tar.gz")
            .is_ok());
        assert!(Format::AbsPath.validate("relative/path").is_err());
        assert!(Format::AbsPath.validate("/a/../../etc").is_err());
    }

    #[test]
    fn nixos_versions() {
        assert!(Format::NixosVersion.validate("26.05").is_ok());
        assert!(Format::NixosVersion.validate("26.11").is_ok());
        assert!(Format::NixosVersion.validate("2026.05").is_err());
        assert!(Format::NixosVersion.validate("unstable").is_err());
    }

    #[test]
    fn flake_refs() {
        assert!(Format::FlakeRef.validate(".").is_ok());
        assert!(Format::FlakeRef.validate("/etc/nixos").is_ok());
        assert!(Format::FlakeRef
            .validate("github:VictoryTek/vexos-nix")
            .is_ok());
        assert!(Format::FlakeRef.validate("rm -rf /").is_err());
    }
}
