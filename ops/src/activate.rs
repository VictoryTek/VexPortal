//! Making a built generation take effect.
//!
//! The same decision `activate_generation` in the justfile's `lib.sh` makes: apply the
//! system profile's current generation to the running session, unless that would start,
//! stop or swap the display manager. Doing that under a live graphical session is the
//! known black-screen case, so those generations are only made the next boot entry and
//! the session keeps running on the old one until the person reboots.
//!
//! Unlike the recipe, this never reboots by itself. A reboot is its own confirmed action.

use crate::fsutil;
use crate::runner::{Cmd, Runner};
use crate::{fail, stream, Ctx};
use std::path::{Path, PathBuf};

/// How a generation was activated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activated {
    /// It is the next boot entry but not running yet; the machine has to reboot.
    pub queued: bool,
}

pub fn profile(root: &Path) -> PathBuf {
    root.join("nix/var/nix/profiles/system")
}

pub fn current_system(root: &Path) -> PathBuf {
    root.join("run/current-system")
}

/// Does the system at `top` run a display manager?
pub fn has_display_manager(top: &Path) -> bool {
    top.join("etc/systemd/system/display-manager.service")
        .exists()
}

/// The unit `display-manager.service` resolves to: `greetd.service`, or GDM's own
/// `display-manager.service`.
fn display_manager_unit(top: &Path) -> Option<String> {
    let unit = top.join("etc/systemd/system/display-manager.service");
    std::fs::canonicalize(unit)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
}

/// Record that this boot is stale. `/run` is cleared by the reboot, so the marker goes
/// away on its own; the portal reads it to offer a reboot.
pub fn mark_reboot_required(root: &Path) {
    let _ = fsutil::write_atomic(&root.join("run/reboot-required"), b"", 0o644);
}

/// Activate the system profile's current generation. `Err` is the exit code to stop with.
pub async fn activate<R: Runner>(ctx: &Ctx<'_, R>) -> Result<Activated, i32> {
    let profile = profile(ctx.root);
    let top = std::fs::canonicalize(&profile).map_err(|e| {
        fail(
            ctx.sink,
            format!("cannot resolve {}: {e}", profile.display()),
        )
    })?;
    let running = current_system(ctx.root);

    let switch = |action: &str| {
        Cmd::new(
            top.join("bin/switch-to-configuration")
                .display()
                .to_string(),
        )
        .arg(action)
    };

    if has_display_manager(&running) != has_display_manager(&top) {
        ctx.sink.info(
            "This generation changes between graphical and headless, so it cannot be applied \
             to the running session. Making it the next boot entry instead.",
        );
        return queue(ctx, &switch("boot")).await;
    }
    if display_manager_unit(&running) != display_manager_unit(&top) {
        ctx.sink.info(
            "This generation swaps the display manager, which cannot be done under a running \
             session. Making it the next boot entry instead.",
        );
        return queue(ctx, &switch("boot")).await;
    }

    match stream(ctx, &switch("switch")).await? {
        0 => Ok(Activated { queued: false }),
        // switch-to-configuration reports 4 when a unit failed to restart. The generation
        // is active; the person needs to know which units to look at.
        4 => {
            ctx.sink.error(
                "Warning: the generation is active, but one or more units failed to start or \
                 restart (see above). A reboot completes the transition.",
            );
            Ok(Activated { queued: false })
        }
        code => Err(code),
    }
}

async fn queue<R: Runner>(ctx: &Ctx<'_, R>, boot: &Cmd) -> Result<Activated, i32> {
    match stream(ctx, boot).await? {
        0 => {
            mark_reboot_required(ctx.root);
            ctx.sink
                .info("Queued. Reboot to start it; this session keeps running until then.");
            Ok(Activated { queued: true })
        }
        code => Err(code),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::runner::testing::{block_on, FakeRunner};
    use crate::testkit::Kit;
    use std::os::unix::fs::symlink;

    /// Build a system tree under the kit's root and return its path.
    pub fn system(kit: &Kit, name: &str, display_manager: Option<&str>) -> PathBuf {
        let top = kit.root.join(&format!("nix/store/{name}"));
        std::fs::create_dir_all(top.join("etc/systemd/system")).unwrap();
        std::fs::create_dir_all(top.join("bin")).unwrap();
        if let Some(unit) = display_manager {
            let target = top.join(format!("etc/systemd/system/{unit}"));
            std::fs::write(&target, "[Unit]\n").unwrap();
            let link = top.join("etc/systemd/system/display-manager.service");
            if unit != "display-manager.service" {
                symlink(&target, link).unwrap();
            }
        }
        top
    }

    pub fn point(kit: &Kit, running: &Path, profile_gen: &Path) {
        std::fs::create_dir_all(kit.root.join("nix/var/nix/profiles")).unwrap();
        let _ = std::fs::remove_file(current_system(kit.root.path()));
        let _ = std::fs::remove_file(profile(kit.root.path()));
        symlink(running, current_system(kit.root.path())).unwrap();
        symlink(profile_gen, profile(kit.root.path())).unwrap();
    }

    fn lines(runner: &FakeRunner) -> Vec<String> {
        runner.lines()
    }

    #[test]
    fn an_ordinary_generation_is_switched_live() {
        let kit = Kit::new();
        let old = system(&kit, "old", Some("display-manager.service"));
        let new = system(&kit, "new", Some("display-manager.service"));
        point(&kit, &old, &new);
        let runner = FakeRunner::new().ok(
            &new.join("bin/switch-to-configuration")
                .display()
                .to_string(),
            &["switch"],
        );
        let done = block_on(activate(&kit.ctx(&runner))).unwrap();
        assert!(!done.queued);
        assert!(lines(&runner)[0].ends_with("switch-to-configuration switch"));
        assert!(!kit.root.join("run/reboot-required").exists());
    }

    #[test]
    fn graphical_to_headless_is_queued_not_applied() {
        let kit = Kit::new();
        let old = system(&kit, "old", Some("display-manager.service"));
        let new = system(&kit, "new", None);
        point(&kit, &old, &new);
        let runner = FakeRunner::new().ok(
            &new.join("bin/switch-to-configuration")
                .display()
                .to_string(),
            &["boot"],
        );
        let done = block_on(activate(&kit.ctx(&runner))).unwrap();
        assert!(done.queued);
        assert!(lines(&runner)[0].ends_with("switch-to-configuration boot"));
        assert!(kit.root.join("run/reboot-required").exists());
        // No reboot was issued.
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn a_swapped_display_manager_is_queued() {
        let kit = Kit::new();
        let old = system(&kit, "old", Some("display-manager.service"));
        let new = system(&kit, "new", Some("greetd.service"));
        point(&kit, &old, &new);
        let runner = FakeRunner::new().ok(
            &new.join("bin/switch-to-configuration")
                .display()
                .to_string(),
            &["boot"],
        );
        assert!(block_on(activate(&kit.ctx(&runner))).unwrap().queued);
    }

    #[test]
    fn units_that_fail_to_restart_are_a_warning_not_a_failed_job() {
        let kit = Kit::new();
        let old = system(&kit, "old", None);
        let new = system(&kit, "new", None);
        point(&kit, &old, &new);
        let runner = FakeRunner::new().failing(
            &new.join("bin/switch-to-configuration")
                .display()
                .to_string(),
            &["switch"],
            4,
            "warning: the following units failed: foo.service",
        );
        let done = block_on(activate(&kit.ctx(&runner))).unwrap();
        assert!(!done.queued);
        assert!(kit.sink.text().contains("failed to start or restart"));
    }

    #[test]
    fn any_other_failure_stops_the_job_with_its_code() {
        let kit = Kit::new();
        let old = system(&kit, "old", None);
        let new = system(&kit, "new", None);
        point(&kit, &old, &new);
        let runner = FakeRunner::new().failing(
            &new.join("bin/switch-to-configuration")
                .display()
                .to_string(),
            &["switch"],
            2,
            "activation failed",
        );
        assert_eq!(block_on(activate(&kit.ctx(&runner))).unwrap_err(), 2);
    }

    #[test]
    fn a_missing_profile_is_an_error() {
        let kit = Kit::new();
        let runner = FakeRunner::new();
        assert_eq!(block_on(activate(&kit.ctx(&runner))).unwrap_err(), 1);
        assert!(kit.sink.text().contains("cannot resolve"));
    }
}
