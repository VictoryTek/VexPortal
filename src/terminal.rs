//! The argv for actions that run in a terminal.
//!
//! These actions run as the logged-in user — exactly what typing the command in a
//! terminal would do — so they need no privilege the user does not already have; any
//! root step goes through the recipe's own `sudo`, which asks in the terminal. The
//! argv is built from an [`Invocation`] the catalog has validated and is exec'd
//! directly: nothing here is ever handed to a shell.

use crate::just::JUSTFILE;
use crate::system::ai;
use gtk::gdk;
use vexportal_catalog::validate::Invocation;
use vexportal_catalog::Backend;

/// Where the command runs, matching `cd /etc/nixos && …`.
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

/// What the terminal runs. A native action is the AI assistant's, which is the
/// `vexos-ai` command itself with no recipe in between; anything else is a recipe. `None`
/// for a native action that has no command, which only a catalog mistake can cause.
pub fn argv(invocation: &Invocation) -> Option<Vec<String>> {
    if invocation.backend == Backend::Native {
        return ai::argv(&invocation.action, &invocation.named);
    }
    Some(just_argv(invocation))
}

/// What the terminal's child gets on top of the user's own environment.
pub fn environment() -> Vec<&'static str> {
    vec!["VEXPORTAL=1"]
}

/// What a `vexos-ai` exit code means, for the assistant's own actions.
pub fn ai_exit_message(action: &str, code: i32) -> Option<String> {
    if !ai::is_assistant_action(action) {
        return None;
    }
    ai::exit_message(action, "The assistant", code)
}

/// The same command in the user's preferred terminal emulator, through the XDG
/// terminal launcher — used only if the built-in terminal cannot start.
pub fn external_argv(title: &str, command: &[String]) -> Vec<String> {
    let mut argv = vec![
        "xdg-terminal-exec".to_string(),
        format!("--dir={WORKING_DIRECTORY}"),
        format!("--title={title}"),
        // Keep the window open after the command ends so its last lines can be read.
        "--hold".to_string(),
        "--".to_string(),
    ];
    argv.extend(command.iter().cloned());
    argv
}

/// A clipboard shortcut the terminal handles itself: VTE leaves these to the host.
#[derive(Debug, PartialEq, Eq)]
pub enum ClipboardKey {
    Copy,
    Paste,
}

/// Ctrl+Shift+C copies; Ctrl+Shift+V and Shift+Insert paste. Caps Lock and Num Lock
/// are ignored, any other modifier is not.
pub fn clipboard_key(key: gdk::Key, state: gdk::ModifierType) -> Option<ClipboardKey> {
    use gdk::ModifierType as M;
    let held =
        state & (M::CONTROL_MASK | M::SHIFT_MASK | M::ALT_MASK | M::SUPER_MASK | M::META_MASK);
    let ctrl_shift = M::CONTROL_MASK | M::SHIFT_MASK;
    match key {
        gdk::Key::C | gdk::Key::c if held == ctrl_shift => Some(ClipboardKey::Copy),
        gdk::Key::V | gdk::Key::v if held == ctrl_shift => Some(ClipboardKey::Paste),
        gdk::Key::Insert | gdk::Key::KP_Insert if held == M::SHIFT_MASK => {
            Some(ClipboardKey::Paste)
        }
        _ => None,
    }
}

/// A web address in the terminal's text (PCRE2), without trailing punctuation such as
/// the full stop that ends a sentence.
pub const URL_PATTERN: &str = r#"https?://[^\s<>"'`]*[^\s<>"'`.,;:!?)\]}]"#;

/// The link to open, if it is one. Links come from whatever runs in the terminal
/// (OSC 8 can name any URI), so only web addresses are handed to the desktop.
pub fn link_to_open(uri: &str) -> Option<&str> {
    let uri = uri.trim();
    let (scheme, rest) = uri.split_once("://")?;
    let web = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    (web && !rest.is_empty()).then_some(uri)
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
        let argv = just_argv(&invocation("ssh", &[("target", "nimda@10.35.1.50")]));
        assert_eq!(
            argv,
            [
                "just",
                "--justfile",
                "/etc/nixos/justfile",
                "--working-directory",
                "/etc/nixos",
                "ssh",
                "nimda@10.35.1.50"
            ]
        );
    }

    #[test]
    fn the_external_fallback_wraps_the_same_argv() {
        let inv = invocation("reset-defaults", &[]);
        let command = argv(&inv).unwrap();
        let external = external_argv("Reset Defaults", &command);
        assert_eq!(external[0], "xdg-terminal-exec");
        let split = external.iter().position(|a| a == "--").unwrap();
        assert_eq!(external[split + 1..], command[..]);
        assert!(
            !external.iter().any(|a| a == "-c" || a == "-lc"),
            "no shell"
        );
    }

    #[test]
    fn the_assistant_runs_vexos_ai_itself_and_never_just() {
        assert_eq!(argv(&invocation("agent", &[])).unwrap(), ["vexos-ai"]);
        assert_eq!(
            argv(&invocation("diagnose", &[("target", "sshd.service")])).unwrap(),
            ["vexos-ai", "diagnose", "sshd.service"]
        );
        assert_eq!(
            argv(&invocation("diagnose", &[])).unwrap(),
            ["vexos-ai", "diagnose"]
        );
        // A recipe action still goes through `just`.
        assert_eq!(argv(&invocation("reset-defaults", &[])).unwrap()[0], "just");
    }

    #[test]
    fn the_terminal_adds_only_its_own_marker_to_the_environment() {
        assert_eq!(environment(), ["VEXPORTAL=1"]);
    }

    #[test]
    fn ai_exit_codes_have_messages() {
        assert_eq!(ai_exit_message("agent", 0), None);
        assert_eq!(ai_exit_message("agent", 1), None);
        assert!(ai_exit_message("diagnose", 2)
            .unwrap()
            .contains("not valid"));
        assert!(ai_exit_message("agent", 3)
            .unwrap()
            .contains("not installed"));
        assert!(ai_exit_message("agent", 4)
            .unwrap()
            .contains("input that was not given"));
        assert_eq!(ai_exit_message("zfs-pool", 2), None);
    }

    #[test]
    fn clipboard_shortcuts() {
        use gdk::ModifierType as M;
        let ctrl_shift = M::CONTROL_MASK | M::SHIFT_MASK;
        assert_eq!(
            clipboard_key(gdk::Key::C, ctrl_shift),
            Some(ClipboardKey::Copy)
        );
        assert_eq!(
            clipboard_key(gdk::Key::c, ctrl_shift),
            Some(ClipboardKey::Copy)
        );
        assert_eq!(
            clipboard_key(gdk::Key::V, ctrl_shift),
            Some(ClipboardKey::Paste)
        );
        assert_eq!(
            clipboard_key(gdk::Key::Insert, M::SHIFT_MASK),
            Some(ClipboardKey::Paste)
        );
        // Caps Lock / Num Lock do not get in the way.
        assert_eq!(
            clipboard_key(gdk::Key::V, ctrl_shift | M::LOCK_MASK),
            Some(ClipboardKey::Paste)
        );
    }

    #[test]
    fn plain_control_keys_still_reach_the_program() {
        use gdk::ModifierType as M;
        // Ctrl+C must stay an interrupt and Ctrl+V a literal-next.
        assert_eq!(clipboard_key(gdk::Key::c, M::CONTROL_MASK), None);
        assert_eq!(clipboard_key(gdk::Key::v, M::CONTROL_MASK), None);
        assert_eq!(clipboard_key(gdk::Key::Insert, M::empty()), None);
        assert_eq!(
            clipboard_key(gdk::Key::V, M::CONTROL_MASK | M::SHIFT_MASK | M::ALT_MASK),
            None
        );
    }

    #[test]
    fn only_web_links_are_opened() {
        let url = "https://claude.ai/oauth/authorize?code=true&state=x";
        assert_eq!(link_to_open(url), Some(url));
        assert_eq!(
            link_to_open("HTTP://example.com "),
            Some("HTTP://example.com")
        );
        assert_eq!(link_to_open("file:///etc/shadow"), None);
        assert_eq!(link_to_open("javascript://alert(1)"), None);
        assert_eq!(link_to_open("https://"), None);
        assert_eq!(link_to_open("not a link"), None);
    }

    #[test]
    fn wait_statuses_become_exit_codes() {
        assert_eq!(exit_code(0), 0);
        assert_eq!(exit_code(3 << 8), 3);
        assert_eq!(exit_code(9), -1);
    }
}
