//! Building and switching the system: `rebuild`, `switch` and `build`.
//!
//! Every build goes through `git+file:///etc/nixos`, the way `vexos-update` does it, so
//! `secrets/` stays out of the Nix store. [`gitrepo::prepare`] makes that repository
//! ready first. A build is always `nixos-rebuild boot` followed by [`activate`], never
//! `switch`, so a change that cannot be applied live is queued for the next boot
//! instead of breaking the running session.

use crate::nixedit::{self, NixValue};
use crate::runner::{Cmd, Runner};
use crate::sidefile::{self, Changes};
use crate::system::{read_variant, refuse_legacy, target_name};
use crate::{activate, fail, fsutil, gitrepo, stream, Ctx};
use std::path::Path;

/// The flake reference for the host's configuration, once the repository is ready.
pub(crate) async fn flake_base<R: Runner>(ctx: &Ctx<'_, R>) -> Result<String, i32> {
    if refuse_legacy(ctx) {
        return Err(1);
    }
    gitrepo::prepare(ctx.runner, ctx.nixos_dir, ctx.sink)
        .await
        .map_err(|e| fail(ctx.sink, e))?;
    Ok(format!("git+file://{}", ctx.nixos_dir.display()))
}

pub(crate) fn boot_command(base: &str, target: &str) -> Cmd {
    Cmd::new("nixos-rebuild").args(["boot", "--flake", &format!("{base}#{target}")])
}

/// Does the flake offer this target? The wrapper names every variant it builds, so a
/// quoted name in `flake.nix` is the test, the same one `just` makes.
fn target_exists(nixos_dir: &Path, target: &str) -> bool {
    std::fs::read_to_string(nixos_dir.join("flake.nix"))
        .is_ok_and(|text| text.contains(&format!("\"{target}\"")))
}

fn requested_target<R: Runner>(ctx: &Ctx<'_, R>) -> Result<String, i32> {
    let (Some(role), Some(gpu)) = (ctx.args.get("role"), ctx.args.get("variant")) else {
        return Err(fail(ctx.sink, "a role and a GPU variant are required"));
    };
    let target = target_name(role, gpu).ok_or_else(|| {
        fail(
            ctx.sink,
            format!("`{role}` / `{gpu}` is not a role and GPU variant"),
        )
    })?;
    if !target_exists(ctx.nixos_dir, &target) {
        return Err(fail(
            ctx.sink,
            format!(
                "the flake in {} has no target `{target}`",
                ctx.nixos_dir.display()
            ),
        ));
    }
    Ok(target)
}

/// What the kernel cache check found.
#[derive(Debug, PartialEq, Eq)]
enum Guard {
    Clear,
    Blocked { version: String, url: String },
}

/// Refuse to rebuild when a custom kernel is enabled but the pinned version is not yet in
/// the binary cache, which would otherwise compile the kernel on this machine for hours.
///
/// Anything unclear (feature off, no cache configured, nothing to evaluate) lets the
/// rebuild go ahead. Only a clear "this kernel is not in the cache" stops it.
async fn kernel_cache_guard<R: Runner>(
    ctx: &Ctx<'_, R>,
    base: &str,
    target: &str,
) -> Result<Guard, i32> {
    let features = std::fs::read_to_string(ctx.nixos_dir.join("features.nix")).unwrap_or_default();
    if !nixedit::is_set(
        &features,
        "vexos.features.kernel.enable",
        &NixValue::Bool(true),
    ) {
        return Ok(Guard::Clear);
    }

    let eval = |attr: &str| {
        Cmd::new("nix").args([
            "eval".to_string(),
            "--raw".to_string(),
            format!("{base}#nixosConfigurations.{target}.config.{attr}"),
        ])
    };
    // A probe that cannot run is "unclear", not a failure; only a cancel stops the job.
    let probe = |cmd: Cmd| async move {
        match ctx.runner.run(&cmd).await {
            Ok(out) if out.success() => Ok(Some(out.stdout.trim().to_string())),
            Ok(_) | Err(crate::runner::RunError::Spawn { .. }) => Ok(None),
            Err(crate::runner::RunError::Cancelled) => {
                ctx.sink.error("Cancelled.");
                Err(1)
            }
        }
    };

    let Some(out_path) = probe(eval("boot.kernelPackages.kernel.outPath"))
        .await?
        .filter(|p| !p.is_empty())
    else {
        return Ok(Guard::Clear);
    };
    // Already in this machine's store: nothing to download.
    if ctx.root.join(out_path.trim_start_matches('/')).exists() {
        return Ok(Guard::Clear);
    }
    let Some(url) = probe(eval("vexos.harmonia.cacheUrl"))
        .await?
        .filter(|u| !u.is_empty() && u != "null")
    else {
        return Ok(Guard::Clear);
    };
    // The URL is passed to curl as one argument; a scheme check keeps it from ever being
    // read as an option.
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Ok(Guard::Clear);
    }
    let Some(hash) = Path::new(&out_path)
        .file_name()
        .map(|n| n.to_string_lossy().chars().take(32).collect::<String>())
    else {
        return Ok(Guard::Clear);
    };

    let curl = Cmd::new("curl").args([
        "-s".to_string(),
        "-o".to_string(),
        "/dev/null".to_string(),
        "-w".to_string(),
        "%{http_code}".to_string(),
        "--max-time".to_string(),
        "10".to_string(),
        format!("{url}/{hash}.narinfo"),
    ]);
    if probe(curl).await?.is_none_or(|code| code == "200") {
        return Ok(Guard::Clear);
    }

    let version = probe(eval("boot.kernelPackages.kernel.version"))
        .await?
        .unwrap_or_else(|| "?".to_string());
    Ok(Guard::Blocked { version, url })
}

pub async fn rebuild<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    match rebuild_inner(ctx).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async fn rebuild_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    let Some(target) = read_variant(ctx.nixos_dir) else {
        return Err(fail(
            ctx.sink,
            "/etc/nixos/vexos-variant is missing or unreadable. Run Switch Role or GPU once to \
             pick a variant.",
        ));
    };
    let base = flake_base(ctx).await?;

    if ctx.args.get("kernel_cache").map(String::as_str) == Some("skip") {
        ctx.sink.info("Skipping the kernel cache check as asked.");
    } else if let Guard::Blocked { version, url } = kernel_cache_guard(ctx, &base, &target).await? {
        ctx.sink.error(&format!(
            "The custom kernel {version} is not in the cache at {url}."
        ));
        ctx.sink.error(
            "Rebuilding now would compile the kernel on this machine, which takes hours. \
             Either wait for the build host's nightly build, run Build Kernel on the build host, \
             turn the kernel feature off, or run this again with the kernel cache check set to \
             skip to compile it here.",
        );
        return Err(1);
    }

    ctx.sink.info(&format!("Rebuilding {target}…"));
    expect_ok(stream(ctx, &boot_command(&base, &target)).await?)?;
    let activated = activate::activate(ctx).await?;
    finish(ctx, activated);
    Ok(())
}

fn expect_ok(code: i32) -> Result<(), i32> {
    if code == 0 {
        Ok(())
    } else {
        Err(code)
    }
}

fn finish<R: Runner>(ctx: &Ctx<'_, R>, activated: activate::Activated) {
    ctx.sink.info(if activated.queued {
        "Done. Reboot to start the new system."
    } else {
        "Done."
    });
}

/// `build`: compile a target without activating it.
pub async fn build<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    match build_inner(ctx).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async fn build_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    let target = requested_target(ctx)?;
    let base = flake_base(ctx).await?;
    // `nixos-rebuild build` leaves a `result` link in its working directory. A scratch
    // directory takes it, so a test build leaves nothing behind.
    let scratch = fsutil::Scratch::new().map_err(|e| fail(ctx.sink, e))?;
    let cmd = Cmd::new("nixos-rebuild")
        .args(["build", "--flake", &format!("{base}#{target}")])
        .cwd(scratch.path());
    ctx.sink
        .info(&format!("Building {target} (nothing is activated)…"));
    expect_ok(stream(ctx, &cmd).await?)?;
    ctx.sink.info(&format!(
        "{target} builds. Nothing on this machine changed."
    ));
    Ok(())
}

/// The hypervisor the host is set up for: `vm-platform.nix`, or the line an older
/// `just switch` wrote to `features.nix`. `qemu` is the module's own default.
fn current_vm_platform(nixos_dir: &Path) -> String {
    ["vm-platform.nix", "features.nix"]
        .iter()
        .find_map(|f| {
            let text = std::fs::read_to_string(nixos_dir.join(f)).ok()?;
            nixedit::get_str(&text, "vexos.vm.platform")
        })
        .unwrap_or_else(|| "qemu".to_string())
}

fn current_desktop(nixos_dir: &Path) -> String {
    std::fs::read_to_string(nixos_dir.join("features.nix"))
        .ok()
        .and_then(|text| nixedit::get_str(&text, "vexos.desktop.environment"))
        .unwrap_or_else(|| "gnome".to_string())
}

pub async fn switch<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let mut changes = Changes::default();
    let result = switch_inner(ctx, &mut changes).await;
    match result {
        Ok(()) => 0,
        Err(code) => {
            if !changes.is_empty() {
                ctx.sink
                    .info("The switch did not complete, so the settings it changed are put back.");
                changes.restore(ctx.sink).await;
            }
            code
        }
    }
}

async fn switch_inner<R: Runner>(ctx: &Ctx<'_, R>, changes: &mut Changes) -> Result<(), i32> {
    let target = requested_target(ctx)?;
    let role = ctx.args.get("role").map(String::as_str).unwrap_or_default();
    let gpu = ctx
        .args
        .get("variant")
        .map(String::as_str)
        .unwrap_or_default();
    if refuse_legacy(ctx) {
        return Err(1);
    }

    // Changes that cannot be applied to the running session, and why.
    let mut queue_reasons: Vec<String> = Vec::new();

    if gpu == "vm" {
        let old = current_vm_platform(ctx.nixos_dir);
        let new = ctx.args.get("vmp").cloned().unwrap_or_else(|| old.clone());
        if new != old {
            queue_reasons.push(format!(
                "VM platform is changing: {old} → {new} (kernel change)"
            ));
        }
        let vm_file = ctx.nixos_dir.join("vm-platform.nix");
        let features =
            std::fs::read_to_string(ctx.nixos_dir.join("features.nix")).unwrap_or_default();
        let value = NixValue::str(&new);
        // An older `just switch` wrote this to features.nix. Left alone it would hold a
        // second, different definition, which the module system rejects.
        if nixedit::get_str(&features, "vexos.vm.platform").is_some() {
            sidefile::set_option(
                ctx.runner,
                changes,
                &ctx.nixos_dir.join("features.nix"),
                sidefile::FEATURES_SEED,
                "vexos.vm.platform",
                &value,
            )
            .await
            .map_err(|e| fail(ctx.sink, e))?;
        }
        // `qemu` is the option's default, so it needs a file only if one is already there.
        if new != "qemu" || vm_file.exists() {
            sidefile::set_option(
                ctx.runner,
                changes,
                &vm_file,
                sidefile::VM_PLATFORM_SEED,
                "vexos.vm.platform",
                &value,
            )
            .await
            .map_err(|e| fail(ctx.sink, e))?;
        }
    } else if ctx.args.contains_key("vmp") {
        ctx.sink
            .info("The VM platform only applies to the vm variant; ignoring it.");
    }

    if role == "desktop" {
        let old = current_desktop(ctx.nixos_dir);
        let new = ctx.args.get("de").cloned().unwrap_or_else(|| old.clone());
        if new != old {
            queue_reasons.push(format!("Desktop environment is changing: {old} → {new}"));
        }
        let features = ctx.nixos_dir.join("features.nix");
        // gnome is the option's default, so it needs a file only if one is already there.
        if new != "gnome" || features.exists() {
            sidefile::set_option(
                ctx.runner,
                changes,
                &features,
                sidefile::FEATURES_SEED,
                "vexos.desktop.environment",
                &NixValue::str(&new),
            )
            .await
            .map_err(|e| fail(ctx.sink, e))?;
        }
    } else if ctx.args.contains_key("de") {
        ctx.sink
            .info("The desktop environment only applies to the desktop role; ignoring it.");
    }

    let running_graphical = activate::has_display_manager(&activate::current_system(ctx.root));
    let will_be_graphical = role != "headless-server";
    if running_graphical != will_be_graphical {
        queue_reasons.push(if will_be_graphical {
            "Switching from a headless system to a graphical one (a display manager starts)".into()
        } else {
            "Switching from a graphical system to a headless one (the display manager stops)".into()
        });
    }

    let base = flake_base(ctx).await?;
    ctx.sink.info(&format!("Switching to {target}…"));
    expect_ok(stream(ctx, &boot_command(&base, &target)).await?)?;
    // The new generation is installed as the boot default and was built from these
    // settings. From here a failure (activation, a cancel) must not take them back, or
    // the boot default and the files that describe it would disagree.
    *changes = Changes::default();

    if queue_reasons.is_empty() {
        let activated = activate::activate(ctx).await?;
        finish(ctx, activated);
        return Ok(());
    }

    for reason in &queue_reasons {
        ctx.sink.info(reason);
    }
    ctx.sink.info(&format!(
        "That is not safe to apply to this session, so {target} is the next boot entry instead."
    ));
    activate::mark_reboot_required(ctx.root);
    ctx.sink
        .info("Reboot to start it, then use Clean Up to remove leftovers from the old system.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activate::tests::{point, system};
    use crate::runner::testing::{block_on, FakeRunner};
    use crate::runner::Output;
    use crate::system::valid_target;
    use crate::testkit::Kit;

    const WRAPPER: &str = "{ vexos-desktop-amd = mkVariant; \"vexos-desktop-amd\"; \"vexos-desktop-vm\"; \"vexos-headless-server-amd\"; }\n";

    fn git_ready() -> FakeRunner {
        FakeRunner::new()
            .ok("git", &[])
            .ok("nix-instantiate", &["--parse"])
    }

    fn kit_with(args: &[(&str, &str)]) -> Kit {
        let kit = Kit::new().with_args(args);
        kit.nixos_file("flake.nix", WRAPPER);
        kit.nixos_file("vexos-variant", "vexos-desktop-amd\n");
        kit
    }

    fn switch_cmd(top: &Path, action: &str) -> (String, Vec<String>) {
        (
            top.join("bin/switch-to-configuration")
                .display()
                .to_string(),
            vec![action.to_string()],
        )
    }

    fn with_switch(runner: FakeRunner, top: &Path, action: &str) -> FakeRunner {
        let (program, args) = switch_cmd(top, action);
        runner.ok(&program, &[args[0].as_str()])
    }

    fn rebuild_runner(top: &Path) -> FakeRunner {
        with_switch(git_ready().ok("nixos-rebuild", &["boot"]), top, "switch")
    }

    #[test]
    fn rebuild_builds_the_current_variant_then_activates() {
        let kit = kit_with(&[]);
        let old = system(&kit, "old", Some("display-manager.service"));
        let new = system(&kit, "new", Some("display-manager.service"));
        point(&kit, &old, &new);
        let runner = rebuild_runner(&new);
        assert_eq!(block_on(rebuild(&kit.ctx(&runner))), 0);
        let lines = runner.lines();
        let build = lines
            .iter()
            .position(|l| l.starts_with("nixos-rebuild boot --flake git+file://"))
            .expect("a boot build");
        assert!(lines[build].ends_with("#vexos-desktop-amd"));
        assert!(lines[build + 1].ends_with("switch-to-configuration switch"));
        assert!(!lines.iter().any(|l| l.contains("--impure")));
    }

    #[test]
    fn rebuild_without_a_variant_file_says_what_to_do() {
        let kit = Kit::new();
        let runner = FakeRunner::new();
        assert_eq!(block_on(rebuild(&kit.ctx(&runner))), 1);
        assert!(kit.sink.text().contains("vexos-variant"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn a_failed_build_is_not_activated() {
        let kit = kit_with(&[]);
        let old = system(&kit, "old", None);
        let new = system(&kit, "new", None);
        point(&kit, &old, &new);
        let runner = git_ready().failing("nixos-rebuild", &["boot"], 1, "evaluation failed");
        assert_eq!(block_on(rebuild(&kit.ctx(&runner))), 1);
        assert!(runner
            .lines()
            .iter()
            .all(|l| !l.contains("switch-to-configuration")));
    }

    #[test]
    fn a_custom_kernel_missing_from_the_cache_stops_the_rebuild_before_the_build() {
        let kit = kit_with(&[]);
        kit.nixos_file(
            "features.nix",
            "{\n  vexos.features.kernel.enable = true;\n}\n",
        );
        let out = |text: &str| Output {
            code: 0,
            stdout: text.to_string(),
            stderr: String::new(),
        };
        let runner = git_ready()
            .on_arg(
                "nix",
                "kernel.outPath",
                out("/nix/store/abcdefghijklmnopqrstuvwxyz012345-linux-7.1"),
            )
            .on_arg("nix", "harmonia.cacheUrl", out("https://cache.example"))
            .on_arg("nix", "kernel.version", out("7.1"))
            .on("curl", &[], out("404"))
            .ok("nixos-rebuild", &["boot"]);
        assert_eq!(block_on(rebuild(&kit.ctx(&runner))), 1);
        assert!(
            kit.sink
                .text()
                .contains("kernel 7.1 is not in the cache at https://cache.example"),
            "{}",
            kit.sink.text()
        );
        assert!(runner
            .lines()
            .iter()
            .all(|l| !l.starts_with("nixos-rebuild")));
    }

    #[test]
    fn the_kernel_cache_check_can_be_skipped() {
        let kit = kit_with(&[("kernel_cache", "skip")]);
        kit.nixos_file(
            "features.nix",
            "{\n  vexos.features.kernel.enable = true;\n}\n",
        );
        let old = system(&kit, "old", None);
        let new = system(&kit, "new", None);
        point(&kit, &old, &new);
        let runner = rebuild_runner(&new);
        assert_eq!(block_on(rebuild(&kit.ctx(&runner))), 0);
        assert!(runner.lines().iter().all(|l| !l.starts_with("nix eval")));
    }

    #[test]
    fn a_kernel_already_in_the_store_is_not_checked_against_the_cache() {
        let kit = kit_with(&[]);
        kit.nixos_file(
            "features.nix",
            "{\n  vexos.features.kernel.enable = true;\n}\n",
        );
        let present = "nix/store/abcdefghijklmnopqrstuvwxyz012345-linux-7.1";
        std::fs::create_dir_all(kit.root.join(present)).unwrap();
        let old = system(&kit, "old", None);
        let new = system(&kit, "new", None);
        point(&kit, &old, &new);
        let runner = with_switch(
            git_ready()
                .on(
                    "nix",
                    &["eval", "--raw"],
                    Output {
                        code: 0,
                        stdout: format!("/{present}"),
                        stderr: String::new(),
                    },
                )
                .ok("nixos-rebuild", &["boot"]),
            &new,
            "switch",
        );
        assert_eq!(block_on(rebuild(&kit.ctx(&runner))), 0);
        assert!(runner.lines().iter().all(|l| !l.starts_with("curl")));
    }

    #[test]
    fn build_runs_in_a_scratch_directory_and_never_activates() {
        let kit = kit_with(&[("role", "desktop"), ("variant", "amd")]);
        let runner = git_ready().ok("nixos-rebuild", &["build"]);
        assert_eq!(block_on(build(&kit.ctx(&runner))), 0);
        let call = runner
            .calls()
            .into_iter()
            .find(|c| c.program == "nixos-rebuild")
            .unwrap();
        assert_eq!(call.args[0], "build");
        let cwd = call.cwd.expect("a working directory");
        assert_ne!(cwd, kit.nixos);
        assert!(!cwd.exists(), "the scratch directory is removed afterwards");
    }

    #[test]
    fn a_target_the_flake_does_not_offer_is_refused() {
        let kit = kit_with(&[("role", "htpc"), ("variant", "intel")]);
        let runner = FakeRunner::new();
        assert_eq!(block_on(build(&kit.ctx(&runner))), 1);
        assert!(kit.sink.text().contains("no target"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn switching_roles_with_nothing_special_builds_and_activates() {
        let kit = kit_with(&[("role", "desktop"), ("variant", "amd")]);
        let old = system(&kit, "old", Some("display-manager.service"));
        let new = system(&kit, "new", Some("display-manager.service"));
        point(&kit, &old, &new);
        let runner = rebuild_runner(&new);
        assert_eq!(block_on(switch(&kit.ctx(&runner))), 0);
        assert!(runner
            .lines()
            .iter()
            .any(|l| l.ends_with("switch-to-configuration switch")));
        assert!(!kit.root.join("run/reboot-required").exists());
    }

    #[test]
    fn a_desktop_environment_change_is_written_then_queued_not_applied() {
        let kit = kit_with(&[("role", "desktop"), ("variant", "amd"), ("de", "cosmic")]);
        let old = system(&kit, "old", Some("display-manager.service"));
        let new = system(&kit, "new", Some("display-manager.service"));
        point(&kit, &old, &new);
        let runner = rebuild_runner(&new);
        assert_eq!(block_on(switch(&kit.ctx(&runner))), 0);
        let features = std::fs::read_to_string(kit.nixos.join("features.nix")).unwrap();
        assert!(
            features.contains("vexos.desktop.environment = \"cosmic\";"),
            "{features}"
        );
        assert!(runner
            .lines()
            .iter()
            .all(|l| !l.contains("switch-to-configuration")));
        assert!(kit.root.join("run/reboot-required").exists());
        assert!(kit.sink.text().contains("gnome → cosmic"));
    }

    #[test]
    fn a_failed_build_puts_the_edited_settings_back() {
        let kit = kit_with(&[("role", "desktop"), ("variant", "amd"), ("de", "cosmic")]);
        let original = "{\n  vexos.desktop.environment = \"gnome\";\n}\n";
        kit.nixos_file("features.nix", original);
        let runner = git_ready().failing("nixos-rebuild", &["boot"], 1, "boom");
        assert_eq!(block_on(switch(&kit.ctx(&runner))), 1);
        assert_eq!(
            std::fs::read_to_string(kit.nixos.join("features.nix")).unwrap(),
            original
        );
    }

    #[test]
    fn a_new_vm_platform_goes_in_the_universal_side_file() {
        let kit = kit_with(&[
            ("role", "desktop"),
            ("variant", "vm"),
            ("vmp", "virtualbox"),
        ]);
        let old = system(&kit, "old", Some("display-manager.service"));
        let new = system(&kit, "new", Some("display-manager.service"));
        point(&kit, &old, &new);
        let runner = rebuild_runner(&new);
        assert_eq!(block_on(switch(&kit.ctx(&runner))), 0);
        let vm = std::fs::read_to_string(kit.nixos.join("vm-platform.nix")).unwrap();
        assert!(vm.contains("vexos.vm.platform = \"virtualbox\";"), "{vm}");
        // The role did not need a features.nix, and none was invented for it.
        assert!(!kit.nixos.join("features.nix").exists());
        assert!(kit.sink.text().contains("qemu → virtualbox"));
    }

    #[test]
    fn an_older_features_line_for_the_vm_platform_is_kept_in_step() {
        let kit = kit_with(&[("role", "server"), ("variant", "vm"), ("vmp", "qemu")]);
        kit.nixos_file(
            "flake.nix",
            "{ \"vexos-server-vm\"; }
",
        );
        kit.nixos_file(
            "features.nix",
            "{
  vexos.vm.platform = \"virtualbox\";
}
",
        );
        let old = system(&kit, "old", Some("display-manager.service"));
        let new = system(&kit, "new", Some("display-manager.service"));
        point(&kit, &old, &new);
        let runner = rebuild_runner(&new);
        assert_eq!(block_on(switch(&kit.ctx(&runner))), 0);
        // The line was updated in place, not left disagreeing with a new vm-platform.nix.
        let features = std::fs::read_to_string(kit.nixos.join("features.nix")).unwrap();
        assert!(
            features.contains("vexos.vm.platform = \"qemu\";"),
            "{features}"
        );
        assert!(!features.contains("virtualbox"));
        assert!(!kit.nixos.join("vm-platform.nix").exists());
    }

    #[test]
    fn a_failed_activation_keeps_the_settings_the_boot_entry_was_built_from() {
        let kit = kit_with(&[("role", "desktop"), ("variant", "amd"), ("de", "gnome")]);
        kit.nixos_file(
            "features.nix",
            "{
  vexos.desktop.environment = \"gnome\";
}
",
        );
        let old = system(&kit, "old", Some("display-manager.service"));
        let new = system(&kit, "new", Some("display-manager.service"));
        point(&kit, &old, &new);
        let (program, _) = switch_cmd(&new, "switch");
        let runner = git_ready().ok("nixos-rebuild", &["boot"]).failing(
            &program,
            &["switch"],
            2,
            "activation failed",
        );
        assert_eq!(block_on(switch(&kit.ctx(&runner))), 2);
        let features = std::fs::read_to_string(kit.nixos.join("features.nix")).unwrap();
        assert!(features.contains("\"gnome\""), "{features}");
        assert!(!kit.sink.text().contains("Restored"));
    }

    #[test]
    fn going_headless_is_queued_even_with_no_setting_changed() {
        let kit = kit_with(&[("role", "headless-server"), ("variant", "amd")]);
        let old = system(&kit, "old", Some("display-manager.service"));
        let new = system(&kit, "new", None);
        point(&kit, &old, &new);
        let runner = rebuild_runner(&new);
        assert_eq!(block_on(switch(&kit.ctx(&runner))), 0);
        assert!(runner
            .lines()
            .iter()
            .all(|l| !l.contains("switch-to-configuration")));
        assert!(kit.sink.text().contains("headless"));
    }

    #[test]
    fn target_names_read_from_files_must_have_the_shape_of_one() {
        assert!(valid_target("vexos-desktop-amd"));
        assert!(!valid_target("desktop"));
        assert!(!valid_target("vexos-a b"));
    }
}
