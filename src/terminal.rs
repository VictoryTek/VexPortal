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

/// What the terminal's child gets on top of the user's own environment.
/// `vexos-ai` is told never to open a dialog of its own over VexPortal: an empty
/// choice makes it exit 4 instead.
pub fn environment(invocation: &Invocation) -> Vec<&'static str> {
    let mut env = vec!["VEXPORTAL=1"];
    if invocation.action.starts_with("ai-") {
        env.push("VEXOS_AI_NONINTERACTIVE=1");
    }
    env
}

/// What a `vexos-ai` exit code means, for the `ai-*` actions.
pub fn ai_exit_message(action: &str, code: i32) -> Option<&'static str> {
    if !action.starts_with("ai-") {
        return None;
    }
    match code {
        2 => Some("That input was not valid."),
        3 => Some("Not found: no such account, or the assistant is not installed."),
        4 => Some("It needs input that was not given."),
        5 if action == "ai-account-remove" => Some("Close the running Claude session first."),
        5 => Some("Busy: a running Claude session is using that account."),
        _ => None,
    }
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
    fn only_ai_actions_run_vexos_ai_non_interactively() {
        let ai = environment(&invocation("ai-pick", &[("agent", "claude")]));
        assert!(ai.contains(&"VEXOS_AI_NONINTERACTIVE=1") && ai.contains(&"VEXPORTAL=1"));
        let other = environment(&invocation("zfs-pool", &[]));
        assert_eq!(other, ["VEXPORTAL=1"]);
    }

    #[test]
    fn ai_exit_codes_have_messages() {
        assert_eq!(ai_exit_message("ai-pick", 0), None);
        assert_eq!(ai_exit_message("ai-pick", 1), None);
        assert!(ai_exit_message("ai-pick", 2).unwrap().contains("not valid"));
        assert!(ai_exit_message("ai-account-use", 3)
            .unwrap()
            .contains("Not found"));
        assert!(ai_exit_message("ai-pick", 4)
            .unwrap()
            .contains("needs input"));
        assert_eq!(
            ai_exit_message("ai-account-remove", 5),
            Some("Close the running Claude session first.")
        );
        assert!(ai_exit_message("ai-account-use", 5)
            .unwrap()
            .contains("Busy"));
        assert_eq!(ai_exit_message("zfs-pool", 2), None);
    }

    #[test]
    fn wait_statuses_become_exit_codes() {
        assert_eq!(exit_code(0), 0);
        assert_eq!(exit_code(3 << 8), 3);
        assert_eq!(exit_code(9), -1);
    }
}
