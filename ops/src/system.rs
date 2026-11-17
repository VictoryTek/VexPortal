//! Whole-machine operations: which variant this is, power, the hostname, and moving
//! between generations.

use crate::nixedit::{self, NixValue};
use crate::runner::{Cmd, Runner};
use crate::sidefile::{self, Changes};
use crate::{activate, capture, fail, Ctx};
use std::path::Path;

pub const ROLES: [&str; 6] = [
    "desktop",
    "stateless",
    "htpc",
    "server",
    "headless-server",
    "vanilla",
];
pub const GPUS: [&str; 5] = ["amd", "nvidia", "nvidia-legacy580", "intel", "vm"];

/// `vexos-<role>-<gpu>` for a role and GPU variant the flake offers.
pub fn target_name(role: &str, gpu: &str) -> Option<String> {
    (ROLES.contains(&role) && GPUS.contains(&gpu)).then(|| format!("vexos-{role}-{gpu}"))
}

/// The shape of a flake target. It is read from a file and put in a flake reference, so
/// it is checked rather than trusted.
pub fn valid_target(name: &str) -> bool {
    name.starts_with("vexos-")
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The target this machine was last built as, from `/etc/nixos/vexos-variant`.
pub fn read_variant(nixos_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(nixos_dir.join("vexos-variant")).ok()?;
    let name = text.trim();
    valid_target(name).then(|| name.to_string())
}

/// A single RFC 1123 label. A dot would make a domain name, which `hostnamectl` and
/// `networking.hostName` do not want here.
pub fn valid_hostname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// A wrapper from before the side-file layout, with the bootloader and hardware settings
/// written inline. Such a file does not import `hostname.nix`, `bootloader.nix` and the
/// others, so an edit to them would never take effect. `scripts/upgrade-wrapper.sh` in
/// the vexos-nix repository migrates it; this does not.
pub fn legacy_wrapper(nixos_dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(nixos_dir.join("flake.nix")) else {
        return false;
    };
    text.lines().any(|line| {
        let line = line.trim_start();
        ["hardwareModule", "bootloaderModule"].iter().any(|name| {
            line.strip_prefix(name)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        })
    })
}

/// Say so and return true when the wrapper is the legacy layout.
pub fn refuse_legacy<R: Runner>(ctx: &Ctx<'_, R>) -> bool {
    if legacy_wrapper(ctx.nixos_dir) {
        fail(
            ctx.sink,
            "/etc/nixos/flake.nix uses the old layout with the bootloader and hardware settings \
             written inline. Migrate it first with scripts/upgrade-wrapper.sh from the vexos-nix \
             repository; VexPortal does not change that file.",
        );
        true
    } else {
        false
    }
}

pub fn variant<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    match std::fs::read_to_string(ctx.nixos_dir.join("vexos-variant")) {
        Ok(text) if !text.trim().is_empty() => ctx.sink.info(text.trim()),
        _ => ctx.sink.info("unknown (run a build first)"),
    }
    0
}

pub async fn reboot<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    power(ctx, "reboot", "Rebooting…").await
}

pub async fn shutdown<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    power(ctx, "poweroff", "Powering off…").await
}

async fn power<R: Runner>(ctx: &Ctx<'_, R>, verb: &str, message: &str) -> i32 {
    ctx.sink.info(message);
    match capture(ctx, &Cmd::new("systemctl").arg(verb)).await {
        Ok(out) if out.success() => 0,
        Ok(out) => fail(ctx.sink, format!("systemctl {verb}: {}", out.stderr.trim())),
        Err(code) => code,
    }
}

/// An uncommented line in `text` that assigns `networking.hostName`, in whatever shape.
fn declares_hostname(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        !line.starts_with('#') && line.contains("networking.hostName") && line.contains('=')
    })
}

pub async fn set_hostname<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let Some(name) = ctx.args.get("name") else {
        return fail(ctx.sink, "a hostname is required");
    };
    if !valid_hostname(name) {
        return fail(
            ctx.sink,
            format!(
                "`{name}` is not a valid hostname. Use letters, digits and hyphens, not at the \
                 start or end, up to 63 characters."
            ),
        );
    }

    let current = std::fs::read_to_string(ctx.root.join("proc/sys/kernel/hostname"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if *name == current {
        ctx.sink
            .info(&format!("Hostname is already '{name}'. Nothing to do."));
        return 0;
    }
    ctx.sink.info(&format!(
        "Changing hostname: {} → {name}",
        if current.is_empty() {
            "unknown"
        } else {
            &current
        }
    ));

    // Persist first: a problem with the files should not leave the running system
    // renamed and the next rebuild about to rename it back.
    let mut persisted = false;
    if ctx.nixos_dir.join("flake.nix").is_file() {
        if refuse_legacy(ctx) {
            return 1;
        }
        match persist_hostname(ctx, name).await {
            Ok(path) => {
                ctx.sink
                    .info(&format!("✓ Set networking.hostName in {path}"));
                persisted = true;
            }
            Err(code) => return code,
        }
    }

    // `--transient` leaves /etc/hostname alone, which is a link into the Nix store on
    // NixOS. The static name follows from the file written above on the next rebuild.
    let apply = Cmd::new("hostnamectl").args(["--transient", "hostname", name]);
    match capture(ctx, &apply).await {
        Ok(out) if out.success() => ctx
            .sink
            .info("✓ Applied to the running system (until the next rebuild sets it for good)"),
        Ok(out) => return fail(ctx.sink, format!("hostnamectl: {}", out.stderr.trim())),
        Err(code) => return code,
    }

    if persisted {
        ctx.sink
            .info("The new name is saved. Rebuild to apply it everywhere.");
    } else {
        ctx.sink.info(&format!(
            "/etc/nixos/flake.nix was not found, so the name was not saved. To keep it across \
             rebuilds add networking.hostName = \"{name}\"; to a module the flake imports."
        ));
    }
    0
}

/// Write the name where the next rebuild reads it. Returns the file's path.
async fn persist_hostname<R: Runner>(ctx: &Ctx<'_, R>, name: &str) -> Result<String, i32> {
    let value = NixValue::str(name);
    // Wherever the name is already set it is updated there, so there is never a second
    // definition for the module system to reject.
    for file in ["flake.nix", "hardware-local.nix", "hostname.nix"] {
        let path = ctx.nixos_dir.join(file);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !declares_hostname(&text) {
            continue;
        }
        if nixedit::get_str(&text, "networking.hostName").is_none() {
            return Err(fail(
                ctx.sink,
                format!(
                    "{} sets networking.hostName in a form this cannot edit safely. Change it by \
                     hand.",
                    path.display()
                ),
            ));
        }
        return match nixedit::set_in_file(ctx.runner, &path, "networking.hostName", &value).await {
            Ok(_) => Ok(path.display().to_string()),
            Err(e) => Err(fail(ctx.sink, e)),
        };
    }

    let path = ctx.nixos_dir.join("hostname.nix");
    let mut changes = Changes::default();
    match sidefile::set_option(
        ctx.runner,
        &mut changes,
        &path,
        HOSTNAME_SEED,
        "networking.hostName",
        &value,
    )
    .await
    {
        Ok(()) => Ok(path.display().to_string()),
        Err(e) => {
            changes.restore(ctx.sink).await;
            Err(fail(ctx.sink, e))
        }
    }
}

const HOSTNAME_SEED: &str =
    "# /etc/nixos/hostname.nix\n# Written by VexPortal (Set Hostname).\n{\n}\n";

/// One generation of the system profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    pub number: u64,
    pub current: bool,
}

/// Parse `nix-env --list-generations`: a number, a date and time, and `(current)` on one.
pub fn parse_generations(text: &str) -> Vec<Generation> {
    text.lines()
        .filter_map(|line| {
            let number = line.split_whitespace().next()?.parse().ok()?;
            Some(Generation {
                number,
                current: line.contains("(current)"),
            })
        })
        .collect()
}

async fn generations<R: Runner>(ctx: &Ctx<'_, R>) -> Result<Vec<Generation>, i32> {
    let profile = activate::profile(ctx.root);
    let out = capture(
        ctx,
        &Cmd::new("nix-env").args(["-p", &profile.display().to_string(), "--list-generations"]),
    )
    .await?;
    if !out.success() {
        return Err(fail(ctx.sink, format!("nix-env: {}", out.stderr.trim())));
    }
    Ok(parse_generations(&out.stdout))
}

fn current_number(list: &[Generation]) -> Option<u64> {
    list.iter().find(|g| g.current).map(|g| g.number)
}

async fn run_profile<R: Runner>(ctx: &Ctx<'_, R>, extra: &[&str]) -> Result<(), i32> {
    let profile = activate::profile(ctx.root).display().to_string();
    let mut args = vec!["--profile", profile.as_str()];
    args.extend_from_slice(extra);
    let out = capture(ctx, &Cmd::new("nix-env").args(args)).await?;
    if out.success() {
        Ok(())
    } else {
        Err(fail(ctx.sink, format!("nix-env: {}", out.stderr.trim())))
    }
}

pub async fn rollback<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    match roll(ctx, None).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

pub async fn rollforward<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let list = match generations(ctx).await {
        Ok(list) => list,
        Err(code) => return code,
    };
    let Some(current) = current_number(&list) else {
        return fail(ctx.sink, "could not tell which generation is current");
    };
    let Some(next) = list.iter().map(|g| g.number).filter(|n| *n > current).min() else {
        ctx.sink.info(&format!(
            "Already at the latest generation ({current}). Nothing to roll forward to."
        ));
        return 0;
    };
    ctx.sink
        .info(&format!("Rolling forward: generation {current} → {next}"));
    match roll(ctx, Some(next)).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

/// Move the system profile to the previous generation, or to `to`, then activate it.
async fn roll<R: Runner>(ctx: &Ctx<'_, R>, to: Option<u64>) -> Result<(), i32> {
    if to.is_none() {
        let before = generations(ctx).await?;
        match current_number(&before) {
            Some(n) => ctx.sink.info(&format!("Current generation: {n}")),
            None => return Err(fail(ctx.sink, "could not tell which generation is current")),
        }
    }
    // What `nixos-rebuild switch --rollback` does, split so a graphical <-> headless
    // move can be queued for the next boot instead of applied to the live session.
    match to {
        None => run_profile(ctx, &["--rollback"]).await?,
        Some(n) => run_profile(ctx, &["--switch-generation", &n.to_string()]).await?,
    }
    let activated = activate::activate(ctx).await?;
    if let Some(n) = current_number(&generations(ctx).await?) {
        ctx.sink.info(&format!("Now on generation: {n}"));
    }
    if activated.queued {
        ctx.sink.info("Reboot to finish the change.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activate::tests::{point, system};
    use crate::runner::testing::{block_on, FakeRunner};
    use crate::runner::Output;
    use crate::testkit::Kit;

    #[test]
    fn targets_are_built_only_from_known_roles_and_gpus() {
        assert_eq!(
            target_name("headless-server", "nvidia-legacy580").as_deref(),
            Some("vexos-headless-server-nvidia-legacy580")
        );
        assert_eq!(target_name("desktop", "amd; reboot"), None);
        assert_eq!(target_name("kiosk", "amd"), None);
    }

    #[test]
    fn a_variant_file_that_is_not_a_target_is_ignored() {
        let kit = Kit::new();
        kit.nixos_file("vexos-variant", "vexos-desktop-amd\n");
        assert_eq!(
            read_variant(&kit.nixos).as_deref(),
            Some("vexos-desktop-amd")
        );
        kit.nixos_file("vexos-variant", "vexos-x --flake evil\n");
        assert_eq!(read_variant(&kit.nixos), None);
    }

    #[test]
    fn hostnames_are_a_single_label() {
        for ok in ["mypc", "a", "vexos-desktop", "Box1"] {
            assert!(valid_hostname(ok), "{ok}");
        }
        for bad in [
            "",
            "-a",
            "a-",
            "my.pc",
            "a b",
            "a_b",
            "x\n",
            &"a".repeat(64),
        ] {
            assert!(!valid_hostname(bad), "{bad:?}");
        }
    }

    #[test]
    fn variant_prints_the_file_or_says_it_is_unknown() {
        let kit = Kit::new();
        let runner = FakeRunner::new();
        assert_eq!(variant(&kit.ctx(&runner)), 0);
        assert_eq!(kit.sink.text(), "unknown (run a build first)");

        let kit = Kit::new();
        kit.nixos_file("vexos-variant", "vexos-htpc-amd\n");
        variant(&kit.ctx(&runner));
        assert_eq!(kit.sink.text(), "vexos-htpc-amd");
    }

    #[test]
    fn power_actions_call_systemctl_once() {
        let kit = Kit::new();
        let runner = FakeRunner::new().ok("systemctl", &["reboot"]);
        assert_eq!(block_on(reboot(&kit.ctx(&runner))), 0);
        assert_eq!(runner.lines(), ["systemctl reboot"]);

        let runner = FakeRunner::new().failing("systemctl", &["poweroff"], 1, "denied");
        assert_eq!(block_on(shutdown(&kit.ctx(&runner))), 1);
    }

    fn proc_hostname(kit: &Kit, name: &str) {
        std::fs::create_dir_all(kit.root.join("proc/sys/kernel")).unwrap();
        std::fs::write(
            kit.root.join("proc/sys/kernel/hostname"),
            format!("{name}\n"),
        )
        .unwrap();
    }

    fn hostname_runner() -> FakeRunner {
        FakeRunner::new()
            .ok("nix-instantiate", &["--parse"])
            .ok("hostnamectl", &["--transient", "hostname"])
    }

    #[test]
    fn the_name_is_written_to_a_new_hostname_file_and_applied_transiently() {
        let kit = Kit::new().with_args(&[("name", "newbox")]);
        proc_hostname(&kit, "oldbox");
        kit.nixos_file("flake.nix", "{ outputs = _: {}; }\n");
        let runner = hostname_runner();
        assert_eq!(block_on(set_hostname(&kit.ctx(&runner))), 0);
        let text = std::fs::read_to_string(kit.nixos.join("hostname.nix")).unwrap();
        assert!(text.contains("networking.hostName = \"newbox\";"), "{text}");
        assert!(runner
            .lines()
            .contains(&"hostnamectl --transient hostname newbox".to_string()));
    }

    #[test]
    fn a_name_already_set_in_a_side_file_is_updated_there_not_duplicated() {
        let kit = Kit::new().with_args(&[("name", "newbox")]);
        proc_hostname(&kit, "oldbox");
        kit.nixos_file("flake.nix", "{ outputs = _: {}; }\n");
        kit.nixos_file(
            "hardware-local.nix",
            "{\n  networking.hostName = \"oldbox\";\n  vexos.hardware.asus.enable = true;\n}\n",
        );
        let runner = hostname_runner();
        assert_eq!(block_on(set_hostname(&kit.ctx(&runner))), 0);
        let text = std::fs::read_to_string(kit.nixos.join("hardware-local.nix")).unwrap();
        assert!(text.contains("networking.hostName = \"newbox\";"));
        assert!(text.contains("asus"));
        assert!(!kit.nixos.join("hostname.nix").exists());
    }

    #[test]
    fn a_hostname_set_in_a_shape_that_cannot_be_edited_is_left_alone() {
        let kit = Kit::new().with_args(&[("name", "newbox")]);
        proc_hostname(&kit, "oldbox");
        kit.nixos_file(
            "flake.nix",
            "{ x = { networking.hostName = \"oldbox\"; }; }\n",
        );
        let runner = hostname_runner();
        assert_eq!(block_on(set_hostname(&kit.ctx(&runner))), 1);
        assert!(kit.sink.text().contains("by hand"));
        assert!(runner.calls().iter().all(|c| c.program != "hostnamectl"));
    }

    #[test]
    fn the_same_name_is_a_no_op_and_a_bad_one_is_refused() {
        let kit = Kit::new().with_args(&[("name", "same")]);
        proc_hostname(&kit, "same");
        let runner = FakeRunner::new();
        assert_eq!(block_on(set_hostname(&kit.ctx(&runner))), 0);
        assert!(runner.calls().is_empty());

        let kit = Kit::new().with_args(&[("name", "a.b")]);
        assert_eq!(block_on(set_hostname(&kit.ctx(&runner))), 1);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn a_legacy_wrapper_is_refused_before_anything_changes() {
        let kit = Kit::new().with_args(&[("name", "newbox")]);
        proc_hostname(&kit, "oldbox");
        kit.nixos_file(
            "flake.nix",
            "# hardwareModule = in a comment\n  bootloaderModule = { };\n",
        );
        let runner = hostname_runner();
        assert_eq!(block_on(set_hostname(&kit.ctx(&runner))), 1);
        assert!(runner.calls().is_empty());
        assert!(!kit.nixos.join("hostname.nix").exists());
    }

    #[test]
    fn a_commented_inline_block_does_not_make_a_wrapper_legacy() {
        let kit = Kit::new();
        kit.nixos_file("flake.nix", "# hardwareModule = old;\n{ }\n");
        assert!(!legacy_wrapper(&kit.nixos));
    }

    const LIST: &str = "   9   2025-01-01 10:00:00   \n  10   2025-02-01 10:00:00   \n  11   2025-03-01 10:00:00   (current)\n  12   2025-04-01 10:00:00   \n";

    #[test]
    fn generations_are_parsed_with_the_current_one_marked() {
        let gens = parse_generations(LIST);
        assert_eq!(gens.len(), 4);
        assert_eq!(current_number(&gens), Some(11));
        assert!(parse_generations("nothing here\n").is_empty());
    }

    fn profile_runner(listing: &str, new_top: &std::path::Path) -> FakeRunner {
        FakeRunner::new()
            .on(
                "nix-env",
                &["-p"],
                Output {
                    code: 0,
                    stdout: listing.to_string(),
                    stderr: String::new(),
                },
            )
            .ok("nix-env", &["--profile"])
            .ok(
                &new_top
                    .join("bin/switch-to-configuration")
                    .display()
                    .to_string(),
                &["switch"],
            )
            .ok(
                &new_top
                    .join("bin/switch-to-configuration")
                    .display()
                    .to_string(),
                &["boot"],
            )
    }

    #[test]
    fn rollforward_goes_to_the_next_newer_generation() {
        let kit = Kit::new();
        let old = system(&kit, "old", None);
        let new = system(&kit, "new", None);
        point(&kit, &old, &new);
        let runner = profile_runner(LIST, &new);
        assert_eq!(block_on(rollforward(&kit.ctx(&runner))), 0);
        assert!(runner
            .lines()
            .iter()
            .any(|l| l.ends_with("--switch-generation 12")));
    }

    #[test]
    fn rollforward_at_the_newest_generation_does_nothing() {
        let kit = Kit::new();
        let newest = "  11   2025-03-01 10:00:00   (current)\n";
        let runner = FakeRunner::new().on(
            "nix-env",
            &["-p"],
            Output {
                code: 0,
                stdout: newest.into(),
                stderr: String::new(),
            },
        );
        assert_eq!(block_on(rollforward(&kit.ctx(&runner))), 0);
        assert!(kit
            .sink
            .text()
            .contains("Already at the latest generation (11)"));
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn rollback_selects_the_previous_generation_then_activates_it() {
        let kit = Kit::new();
        let old = system(&kit, "old", None);
        let new = system(&kit, "new", None);
        point(&kit, &old, &new);
        let runner = profile_runner(LIST, &new);
        assert_eq!(block_on(rollback(&kit.ctx(&runner))), 0);
        let lines = runner.lines();
        let rollback_at = lines
            .iter()
            .position(|l| l.ends_with("--rollback"))
            .unwrap();
        let switch_at = lines
            .iter()
            .position(|l| l.ends_with("switch-to-configuration switch"))
            .unwrap();
        assert!(rollback_at < switch_at);
    }

    #[test]
    fn a_failed_rollback_does_not_activate_anything() {
        let kit = Kit::new();
        let old = system(&kit, "old", None);
        let new = system(&kit, "new", None);
        point(&kit, &old, &new);
        let runner = FakeRunner::new()
            .on(
                "nix-env",
                &["-p"],
                Output {
                    code: 0,
                    stdout: LIST.into(),
                    stderr: String::new(),
                },
            )
            .failing(
                "nix-env",
                &["--profile"],
                1,
                "no generation older than that",
            );
        assert_eq!(block_on(rollback(&kit.ctx(&runner))), 1);
        assert!(runner
            .lines()
            .iter()
            .all(|l| !l.contains("switch-to-configuration")));
    }
}
