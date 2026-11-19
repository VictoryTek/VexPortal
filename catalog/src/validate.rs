//! Turning user answers into an argv that is allowed to run.
//!
//! This is the allowlist. The daemon runs [`build`] on every request and executes
//! nothing else: an action id that is not in the catalog has no path to `exec`, and
//! neither does a parameter value that fails its format. The GUI's built-in terminal
//! runs [`build_for_terminal`], the same checks for the actions that run as the user.

use crate::format::FormatError;
use crate::{Action, Backend, Catalog, Mode, Param, Risk, Widget};
use std::collections::BTreeMap;

/// A secret answer. `Debug` hides it, so a stray `{:?}` in a log line or a panic message
/// cannot leak it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// A validated request, ready to hand to `just` or to a native operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The action id, guaranteed to exist in the catalog.
    pub action: String,
    /// The argv prefix after `just`, from the catalog.
    pub command: Vec<String>,
    /// Positional arguments, trailing empties trimmed.
    pub argv: Vec<String>,
    /// Written to the child's stdin rather than argv, so it stays out of `ps`,
    /// the journal, and the audit record.
    pub stdin: Option<Secret>,
    pub risk: Risk,
    /// Absolute paths the daemon should confirm exist before starting.
    pub must_exist: Vec<String>,
    pub backend: Backend,
    /// Every validated answer that is not a secret, by parameter name. Empty optional
    /// answers are left out. This is what a native operation reads; `argv` is the same
    /// information positioned for `just`.
    pub named: BTreeMap<String, String>,
    /// Secret answers by parameter name, for a native operation.
    pub secrets: BTreeMap<String, Secret>,
}

impl Invocation {
    /// The full `just` argument vector, for logging and for `Command::args`.
    pub fn just_args(&self) -> Vec<String> {
        let mut args = self.command.clone();
        args.extend(self.argv.iter().cloned());
        args
    }

    /// A redacted, human-readable rendering for the audit log.
    pub fn audit_line(&self) -> String {
        if self.backend == Backend::Native {
            let mut line = format!("op {}", self.action);
            for (name, value) in &self.named {
                line.push_str(&format!(" {name}={value}"));
            }
            for name in self.secrets.keys() {
                line.push_str(&format!(" {name}=<redacted>"));
            }
            return line;
        }
        let mut line = format!("just {}", self.command.join(" "));
        for arg in &self.argv {
            line.push(' ');
            line.push_str(if arg.is_empty() { "''" } else { arg });
        }
        if self.stdin.is_some() {
            line.push_str(" <secret-on-stdin>");
        }
        line
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("`{0}` is not an action VexPortal knows about")]
    UnknownAction(String),
    #[error("`{0}` runs interactively and can only be started in a terminal")]
    TerminalOnly(String),
    #[error("`{0}` runs as root through the VexPortal daemon, not in a terminal")]
    DaemonOnly(String),
    #[error("`{action}` has no parameter named `{param}`")]
    UnknownParam { action: String, param: String },
    #[error("`{label}` is required")]
    Missing { label: String },
    #[error("`{label}`: {source}")]
    BadValue {
        label: String,
        #[source]
        source: FormatError,
    },
    #[error("`{label}`: `{got}` is not one of the accepted values")]
    NotAChoice { label: String, got: String },
    #[error("`{label}`: type `{expected}` exactly to confirm")]
    ConfirmMismatch { label: String, expected: String },
}

/// Validate `answers` for an action the daemon runs, and produce an [`Invocation`].
///
/// `answers` is keyed by parameter name. Missing optional parameters fall back to the
/// catalog default and then to empty, which makes `just` apply the recipe's own
/// default — the same thing that happens when a user omits the argument on the CLI.
pub fn build(
    catalog: &Catalog,
    action_id: &str,
    answers: &BTreeMap<String, String>,
) -> Result<Invocation, ValidationError> {
    let action = lookup(catalog, action_id)?;
    if action.mode != Mode::Daemon {
        return Err(ValidationError::TerminalOnly(action.id.clone()));
    }
    invocation(action, answers)
}

/// Validate `answers` for an action that runs as the user in the GUI's terminal.
///
/// Refuses daemon actions, so nothing meant to run behind polkit can be started
/// unprivileged by mistake — they would fail half-way at their first `sudo`.
pub fn build_for_terminal(
    catalog: &Catalog,
    action_id: &str,
    answers: &BTreeMap<String, String>,
) -> Result<Invocation, ValidationError> {
    let action = lookup(catalog, action_id)?;
    if action.mode != Mode::Terminal {
        return Err(ValidationError::DaemonOnly(action.id.clone()));
    }
    invocation(action, answers)
}

fn lookup<'a>(catalog: &'a Catalog, action_id: &str) -> Result<&'a Action, ValidationError> {
    catalog
        .action(action_id)
        .ok_or_else(|| ValidationError::UnknownAction(action_id.to_string()))
}

fn invocation(
    action: &Action,
    answers: &BTreeMap<String, String>,
) -> Result<Invocation, ValidationError> {
    // Reject unknown keys rather than ignoring them: a typo in a parameter name would
    // otherwise silently run the action with its default.
    for key in answers.keys() {
        if action.param(key).is_none() {
            return Err(ValidationError::UnknownParam {
                action: action.id.clone(),
                param: key.clone(),
            });
        }
    }

    let mut argv: Vec<String> = Vec::new();
    let mut stdin: Option<Secret> = None;
    let mut must_exist: Vec<String> = Vec::new();
    let mut named: BTreeMap<String, String> = BTreeMap::new();
    let mut secrets: BTreeMap<String, Secret> = BTreeMap::new();

    for param in &action.params {
        let value = answers
            .get(&param.name)
            .map(String::as_str)
            .or(param.default.as_deref())
            .unwrap_or("")
            .to_string();

        if param.is_secret() {
            if value.is_empty() && param.required {
                return Err(ValidationError::Missing {
                    label: param.label.clone(),
                });
            }
            if !value.is_empty() {
                secrets.insert(param.name.clone(), Secret(value.clone()));
                stdin = Some(Secret(value));
            }
            continue;
        }

        if value.is_empty() {
            if param.required {
                return Err(ValidationError::Missing {
                    label: param.label.clone(),
                });
            }
            argv.push(String::new());
            continue;
        }

        if param.confirm().is_none() {
            check_value(param, &value)?;
        }
        if let Widget::Path { must_exist: true } = param.widget {
            must_exist.push(value.clone());
        }
        named.insert(param.name.clone(), value.clone());
        argv.push(value);
    }

    // A typed confirmation is checked here, in the daemon's own validation, so a client
    // that never asked the person cannot skip it. It compares against the answer to the
    // parameter it names, which is why this waits until every answer is read.
    for param in &action.params {
        let Some((expect, expect_param)) = param.confirm() else {
            continue;
        };
        let expected = match (expect, expect_param) {
            (Some(phrase), _) => phrase.to_string(),
            (None, Some(other)) => named.get(other).cloned().unwrap_or_default(),
            (None, None) => String::new(),
        };
        let typed = named.get(&param.name).map(String::as_str).unwrap_or("");
        if expected.is_empty() || typed != expected {
            return Err(ValidationError::ConfirmMismatch {
                label: param.label.clone(),
                expected,
            });
        }
    }

    // `just` applies a recipe's own defaults for arguments that are simply absent, so
    // trailing empties are noise. Interior empties must stay: they hold the position
    // of a later argument that does have a value.
    while argv.last().is_some_and(String::is_empty) {
        argv.pop();
    }

    Ok(Invocation {
        action: action.id.clone(),
        command: action.command.clone(),
        argv,
        stdin,
        risk: action.risk,
        must_exist,
        backend: action.backend,
        named,
        secrets,
    })
}

fn check_value(param: &Param, value: &str) -> Result<(), ValidationError> {
    let bad_value = |source| ValidationError::BadValue {
        label: param.label.clone(),
        source,
    };

    match &param.widget {
        Widget::Choice { choices } => {
            if !choices.iter().any(|c| c == value) {
                return Err(ValidationError::NotAChoice {
                    label: param.label.clone(),
                    got: value.to_string(),
                });
            }
        }
        Widget::ChoiceDynamic { extra, .. } => {
            // The dynamic list lives in the justfile, which the daemon deliberately
            // does not parse — a slug check plus the recipe's own "unknown service"
            // handling is enough, and keeps the daemon from trusting client input
            // about what the list contains.
            if !extra.iter().any(|e| e == value) {
                crate::Format::Slug.validate(value).map_err(bad_value)?;
            }
        }
        Widget::Text { format } => format.validate(value).map_err(bad_value)?,
        Widget::Path { .. } => crate::Format::AbsPath.validate(value).map_err(bad_value)?,
        Widget::Secret => unreachable!("secrets are handled before check_value"),
        Widget::Confirm { .. } => unreachable!("confirmations are checked after every answer"),
    }
    Ok(())
}

/// Every action the daemon may be asked to run, for the daemon's startup log.
pub fn runnable_actions(catalog: &Catalog) -> Vec<&Action> {
    catalog
        .actions
        .iter()
        .filter(|a| a.mode == Mode::Daemon)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn catalog() -> Catalog {
        Catalog::load().unwrap()
    }

    #[test]
    fn rejects_actions_outside_the_catalog() {
        let err = build(&catalog(), "definitely-not-an-action", &answers(&[])).unwrap_err();
        assert!(matches!(err, ValidationError::UnknownAction(_)));
    }

    #[test]
    fn rejects_a_shell_injection_attempt_in_every_parameter() {
        let catalog = catalog();
        for action in &catalog.actions {
            for param in &action.params {
                if param.is_secret() {
                    continue;
                }
                let attempt = answers(&[(param.name.as_str(), "; rm -rf / #")]);
                let result = match action.mode {
                    Mode::Daemon => build(&catalog, &action.id, &attempt),
                    Mode::Terminal => build_for_terminal(&catalog, &action.id, &attempt),
                };
                assert!(
                    result.is_err(),
                    "`{}` accepted an injection attempt in `{}`",
                    action.id,
                    param.name
                );
            }
        }
    }

    #[test]
    fn rejects_unknown_parameter_names() {
        let err = build(&catalog(), "rebuild", &answers(&[("wat", "1")])).unwrap_err();
        assert!(matches!(err, ValidationError::UnknownParam { .. }));
    }

    fn confirm_catalog() -> Catalog {
        Catalog::parse(
            "[[page]]\nid = \"p\"\ntitle = \"P\"\nicon = \"i\"\n\n\
             [[action]]\nid = \"wipe\"\nbackend = \"native\"\ntitle = \"W\"\nblurb = \"b\"\n\
             icon = \"i\"\npage = \"p\"\ngroup = \"g\"\nroles = [\"server\"]\n\
             risk = \"destructive\"\nconfirm = \"Sure?\"\n\
             [[action.params]]\nname = \"pool\"\nlabel = \"Pool\"\nrequired = true\nwidget = \"text\"\n\
             [[action.params]]\nname = \"typed\"\nlabel = \"Confirm\"\nrequired = true\n\
             widget = \"confirm\"\nexpect_param = \"pool\"\n\
             [[action.params]]\nname = \"word\"\nlabel = \"Word\"\nrequired = true\n\
             widget = \"confirm\"\nexpect = \"erase\"\n",
        )
        .unwrap()
    }

    #[test]
    fn a_destructive_request_is_refused_unless_the_phrase_was_typed() {
        let catalog = confirm_catalog();
        let ok = answers(&[("pool", "tank"), ("typed", "tank"), ("word", "erase")]);
        assert!(build(&catalog, "wipe", &ok).is_ok());

        for bad in [
            // Typed the wrong pool.
            answers(&[("pool", "tank"), ("typed", "other"), ("word", "erase")]),
            // Close is not enough: no trimming, no case folding.
            answers(&[("pool", "tank"), ("typed", "tank "), ("word", "erase")]),
            answers(&[("pool", "tank"), ("typed", "tank"), ("word", "Erase")]),
            // A client that never asked leaves it out.
            answers(&[("pool", "tank")]),
            answers(&[("pool", "tank"), ("typed", "tank")]),
        ] {
            let err = build(&catalog, "wipe", &bad).unwrap_err();
            assert!(
                matches!(
                    err,
                    ValidationError::ConfirmMismatch { .. } | ValidationError::Missing { .. }
                ),
                "{err:?}"
            );
        }
    }

    #[test]
    fn the_typed_phrase_is_part_of_the_audit_line() {
        let inv = build(
            &confirm_catalog(),
            "wipe",
            &answers(&[("pool", "tank"), ("typed", "tank"), ("word", "erase")]),
        )
        .unwrap();
        assert_eq!(inv.audit_line(), "op wipe pool=tank typed=tank word=erase");
    }

    #[test]
    fn menu_actions_prefix_their_command() {
        let inv = build(&catalog(), "cache-push", &answers(&[("cache", "vexos")])).unwrap();
        assert_eq!(inv.just_args(), ["cache", "push", "vexos"]);
        assert_eq!(inv.audit_line(), "just cache push vexos");
    }

    /// A recipe-backed action with three parameters, the middle one optional.
    fn recipe_catalog() -> Catalog {
        Catalog::parse(
            "[[page]]
id = \"p\"
title = \"P\"
icon = \"i\"

\n             [[action]]
id = \"r\"
command = [\"r\"]
implements = \"r\"
title = \"R\"
blurb = \"b\"
\n             icon = \"i\"
page = \"p\"
group = \"g\"
roles = [\"desktop\"]
risk = \"safe\"
\n             [[action.params]]
name = \"a\"
label = \"A\"
required = true
widget = \"text\"
\n             [[action.params]]
name = \"b\"
label = \"B\"
widget = \"text\"
\n             [[action.params]]
name = \"c\"
label = \"C\"
widget = \"text\"
",
        )
        .unwrap()
    }

    #[test]
    fn a_recipe_action_builds_a_positional_argv() {
        let inv = build(&recipe_catalog(), "r", &answers(&[("a", "one")])).unwrap();
        assert_eq!(inv.just_args(), ["r", "one"]);
        // The trailing optionals are dropped so `just` applies its own defaults.
        assert_eq!(inv.argv.len(), 1);
    }

    #[test]
    fn interior_optionals_keep_their_position() {
        let inv = build(
            &recipe_catalog(),
            "r",
            &answers(&[("a", "one"), ("c", "three")]),
        )
        .unwrap();
        assert_eq!(inv.just_args(), ["r", "one", "", "three"]);
    }

    #[test]
    fn a_native_action_reads_its_answers_by_name() {
        let inv = build(
            &catalog(),
            "switch",
            &answers(&[("role", "desktop"), ("variant", "nvidia"), ("de", "cosmic")]),
        )
        .unwrap();
        assert_eq!(inv.backend, Backend::Native);
        assert_eq!(inv.named["de"], "cosmic");
        assert_eq!(
            inv.audit_line(),
            "op switch de=cosmic role=desktop variant=nvidia"
        );
    }

    #[test]
    fn the_legacy_nvidia_driver_is_580() {
        build(
            &catalog(),
            "switch",
            &answers(&[("role", "desktop"), ("variant", "nvidia-legacy580")]),
        )
        .unwrap();
    }

    #[test]
    fn required_parameters_are_enforced() {
        let err = build(&catalog(), "build", &answers(&[("role", "desktop")])).unwrap_err();
        assert!(matches!(err, ValidationError::Missing { .. }));
    }

    #[test]
    fn choices_are_closed() {
        let err = build(
            &catalog(),
            "switch",
            &answers(&[("role", "toaster"), ("variant", "amd")]),
        )
        .unwrap_err();
        assert!(matches!(err, ValidationError::NotAChoice { .. }));
    }

    #[test]
    fn terminal_actions_cannot_be_run_by_the_daemon() {
        let err = build(&catalog(), "zfs-pool", &answers(&[])).unwrap_err();
        assert!(matches!(err, ValidationError::TerminalOnly(_)));
    }

    #[test]
    fn daemon_actions_cannot_be_run_in_the_terminal() {
        let err = build_for_terminal(&catalog(), "rebuild", &answers(&[])).unwrap_err();
        assert!(matches!(err, ValidationError::DaemonOnly(_)));
    }

    #[test]
    fn terminal_actions_validate_their_parameters_too() {
        let inv = build_for_terminal(
            &catalog(),
            "ssh",
            &answers(&[("target", "nimda@10.35.1.50")]),
        )
        .unwrap();
        assert_eq!(inv.just_args(), ["ssh", "nimda@10.35.1.50"]);
        assert!(build_for_terminal(&catalog(), "ssh", &answers(&[])).is_err());
        assert!(build_for_terminal(&catalog(), "ssh", &answers(&[("target", "; id")])).is_err());
    }

    #[test]
    fn secrets_go_to_stdin_and_stay_out_of_the_audit_line() {
        // No shipped action takes a secret today, but the daemon keeps the stdin path,
        // so it is exercised against a catalog of its own.
        let catalog = Catalog::parse(
            "[[page]]\nid = \"p\"\ntitle = \"P\"\nicon = \"i\"\n\n\
             [[action]]\nid = \"set-password\"\ncommand = [\"set-password\"]\n\
             implements = \"set-password\"\ntitle = \"T\"\nblurb = \"b\"\nicon = \"i\"\n\
             page = \"p\"\ngroup = \"g\"\nroles = [\"desktop\"]\nrisk = \"medium\"\n\
             [[action.params]]\nname = \"password\"\nlabel = \"Password\"\n\
             required = true\nwidget = \"secret\"\n",
        )
        .unwrap();
        let inv = build(
            &catalog,
            "set-password",
            &answers(&[("password", "hunter2")]),
        )
        .unwrap();
        assert_eq!(inv.stdin.as_ref().map(Secret::expose), Some("hunter2"));
        assert!(!inv.argv.iter().any(|a| a.contains("hunter2")));
        assert!(!inv.audit_line().contains("hunter2"));
    }

    #[test]
    fn answers_are_available_by_name_and_secrets_stay_apart() {
        let catalog = Catalog::parse(
            "[[page]]\nid = \"p\"\ntitle = \"P\"\nicon = \"i\"\n\n\
             [[action]]\nid = \"login\"\ncommand = [\"login\"]\nbackend = \"native\"\n\
             implements = \"login\"\ntitle = \"T\"\nblurb = \"b\"\nicon = \"i\"\n\
             page = \"p\"\ngroup = \"g\"\nroles = [\"desktop\"]\nrisk = \"medium\"\n\
             [[action.params]]\nname = \"user\"\nlabel = \"User\"\nrequired = true\nwidget = \"text\"\n\
             [[action.params]]\nname = \"password\"\nlabel = \"Password\"\n\
             required = true\nwidget = \"secret\"\n",
        )
        .unwrap();
        let inv = build(
            &catalog,
            "login",
            &answers(&[("user", "bob"), ("password", "hunter2")]),
        )
        .unwrap();
        assert_eq!(inv.named.get("user").map(String::as_str), Some("bob"));
        assert!(!inv.named.contains_key("password"));
        assert_eq!(inv.secrets["password"].expose(), "hunter2");
        // Neither a debug dump nor the audit line can carry the secret.
        assert!(!format!("{inv:?}").contains("hunter2"));
        assert_eq!(inv.audit_line(), "op login user=bob password=<redacted>");
    }

    #[test]
    fn recipe_actions_still_audit_as_just_commands() {
        let inv = build(&catalog(), "cache-push", &answers(&[("cache", "vexos")])).unwrap();
        assert_eq!(inv.backend, Backend::Recipe);
        assert!(inv.audit_line().starts_with("just "));
    }

    #[test]
    fn defaults_come_from_the_catalog() {
        let inv = build(&catalog(), "cache-push", &answers(&[])).unwrap();
        assert_eq!(inv.just_args(), ["cache", "push", "vexos"]);
    }

    #[test]
    fn paths_that_must_exist_are_reported_to_the_daemon() {
        let inv = build(
            &catalog(),
            "restore-plex",
            &answers(&[("tarball", "/tmp/plex.tar.gz"), ("confirm", "plex")]),
        )
        .unwrap();
        assert_eq!(inv.must_exist, ["/tmp/plex.tar.gz"]);
    }

    #[test]
    fn every_destructive_native_action_that_deletes_data_asks_for_a_typed_phrase() {
        // These are refused by the daemon unless the phrase is typed, whatever the GUI did.
        let catalog = catalog();
        for id in ["restore-plex", "restore-service", "bootloader-cleanup"] {
            let action = catalog.action(id).unwrap();
            assert!(
                action.params.iter().any(|p| p.confirm().is_some()),
                "`{id}` has no typed confirmation"
            );
        }
        let err = build(
            &catalog,
            "restore-plex",
            &answers(&[("tarball", "/tmp/p.tgz")]),
        );
        assert!(err.is_err());
        let err = build(
            &catalog,
            "restore-service",
            &answers(&[("name", "jellyfin"), ("confirm", "plex")]),
        );
        assert!(matches!(
            err.unwrap_err(),
            ValidationError::ConfirmMismatch { .. }
        ));
        build(
            &catalog,
            "restore-service",
            &answers(&[("name", "jellyfin"), ("confirm", "jellyfin")]),
        )
        .unwrap();
    }
}
