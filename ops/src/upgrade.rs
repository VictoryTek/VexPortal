//! `upgrade-analysis`: would this configuration survive a move to a newer NixOS release?
//!
//! Read-only. It evaluates the host's configuration against the new release's nixpkgs and
//! home-manager without touching `flake.lock`, then looks at what that build would have to
//! compile. A kernel source build takes hours, so it is the thing worth warning about.

use crate::runner::{Cmd, Runner};
use crate::system::read_variant;
use crate::{capture, fail, Ctx};
use std::path::Path;

/// The release the host follows now, from the `nixpkgs` node of `flake.lock`. The exact
/// node name, so `nixpkgs-unstable` and the like are not picked up.
pub fn current_release(lock: &str) -> String {
    serde_json::from_str::<serde_json::Value>(lock)
        .ok()
        .and_then(|v| {
            v.pointer("/nodes/nixpkgs/original/ref")
                .and_then(|r| r.as_str().map(str::to_string))
        })
        .map(|r| r.strip_prefix("nixos-").unwrap_or(&r).to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// A release such as `26.11`.
fn valid_release(text: &str) -> bool {
    let mut parts = text.split('.');
    let (Some(year), Some(month), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    [year, month]
        .iter()
        .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_digit()))
}

/// What the dry build said it would do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub to_build: Vec<String>,
    pub to_fetch: Vec<String>,
}

/// The derivation name of a store path line: `/nix/store/<hash>-name` becomes `name`.
fn store_name(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("/nix/store/")?;
    let (hash, name) = rest.split_once('-')?;
    (hash.len() == 32).then(|| name.to_string())
}

/// Read the "will be built:" and "will be fetched" lists out of `nix`'s dry-run text.
pub fn parse_plan(output: &str) -> Plan {
    #[derive(Clone, Copy, PartialEq)]
    enum Section {
        None,
        Build,
        Fetch,
    }
    let mut plan = Plan::default();
    let mut section = Section::None;
    for line in output.lines() {
        if line.contains("will be built:") {
            section = Section::Build;
        } else if line.contains("will be fetched") {
            section = Section::Fetch;
        } else if line.starts_with("building ") || !line.starts_with(char::is_whitespace) {
            // A new unindented line ends the list.
            section = Section::None;
        } else if let Some(name) = store_name(line) {
            match section {
                Section::Build => plan.to_build.push(name),
                Section::Fetch => plan.to_fetch.push(name),
                Section::None => {}
            }
        }
    }
    plan
}

/// The kernel source build: `linux-7.1`, `linux-7.1-rc2`, with or without `.drv`. The
/// kernel module aggregates and NVIDIA userspace are local builds of minutes, not this.
/// The same rule `vexos-update` applies.
pub fn is_heavy(name: &str) -> bool {
    let name = name.strip_suffix(".drv").unwrap_or(name);
    let Some(version) = name.strip_prefix("linux-") else {
        return false;
    };
    let (numbers, rc) = match version.split_once("-rc") {
        Some((numbers, rc)) => (numbers, Some(rc)),
        None => (version, None),
    };
    let numbers_ok = numbers.chars().next().is_some_and(|c| c.is_ascii_digit())
        && numbers.chars().all(|c| c.is_ascii_digit() || c == '.');
    let rc_ok = rc.is_none_or(|rc| !rc.is_empty() && rc.chars().all(|c| c.is_ascii_digit()));
    numbers_ok && rc_ok
}

fn rule(sink: &dyn crate::runner::Sink) {
    sink.info("────────────────────────────────────────────────────────────────");
}

pub async fn analyse<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    match analyse_inner(ctx).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async fn analyse_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    let target = ctx
        .args
        .get("target_version")
        .map(String::as_str)
        .unwrap_or_default();
    if !valid_release(target) {
        return Err(fail(ctx.sink, "the target release must look like 26.11"));
    }
    let Some(variant) = read_variant(ctx.nixos_dir) else {
        return Err(fail(
            ctx.sink,
            "/etc/nixos/vexos-variant is missing. Run Switch Role or GPU first.",
        ));
    };
    if crate::system::refuse_legacy(ctx) {
        return Err(1);
    }

    let current = current_release(
        &std::fs::read_to_string(ctx.nixos_dir.join("flake.lock")).unwrap_or_default(),
    );
    let sink = ctx.sink;
    sink.info(&format!("Upgrade analysis: {current} → {target}"));
    sink.info(&format!("Variant: {variant}"));
    sink.info("Read-only: nothing on this system is changed. The first run may take a minute to fetch the new nixpkgs.");
    sink.info("");

    // Read-only means the repository too: it is not created, staged or committed to here.
    // The analysis sees what a rebuild last committed.
    let repo = capture(
        ctx,
        &Cmd::new("git")
            .arg("-C")
            .arg(ctx.nixos_dir.display().to_string())
            .args(["rev-parse", "--git-dir"]),
    )
    .await?;
    if !repo.success() {
        return Err(fail(
            sink,
            "/etc/nixos is not a git repository yet. Run Rebuild System once, then analyse.",
        ));
    }
    sink.info("The analysis uses the configuration as last committed by a rebuild.");
    sink.info("");

    rule(sink);
    sink.info("[1/3] Does the configuration evaluate?");
    rule(sink);
    let dry = capture(ctx, &dry_build(ctx.nixos_dir, &variant, target)).await?;
    let combined = format!("{}{}", dry.stdout, dry.stderr);
    let evaluates = dry.success();
    if evaluates {
        sink.info(&format!(
            "✓ PASS  The configuration evaluates cleanly against nixos-{target}."
        ));
    } else {
        sink.error(&format!(
            "✗ FAIL  The configuration does not evaluate against nixos-{target}."
        ));
        for line in combined.lines().filter(|l| !l.starts_with("[sudo]")) {
            sink.error(&format!("    {line}"));
        }
        sink.info("Common causes: an option was renamed or removed, a package left nixpkgs, or a module interface changed.");
        sink.info("Release notes: https://nixos.org/manual/nixos/stable/release-notes");
    }
    sink.info("");

    rule(sink);
    sink.info("[2/3] Package cache");
    rule(sink);
    let mut heavy: Vec<String> = Vec::new();
    if evaluates {
        let plan = parse_plan(&combined);
        let (hours, minutes): (Vec<String>, Vec<String>) =
            plan.to_build.into_iter().partition(|n| is_heavy(n));
        sink.info(&format!(
            "{:>5}  in the binary cache, ready to fetch",
            plan.to_fetch.len()
        ));
        sink.info(&format!(
            "{:>5}  other local builds (minutes; the update proceeds)",
            minutes.len()
        ));
        sink.info(&format!(
            "{:>5}  kernel source builds (hours; the update would stop)",
            hours.len()
        ));
        for name in &minutes {
            sink.info(&format!("    local build: {name}"));
        }
        for name in &hours {
            sink.info(&format!("    kernel not in cache: {name}"));
        }
        heavy = hours;
    } else {
        sink.info("Skipped: fix the evaluation errors first.");
    }
    sink.info("");

    rule(sink);
    sink.info("[3/3] Recommendation");
    rule(sink);
    if !evaluates {
        sink.error(
            "✗ NOT READY: resolve the configuration errors above first, then run this again.",
        );
    } else if !heavy.is_empty() {
        sink.info(&format!(
            "⚠ CONFIG OK, but {} kernel package(s) are not in the cache yet.",
            heavy.len()
        ));
        sink.info("Wait and run this again in a day or two (recommended; the cache usually fills within 1-3 days), or upgrade now and accept compiling the kernel here.");
    } else {
        sink.info("✓ READY: the configuration is clean and everything is in the cache.");
    }
    sink.info("");
    sink.info("Release notes:  https://nixos.org/manual/nixos/stable/release-notes");
    sink.info(&format!(
        "Package search: https://search.nixos.org/packages?channel={target}"
    ));
    Ok(())
}

fn dry_build(nixos_dir: &Path, variant: &str, release: &str) -> Cmd {
    Cmd::new("nixos-rebuild").args([
        "dry-build".to_string(),
        "--flake".to_string(),
        format!("git+file://{}#{variant}", nixos_dir.display()),
        "--override-input".to_string(),
        "vexos-nix/nixpkgs".to_string(),
        format!("github:NixOS/nixpkgs/nixos-{release}"),
        "--override-input".to_string(),
        "vexos-nix/home-manager".to_string(),
        format!("github:nix-community/home-manager/release-{release}"),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::testing::{block_on, FakeRunner};
    use crate::runner::Output;
    use crate::testkit::Kit;

    const HASH: &str = "abcdefghijklmnopqrstuvwxyz012345";

    #[test]
    fn the_current_release_is_the_exact_nixpkgs_node() {
        let lock = r#"{"nodes":{"nixpkgs-unstable":{"original":{"ref":"nixos-unstable"}},
            "nixpkgs":{"original":{"ref":"nixos-25.11"}}}}"#;
        assert_eq!(current_release(lock), "25.11");
        assert_eq!(current_release("not json"), "unknown");
        assert_eq!(current_release("{}"), "unknown");
    }

    #[test]
    fn releases_must_look_like_releases() {
        assert!(valid_release("26.11"));
        for bad in [
            "",
            "26",
            "26.1",
            "26.11.1",
            "ab.cd",
            "26.11; reboot",
            "2611",
        ] {
            assert!(!valid_release(bad), "{bad}");
        }
    }

    #[test]
    fn only_the_kernel_source_build_is_heavy() {
        for heavy in [
            "linux-7.1",
            "linux-6.18.4",
            "linux-7.2-rc3",
            "linux-7.1.drv",
        ] {
            assert!(is_heavy(heavy), "{heavy}");
        }
        for light in [
            "linux-7.1-modules",
            "linux-firmware-20260101",
            "nvidia-x11-580.1-6.18",
            "linux-",
            "linux-rc2",
            "linux-7.1-rc",
            "firefox-130",
        ] {
            assert!(!is_heavy(light), "{light}");
        }
    }

    #[test]
    fn the_dry_run_lists_are_read_by_section() {
        let text = format!(
            "this derivation will be built:\n  /nix/store/{HASH}-linux-7.1.drv\n  /nix/store/{HASH}-hello-2.12.drv\n\
             these 2 paths will be fetched (10 MiB download, 20 MiB unpacked):\n  /nix/store/{HASH}-glibc-2.40\n  /nix/store/{HASH}-bash-5.2\n\
             done\n  /nix/store/{HASH}-ignored\n"
        );
        let plan = parse_plan(&text);
        assert_eq!(plan.to_build, ["linux-7.1.drv", "hello-2.12.drv"]);
        assert_eq!(plan.to_fetch, ["glibc-2.40", "bash-5.2"]);
    }

    fn ready(kit: &Kit) {
        kit.nixos_file("vexos-variant", "vexos-desktop-amd\n");
        kit.nixos_file(
            "flake.lock",
            r#"{"nodes":{"nixpkgs":{"original":{"ref":"nixos-25.11"}}}}"#,
        );
    }

    fn runner(dry: Output) -> FakeRunner {
        FakeRunner::new()
            .ok("git", &[])
            .on("nixos-rebuild", &["dry-build"], dry)
    }

    #[test]
    fn a_clean_config_with_no_kernel_build_is_ready() {
        let kit = Kit::new().with_args(&[("target_version", "26.11")]);
        ready(&kit);
        let runner = runner(Output {
            code: 0,
            stdout: format!("these paths will be fetched:\n  /nix/store/{HASH}-bash-5.2\n"),
            stderr: String::new(),
        });
        assert_eq!(block_on(analyse(&kit.ctx(&runner))), 0);
        let text = kit.sink.text();
        assert!(text.contains("25.11 → 26.11"), "{text}");
        assert!(text.contains("✓ PASS"));
        assert!(text.contains("✓ READY"));
        let dry = runner
            .calls()
            .into_iter()
            .find(|c| c.program == "nixos-rebuild")
            .unwrap();
        assert!(dry
            .args
            .contains(&"github:NixOS/nixpkgs/nixos-26.11".to_string()));
        assert!(dry
            .args
            .contains(&"github:nix-community/home-manager/release-26.11".to_string()));
    }

    #[test]
    fn a_missing_kernel_is_reported_as_a_wait() {
        let kit = Kit::new().with_args(&[("target_version", "26.11")]);
        ready(&kit);
        let runner = runner(Output {
            code: 0,
            stdout: format!("this derivation will be built:\n  /nix/store/{HASH}-linux-7.1.drv\n"),
            stderr: String::new(),
        });
        assert_eq!(block_on(analyse(&kit.ctx(&runner))), 0);
        assert!(kit
            .sink
            .text()
            .contains("1 kernel package(s) are not in the cache"));
    }

    #[test]
    fn an_evaluation_failure_prints_the_errors_and_is_not_ready() {
        let kit = Kit::new().with_args(&[("target_version", "26.11")]);
        ready(&kit);
        let runner = runner(Output {
            code: 1,
            stdout: String::new(),
            stderr: "error: The option `services.foo` has been removed.\n".into(),
        });
        // The analysis itself ran, so the job succeeds; the report says NOT READY.
        assert_eq!(block_on(analyse(&kit.ctx(&runner))), 0);
        let text = kit.sink.text();
        assert!(text.contains("✗ FAIL"));
        assert!(text.contains("services.foo"));
        assert!(text.contains("NOT READY"));
    }

    #[test]
    fn analysis_never_touches_the_repository() {
        let kit = Kit::new().with_args(&[("target_version", "26.11")]);
        ready(&kit);
        let runner = runner(Output::default());
        block_on(analyse(&kit.ctx(&runner)));
        for line in runner.lines() {
            assert!(
                !line.starts_with("git") || line.contains("rev-parse"),
                "unexpected git call: {line}"
            );
        }
    }

    #[test]
    fn without_a_repository_it_says_to_rebuild_first() {
        let kit = Kit::new().with_args(&[("target_version", "26.11")]);
        ready(&kit);
        let runner = FakeRunner::new().failing("git", &["-C"], 128, "not a git repository");
        assert_eq!(block_on(analyse(&kit.ctx(&runner))), 1);
        assert!(kit.sink.text().contains("Run Rebuild System once"));
        assert!(runner
            .lines()
            .iter()
            .all(|l| !l.starts_with("nixos-rebuild")));
    }

    #[test]
    fn a_bad_release_is_refused_before_anything_runs() {
        let kit = Kit::new().with_args(&[("target_version", "26.11; reboot")]);
        ready(&kit);
        let runner = FakeRunner::new();
        assert_eq!(block_on(analyse(&kit.ctx(&runner))), 1);
        assert!(runner.calls().is_empty());
    }
}
