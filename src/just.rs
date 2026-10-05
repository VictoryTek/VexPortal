//! Reading the justfile the daemon will run.
//!
//! `just --dump --dump-format json` needs no privileges — the justfile is
//! world-readable — so the GUI runs it directly rather than through the daemon. It
//! supplies the values behind the dynamic lists (`_feature_names`,
//! `_server_service_names`, `_service_catalog`) and the ground truth for the drift
//! check.

use std::collections::BTreeSet;
use std::process::Command;
use vexportal_catalog::drift::{compare, Drift, JustDump, ServiceEntry};
use vexportal_catalog::{Action, Catalog, DynamicSource};

pub const JUSTFILE: &str = "/etc/nixos/justfile";

/// What the GUI learned from the installed justfile.
#[derive(Debug, Default)]
pub struct JustfileFacts {
    pub features: Vec<String>,
    pub server_services: Vec<String>,
    /// Every server service module, grouped and described, from `_service_catalog`.
    pub service_catalog: Vec<ServiceEntry>,
    /// Recipe names this host's justfile actually defines.
    ///
    /// `/etc/nixos/justfile` is a copy taken by the last rebuild, so a host that has
    /// not rebuilt since vexos-nix gained a recipe will not have it. Offering a card
    /// for a recipe that is not there would fail at the daemon with a confusing
    /// error, so the GUI hides it instead.
    pub available: BTreeSet<String>,
    /// Ways the catalog and the justfile disagree. Non-empty puts a banner on the
    /// window rather than failing: an out-of-date catalog still runs everything it
    /// does know about.
    pub drift: Vec<Drift>,
    /// Set when `just` could not be run at all.
    pub error: Option<String>,
}

impl JustfileFacts {
    pub fn read(catalog: &Catalog) -> Self {
        let output = Command::new("just")
            .args([
                "--justfile",
                JUSTFILE,
                "--working-directory",
                "/etc/nixos",
                "--dump",
                "--dump-format",
                "json",
            ])
            .output();

        let output = match output {
            Ok(output) if output.status.success() => output,
            Ok(output) => {
                return Self {
                    error: Some(format!(
                        "`just --dump` failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )),
                    ..Default::default()
                }
            }
            Err(e) => {
                return Self {
                    error: Some(format!("could not run `just`: {e}")),
                    ..Default::default()
                }
            }
        };

        let dump = match JustDump::parse(&String::from_utf8_lossy(&output.stdout)) {
            Ok(dump) => dump,
            Err(e) => {
                return Self {
                    error: Some(format!("could not read `just --dump` output: {e}")),
                    ..Default::default()
                }
            }
        };

        Self {
            features: dump.list_variable(DynamicSource::Features.just_variable()),
            server_services: dump.list_variable(DynamicSource::ServerServices.just_variable()),
            service_catalog: dump.service_catalog(),
            available: dump.recipe_names().map(str::to_string).collect(),
            drift: compare(catalog, &dump),
            error: None,
        }
    }

    /// The choices for one dynamic dropdown, with the catalog's extras (`all`) first.
    pub fn choices(&self, source: DynamicSource, extra: &[String]) -> Vec<String> {
        let mut choices: Vec<String> = extra.to_vec();
        choices.extend(
            match source {
                DynamicSource::Features => &self.features,
                DynamicSource::ServerServices => &self.server_services,
            }
            .iter()
            .cloned(),
        );
        choices
    }

    /// Whether a catalog action can be run on this host at all: both the recipe it
    /// starts and the recipe that does the work have to exist.
    ///
    /// An empty `available` set means `just --dump` did not run, in which case
    /// nothing is hidden — a failure to introspect the justfile should not empty out
    /// the whole portal.
    pub fn is_available(&self, action: &Action) -> bool {
        self.available.is_empty()
            || (self.available.contains(&action.command[0])
                && self.available.contains(&action.implements))
    }

    /// Recipes the catalog knows about that this host's justfile does not have yet.
    pub fn missing(&self) -> Vec<&str> {
        self.drift
            .iter()
            .filter(|d| !d.is_catalog_defect())
            .map(Drift::recipe)
            .collect()
    }

    /// One line for the banner, or `None` when the catalog and the host agree.
    pub fn drift_summary(&self) -> Option<String> {
        if let Some(error) = &self.error {
            return Some(format!("Could not read this host's justfile: {error}"));
        }

        let missing = self.missing();
        let defects: Vec<&Drift> = self
            .drift
            .iter()
            .filter(|d| d.is_catalog_defect())
            .collect();

        // A host that simply has not rebuilt recently is a different story from a
        // catalog that no longer matches the justfile, and saying so is the difference
        // between an actionable message and an alarming one.
        match (defects.as_slice(), missing.as_slice()) {
            ([], []) => None,
            ([], missing) => Some(format!(
                "{} hidden — this host has not rebuilt since vexos-nix added {}.",
                describe_count(missing.len(), "operation is", "operations are"),
                if missing.len() == 1 { "it" } else { "them" },
            )),
            (defects, _) => Some(format!(
                "VexPortal is out of step with this host's justfile: {}{}",
                defects[0].describe(),
                if defects.len() > 1 {
                    format!(" (and {} more)", defects.len() - 1)
                } else {
                    String::new()
                }
            )),
        }
    }
}

fn describe_count(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vexportal_catalog::drift::Drift;

    fn facts(drift: Vec<Drift>) -> JustfileFacts {
        JustfileFacts {
            drift,
            ..Default::default()
        }
    }

    #[test]
    fn agreement_shows_no_banner() {
        assert!(facts(Vec::new()).drift_summary().is_none());
    }

    #[test]
    fn a_host_that_has_not_rebuilt_is_told_so_without_alarm() {
        let summary = facts(vec![
            Drift::Missing {
                recipe: "_cache-harmonia".into(),
            },
            Drift::Missing {
                recipe: "_kernel-now".into(),
            },
        ])
        .drift_summary()
        .expect("missing recipes should produce a banner");

        assert_eq!(
            summary,
            "2 operations are hidden — this host has not rebuilt since vexos-nix added them."
        );
    }

    #[test]
    fn one_missing_recipe_reads_as_singular() {
        let summary = facts(vec![Drift::Missing {
            recipe: "_cache-harmonia".into(),
        }])
        .drift_summary()
        .unwrap();
        assert_eq!(
            summary,
            "1 operation is hidden — this host has not rebuilt since vexos-nix added it."
        );
    }

    #[test]
    fn a_catalog_defect_takes_precedence_over_a_stale_host() {
        // Both at once: the defect is the one worth the user's attention, because it
        // is the one that means VexPortal itself is wrong.
        let summary = facts(vec![
            Drift::Missing {
                recipe: "_cache-harmonia".into(),
            },
            Drift::Unlisted {
                recipe: "brand-new".into(),
                doc: None,
            },
        ])
        .drift_summary()
        .unwrap();
        assert!(summary.starts_with("VexPortal is out of step"), "{summary}");
    }

    #[test]
    fn a_justfile_that_cannot_be_read_says_why() {
        let facts = JustfileFacts {
            error: Some("could not run `just`: not found".into()),
            ..Default::default()
        };
        assert!(facts
            .drift_summary()
            .unwrap()
            .contains("could not run `just`"));
    }

    #[test]
    fn nothing_is_hidden_when_the_justfile_could_not_be_introspected() {
        // An empty `available` set means `just --dump` failed. Hiding every recipe on
        // that basis would leave an empty portal, which is worse than letting the
        // daemon report the real error for one recipe.
        let catalog = Catalog::load().unwrap();
        assert!(JustfileFacts::default().is_available(catalog.action("rebuild").unwrap()));
    }

    #[test]
    fn a_recipe_absent_from_this_host_is_hidden() {
        let catalog = Catalog::load().unwrap();
        let facts = JustfileFacts {
            available: ["rebuild", "cache"].into_iter().map(String::from).collect(),
            ..Default::default()
        };
        assert!(facts.is_available(catalog.action("rebuild").unwrap()));
        // The menu exists but the recipe behind this entry does not.
        assert!(!facts.is_available(catalog.action("cache-harmonia").unwrap()));
    }
}
