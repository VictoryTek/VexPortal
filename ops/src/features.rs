//! Optional feature modules, switched on and off in `/etc/nixos/features.nix`.
//!
//! One definition of "enabled" for the whole portal: the GUI reads the file with
//! [`status`] and the daemon edits it with [`enable`] and [`disable`], so the page and the
//! operations can never disagree about what a missing line means.
//!
//! The file format is the one `just feature` reads and writes, one
//! `vexos.features.<name>.enable = <bool>;` per line, so either path can edit what the
//! other wrote.

use crate::nixedit::{self, EditError, NixValue};
use crate::runner::Runner;
use crate::sidefile::FEATURES_SEED;
use crate::system::{read_variant, ROLES};
use crate::{fail, fsutil, Ctx};
use std::path::Path;

/// A feature module and what is true of it that the file does not say.
pub struct Feature {
    pub name: &'static str,
    /// What the module does when `features.nix` does not mention it.
    pub default_on: bool,
    /// Roles that load features.nix but do not have this module, so setting the option
    /// would break the build with "option does not exist".
    pub unavailable_roles: &'static [&'static str],
    /// What it adds, one line each, shown after it is turned on.
    pub adds: &'static [&'static str],
}

pub const FEATURES: &[Feature] = &[
    Feature {
        name: "gaming",
        default_on: false,
        unavailable_roles: &[],
        adds: &[
            "Packages   Steam, Proton-GE, GameMode, Gamescope, MangoHud, Wine, protontricks,",
            "           umu-launcher, vkbasalt, distrobox, Ryujinx, RetroArch, vesktop, Discord",
            "Flatpak    Lutris, ProtonPlus, PrismLauncher",
            "Hardware   Xbox, Switch Pro, DualShock 4 and DualSense controllers",
            "GPU        32-bit libraries, shader cache tuning, the SCX LAVD gaming scheduler",
        ],
    },
    Feature {
        name: "development",
        default_on: false,
        unavailable_roles: &[],
        adds: &[
            "Services   Docker (with weekly auto-prune)",
            "Editor     VSCodium",
            "Languages  Python (uv, ruff), TypeScript, Node (pnpm, bun), Rust (rustup), Go",
            "Tools      GitHub CLI, git-lfs, jq, yq, pre-commit, sqlite, httpie, mkcert, gcc",
            "AI         Claude Code",
            "Nix        nil (language server), nixpkgs-fmt, nix-output-monitor",
        ],
    },
    Feature {
        name: "print3d",
        default_on: false,
        unavailable_roles: &[],
        adds: &["Flatpak    Blender, OrcaSlicer, SolidCraft"],
    },
    Feature {
        name: "virtualization",
        default_on: false,
        unavailable_roles: &[],
        adds: &[
            "Services   libvirtd and the QEMU/KVM hypervisor",
            "Apps       GNOME Boxes",
            "Features   Virtual TPM 2.0, needed by Windows 11 guests",
            "Groups     your user joins libvirtd, so VMs need no sudo",
        ],
    },
    Feature {
        name: "sunshine",
        default_on: true,
        unavailable_roles: &[],
        adds: &[
            "Services   Sunshine, the self-hosted Moonlight game-stream host",
            "Firewall   Sunshine's TCP and UDP ports are opened",
            "Groups     your user joins uinput, for remote mouse and keyboard input",
            "After the rebuild, two manual steps remain: create the WebUI admin account at",
            "https://<tailscale-ip>:47990, then add this host in Moonlight by IP and enter the",
            "PIN shown under the WebUI's PIN tab.",
        ],
    },
    Feature {
        name: "vpn",
        default_on: false,
        unavailable_roles: &["server"],
        adds: &[
            "Services   PIA VPN tunnel (WireGuard, OpenVPN backup) and an nftables kill switch",
            "Apps       the vexos-vpn CLI and the vex-vpn GUI and tray",
            "The VPN stays off until you sign in and connect. The kill switch is available but",
            "stopped by default.",
        ],
    },
    Feature {
        name: "kernel",
        default_on: false,
        unavailable_roles: &["htpc", "server"],
        adds: &[
            "Kernel     A custom kernel from the VexOS registry (default: ogc, the Open Gaming",
            "           Collective kernel behind Bazzite, ChimeraOS, Nobara and ASUS Linux)",
            "Pick another by setting vexos.features.kernel.name in /etc/nixos/features.nix.",
            "These kernels are not on cache.nixos.org. A build host serves them over Harmonia;",
            "if it has not built the current pin, this machine would compile the kernel itself,",
            "which takes hours. Rebuild checks the cache first and stops you before that.",
        ],
    },
    Feature {
        name: "ai",
        default_on: false,
        unavailable_roles: &[],
        adds: &[
            "Apps       The AI assistant you choose (Claude Code, Codex or OpenCode), and",
            "           'VexOS Assistant' in the app menu (Super+Shift+A)",
            "Skills     Teaches the assistant how VexOS is configured: it edits the /etc/nixos",
            "           side files and dry-builds, and you apply its changes with a rebuild",
            "Diagnose   A 'Diagnose with AI' notification when a program crashes",
            "On first launch you sign in through the assistant's own login.",
        ],
    },
];

pub fn find(name: &str) -> Option<&'static Feature> {
    FEATURES.iter().find(|f| f.name == name)
}

/// The role a `vexos-<role>-<gpu>` target belongs to.
pub fn role_of(variant: &str) -> Option<&'static str> {
    let rest = variant.strip_prefix("vexos-")?;
    ROLES
        .iter()
        .copied()
        .find(|role| rest.strip_prefix(role).is_some_and(|r| r.starts_with('-')))
}

/// Only the desktop, htpc and server wrappers import `features.nix`. Stateless,
/// headless-server and vanilla never do, so nothing written there would take effect.
pub fn role_supports_features(role: &str) -> bool {
    matches!(role, "desktop" | "htpc" | "server")
}

impl Feature {
    pub fn available_on(&self, role: &str) -> bool {
        role_supports_features(role) && !self.unavailable_roles.contains(&role)
    }

    fn option(&self) -> String {
        format!("vexos.features.{}.enable", self.name)
    }
}

/// Whether a feature is on, and whether that is only the module default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub enabled: bool,
    /// True when `features.nix` says nothing, so the module default applies.
    pub defaulted: bool,
}

/// Read a feature's state from the text of `features.nix` (empty if there is none).
///
/// Only an uncommented `= true;` or `= false;` counts. A commented template line, such as
/// `# vexos.features.x.enable = false;`, says nothing, so the module default stands.
pub fn status(text: &str, feature: &Feature) -> Status {
    match nixedit::get_bool(text, &feature.option()) {
        Some(enabled) => Status {
            enabled,
            defaulted: false,
        },
        None => Status {
            enabled: feature.default_on,
            defaulted: true,
        },
    }
}

/// Change `features` to `on` in `text`. Returns the new text and the names that actually
/// changed; a feature already in the wanted state is left alone.
///
/// Turning something off writes an explicit `= false;`. Commenting the line out would turn
/// a default-on feature such as Sunshine back on, so a feature that is on by default is
/// only ever disabled by saying so.
fn apply(
    text: &str,
    features: &[&'static Feature],
    on: bool,
) -> Result<(String, Vec<&'static str>), EditError> {
    let mut current = text.to_string();
    let mut changed = Vec::new();
    for feature in features {
        let now = status(&current, feature);
        let already = if on {
            // Enabling is written out even where the default is on, as `just` does, so
            // the choice is on record.
            !now.defaulted && now.enabled
        } else {
            // Off, whether said so or off by default and never mentioned: nothing to write.
            !now.enabled
        };
        if already {
            continue;
        }
        current = nixedit::set(&current, &feature.option(), &NixValue::Bool(on))?;
        changed.push(feature.name);
    }
    Ok((current, changed))
}

/// The role this host runs, or the reason the operation cannot go on.
fn host_role<R: Runner>(ctx: &Ctx<'_, R>) -> Result<&'static str, i32> {
    let Some(variant) = read_variant(ctx.nixos_dir) else {
        return Err(fail(
            ctx.sink,
            "/etc/nixos/vexos-variant is missing, so the role is unknown. Run Switch Role or GPU once.",
        ));
    };
    let Some(role) = role_of(&variant) else {
        return Err(fail(ctx.sink, format!("`{variant}` is not a VexOS target")));
    };
    if !role_supports_features(role) {
        return Err(fail(
            ctx.sink,
            format!("Optional features are not available on the {role} role: it does not load features.nix."),
        ));
    }
    Ok(role)
}

/// The features a request names. `all` is every feature this role has; one feature the
/// role lacks is an error, while `all` quietly passes over it.
fn selected<R: Runner>(ctx: &Ctx<'_, R>, role: &str) -> Result<Vec<&'static Feature>, i32> {
    let Some(name) = ctx.args.get("feature") else {
        return Err(fail(ctx.sink, "a feature is required"));
    };
    if name == "all" {
        return Ok(FEATURES.iter().filter(|f| f.available_on(role)).collect());
    }
    let Some(feature) = find(name) else {
        let names: Vec<&str> = FEATURES.iter().map(|f| f.name).collect();
        return Err(fail(
            ctx.sink,
            format!("unknown feature `{name}`. Available: {}", names.join(", ")),
        ));
    };
    if !feature.available_on(role) {
        return Err(fail(
            ctx.sink,
            format!("{name} is not available on the {role} role."),
        ));
    }
    Ok(vec![feature])
}

fn features_path(nixos_dir: &Path) -> std::path::PathBuf {
    nixos_dir.join("features.nix")
}

pub async fn list<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let role = match host_role(ctx) {
        Ok(role) => role,
        Err(code) => return code,
    };
    let text = std::fs::read_to_string(features_path(ctx.nixos_dir)).unwrap_or_default();
    ctx.sink
        .info("Optional features (/etc/nixos/features.nix):");
    for feature in FEATURES.iter().filter(|f| f.available_on(role)) {
        let s = status(&text, feature);
        ctx.sink.info(&format!(
            "  {} {}{}",
            if s.enabled { "✓" } else { "✗" },
            feature.name,
            if s.defaulted && s.enabled {
                " (on by default)"
            } else {
                ""
            }
        ));
    }
    0
}

pub async fn enable<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    switch_features(ctx, true).await
}

pub async fn disable<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    switch_features(ctx, false).await
}

async fn switch_features<R: Runner>(ctx: &Ctx<'_, R>, on: bool) -> i32 {
    match switch_inner(ctx, on).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async fn switch_inner<R: Runner>(ctx: &Ctx<'_, R>, on: bool) -> Result<(), i32> {
    let role = host_role(ctx)?;
    let features = selected(ctx, role)?;
    let path = features_path(ctx.nixos_dir);
    let verb = if on { "enabled" } else { "disabled" };

    // Decide everything against the current text first, then write once, so `all` is one
    // edit and one syntax check, not eight.
    let existing = std::fs::read_to_string(&path).ok();
    let base = existing.as_deref().unwrap_or(FEATURES_SEED);
    let (_, would_change) = apply(base, &features, on).map_err(|e| fail(ctx.sink, e))?;
    if would_change.is_empty() {
        ctx.sink.info(&if features.len() == 1 {
            format!("{} is already {verb}.", features[0].name)
        } else {
            format!("Every feature is already {verb}.")
        });
        return Ok(());
    }

    let created = existing.is_none();
    if created {
        fsutil::create_new(&path, FEATURES_SEED.as_bytes(), 0o644)
            .or_else(|e| {
                // Someone else created it first; edit theirs.
                (e.kind() == std::io::ErrorKind::AlreadyExists)
                    .then_some(())
                    .ok_or(e)
            })
            .map_err(|e| fail(ctx.sink, e))?;
    }

    let mut changed: Vec<&'static str> = Vec::new();
    let edited = nixedit::edit_file(ctx.runner, &path, |text| {
        let (updated, names) = apply(text, &features, on)?;
        changed = names;
        Ok((!changed.is_empty()).then_some(updated))
    })
    .await;
    if let Err(e) = edited {
        if created {
            // A seed nothing was written to is only clutter.
            let _ = nixedit::replace_if_unchanged(&path, FEATURES_SEED, None).await;
        }
        return Err(fail(ctx.sink, e));
    }

    for name in &changed {
        ctx.sink
            .info(&format!("✓ {name} {verb} in {}", path.display()));
    }
    if on {
        for name in &changed {
            if let Some(feature) = find(name) {
                ctx.sink.info("");
                ctx.sink.info(&format!("What {name} adds:"));
                for line in feature.adds {
                    ctx.sink.info(&format!("  {line}"));
                }
            }
        }
    }
    ctx.sink.info("");
    ctx.sink.info("Rebuild to apply.");
    if on && changed.contains(&"ai") {
        ctx.sink.info(
            "The assistant's launcher, shortcut and crash notifications load at login, so reboot \
             after the rebuild.",
        );
    }
    if wrapper_wired_for(ctx.nixos_dir, role) == Some(false) {
        ctx.sink.error(
            "Warning: /etc/nixos/flake.nix does not load features.nix, so these settings reset on \
             every rebuild. Run Repair Feature Wiring, then rebuild.",
        );
    }
    Ok(())
}

const FEATURES_LINE: &str =
    "++ (if builtins.pathExists ./features.nix then [ ./features.nix ] else [])";

/// The wrapper's builder for each role that loads `features.nix`. A builder not in this
/// list (stateless, vanilla, headless-server) never loads it and is left alone.
const FEATURE_BUILDERS: [(&str, &str); 3] = [
    ("_mkVariantWith", "desktop"),
    ("mkHtpcVariant", "htpc"),
    ("mkServerVariant", "server"),
];

/// Every builder definition in the wrapper, which is what bounds one builder's text.
const ALL_BUILDERS: [&str; 7] = [
    "_mkVariantWith",
    "mkVariant",
    "mkStatelessVariant",
    "mkHtpcVariant",
    "mkVanillaVariant",
    "mkHeadlessServerVariant",
    "mkServerVariant",
];

/// Where `name = ...` is defined: the name as a whole word, then `=`.
fn definition_start(text: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(found) = text[from..].find(name) {
        let at = from + found;
        let before_ok = text[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        let rest = text[at + name.len()..].trim_start_matches([' ', '\t']);
        if before_ok && rest.starts_with('=') && !rest.starts_with("==") {
            return Some(at);
        }
        from = at + name.len();
    }
    None
}

/// The text of one builder: from its definition to the next builder's.
fn builder_range(text: &str, name: &str) -> Option<std::ops::Range<usize>> {
    let start = definition_start(text, name)?;
    let end = ALL_BUILDERS
        .iter()
        .filter(|other| **other != name)
        .filter_map(|other| definition_start(text, other))
        .filter(|at| *at > start)
        .min()
        .unwrap_or(text.len());
    Some(start..end)
}

/// The text with Nix comments taken out: `/* ... */` blocks (which do not nest) and `#` to the
/// end of a line.
fn without_comments(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut rest = code;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            // An unclosed block comment runs to the end of the file.
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(
            "
",
        )
}

/// Does this code import `features.nix`? Comments are not code: the wrapper's header talks
/// about the file at length.
fn imports_features(code: &str) -> bool {
    let code = without_comments(code);
    code.contains("features.nix") || code.contains("featuresFile")
}

/// One builder's text patched to import `features.nix`, in one of the two shapes the old
/// wrappers had. `None` if it has neither.
fn patch_builder(block: &str) -> Option<String> {
    // The oldest wrappers end the module list with `] ++ modules;`.
    if block.contains("] ++ modules;") {
        return Some(block.replace(
            "] ++ modules;",
            &format!("] ++ modules\n          {FEATURES_LINE};"),
        ));
    }
    // Later ones add the kernel override with `lib.optional`.
    let anchor = "++ lib.optional hasKernelOverride";
    block
        .contains(anchor)
        .then(|| block.replacen(anchor, &format!("{FEATURES_LINE}\n          {anchor}"), 1))
}

/// What [`wire_wrapper`] found and did.
#[derive(Debug, PartialEq, Eq)]
pub struct Wiring {
    pub text: String,
    /// Builders this patched.
    pub patched: Vec<&'static str>,
    /// Builders that still do not import `features.nix`, because their module list has
    /// neither known shape.
    pub unwired: Vec<&'static str>,
}

/// Make every builder that should import `features.nix` do so. `None` if the wrapper has
/// none of those builders, so there is nothing to recognise.
///
/// Each builder is checked and patched on its own: a wrapper where the desktop builder
/// has the import and the htpc one does not is repaired, not reported as fine.
pub fn wire_wrapper(text: &str) -> Option<Wiring> {
    let mut text = text.to_string();
    let mut patched = Vec::new();
    let mut unwired = Vec::new();
    let mut found = false;
    for (builder, _role) in FEATURE_BUILDERS {
        let Some(range) = builder_range(&text, builder) else {
            continue;
        };
        found = true;
        if imports_features(&text[range.clone()]) {
            continue;
        }
        match patch_builder(&text[range.clone()]) {
            Some(new_block) => {
                text.replace_range(range, &new_block);
                patched.push(builder);
            }
            None => unwired.push(builder),
        }
    }
    found.then_some(Wiring {
        text,
        patched,
        unwired,
    })
}

/// Does this role's builder import `features.nix`? Falls back to the whole file for a
/// wrapper whose builders are not recognised.
fn wrapper_wired_for(nixos_dir: &Path, role: &str) -> Option<bool> {
    let text = std::fs::read_to_string(nixos_dir.join("flake.nix")).ok()?;
    let builder = FEATURE_BUILDERS.iter().find(|(_, r)| *r == role)?.0;
    Some(match builder_range(&text, builder) {
        Some(range) => imports_features(&text[range]),
        None => imports_features(&text),
    })
}

pub async fn fix_flake<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let path = ctx.nixos_dir.join("flake.nix");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return fail(ctx.sink, format!("{} not found", path.display()));
    };
    let Some(plan) = wire_wrapper(&text) else {
        // No builder this recognises. If the file still imports features.nix there is
        // nothing for this to do; if not, it cannot tell where the import goes.
        if imports_features(&text) {
            ctx.sink
                .info("✓ The wrapper already loads features.nix. Nothing to do.");
            return 0;
        }
        return unrecognised(ctx);
    };
    if plan.patched.is_empty() && plan.unwired.is_empty() {
        ctx.sink.info(&format!(
            "✓ {} already loads features.nix for every role that uses it. Nothing to do.",
            path.display()
        ));
        return 0;
    }

    let mut unwired: Vec<&'static str> = plan.unwired.clone();
    if !plan.patched.is_empty() {
        // The edit is checked with `nix-instantiate --parse`, the old file is kept as
        // flake.nix.bak, and the new one replaces it in one step.
        // Set when, on a retry, the file turned out to be something this cannot read.
        let mut lost = false;
        let edited = nixedit::edit_file(ctx.runner, &path, |text| {
            lost = false;
            Ok(match wire_wrapper(text) {
                Some(w) => {
                    unwired = w.unwired.clone();
                    (!w.patched.is_empty()).then_some(w.text)
                }
                None => {
                    lost = !imports_features(text);
                    None
                }
            })
        })
        .await;
        if lost {
            return unrecognised(ctx);
        }
        match edited {
            Ok(nixedit::Edited::Changed) => ctx.sink.info(&format!(
                "✓ {} patched ({}). The previous version is in flake.nix.bak.",
                path.display(),
                plan.patched.join(", ")
            )),
            // Another job patched it first; what is left is judged below.
            Ok(nixedit::Edited::Unchanged) => {}
            Err(e) => return fail(ctx.sink, e),
        }
    }

    if unwired.is_empty() {
        ctx.sink.info("Rebuild to apply.");
        return 0;
    }
    ctx.sink.error(&format!(
        "Could not wire {} in /etc/nixos/flake.nix: its module list has neither of the shapes this \
         knows. Add this to that builder's module list by hand:",
        unwired.join(", ")
    ));
    ctx.sink.error(&format!("    {FEATURES_LINE}"));
    1
}

fn unrecognised<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    ctx.sink.error(
        "Could not identify this wrapper's version, so it was not changed. Add this to the module \
         list in _mkVariantWith in /etc/nixos/flake.nix by hand:",
    );
    ctx.sink.error(&format!("    {FEATURES_LINE}"));
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::testing::{block_on, FakeRunner};
    use crate::testkit::Kit;

    fn feature(name: &str) -> &'static Feature {
        find(name).unwrap()
    }

    fn on_host(variant: &str, args: &[(&str, &str)]) -> Kit {
        let kit = Kit::new().with_args(args);
        kit.nixos_file("vexos-variant", &format!("{variant}\n"));
        kit.nixos_file("flake.nix", "{ features.nix; }\n");
        kit
    }

    fn parse_ok() -> FakeRunner {
        FakeRunner::new().ok("nix-instantiate", &["--parse"])
    }

    fn features_file(kit: &Kit) -> String {
        std::fs::read_to_string(kit.nixos.join("features.nix")).unwrap()
    }

    #[test]
    fn roles_come_from_the_target_name() {
        assert_eq!(role_of("vexos-desktop-amd"), Some("desktop"));
        assert_eq!(
            role_of("vexos-headless-server-nvidia"),
            Some("headless-server")
        );
        assert_eq!(role_of("vexos-server-vm"), Some("server"));
        assert_eq!(role_of("vexos-kiosk-amd"), None);
        assert_eq!(role_of("desktop-amd"), None);
        assert!(!role_supports_features("stateless"));
        assert!(!role_supports_features("headless-server"));
        assert!(!role_supports_features("vanilla"));
        for role in ["desktop", "htpc", "server"] {
            assert!(role_supports_features(role));
        }
    }

    #[test]
    fn a_missing_line_means_the_module_default() {
        let on = status("", feature("sunshine"));
        assert_eq!((on.enabled, on.defaulted), (true, true));
        let off = status("", feature("gaming"));
        assert_eq!((off.enabled, off.defaulted), (false, true));
    }

    #[test]
    fn only_an_uncommented_boolean_counts() {
        let text = "{\n  # vexos.features.gaming.enable = true;\n  vexos.features.print3d.enable = false;\n  vexos.features.vpn.enable = true; # PIA\n}\n";
        assert!(status(text, feature("gaming")).defaulted);
        assert!(!status(text, feature("gaming")).enabled);
        let print = status(text, feature("print3d"));
        assert_eq!((print.enabled, print.defaulted), (false, false));
        assert!(status(text, feature("vpn")).enabled);
        // The Sunshine case the old recipes got wrong: an explicit false beats the default.
        let off = "{\n  vexos.features.sunshine.enable = false;\n}\n";
        assert!(!status(off, feature("sunshine")).enabled);
    }

    #[test]
    fn enabling_one_writes_one_explicit_line_and_keeps_the_rest() {
        let kit = on_host("vexos-desktop-amd", &[("feature", "gaming")]);
        kit.nixos_file(
            "features.nix",
            "{\n  # vexos.features.gaming.enable = false;   # template\n  vexos.hostname = \"x\";\n}\n",
        );
        assert_eq!(block_on(enable(&kit.ctx(&parse_ok()))), 0);
        let text = features_file(&kit);
        assert!(
            text.contains("vexos.features.gaming.enable = true;"),
            "{text}"
        );
        assert!(text.contains("vexos.hostname = \"x\";"));
        assert!(kit.sink.text().contains("✓ gaming enabled"));
        assert!(kit.sink.text().contains("Rebuild to apply."));
    }

    #[test]
    fn a_missing_features_file_is_created_for_an_enable() {
        let kit = on_host("vexos-desktop-amd", &[("feature", "vpn")]);
        assert_eq!(block_on(enable(&kit.ctx(&parse_ok()))), 0);
        assert!(features_file(&kit).contains("vexos.features.vpn.enable = true;"));
    }

    #[test]
    fn disabling_sunshine_writes_an_explicit_false() {
        let kit = on_host("vexos-desktop-amd", &[("feature", "sunshine")]);
        // No features.nix at all: Sunshine is on by default, so saying so is required.
        assert_eq!(block_on(disable(&kit.ctx(&parse_ok()))), 0);
        let text = features_file(&kit);
        assert!(
            text.contains("vexos.features.sunshine.enable = false;"),
            "{text}"
        );
        assert!(!status(&text, feature("sunshine")).enabled);
    }

    #[test]
    fn disabling_an_enabled_feature_keeps_its_trailing_comment() {
        let kit = on_host("vexos-desktop-amd", &[("feature", "vpn")]);
        kit.nixos_file(
            "features.nix",
            "{\n  vexos.features.vpn.enable = true; # PIA\n}\n",
        );
        assert_eq!(block_on(disable(&kit.ctx(&parse_ok()))), 0);
        assert!(features_file(&kit).contains("vexos.features.vpn.enable = false; # PIA"));
    }

    #[test]
    fn disabling_something_off_by_default_and_unmentioned_changes_nothing() {
        let kit = on_host("vexos-desktop-amd", &[("feature", "gaming")]);
        let runner = parse_ok();
        assert_eq!(block_on(disable(&kit.ctx(&runner))), 0);
        assert!(!kit.nixos.join("features.nix").exists());
        assert!(kit.sink.text().contains("already disabled"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn a_line_already_in_the_wanted_state_is_not_rewritten() {
        let kit = on_host("vexos-desktop-amd", &[("feature", "gaming")]);
        kit.nixos_file(
            "features.nix",
            "{\n  vexos.features.gaming.enable = true;\n}\n",
        );
        let runner = parse_ok();
        assert_eq!(block_on(enable(&kit.ctx(&runner))), 0);
        assert!(kit.sink.text().contains("already enabled"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn all_is_one_edit_and_skips_what_the_role_lacks() {
        let kit = on_host("vexos-server-amd", &[("feature", "all")]);
        let runner = parse_ok();
        assert_eq!(block_on(enable(&kit.ctx(&runner))), 0);
        let text = features_file(&kit);
        assert!(text.contains("vexos.features.gaming.enable = true;"));
        assert!(text.contains("vexos.features.ai.enable = true;"));
        // The server role has neither module; writing them would break evaluation.
        assert!(!text.contains("vexos.features.vpn"), "{text}");
        assert!(!text.contains("vexos.features.kernel"), "{text}");
        let parses = runner
            .calls()
            .iter()
            .filter(|c| c.program == "nix-instantiate")
            .count();
        assert_eq!(parses, 1, "one syntax check for the whole batch");
    }

    #[test]
    fn each_role_gets_exactly_the_features_its_modules_declare() {
        let names = |role: &str| -> Vec<&str> {
            FEATURES
                .iter()
                .filter(|f| f.available_on(role))
                .map(|f| f.name)
                .collect()
        };
        assert_eq!(names("desktop").len(), 8);
        assert!(names("htpc").contains(&"vpn") && !names("htpc").contains(&"kernel"));
        assert!(!names("server").contains(&"vpn") && !names("server").contains(&"kernel"));
        assert!(names("vanilla").is_empty());
    }

    #[test]
    fn one_feature_a_role_lacks_is_an_error_not_a_silent_skip() {
        let kit = on_host("vexos-server-amd", &[("feature", "vpn")]);
        assert_eq!(block_on(enable(&kit.ctx(&parse_ok()))), 1);
        assert!(kit.sink.text().contains("not available on the server role"));
        assert!(!kit.nixos.join("features.nix").exists());
    }

    #[test]
    fn stateless_and_headless_roles_are_refused() {
        for variant in [
            "vexos-stateless-amd",
            "vexos-headless-server-amd",
            "vexos-vanilla-amd",
        ] {
            let kit = on_host(variant, &[("feature", "gaming")]);
            assert_eq!(block_on(enable(&kit.ctx(&parse_ok()))), 1, "{variant}");
            assert!(kit.sink.text().contains("does not load features.nix"));
            assert!(!kit.nixos.join("features.nix").exists());
        }
    }

    #[test]
    fn an_unknown_feature_is_refused_before_anything_is_written() {
        let kit = on_host("vexos-desktop-amd", &[("feature", "nope")]);
        assert_eq!(block_on(enable(&kit.ctx(&parse_ok()))), 1);
        assert!(kit.sink.text().contains("unknown feature"));
        assert!(!kit.nixos.join("features.nix").exists());
    }

    #[test]
    fn a_failed_syntax_check_leaves_no_file_behind() {
        let kit = on_host("vexos-desktop-amd", &[("feature", "gaming")]);
        let runner = FakeRunner::new().failing("nix-instantiate", &["--parse"], 1, "bad");
        assert_eq!(block_on(enable(&kit.ctx(&runner))), 1);
        assert!(!kit.nixos.join("features.nix").exists());
    }

    #[test]
    fn a_file_whose_last_line_is_not_a_bare_brace_still_takes_the_edit() {
        let kit = on_host("vexos-desktop-amd", &[("feature", "gaming")]);
        kit.nixos_file("features.nix", "{\n  vexos.hostname = \"x\";\n} # end\n");
        assert_eq!(block_on(enable(&kit.ctx(&parse_ok()))), 0);
        assert!(features_file(&kit).contains("vexos.features.gaming.enable = true;"));
    }

    #[test]
    fn enabling_ai_says_a_reboot_follows_the_rebuild_and_names_the_agents() {
        let kit = on_host("vexos-desktop-amd", &[("feature", "ai")]);
        assert_eq!(block_on(enable(&kit.ctx(&parse_ok()))), 0);
        let text = kit.sink.text();
        assert!(text.contains("reboot"));
        for agent in ["Claude Code", "Codex", "OpenCode"] {
            assert!(text.contains(agent), "{agent} missing from {text}");
        }
    }

    #[test]
    fn a_wrapper_that_does_not_load_features_is_warned_about() {
        let kit = on_host("vexos-desktop-amd", &[("feature", "gaming")]);
        kit.nixos_file("flake.nix", "{ outputs = _: {}; }\n");
        assert_eq!(block_on(enable(&kit.ctx(&parse_ok()))), 0);
        assert!(kit.sink.text().contains("Repair Feature Wiring"));
    }

    #[test]
    fn list_shows_state_defaults_and_hides_what_the_role_lacks() {
        let kit = on_host("vexos-htpc-amd", &[]);
        kit.nixos_file(
            "features.nix",
            "{
  vexos.features.gaming.enable = true;
  vexos.features.sunshine.enable = false;
}
",
        );
        assert_eq!(block_on(list(&kit.ctx(&FakeRunner::new()))), 0);
        let text = kit.sink.text();
        assert!(text.contains("✓ gaming"));
        assert!(text.contains("✗ sunshine"), "{text}");
        assert!(text.contains("vpn"));
        assert!(
            !text.contains("kernel"),
            "htpc has no custom kernel: {text}"
        );

        let kit = on_host("vexos-desktop-amd", &[]);
        block_on(list(&kit.ctx(&FakeRunner::new())));
        assert!(kit.sink.text().contains("✓ sunshine (on by default)"));
    }

    /// The oldest shape: one builder ending its module list with `] ++ modules;`.
    const OLDEST: &str = "  _mkVariantWith = base: v: gpu: lib.nixosSystem {\n    modules = [ a ] ++ modules;\n  };\n  mkVariant = _mkVariantWith base;\n";

    fn builder(name: &str, extra: &str) -> String {
        format!(
            "  {name} = v: g: lib.nixosSystem {{\n    modules = [ a ] ++ localFiles{extra}\n      ++ lib.optional hasKernelOverride kernelOverrideFile;\n  }};\n"
        )
    }

    /// The later shape, with every builder the real wrapper has.
    fn newer(desktop: &str, htpc: &str, server: &str) -> String {
        [
            builder("_mkVariantWith", desktop),
            "  mkVariant = _mkVariantWith base;\n".to_string(),
            builder("mkStatelessVariant", ""),
            builder("mkHtpcVariant", htpc),
            builder("mkVanillaVariant", ""),
            builder("mkHeadlessServerVariant", ""),
            builder("mkServerVariant", server),
        ]
        .concat()
    }

    const WIRED: &str = "\n      ++ lib.optional hasFeatures featuresFile";

    #[test]
    fn the_oldest_wrapper_shape_is_patched() {
        let wiring = wire_wrapper(OLDEST).unwrap();
        assert_eq!(wiring.patched, ["_mkVariantWith"]);
        assert!(wiring.unwired.is_empty());
        assert!(wiring
            .text
            .contains("] ++ modules\n          ++ (if builtins.pathExists ./features.nix"));
    }

    #[test]
    fn every_builder_that_should_load_features_is_patched_and_the_rest_are_not() {
        let wiring = wire_wrapper(&newer("", "", "")).unwrap();
        assert_eq!(
            wiring.patched,
            ["_mkVariantWith", "mkHtpcVariant", "mkServerVariant"]
        );
        assert!(wiring.unwired.is_empty());
        // Desktop, htpc and server each got the line; stateless, vanilla and headless did
        // not, because those roles do not load features.nix.
        assert_eq!(wiring.text.matches("./features.nix ]").count(), 3);
        for name in [
            "mkStatelessVariant",
            "mkVanillaVariant",
            "mkHeadlessServerVariant",
        ] {
            let range = builder_range(&wiring.text, name).unwrap();
            assert!(!imports_features(&wiring.text[range]), "{name}");
        }
    }

    #[test]
    fn a_builder_that_already_imports_the_file_is_left_and_the_others_are_repaired() {
        // The case the recipe got wrong: desktop wired, htpc and server not.
        let wiring = wire_wrapper(&newer(WIRED, "", "")).unwrap();
        assert_eq!(wiring.patched, ["mkHtpcVariant", "mkServerVariant"]);
        assert_eq!(wire_wrapper(&wiring.text).unwrap().patched.len(), 0);
    }

    #[test]
    fn a_block_commented_import_does_not_count_as_loading_the_file() {
        let commented = "
      /* ++ lib.optional hasFeatures featuresFile */";
        let wiring = wire_wrapper(&newer("", commented, WIRED)).unwrap();
        assert_eq!(wiring.patched, ["_mkVariantWith", "mkHtpcVariant"]);
        assert!(!imports_features("a /* featuresFile */ b"));
        assert!(!imports_features("a /* never closed featuresFile"));
        assert!(imports_features("a /* note */ ++ featuresFile"));
    }

    #[test]
    fn a_comment_that_mentions_the_file_does_not_count_as_loading_it() {
        let text = format!("# features.nix is loaded below\n{}", newer("", "", ""));
        assert_eq!(wire_wrapper(&text).unwrap().patched.len(), 3);
    }

    #[test]
    fn a_builder_with_an_unknown_module_list_is_reported_not_skipped() {
        let text = format!(
            "{}  mkServerVariant = v: g: lib.nixosSystem {{ modules = other; }};\n",
            [
                builder("_mkVariantWith", WIRED),
                builder("mkHtpcVariant", WIRED)
            ]
            .concat()
        );
        let wiring = wire_wrapper(&text).unwrap();
        assert!(wiring.patched.is_empty());
        assert_eq!(wiring.unwired, ["mkServerVariant"]);
    }

    #[test]
    fn a_wrapper_with_no_known_builder_is_not_recognised() {
        assert_eq!(wire_wrapper("{ outputs = _: {}; }\n"), None);
    }

    #[test]
    fn the_toggle_warning_follows_the_hosts_own_builder() {
        let kit = Kit::new();
        kit.nixos_file("flake.nix", &newer(WIRED, "", WIRED));
        assert_eq!(wrapper_wired_for(&kit.nixos, "desktop"), Some(true));
        assert_eq!(wrapper_wired_for(&kit.nixos, "htpc"), Some(false));
        assert_eq!(wrapper_wired_for(&kit.nixos, "server"), Some(true));
    }

    #[test]
    fn fix_flake_patches_checks_and_backs_up() {
        let kit = Kit::new();
        kit.nixos_file("flake.nix", OLDEST);
        assert_eq!(block_on(fix_flake(&kit.ctx(&parse_ok()))), 0);
        assert!(std::fs::read_to_string(kit.nixos.join("flake.nix"))
            .unwrap()
            .contains("features.nix"));
        assert_eq!(
            std::fs::read_to_string(kit.nixos.join("flake.nix.bak")).unwrap(),
            OLDEST
        );
    }

    #[test]
    fn fix_flake_repairs_only_what_is_missing_and_says_so() {
        let kit = Kit::new();
        kit.nixos_file("flake.nix", &newer(WIRED, "", WIRED));
        assert_eq!(block_on(fix_flake(&kit.ctx(&parse_ok()))), 0);
        let text = std::fs::read_to_string(kit.nixos.join("flake.nix")).unwrap();
        let htpc = builder_range(&text, "mkHtpcVariant").unwrap();
        assert!(imports_features(&text[htpc]));
        assert!(kit.sink.text().contains("mkHtpcVariant"));
    }

    #[test]
    fn fix_flake_does_nothing_when_every_builder_is_wired() {
        let kit = Kit::new();
        kit.nixos_file("flake.nix", &newer(WIRED, WIRED, WIRED));
        let runner = parse_ok();
        assert_eq!(block_on(fix_flake(&kit.ctx(&runner))), 0);
        assert!(runner.calls().is_empty());
        assert!(kit.sink.text().contains("Nothing to do"));
    }

    #[test]
    fn fix_flake_refuses_an_unrecognised_wrapper_and_leaves_it_alone() {
        let kit = Kit::new();
        kit.nixos_file("flake.nix", "{ outputs = _: {}; }\n");
        assert_eq!(block_on(fix_flake(&kit.ctx(&parse_ok()))), 1);
        assert!(kit.sink.text().contains("by hand"));
        assert_eq!(
            std::fs::read_to_string(kit.nixos.join("flake.nix")).unwrap(),
            "{ outputs = _: {}; }\n"
        );
    }

    #[test]
    fn a_wrapper_that_cannot_be_fully_wired_fails_and_names_the_builder() {
        let kit = Kit::new();
        let text = format!(
            "{}  mkServerVariant = v: g: lib.nixosSystem {{ modules = other; }};\n",
            builder("_mkVariantWith", "")
        );
        kit.nixos_file("flake.nix", &text);
        assert_eq!(block_on(fix_flake(&kit.ctx(&parse_ok()))), 1);
        assert!(kit.sink.text().contains("mkServerVariant"));
        // The builder it could patch was patched.
        assert!(std::fs::read_to_string(kit.nixos.join("flake.nix"))
            .unwrap()
            .contains("./features.nix"));
    }

    #[test]
    fn a_patch_that_does_not_parse_is_never_written() {
        let kit = Kit::new();
        kit.nixos_file("flake.nix", OLDEST);
        let runner = FakeRunner::new().failing("nix-instantiate", &["--parse"], 1, "syntax");
        assert_eq!(block_on(fix_flake(&kit.ctx(&runner))), 1);
        assert_eq!(
            std::fs::read_to_string(kit.nixos.join("flake.nix")).unwrap(),
            OLDEST
        );
    }
}
