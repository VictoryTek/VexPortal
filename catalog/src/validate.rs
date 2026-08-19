//! Turning user answers into an argv the daemon is willing to execute.
//!
//! This is the allowlist. The daemon runs [`build`] on every request and executes
//! nothing else: a recipe name that is not in the catalog has no path to `exec`, and
//! neither does a parameter value that fails its format.

use crate::format::FormatError;
use crate::{Catalog, Param, Recipe, Risk, Widget};
use std::collections::BTreeMap;

/// A validated request, ready to hand to `just`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The recipe name, guaranteed to exist in the catalog.
    pub recipe: String,
    /// Positional arguments, trailing empties trimmed.
    pub argv: Vec<String>,
    /// Written to the child's stdin rather than argv, so it stays out of `ps`,
    /// the journal, and the audit record.
    pub stdin: Option<String>,
    pub risk: Risk,
    /// Absolute paths the daemon should confirm exist before starting.
    pub must_exist: Vec<String>,
}

impl Invocation {
    /// The full `just` argument vector, for logging and for `Command::args`.
    pub fn just_args(&self) -> Vec<String> {
        let mut args = vec![self.recipe.clone()];
        args.extend(self.argv.iter().cloned());
        args
    }

    /// A redacted, human-readable rendering for the audit log.
    pub fn audit_line(&self) -> String {
        let mut line = format!("just {}", self.recipe);
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
    #[error("`{0}` is not a recipe VexPortal knows about")]
    UnknownRecipe(String),
    #[error("`{0}` runs interactively and can only be started in a terminal")]
    TerminalOnly(String),
    #[error("`{recipe}` has no parameter named `{param}`")]
    UnknownParam { recipe: String, param: String },
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
}

/// Validate `answers` against the catalog and produce an [`Invocation`].
///
/// `answers` is keyed by parameter name. Missing optional parameters fall back to the
/// catalog default and then to empty, which makes `just` apply the recipe's own
/// default — the same thing that happens when a user omits the argument on the CLI.
pub fn build(
    catalog: &Catalog,
    recipe_name: &str,
    answers: &BTreeMap<String, String>,
) -> Result<Invocation, ValidationError> {
    let recipe = catalog
        .recipe(recipe_name)
        .ok_or_else(|| ValidationError::UnknownRecipe(recipe_name.to_string()))?;

    if recipe.terminal {
        return Err(ValidationError::TerminalOnly(recipe.name.clone()));
    }

    // Reject unknown keys rather than ignoring them: a typo in a parameter name would
    // otherwise silently run the recipe with its default.
    for key in answers.keys() {
        if recipe.param(key).is_none() {
            return Err(ValidationError::UnknownParam {
                recipe: recipe.name.clone(),
                param: key.clone(),
            });
        }
    }

    let mut argv: Vec<String> = Vec::new();
    let mut stdin: Option<String> = None;
    let mut must_exist: Vec<String> = Vec::new();

    for param in &recipe.params {
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
                stdin = Some(value);
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

        check_value(param, &value)?;
        if let Widget::Path { must_exist: true } = param.widget {
            must_exist.push(value.clone());
        }
        argv.push(value);
    }

    // `just` applies a recipe's own defaults for arguments that are simply absent, so
    // trailing empties are noise. Interior empties must stay: they hold the position
    // of a later argument that does have a value.
    while argv.last().is_some_and(String::is_empty) {
        argv.pop();
    }

    Ok(Invocation {
        recipe: recipe.name.clone(),
        argv,
        stdin,
        risk: recipe.risk,
        must_exist,
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
    }
    Ok(())
}

/// Every recipe the daemon may be asked to run, for the D-Bus introspection surface
/// and for the daemon's own startup log.
pub fn runnable_recipes(catalog: &Catalog) -> Vec<&Recipe> {
    catalog.recipes.iter().filter(|r| !r.terminal).collect()
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
    fn rejects_recipes_outside_the_catalog() {
        let err = build(&catalog(), "definitely-not-a-recipe", &answers(&[])).unwrap_err();
        assert!(matches!(err, ValidationError::UnknownRecipe(_)));
    }

    #[test]
    fn rejects_a_shell_injection_attempt_in_every_parameter() {
        let catalog = catalog();
        for recipe in super::runnable_recipes(&catalog) {
            for param in &recipe.params {
                if param.is_secret() {
                    continue;
                }
                let attempt = answers(&[(param.name.as_str(), "; rm -rf / #")]);
                assert!(
                    build(&catalog, &recipe.name, &attempt).is_err(),
                    "`{}` accepted an injection attempt in `{}`",
                    recipe.name,
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

    #[test]
    fn switch_builds_a_positional_argv() {
        let inv = build(
            &catalog(),
            "switch",
            &answers(&[("role", "desktop"), ("variant", "nvidia")]),
        )
        .unwrap();
        assert_eq!(inv.just_args(), ["switch", "desktop", "nvidia"]);
        // The trailing optional `flake` is dropped so `just` applies its own default.
        assert_eq!(inv.argv.len(), 2);
    }

    #[test]
    fn interior_optionals_keep_their_position() {
        let inv = build(
            &catalog(),
            "switch",
            &answers(&[("role", "desktop"), ("variant", "amd"), ("flake", ".")]),
        )
        .unwrap();
        assert_eq!(inv.just_args(), ["switch", "desktop", "amd", "."]);
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
    fn terminal_recipes_cannot_be_run_by_the_daemon() {
        let catalog = catalog();
        let terminal = catalog
            .recipes
            .iter()
            .find(|r| r.terminal)
            .expect("the catalog should mark the storage wizards terminal-only");
        let err = build(&catalog, &terminal.name, &answers(&[])).unwrap_err();
        assert!(matches!(err, ValidationError::TerminalOnly(_)));
    }

    #[test]
    fn secrets_go_to_stdin_and_stay_out_of_the_audit_line() {
        let catalog = catalog();
        let recipe = catalog
            .recipes
            .iter()
            .find(|r| r.params.iter().any(Param::is_secret))
            .expect("setup-rdp should take a secret");
        let secret = recipe.params.iter().find(|p| p.is_secret()).unwrap();
        let inv = build(
            &catalog,
            &recipe.name,
            &answers(&[(secret.name.as_str(), "hunter2")]),
        )
        .unwrap();
        assert_eq!(inv.stdin.as_deref(), Some("hunter2"));
        assert!(!inv.argv.iter().any(|a| a.contains("hunter2")));
        assert!(!inv.audit_line().contains("hunter2"));
    }

    #[test]
    fn defaults_come_from_the_catalog() {
        let inv = build(&catalog(), "attic-push", &answers(&[])).unwrap();
        assert_eq!(inv.just_args(), ["attic-push", "vexos"]);
    }

    #[test]
    fn paths_that_must_exist_are_reported_to_the_daemon() {
        let inv = build(
            &catalog(),
            "restore-plex",
            &answers(&[("tarball", "/tmp/plex.tar.gz")]),
        )
        .unwrap();
        assert_eq!(inv.must_exist, ["/tmp/plex.tar.gz"]);
    }
}
