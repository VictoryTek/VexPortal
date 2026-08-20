//! The VexPortal recipe catalog.
//!
//! `just` can describe the vexos-nix justfile itself (`just --dump --dump-format json`),
//! but not well enough to drive a GUI: it keeps only the *last* comment line as a
//! recipe's doc (so `backup-plex` documents itself as "suitable for moving to a new
//! server"), it has no notion of which role a recipe applies to, and it cannot say
//! whether a recipe is safe to run or will repartition a disk.
//!
//! This crate carries that metadata as a curated TOML document compiled into both the
//! GUI and the daemon, plus the validation that turns a set of user-supplied answers
//! into an argv the daemon is willing to execute. See [`drift`] for the check that
//! keeps the catalog honest against the real justfile.

pub mod drift;
pub mod format;
pub mod validate;

use serde::Deserialize;
use std::collections::HashMap;

/// The catalog TOML, compiled into every binary that links this crate.
const CATALOG_TOML: &str = include_str!("catalog.toml");

/// A VexOS role, as it appears in `/etc/nixos/vexos-variant` (`vexos-<role>-<gpu>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Desktop,
    Htpc,
    Server,
    HeadlessServer,
    Stateless,
    Vanilla,
}

impl Role {
    pub const ALL: [Role; 6] = [
        Role::Desktop,
        Role::Htpc,
        Role::Server,
        Role::HeadlessServer,
        Role::Stateless,
        Role::Vanilla,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Desktop => "desktop",
            Role::Htpc => "htpc",
            Role::Server => "server",
            Role::HeadlessServer => "headless-server",
            Role::Stateless => "stateless",
            Role::Vanilla => "vanilla",
        }
    }

    /// Human-facing name for the dashboard badge.
    pub fn title(self) -> &'static str {
        match self {
            Role::Desktop => "Desktop",
            Role::Htpc => "HTPC",
            Role::Server => "Server",
            Role::HeadlessServer => "Headless Server",
            Role::Stateless => "Stateless",
            Role::Vanilla => "Vanilla",
        }
    }
}

/// How much damage a recipe can do, which decides both the confirmation UX and
/// which polkit action the daemon checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Risk {
    /// Reads state only. No polkit prompt (`allow_active=yes`).
    Safe,
    /// Changes the system but is reversible. `auth_admin_keep`.
    Medium,
    /// Destroys data or cuts network access. `auth_admin`, no credential caching.
    Destructive,
}

impl Risk {
    /// The polkit action id the daemon checks before running a recipe of this risk.
    pub fn polkit_action(self) -> &'static str {
        match self {
            Risk::Safe => "io.github.vexportal.run-readonly",
            Risk::Medium => "io.github.vexportal.run-recipe",
            Risk::Destructive => "io.github.vexportal.run-destructive",
        }
    }
}

/// Which runtime list feeds a `choice-dynamic` parameter. Both are `just` variables
/// read out of `just --dump --dump-format json` at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DynamicSource {
    /// The `_feature_names` justfile variable.
    Features,
    /// The `_server_service_names` justfile variable.
    ServerServices,
}

impl DynamicSource {
    pub fn just_variable(self) -> &'static str {
        match self {
            DynamicSource::Features => "_feature_names",
            DynamicSource::ServerServices => "_server_service_names",
        }
    }
}

/// The value shape a parameter accepts.
///
/// Deliberately a closed set rather than user-supplied regexes: the daemon validates
/// against these before exec, and a fixed set is auditable in a way an arbitrary
/// pattern from a config file is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    /// `[a-z0-9][a-z0-9._-]*` — service names, feature names, cache names.
    Slug,
    /// A DNS label: letters, digits and inner hyphens.
    Hostname,
    /// `user@host` or `host`, where host is a hostname or IPv4 address.
    SshTarget,
    /// An absolute path with no NUL, `..` component, or newline.
    AbsPath,
    /// A NixOS release such as `26.05`.
    NixosVersion,
    /// A flake reference — a path or `github:owner/repo` style URL.
    FlakeRef,
}

/// How the GUI renders a parameter, and what the daemon will accept for it.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "widget", rename_all = "kebab-case")]
pub enum Widget {
    /// A fixed dropdown.
    Choice { choices: Vec<String> },
    /// A dropdown populated at runtime from a justfile variable.
    ChoiceDynamic {
        source: DynamicSource,
        /// Extra entries offered alongside the dynamic list (e.g. `all`).
        #[serde(default)]
        extra: Vec<String>,
    },
    /// A single-line entry validated against `format`.
    Text {
        #[serde(default = "default_format")]
        format: Format,
    },
    /// A file chooser. Always validated as an absolute path.
    Path {
        /// Require the path to exist at validation time.
        #[serde(default)]
        must_exist: bool,
    },
    /// A password. Never placed in argv — written to the recipe's stdin instead.
    Secret,
}

fn default_format() -> Format {
    Format::Slug
}

/// One positional argument of a `just` recipe.
#[derive(Debug, Clone, Deserialize)]
pub struct Param {
    /// Must match the parameter name in the justfile.
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub help: Option<String>,
    /// Optional parameters may be left empty; `just` then applies the recipe's own
    /// default, which is exactly what the CLI does when a user omits the argument.
    #[serde(default)]
    pub required: bool,
    /// Pre-filled value in the GUI. Should mirror the justfile default when there is one.
    #[serde(default)]
    pub default: Option<String>,
    #[serde(flatten)]
    pub widget: Widget,
}

impl Param {
    /// Secrets travel over the private D-Bus call to the daemon's stdin, never argv,
    /// so they are absent from `ps`, the journal, and the audit record.
    pub fn is_secret(&self) -> bool {
        matches!(self.widget, Widget::Secret)
    }
}

/// A single entry in the portal.
#[derive(Debug, Clone, Deserialize)]
pub struct Recipe {
    /// The `just` recipe name. This is the only string the daemon will exec.
    pub name: String,
    pub title: String,
    pub blurb: String,
    pub icon: String,
    pub category: String,
    pub roles: Vec<Role>,
    pub risk: Risk,
    /// Shown in a confirmation dialog before the recipe runs.
    #[serde(default)]
    pub confirm: Option<String>,
    /// System state keys the GUI should re-read once this recipe succeeds.
    #[serde(default)]
    pub refresh: Vec<String>,
    /// Recipes whose interaction cannot reasonably be expressed as a form — live
    /// partitioning wizards and the like. The GUI offers a terminal launch instead.
    #[serde(default)]
    pub terminal: bool,
    /// True while this recipe still prompts in vexos-nix and needs the
    /// `VEXOS_ASSUME_YES` / parameter changes before the form path works.
    #[serde(default)]
    pub needs_upstream: bool,
    #[serde(default)]
    pub params: Vec<Param>,
}

impl Recipe {
    pub fn applies_to(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }

    pub fn param(&self, name: &str) -> Option<&Param> {
        self.params.iter().find(|p| p.name == name)
    }
}

/// A sidebar section.
#[derive(Debug, Clone, Deserialize)]
pub struct Category {
    pub id: String,
    pub title: String,
    pub icon: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Recipes present in the justfile that the catalog deliberately does not surface.
#[derive(Debug, Clone, Deserialize)]
pub struct Excluded {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Catalog {
    #[serde(rename = "category")]
    pub categories: Vec<Category>,
    #[serde(rename = "recipe")]
    pub recipes: Vec<Recipe>,
    #[serde(default, rename = "excluded")]
    pub excluded: Vec<Excluded>,
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("catalog is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("catalog is internally inconsistent: {0}")]
    Invalid(String),
}

impl Catalog {
    /// Parse the compiled-in catalog. Fails only on a bug in `catalog.toml`, which
    /// [`Catalog::load`]'s test coverage catches at build time.
    pub fn load() -> Result<Self, CatalogError> {
        let catalog: Catalog = toml::from_str(CATALOG_TOML)?;
        catalog.check_consistency()?;
        Ok(catalog)
    }

    fn check_consistency(&self) -> Result<(), CatalogError> {
        let known: Vec<&str> = self.categories.iter().map(|c| c.id.as_str()).collect();
        let mut seen: HashMap<&str, ()> = HashMap::new();

        for recipe in &self.recipes {
            if !known.contains(&recipe.category.as_str()) {
                return Err(CatalogError::Invalid(format!(
                    "recipe `{}` is in unknown category `{}`",
                    recipe.name, recipe.category
                )));
            }
            if seen.insert(recipe.name.as_str(), ()).is_some() {
                return Err(CatalogError::Invalid(format!(
                    "recipe `{}` is listed twice",
                    recipe.name
                )));
            }
            if recipe.roles.is_empty() {
                return Err(CatalogError::Invalid(format!(
                    "recipe `{}` applies to no role, so nothing could ever show it",
                    recipe.name
                )));
            }
            // A required parameter after an optional one cannot be expressed
            // positionally: `just` would bind the value to the wrong slot.
            let mut seen_optional = false;
            for param in &recipe.params {
                if param.required && seen_optional {
                    return Err(CatalogError::Invalid(format!(
                        "recipe `{}`: required parameter `{}` follows an optional one",
                        recipe.name, param.name
                    )));
                }
                seen_optional |= !param.required;
            }
        }
        Ok(())
    }

    pub fn recipe(&self, name: &str) -> Option<&Recipe> {
        self.recipes.iter().find(|r| r.name == name)
    }

    pub fn category(&self, id: &str) -> Option<&Category> {
        self.categories.iter().find(|c| c.id == id)
    }

    /// Recipes for one role, in catalog order.
    pub fn for_role(&self, role: Role) -> impl Iterator<Item = &Recipe> {
        self.recipes.iter().filter(move |r| r.applies_to(role))
    }

    /// Recipes in one category for one role. Empty means the GUI hides the section.
    pub fn in_category(&self, category: &str, role: Role) -> Vec<&Recipe> {
        self.recipes
            .iter()
            .filter(|r| r.category == category && r.applies_to(role))
            .collect()
    }

    /// Categories that have at least one recipe for this role.
    pub fn categories_for_role(&self, role: Role) -> Vec<&Category> {
        self.categories
            .iter()
            .filter(|c| {
                self.recipes
                    .iter()
                    .any(|r| r.category == c.id && r.applies_to(role))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_catalog_parses_and_is_consistent() {
        Catalog::load().expect("catalog.toml must parse");
    }

    #[test]
    fn every_role_has_something_to_show() {
        let catalog = Catalog::load().unwrap();
        for role in Role::ALL {
            assert!(
                catalog.for_role(role).count() > 0,
                "role {} has no recipes at all",
                role.as_str()
            );
        }
    }

    #[test]
    fn every_category_is_used() {
        let catalog = Catalog::load().unwrap();
        for category in &catalog.categories {
            assert!(
                catalog.recipes.iter().any(|r| r.category == category.id),
                "category `{}` has no recipes",
                category.id
            );
        }
    }

    #[test]
    fn secrets_are_never_positional() {
        // A secret must be the sole trailing parameter: it is dropped from argv, so a
        // positional parameter after it would silently shift into the wrong slot.
        let catalog = Catalog::load().unwrap();
        for recipe in &catalog.recipes {
            if let Some(idx) = recipe.params.iter().position(Param::is_secret) {
                assert_eq!(
                    idx,
                    recipe.params.len() - 1,
                    "recipe `{}`: secret parameter must come last",
                    recipe.name
                );
            }
        }
    }
}
