//! The argv for actions that run in a terminal.
//!
//! These actions run as the logged-in user — exactly what typing the command in a
//! terminal would do — so they need no privilege the user does not already have; any
//! root step goes through the recipe's own `sudo`, which asks in the terminal. The
//! argv is built from an [`Invocation`] the catalog has validated and is exec'd
//! directly: nothing here is ever handed to a shell.

use crate::just::JUSTFILE;
use vexportal_catalog::validate::Invocation;

/// Where `just` runs, matching `cd /etc/nixos && just …`.
pub const WORKING_DIRECTORY: &str = "/etc/nixos";

/// `just --justfile /etc/nixos/justfile --working-directory /etc/nixos <command> <args>`.
pub fn just_argv(invocation: &Invocation) -> Vec<String> {
    let mut argv = vec![
        "just".to_string(),
        "--justfile".to_string(),
        JUSTFILE.to_string(),
        "--working-directory".to_string(),
        WORKING_DIRECTORY.to_string(),
    ];
    argv.extend(invocation.just_args());
    argv
}

/// The same command in the user's preferred terminal emulator, through the XDG
/// terminal launcher — used only if the built-in terminal cannot start.
pub fn external_argv(title: &str, invocation: &Invocation) -> Vec<String> {
    let mut argv = vec![
        "xdg-terminal-exec".to_string(),
        format!("--dir={WORKING_DIRECTORY}"),
        format!("--title={title}"),
        // Keep the window open after the recipe ends so its last lines can be read.
        "--hold".to_string(),
        "--".to_string(),
    ];
    argv.extend(just_argv(invocation));
    argv
}

/// Turn a `waitpid` status, as VTE reports it, into an exit code; a child killed by a
/// signal reads as -1, matching the daemon's convention.
pub fn exit_code(wait_status: i32) -> i32 {
    if wait_status & 0x7f == 0 {
        (wait_status >> 8) & 0xff
    } else {
        -1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use vexportal_catalog::validate::build_for_terminal;
    use vexportal_catalog::Catalog;

    fn invocation(action: &str, pairs: &[(&str, &str)]) -> Invocation {
        let answers: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        build_for_terminal(&Catalog::load().unwrap(), action, &answers).unwrap()
    }

    #[test]
    fn runs_just_directly_with_the_validated_arguments() {
        let argv = just_argv(&invocation("service-enable", &[("service", "plex")]));
        assert_eq!(
            argv,
            [
                "just",
                "--justfile",
                "/etc/nixos/justfile",
                "--working-directory",
                "/etc/nixos",
                "service",
                "enable",
                "plex"
            ]
        );
    }

    #[test]
    fn the_external_fallback_wraps_the_same_argv() {
        let inv = invocation("zfs-pool", &[]);
        let argv = external_argv("ZFS Pool Manager", &inv);
        assert_eq!(argv[0], "xdg-terminal-exec");
        let split = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(argv[split + 1..], just_argv(&inv)[..]);
        assert!(!argv.iter().any(|a| a == "-c" || a == "-lc"), "no shell");
    }

    #[test]
    fn wait_statuses_become_exit_codes() {
        assert_eq!(exit_code(0), 0);
        assert_eq!(exit_code(3 << 8), 3);
        assert_eq!(exit_code(9), -1);
    }
}
