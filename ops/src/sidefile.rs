//! Editing the `/etc/nixos` side files for an operation that may have to take its edits
//! back.
//!
//! `switch` writes the desktop environment and the hypervisor before it builds. If the
//! build then fails, leaving those lines would make every later rebuild fail the same
//! way, so the edits are recorded and undone.
//!
//! Undoing must never throw away somebody else's change. The daemon runs several jobs at
//! once, and another job may edit the same file while a build runs. So what each edit
//! saw and left behind is recorded from inside the edit, under the file's lock, and a
//! file is put back only if its content is still exactly what this left in it, with no
//! change by anyone else in between.

use crate::fsutil;
use crate::nixedit::{self, EditError, NixValue};
use crate::runner::Runner;
use std::path::{Path, PathBuf};

/// A file this host did not have before.
pub const FEATURES_SEED: &str =
    "# /etc/nixos/features.nix\n# Optional feature toggles for this VexOS host.\n{\n}\n";
pub const VM_PLATFORM_SEED: &str =
    "# /etc/nixos/vm-platform.nix\n# Hypervisor for the vm variant.\n{\n}\n";

struct Saved {
    path: PathBuf,
    /// What the file held just before this first edited it; `None` if this created it.
    before: Option<String>,
    /// What this last left in it.
    written: String,
    /// Something else changed the file between two of this operation's edits, so its
    /// own changes can no longer be told apart from theirs.
    tainted: bool,
}

/// The files an operation changed.
#[derive(Default)]
pub struct Changes {
    saved: Vec<Saved>,
}

impl Changes {
    fn record(&mut self, path: &Path, created: bool, seed: &str, saw: String, left: String) {
        match self.saved.iter_mut().find(|s| s.path == path) {
            Some(saved) => {
                if saved.written != saw {
                    saved.tainted = true;
                }
                saved.written = left;
            }
            None => self.saved.push(Saved {
                path: path.to_path_buf(),
                // A file this created is removed again only if nobody touched it before the
                // first edit; otherwise the other writer's content is what goes back.
                before: (!created || saw != seed).then_some(saw),
                written: left,
                tainted: false,
            }),
        }
    }

    /// Put every file back as it was, where that is safe. Best effort: a file that cannot
    /// be restored is named, since the person then has to look at it.
    ///
    /// A file something else changed (another job enabling a feature while a build ran)
    /// is left as it is: putting the old content back would throw that change away.
    pub async fn restore(&self, sink: &dyn crate::runner::Sink) {
        for saved in &self.saved {
            let path = &saved.path;
            let result = if saved.tainted {
                Ok(false)
            } else {
                nixedit::replace_if_unchanged(path, &saved.written, saved.before.as_deref()).await
            };
            match result {
                Ok(true) => sink.info(&format!("Restored {}", path.display())),
                Ok(false) => sink.error(&format!(
                    "{} was changed by something else in the meantime, so it was left as it is.",
                    path.display()
                )),
                Err(e) => sink.error(&format!("Could not restore {}: {e}", path.display())),
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.saved.is_empty()
    }
}

/// Set `opt` in `path`, creating the file from `seed` first if it is missing.
pub async fn set_option<R: Runner>(
    runner: &R,
    changes: &mut Changes,
    path: &Path,
    seed: &str,
    opt: &str,
    value: &NixValue,
) -> Result<(), EditError> {
    let created = match fsutil::create_new(path, seed.as_bytes(), 0o644) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(e.into()),
    };

    // The edit may run more than once if something outside changes the file under it;
    // the last run is the one that took effect, so that is what is kept.
    let mut saw = String::new();
    let mut left = String::new();
    let edited = nixedit::edit_file(runner, path, |text| {
        saw = text.to_string();
        let updated = nixedit::set(text, opt, value)?;
        left = updated.clone();
        Ok(Some(updated))
    })
    .await;

    match edited {
        Ok(_) => {
            changes.record(path, created, seed, saw, left);
            Ok(())
        }
        Err(e) => {
            // A seed file nothing was written to would only be clutter.
            if created && changes.saved.iter().all(|s| s.path != path) {
                // Only if it is still the untouched seed: someone may have written to it.
                let _ = nixedit::replace_if_unchanged(path, seed, None).await;
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testing::TempDir;
    use crate::runner::testing::{block_on, CollectSink, FakeRunner};

    fn runner() -> FakeRunner {
        FakeRunner::new().ok("nix-instantiate", &["--parse"])
    }

    fn set(
        runner: &FakeRunner,
        changes: &mut Changes,
        path: &Path,
        seed: &str,
        opt: &str,
        value: &str,
    ) {
        block_on(set_option(
            runner,
            changes,
            path,
            seed,
            opt,
            &NixValue::str(value),
        ))
        .unwrap();
    }

    #[test]
    fn a_new_file_is_seeded_edited_and_removed_again_on_restore() {
        let dir = TempDir::new();
        let path = dir.join("vm-platform.nix");
        let mut changes = Changes::default();
        set(
            &runner(),
            &mut changes,
            &path,
            VM_PLATFORM_SEED,
            "vexos.vm.platform",
            "virtualbox",
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("vexos.vm.platform = \"virtualbox\";"),
            "{text}"
        );

        block_on(changes.restore(&CollectSink::new()));
        assert!(!path.exists());
    }

    #[test]
    fn an_existing_file_goes_back_to_what_it_held() {
        let dir = TempDir::new();
        let original = "{\n  vexos.desktop.environment = \"gnome\";\n}\n";
        let path = dir.write("features.nix", original);
        let mut changes = Changes::default();
        for value in ["cosmic", "hyprland"] {
            set(
                &runner(),
                &mut changes,
                &path,
                FEATURES_SEED,
                "vexos.desktop.environment",
                value,
            );
        }
        assert!(std::fs::read_to_string(&path).unwrap().contains("hyprland"));
        block_on(changes.restore(&CollectSink::new()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn a_file_changed_by_someone_else_after_the_edit_is_not_overwritten() {
        let dir = TempDir::new();
        let path = dir.write("features.nix", "{\n}\n");
        let mut changes = Changes::default();
        set(
            &runner(),
            &mut changes,
            &path,
            FEATURES_SEED,
            "vexos.desktop.environment",
            "cosmic",
        );
        // Another job enables a feature while the build runs.
        let theirs = "{\n  vexos.desktop.environment = \"cosmic\";\n  vexos.features.gaming.enable = true;\n}\n";
        std::fs::write(&path, theirs).unwrap();
        let sink = CollectSink::new();
        block_on(changes.restore(&sink));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs);
        assert!(sink.text().contains("changed by something else"));
    }

    #[test]
    fn someone_else_editing_between_two_of_our_edits_stops_the_restore() {
        let dir = TempDir::new();
        let path = dir.write("features.nix", "{\n}\n");
        let mut changes = Changes::default();
        set(
            &runner(),
            &mut changes,
            &path,
            FEATURES_SEED,
            "vexos.vm.platform",
            "virtualbox",
        );
        // Between this operation's two edits another job adds a feature.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text = text.replace("}\n", "  vexos.features.gaming.enable = true;\n}\n");
        std::fs::write(&path, text).unwrap();
        set(
            &runner(),
            &mut changes,
            &path,
            FEATURES_SEED,
            "vexos.desktop.environment",
            "cosmic",
        );
        let sink = CollectSink::new();
        block_on(changes.restore(&sink));
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("gaming.enable = true"), "{after}");
        assert!(sink.text().contains("changed by something else"));
    }

    #[test]
    fn a_failed_edit_does_not_leave_a_seed_file_behind() {
        let dir = TempDir::new();
        let path = dir.join("vm-platform.nix");
        let failing = FakeRunner::new().failing("nix-instantiate", &["--parse"], 1, "syntax error");
        let mut changes = Changes::default();
        let err = block_on(set_option(
            &failing,
            &mut changes,
            &path,
            VM_PLATFORM_SEED,
            "vexos.vm.platform",
            &NixValue::str("virtualbox"),
        ));
        assert!(err.is_err());
        assert!(!path.exists());
        assert!(changes.is_empty());
    }

    #[test]
    fn a_created_file_someone_else_then_wrote_to_is_not_deleted() {
        let dir = TempDir::new();
        let path = dir.join("vm-platform.nix");
        let mut changes = Changes::default();
        set(
            &runner(),
            &mut changes,
            &path,
            VM_PLATFORM_SEED,
            "vexos.vm.platform",
            "virtualbox",
        );
        std::fs::write(
            &path,
            "{
  vexos.vm.platform = \"qemu\";
}
",
        )
        .unwrap();
        let sink = CollectSink::new();
        block_on(changes.restore(&sink));
        assert!(path.exists());
        assert!(sink.text().contains("changed by something else"));
    }

    #[test]
    fn replace_if_unchanged_acts_only_on_the_expected_content() {
        let dir = TempDir::new();
        let path = dir.write("f.nix", "one");
        assert!(!block_on(nixedit::replace_if_unchanged(&path, "two", Some("x"))).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one");
        assert!(block_on(nixedit::replace_if_unchanged(&path, "one", Some("x"))).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");
        assert!(block_on(nixedit::replace_if_unchanged(&path, "x", None)).unwrap());
        assert!(!path.exists());
    }
}
