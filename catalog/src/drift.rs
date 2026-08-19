//! Keeping the catalog honest against the real justfile.
//!
//! The catalog is curated, which means it can fall behind. `just --dump --dump-format
//! json` is the ground truth for what recipes exist and what arguments they take, so
//! the GUI compares the two at startup and shows a banner when they disagree, and the
//! test suite does the same against `/etc/nixos/justfile` so the drift fails CI rather
//! than surprising a user.

use crate::{Catalog, Param};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The subset of `just --dump --dump-format json` that VexPortal reads.
#[derive(Debug, Deserialize)]
pub struct JustDump {
    #[serde(default)]
    pub assignments: BTreeMap<String, JustAssignment>,
    #[serde(default)]
    pub recipes: BTreeMap<String, JustRecipe>,
}

#[derive(Debug, Deserialize)]
pub struct JustAssignment {
    pub value: String,
}

#[derive(Debug, Deserialize)]
pub struct JustRecipe {
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub doc: Option<String>,
    #[serde(default)]
    pub parameters: Vec<JustParam>,
}

#[derive(Debug, Deserialize)]
pub struct JustParam {
    pub name: String,
    /// `null` for a required parameter; a string for one with a default.
    #[serde(default)]
    pub default: Option<String>,
}

impl JustParam {
    fn required(&self) -> bool {
        self.default.is_none()
    }
}

impl JustDump {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Every recipe name this justfile defines, private ones included.
    pub fn recipe_names(&self) -> impl Iterator<Item = &str> {
        self.recipes.keys().map(String::as_str)
    }

    /// The whitespace-separated values of a justfile variable, e.g. `_feature_names`.
    pub fn list_variable(&self, name: &str) -> Vec<String> {
        self.assignments
            .get(name)
            .map(|a| a.value.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }
}

/// One way the catalog and the justfile disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    /// The justfile has a public recipe the catalog neither lists nor excludes.
    Unlisted { recipe: String, doc: Option<String> },
    /// The catalog lists a recipe this host's justfile does not have.
    ///
    /// Usually benign: `/etc/nixos/justfile` is a copy made by the last rebuild, so a
    /// host that has not rebuilt since vexos-nix gained a recipe will legitimately be
    /// missing it. The GUI hides those recipes rather than offering a card that would
    /// fail.
    Missing { recipe: String },
    /// The catalog and the justfile disagree about a recipe's arguments, which means
    /// the generated argv would land in the wrong positional slots.
    Parameters {
        recipe: String,
        catalog: Vec<String>,
        justfile: Vec<String>,
    },
    /// The catalog treats a parameter as optional that the justfile requires.
    Requiredness { recipe: String, param: String },
}

impl Drift {
    pub fn recipe(&self) -> &str {
        match self {
            Drift::Unlisted { recipe, .. }
            | Drift::Missing { recipe }
            | Drift::Parameters { recipe, .. }
            | Drift::Requiredness { recipe, .. } => recipe,
        }
    }

    /// Whether this is a defect in the catalog rather than a host that has not
    /// rebuilt recently. Only defects should fail a test: a stale
    /// `/etc/nixos/justfile` is the user's business, not the catalog's.
    pub fn is_catalog_defect(&self) -> bool {
        !matches!(self, Drift::Missing { .. })
    }

    /// A one-line description for the GUI banner and the test failure message.
    pub fn describe(&self) -> String {
        match self {
            Drift::Unlisted { recipe, doc } => match doc {
                Some(doc) => format!("`{recipe}` is in the justfile but not the catalog ({doc})"),
                None => format!("`{recipe}` is in the justfile but not the catalog"),
            },
            Drift::Missing { recipe } => {
                format!("`{recipe}` is not in this host's justfile yet")
            }
            Drift::Parameters {
                recipe,
                catalog,
                justfile,
            } => format!(
                "`{recipe}` takes [{}] in the justfile but the catalog declares [{}]",
                justfile.join(", "),
                catalog.join(", ")
            ),
            Drift::Requiredness { recipe, param } => format!(
                "`{recipe}`: the justfile requires `{param}` but the catalog lets it be empty"
            ),
        }
    }
}

/// Compare a catalog against a justfile dump.
///
/// Private justfile recipes are ignored unless the catalog lists them: role guards
/// like `_require-server-role` and the server recipes hidden from `just --list` are
/// private by design, and the catalog decides which of those to surface.
pub fn compare(catalog: &Catalog, dump: &JustDump) -> Vec<Drift> {
    let mut drift = Vec::new();

    for (name, just_recipe) in &dump.recipes {
        if just_recipe.private || catalog.recipe(name).is_some() {
            continue;
        }
        if catalog.excluded.iter().any(|e| &e.name == name) {
            continue;
        }
        drift.push(Drift::Unlisted {
            recipe: name.clone(),
            doc: just_recipe.doc.clone(),
        });
    }

    for recipe in &catalog.recipes {
        let Some(just_recipe) = dump.recipes.get(&recipe.name) else {
            drift.push(Drift::Missing {
                recipe: recipe.name.clone(),
            });
            continue;
        };

        // Secrets are delivered on stdin, so they have no justfile counterpart.
        let catalog_params: Vec<String> = recipe
            .params
            .iter()
            .filter(|p| !p.is_secret())
            .map(|p| p.name.clone())
            .collect();
        let just_params: Vec<String> = just_recipe
            .parameters
            .iter()
            .map(|p| p.name.clone())
            .collect();

        if catalog_params != just_params {
            drift.push(Drift::Parameters {
                recipe: recipe.name.clone(),
                catalog: catalog_params,
                justfile: just_params,
            });
            continue;
        }

        for (param, just_param) in recipe
            .params
            .iter()
            .filter(|p: &&Param| !p.is_secret())
            .zip(&just_recipe.parameters)
        {
            // Only the permissive direction is a defect. A catalog that requires what
            // the justfile would accept as empty is a deliberate UX call — `just
            // set-hostname` with no argument prompts, and a GUI form should never
            // submit its way into a prompt.
            if just_param.required() && !param.required {
                drift.push(Drift::Requiredness {
                    recipe: recipe.name.clone(),
                    param: param.name.clone(),
                });
            }
        }
    }

    drift.sort_by(|a, b| a.recipe().cmp(b.recipe()));
    drift
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = r#"{
        "assignments": {
            "_feature_names": { "value": "gaming development print3d" }
        },
        "recipes": {
            "rebuild":  { "private": false, "doc": "Rebuild.", "parameters": [] },
            "switch":   { "private": false, "doc": null, "parameters": [
                { "name": "role", "default": "" },
                { "name": "variant", "default": "" },
                { "name": "flake", "default": "" }
            ]},
            "brand-new": { "private": false, "doc": "Freshly added upstream.", "parameters": [] },
            "_guard":   { "private": true, "doc": null, "parameters": [] }
        }
    }"#;

    #[test]
    fn parses_a_real_shaped_dump() {
        let dump = JustDump::parse(DUMP).unwrap();
        assert_eq!(
            dump.list_variable("_feature_names"),
            ["gaming", "development", "print3d"]
        );
        assert!(dump.list_variable("_nonexistent").is_empty());
    }

    #[test]
    fn reports_a_recipe_the_catalog_has_not_caught_up_with() {
        let catalog = Catalog::load().unwrap();
        let dump = JustDump::parse(DUMP).unwrap();
        let drift = compare(&catalog, &dump);
        assert!(drift.iter().any(|d| matches!(
            d,
            Drift::Unlisted { recipe, .. } if recipe == "brand-new"
        )));
    }

    #[test]
    fn ignores_private_guard_recipes() {
        let catalog = Catalog::load().unwrap();
        let dump = JustDump::parse(DUMP).unwrap();
        assert!(!compare(&catalog, &dump)
            .iter()
            .any(|d| d.recipe() == "_guard"));
    }

    #[test]
    fn notices_a_parameter_that_moved() {
        let catalog = Catalog::load().unwrap();
        let dump = JustDump::parse(
            r#"{ "recipes": { "switch": { "private": false, "parameters": [
                { "name": "variant", "default": "" },
                { "name": "role", "default": "" }
            ]}}}"#,
        )
        .unwrap();
        assert!(compare(&catalog, &dump)
            .iter()
            .any(|d| matches!(d, Drift::Parameters { recipe, .. } if recipe == "switch")));
    }
}
