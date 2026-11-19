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
            Format::ProgramName => "a program name such as `firefox`",
            Format::Url => "an address starting with http:// or https://",
            Format::Ipv4 => "an IPv4 address such as `192.168.1.100`",
            Format::Interface => "a network interface name such as `enp2s0`",
            Format::Email => "an e-mail address such as `you@example.com`",
            Format::Domain => "a domain such as `example.com`",
            Format::Subdomain => "a subdomain such as `home` or `vpn.lab`",
            Format::SshKey => "an SSH public key such as `ssh-ed25519 AAAA...`",
            Format::JobName => "a job name such as `tv`: letters, digits, dots, dashes or underscores",
            Format::ResticRepo => {
                "a path such as `/mnt/backup/restic-repo`, or an address such as `sftp:user@host:/path`"
            }
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
            Format::ProgramName => is_program_name(value),
            Format::Url => is_url(value),
            Format::Ipv4 => is_ipv4(value),
            Format::Interface => is_interface(value),
            Format::Email => is_email(value),
            Format::Domain => is_domain(value),
            Format::Subdomain => is_subdomain(value),
            Format::ResticRepo => is_restic_repo(value),
            Format::SshKey => is_ssh_key(value),
            Format::JobName => is_job_name(value),
        };

        if ok {
            Ok(())
        } else {
            // A password inside a repository address must not be echoed back into an
            // error, a log or the audit trail.
            let got = if self == Format::ResticRepo && has_credentials(value) {
                "<hidden>".to_string()
            } else {
                value.to_string()
            };
            Err(FormatError::Malformed {
                expected: if got == "<hidden>" {
                    "an address without a password in it (restic reads one from the password file)"
                } else {
                    self.expectation()
                },
                got,
            })
        }
    }
}

fn is_slug(v: &str) -> bool {
    v.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && v.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
}

/// What `nas-sync-<name>.service` can be named after: the `[A-Za-z0-9][A-Za-z0-9._-]*`
/// the justfile accepted, at most 64 characters.
fn is_job_name(v: &str) -> bool {
    v.len() <= 64
        && v.starts_with(|c: char| c.is_ascii_alphanumeric())
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
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

fn is_program_name(v: &str) -> bool {
    v.len() <= 255
        && !v.starts_with('.')
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
}

/// What follows `http://` or `https://`: only characters that are safe inside a Nix
/// string and a URL, so no space, quote, backslash, `$` or `;`.
fn is_url(v: &str) -> bool {
    let Some(rest) = v
        .strip_prefix("https://")
        .or_else(|| v.strip_prefix("http://"))
    else {
        return false;
    };
    rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '[')
        && rest.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '.' | '_'
                        | '~'
                        | ':'
                        | '/'
                        | '?'
                        | '#'
                        | '@'
                        | '%'
                        | '+'
                        | '='
                        | '&'
                        | ','
                        | '['
                        | ']'
                        | '-'
                )
        })
}

fn is_ipv4(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.chars().all(|c| c.is_ascii_digit())
                && p.parse::<u16>().is_ok_and(|n| n <= 255)
        })
}

fn is_interface(v: &str) -> bool {
    // The kernel allows 15 characters; at least two, starting with a letter.
    v.len() >= 2
        && v.len() <= 15
        && v.starts_with(|c: char| c.is_ascii_alphabetic())
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-'))
}

fn is_email(v: &str) -> bool {
    let Some((local, domain)) = v.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-'))
        && is_domain(domain)
}

fn is_label(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 63
        && v.starts_with(|c: char| c.is_ascii_alphanumeric())
        && v.ends_with(|c: char| c.is_ascii_alphanumeric())
        && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn is_domain(v: &str) -> bool {
    let labels: Vec<&str> = v.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|l| is_label(l))
        && labels
            .last()
            .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))
}

fn is_subdomain(v: &str) -> bool {
    v.split('.').all(is_label)
}

/// `ssh-<type> <base64> [comment]`: only the characters a public key line has, which keeps
/// anything that could end a line or a value out of the file it is written to.
fn is_ssh_key(v: &str) -> bool {
    v.starts_with("ssh-")
        && !v.ends_with(' ')
        && v.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '+' | '/' | '=' | '-' | '_' | '@' | '.' | ':' | ' ')
        })
}

/// `scheme://user:password@host/...`: a secret in the address.
fn has_credentials(v: &str) -> bool {
    let Some((_, rest)) = v.split_once("://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    // The last `@` ends the user info, as in Go's URL parser, so an `@` inside the user
    // name cannot hide the `:` before the password.
    authority
        .rsplit_once('@')
        .is_some_and(|(userinfo, _)| userinfo.contains(':'))
}

fn is_restic_repo(v: &str) -> bool {
    if has_credentials(v) {
        return false;
    }
    if v.starts_with('/') {
        return is_abs_path(v) && !v.contains(char::is_whitespace);
    }
    let Some((backend, rest)) = v.split_once(':') else {
        return false;
    };
    matches!(
        backend,
        "sftp" | "rest" | "s3" | "b2" | "azure" | "gs" | "rclone" | "swift"
    ) && !rest.is_empty()
        && rest.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '.' | '_' | '~' | ':' | '/' | '@' | '%' | '+' | '=' | ',' | '-'
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_answer_formats() {
        for ok in [
            "http://localhost:8080",
            "https://wishlist.example.org/a?b=c#d",
        ] {
            assert!(Format::Url.validate(ok).is_ok(), "{ok}");
        }
        for bad in [
            "ftp://x",
            "localhost",
            "http://",
            "http://a b",
            "http://a\"b",
            "http://a;b",
            "http://a$b",
            "http://a\\b",
        ] {
            assert!(Format::Url.validate(bad).is_err(), "{bad}");
        }

        assert!(Format::Ipv4.validate("192.168.1.100").is_ok());
        for bad in [
            "1.2.3",
            "1.2.3.4.5",
            "256.1.1.1",
            "1.2.3.x",
            "1..2.3",
            "01234.1.1.1",
        ] {
            assert!(Format::Ipv4.validate(bad).is_err(), "{bad}");
        }

        for ok in ["enp2s0", "eth0", "bond0", "br-lan", "eno1.100"] {
            assert!(Format::Interface.validate(ok).is_ok(), "{ok}");
        }
        for bad in ["e", "0eth", "a/b", "a b", "this-name-is-too-long"] {
            assert!(Format::Interface.validate(bad).is_err(), "{bad}");
        }

        assert!(Format::Email.validate("me+tag@mail.example.com").is_ok());
        for bad in [
            "me",
            "@example.com",
            "me@example",
            "me@@example.com",
            "me@exa mple.com",
        ] {
            assert!(Format::Email.validate(bad).is_err(), "{bad}");
        }

        assert!(Format::Domain.validate("example.com").is_ok());
        assert!(Format::Domain.validate("a-b.example.co.uk").is_ok());
        for bad in [
            "example",
            "-a.com",
            "a.c",
            "a..com",
            "http://a.com",
            "a.com/x",
        ] {
            assert!(Format::Domain.validate(bad).is_err(), "{bad}");
        }

        assert!(Format::Subdomain.validate("home").is_ok());
        assert!(Format::Subdomain.validate("vpn.lab").is_ok());
        for bad in ["-a", "a.", ".a", "a_b", "a b"] {
            assert!(Format::Subdomain.validate(bad).is_err(), "{bad}");
        }

        for ok in [
            "/var/lib/vexos-backup/restic-repo",
            "sftp:user@host:/srv/restic",
            "rest:https://host:8000/repo",
            "s3:s3.amazonaws.com/bucket",
        ] {
            assert!(Format::ResticRepo.validate(ok).is_ok(), "{ok}");
        }
        for bad in [
            "relative/repo",
            "ftp:host/x",
            "sftp:",
            "/a/../b",
            "/has space",
            "sftp:host:/a b",
            "sftp:host:/a;b",
        ] {
            assert!(Format::ResticRepo.validate(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_repository_address_cannot_carry_a_password_and_the_error_does_not_repeat_it() {
        let err = Format::ResticRepo
            .validate("rest:https://user:hunter2@host:8000/repo")
            .unwrap_err();
        assert!(!err.to_string().contains("hunter2"), "{err}");
        assert!(!format!("{err:?}").contains("hunter2"), "{err:?}");
        let err = Format::ResticRepo
            .validate("rest:https://backup@example.com:hunter2@host/repo")
            .unwrap_err();
        assert!(!err.to_string().contains("hunter2"), "{err}");
        // A user name alone is fine, and so is sftp's `user@host`.
        assert!(Format::ResticRepo
            .validate("rest:https://user@host:8000/repo")
            .is_ok());
        assert!(Format::ResticRepo.validate("sftp:me@nas:/backups").is_ok());
    }

    #[test]
    fn ssh_keys() {
        assert!(Format::SshKey
            .validate("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIabc+/= hub@host")
            .is_ok());
        for bad in [
            "AAAA",
            "ssh-ed25519 AAAA\"x",
            "ssh-ed25519 AAAA;x",
            "ssh-rsa A\tB",
            "ssh-rsa A ",
        ] {
            assert!(Format::SshKey.validate(bad).is_err(), "{bad}");
        }
    }

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
    fn job_names_keep_the_case_the_host_gave_them() {
        for ok in ["tv", "Media-Mirror", "a.b_c-1", "4k"] {
            assert!(Format::JobName.validate(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-tv", ".tv", "a b", "tv/x", "tv;x", &"a".repeat(65)] {
            assert!(Format::JobName.validate(bad).is_err(), "{bad}");
        }
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
                Format::ProgramName,
                Format::Url,
                Format::Ipv4,
                Format::Interface,
                Format::Email,
                Format::Domain,
                Format::Subdomain,
                Format::ResticRepo,
                Format::SshKey,
                Format::JobName,
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

    #[test]
    fn program_names() {
        assert!(Format::ProgramName.validate("firefox").is_ok());
        assert!(Format::ProgramName.validate("Xwayland").is_ok());
        assert!(Format::ProgramName.validate("gnome-shell").is_ok());
        assert!(Format::ProgramName.validate(".").is_err());
        assert!(Format::ProgramName.validate("..").is_err());
        assert!(Format::ProgramName.validate("a/b").is_err());
    }
}
