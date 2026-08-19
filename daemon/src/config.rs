//! Where the daemon looks for the justfile, and what it puts in a recipe's environment.

use std::path::{Path, PathBuf};

/// A built VexOS host keeps the justfile alongside its flake wrapper. This is the only
/// justfile the daemon will ever run: the path comes from the unit file, not from the
/// D-Bus caller, so an unprivileged client cannot point it at a justfile of its own.
pub const DEFAULT_JUSTFILE: &str = "/etc/nixos/justfile";

#[derive(Debug, Clone)]
pub struct Config {
    pub justfile: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            justfile: PathBuf::from(DEFAULT_JUSTFILE),
        }
    }
}

impl Config {
    /// Parse `--justfile <path>`. Only root can set the daemon's argv (it comes from
    /// the systemd unit), which is why this is an argument rather than an environment
    /// variable a wider set of processes could influence.
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut config = Config::default();
        let mut args = args.into_iter().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--justfile" => {
                    let path = args
                        .next()
                        .ok_or_else(|| "--justfile needs a path".to_string())?;
                    config.justfile = PathBuf::from(path);
                }
                other => return Err(format!("unknown argument `{other}`")),
            }
        }
        Ok(config)
    }

    /// Recipes are run from the directory holding the justfile, matching what a user
    /// gets from `cd /etc/nixos && just …`.
    pub fn working_directory(&self) -> &Path {
        self.justfile.parent().unwrap_or(Path::new("/"))
    }
}

/// The environment a recipe runs in.
///
/// Built from nothing rather than inherited: the daemon's own environment comes from
/// systemd and has no business leaking into a recipe, and a fixed PATH means a recipe
/// resolves `nix`, `sudo` and `bash` the same way every time.
pub fn recipe_environment() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "PATH",
            "/run/wrappers/bin:/run/current-system/sw/bin:/usr/bin:/bin",
        ),
        // Recipes that would otherwise stop at a [y/N] prompt take the yes branch.
        // Until vexos-nix honours this, those recipes see EOF on stdin and fall back
        // to their default answer, which is "no" for every confirmation in the file.
        ("VEXOS_ASSUME_YES", "1"),
        // Lets a recipe tell it is being driven by the portal rather than a terminal.
        ("VEXPORTAL", "1"),
        ("HOME", "/root"),
        ("LANG", "C.UTF-8"),
        // No TTY, so colour escapes would only end up as literal noise in the log.
        ("NO_COLOR", "1"),
        ("TERM", "dumb"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_the_installed_justfile() {
        let config = Config::default();
        assert_eq!(config.justfile, Path::new(DEFAULT_JUSTFILE));
        assert_eq!(config.working_directory(), Path::new("/etc/nixos"));
    }

    #[test]
    fn accepts_a_justfile_override() {
        let config =
            Config::from_args(["vexportal-daemon".into(), "--justfile".into(), "/tmp/j".into()])
                .unwrap();
        assert_eq!(config.justfile, Path::new("/tmp/j"));
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(Config::from_args(["vexportal-daemon".into(), "--root".into()]).is_err());
    }
}
