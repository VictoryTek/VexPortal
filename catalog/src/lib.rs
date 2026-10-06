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
    /// A new extra Claude account: 1–32 letters, digits, `-` or `_`, and not `main`.
    AccountLabel,
    /// An existing Claude account to switch to: a label, `main`, or `next`.
    AccountRef,
    /// A whole percentage, 1–100.
    Percent,
    /// A program's file name, as crash notifications are keyed: no `/`, no leading `.`.
    ProgramName,
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

    /// Whether a valid value for this parameter could contain whitespace.
    pub fn admits_whitespace(&self) -> bool {
        match &self.widget {
            Widget::Choice { choices } => choices.iter().any(|c| c.contains(char::is_whitespace)),
            Widget::ChoiceDynamic { .. } => false,
            Widget::Text { format } => matches!(format, Format::AbsPath | Format::FlakeRef),
            Widget::Path { .. } | Widget::Secret => true,
        }
    }
}

/// Where an action runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Run as root by `vexportal-daemon`, behind polkit, with no terminal.
    #[default]
    Daemon,
    /// Run as the logged-in user in VexPortal's built-in terminal: the recipe holds a
    /// conversation, or acts on the user's own account (`$HOME`, dconf, SSH keys).
    Terminal,
}

/// A single entry in the portal.
#[derive(Debug, Clone, Deserialize)]
pub struct Action {
    /// Stable identifier: what the GUI asks the daemon for, and the daemon's allowlist.
    pub id: String,
    /// The argv after `just`, before any parameters — `["feature", "enable"]`.
    pub command: Vec<String>,
    /// The justfile recipe that takes the parameters, which the drift check compares
    /// against. For a grouped menu this is the hidden `_<group>-<action>` recipe.
    pub implements: String,
    pub title: String,
    pub blurb: String,
    pub icon: String,
    pub page: String,
    /// The heading the action sits under on its page.
    pub group: String,
    pub roles: Vec<Role>,
    pub risk: Risk,
    /// Shown in a confirmation dialog before the action runs.
    #[serde(default)]
    pub confirm: Option<String>,
    #[serde(default)]
    pub mode: Mode,
    /// Why a terminal action cannot be a form. Required for `mode = "terminal"`.
    #[serde(default)]
    pub mode_reason: Option<String>,
    /// Success edits a file the next rebuild reads, so the GUI offers a rebuild.
    #[serde(default)]
    pub rebuild: bool,
    #[serde(default)]
    pub params: Vec<Param>,
}

impl Action {
    pub fn applies_to(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }

    pub fn param(&self, name: &str) -> Option<&Param> {
        self.params.iter().find(|p| p.name == name)
    }

    pub fn is_terminal(&self) -> bool {
        self.mode == Mode::Terminal
    }

    /// Routed through the justfile's `_menu` helper, which joins the parameters into
    /// one string and splits them again on whitespace.
    pub fn is_menu_routed(&self) -> bool {
        self.command.len() > 1
    }

    /// `just feature enable`, for headers and messages.
    pub fn command_line(&self) -> String {
        format!("just {}", self.command.join(" "))
    }
}

/// A sidebar entry.
#[derive(Debug, Clone, Deserialize)]
pub struct Page {
    pub id: String,
    pub title: String,
    pub icon: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// An optional feature module, shown as a switch on the Features page.
#[derive(Debug, Clone, Deserialize)]
pub struct Feature {
    /// The name in `_feature_names` and `vexos.features.<name>.enable`.
    pub name: String,
    pub title: String,
    pub description: String,
    /// The module default when features.nix does not mention the feature.
    #[serde(default)]
    pub default_on: bool,
}

/// Recipes present in the justfile that the catalog deliberately does not surface.
#[derive(Debug, Clone, Deserialize)]
pub struct Excluded {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Catalog {
    #[serde(rename = "page")]
    pub pages: Vec<Page>,
    #[serde(rename = "action")]
    pub actions: Vec<Action>,
    #[serde(default, rename = "feature")]
    pub features: Vec<Feature>,
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
        Self::parse(CATALOG_TOML)
    }

    /// Parse and check a catalog document.
    pub fn parse(source: &str) -> Result<Self, CatalogError> {
        let catalog: Catalog = toml::from_str(source)?;
        catalog.check_consistency()?;
        Ok(catalog)
    }

    fn check_consistency(&self) -> Result<(), CatalogError> {
        let invalid = |message: String| Err(CatalogError::Invalid(message));
        let known: Vec<&str> = self.pages.iter().map(|p| p.id.as_str()).collect();
        let mut seen: HashMap<&str, ()> = HashMap::new();

        for action in &self.actions {
            let id = &action.id;
            if !known.contains(&action.page.as_str()) {
                return invalid(format!(
                    "action `{id}` is on unknown page `{}`",
                    action.page
                ));
            }
            if seen.insert(id.as_str(), ()).is_some() {
                return invalid(format!("action `{id}` is listed twice"));
            }
            if action.roles.is_empty() {
                return invalid(format!(
                    "action `{id}` applies to no role, so nothing could ever show it"
                ));
            }
            if action.command.is_empty() {
                return invalid(format!("action `{id}` has an empty command"));
            }
            // The UI promises a confirmation before anything destructive, in both
            // lanes; a destructive action without a message would break that promise.
            if action.risk == Risk::Destructive && action.confirm.is_none() {
                return invalid(format!(
                    "action `{id}` is destructive but has no `confirm` message"
                ));
            }
            if action.is_terminal() && action.mode_reason.is_none() {
                return invalid(format!(
                    "terminal action `{id}` must say why it cannot be a form (`mode_reason`)"
                ));
            }
            // `_menu` joins its arguments into one string and splits them again on
            // whitespace: an empty interior value would vanish and shift the rest, and
            // a value containing a space would become two.
            if action.is_menu_routed() {
                if action.params.len() > 1 {
                    return invalid(format!(
                        "action `{id}` is routed through a justfile menu and may take one parameter at most"
                    ));
                }
                if let Some(param) = action.params.iter().find(|p| p.admits_whitespace()) {
                    return invalid(format!(
                        "action `{id}`: parameter `{}` could contain whitespace, which the justfile menu would split",
                        param.name
                    ));
                }
            }
            // A required parameter after an optional one cannot be expressed
            // positionally: `just` would bind the value to the wrong slot.
            let mut seen_optional = false;
            for param in &action.params {
                if param.required && seen_optional {
                    return invalid(format!(
                        "action `{id}`: required parameter `{}` follows an optional one",
                        param.name
                    ));
                }
                seen_optional |= !param.required;
            }
        }
        Ok(())
    }

    pub fn action(&self, id: &str) -> Option<&Action> {
        self.actions.iter().find(|a| a.id == id)
    }

    pub fn page(&self, id: &str) -> Option<&Page> {
        self.pages.iter().find(|p| p.id == id)
    }

    /// Actions for one role, in catalog order.
    pub fn for_role(&self, role: Role) -> impl Iterator<Item = &Action> {
        self.actions.iter().filter(move |a| a.applies_to(role))
    }

    /// Actions on one page for one role, in catalog order.
    pub fn on_page(&self, page: &str, role: Role) -> Vec<&Action> {
        self.actions
            .iter()
            .filter(|a| a.page == page && a.applies_to(role))
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
                "role {} has no actions at all",
                role.as_str()
            );
        }
    }

    #[test]
    fn every_page_is_used() {
        let catalog = Catalog::load().unwrap();
        for page in &catalog.pages {
            assert!(
                catalog.actions.iter().any(|a| a.page == page.id),
                "page `{}` has no actions",
                page.id
            );
        }
    }

    #[test]
    fn secrets_are_never_positional() {
        // A secret must be the sole trailing parameter: it is dropped from argv, so a
        // positional parameter after it would silently shift into the wrong slot.
        let catalog = Catalog::load().unwrap();
        for action in &catalog.actions {
            if let Some(idx) = action.params.iter().position(Param::is_secret) {
                assert_eq!(
                    idx,
                    action.params.len() - 1,
                    "action `{}`: secret parameter must come last",
                    action.id
                );
            }
        }
    }

    /// A one-action catalog around `body`, for the consistency rules.
    fn single(body: &str) -> Result<Catalog, CatalogError> {
        Catalog::parse(&format!(
            "[[page]]\nid = \"p\"\ntitle = \"P\"\nicon = \"i\"\n\n\
             [[action]]\nid = \"a\"\nimplements = \"a\"\ntitle = \"A\"\nblurb = \"b\"\n\
             icon = \"i\"\npage = \"p\"\ngroup = \"g\"\nroles = [\"desktop\"]\n{body}"
        ))
    }

    #[test]
    fn destructive_actions_must_confirm() {
        let err = single("command = [\"a\"]\nrisk = \"destructive\"\n").unwrap_err();
        assert!(err.to_string().contains("confirm"), "{err}");
        single("command = [\"a\"]\nrisk = \"destructive\"\nconfirm = \"Sure?\"\n").unwrap();
    }

    #[test]
    fn terminal_actions_must_say_why() {
        let err = single("command = [\"a\"]\nrisk = \"safe\"\nmode = \"terminal\"\n").unwrap_err();
        assert!(err.to_string().contains("mode_reason"), "{err}");
    }

    #[test]
    fn menu_routed_actions_reject_whitespace_parameters() {
        let err = single(
            "command = [\"g\", \"a\"]\nrisk = \"safe\"\n\
             [[action.params]]\nname = \"p\"\nlabel = \"P\"\nwidget = \"path\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("whitespace"), "{err}");
    }

    #[test]
    fn menu_routed_actions_take_one_parameter_at_most() {
        let err = single(
            "command = [\"g\", \"a\"]\nrisk = \"safe\"\n\
             [[action.params]]\nname = \"p\"\nlabel = \"P\"\nwidget = \"text\"\n\
             [[action.params]]\nname = \"q\"\nlabel = \"Q\"\nwidget = \"text\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("one parameter"), "{err}");
    }

    #[test]
    fn features_cover_the_justfile_feature_names() {
        // Mirrors `_feature_names` in tests/fixtures/vexos-nix-justfile.json; the
        // fixture test checks the same list against the dump itself.
        let catalog = Catalog::load().unwrap();
        let names: Vec<&str> = catalog.features.iter().map(|f| f.name.as_str()).collect();
        for name in [
            "gaming",
            "development",
            "print3d",
            "virtualization",
            "sunshine",
            "vpn",
            "kernel",
            "ai",
        ] {
            assert!(
                names.contains(&name),
                "feature `{name}` has no catalog entry"
            );
        }
    }
}
