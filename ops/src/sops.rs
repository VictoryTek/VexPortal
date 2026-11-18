//! One-time setup for sops-nix encrypted secrets: this host's age key.
//!
//! The key is made by the `age-keygen` command, which writes it with mode 0600. The public
//! half is printed with the `.sops.yaml` rule to add to the repository, so secrets can be
//! encrypted for this host. The key file is never read back into the log: only the public
//! key is shown.

use crate::runner::{Cmd, RunError, Runner};
use crate::services::{ensure_private_dir, require_server};
use crate::{fail, Ctx};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Where sops-nix looks for the key, under the filesystem root.
const KEY_FILE: &str = "var/lib/sops-nix/key.txt";

pub async fn init<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    match init_inner(ctx).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async fn init_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let key = ctx.root.join(KEY_FILE);
    let shown = key.display().to_string();

    if key.exists() {
        ctx.sink
            .info(&format!("An age key already exists at {shown}."));
    } else {
        ctx.sink
            .info(&format!("Generating an age key at {shown} ..."));
        generate(ctx, &key).await?;
    }

    // Only a key age-keygen itself can read back counts: a truncated file whose comment
    // still names a public key would give an encryption recipient this host cannot use.
    let Some(public) = public_key(ctx, &shown).await else {
        return Err(fail(
            ctx.sink,
            format!("could not read a valid age key from {shown}."),
        ));
    };

    for line in [
        String::new(),
        format!("Public key: {public}"),
        String::new(),
        "Add this to .sops.yaml in the repo root:".to_string(),
        String::new(),
        "creation_rules:".to_string(),
        "  - path_regex: secrets/.*\\.yaml$".to_string(),
        "    key_groups:".to_string(),
        "      - age:".to_string(),
        format!("          - {public}"),
        String::new(),
        "Then create or edit the secrets file with:  sops secrets/server/secrets.yaml".to_string(),
        "Required keys (see modules/secrets-sops.nix):".to_string(),
        "  nextcloud-admin-pass, photoprism-password,".to_string(),
        "  attic-server-token-rs256-secret-base64,".to_string(),
        "  vexboard-auth-secret, kiji-proxy-openai-key, vaultwarden-admin-token,".to_string(),
        "  authelia-jwt-secret, authelia-session-secret,".to_string(),
        "  authelia-storage-encryption-key".to_string(),
        String::new(),
        "Finally, set in your host config:".to_string(),
        "  vexos.secrets.backend = \"sops\";".to_string(),
        "  vexos.secrets.sopsFile = ./secrets/server/secrets.yaml;".to_string(),
    ] {
        ctx.sink.info(&line);
    }
    Ok(())
}

/// Make the key in a private temporary file beside its place, check it, and only then give
/// it its real name. A failed or cancelled run leaves nothing at the real path, which a
/// later run would take for a good key and never replace.
async fn generate<R: Runner>(ctx: &Ctx<'_, R>, key: &Path) -> Result<(), i32> {
    let dir = key.parent().unwrap_or(ctx.root);
    ensure_private_dir(dir).map_err(|e| {
        fail(
            ctx.sink,
            format!("could not prepare {}: {e}", dir.display()),
        )
    })?;
    // A name of its own, so two runs at once (the daemon allows several jobs) cannot clear
    // or publish each other's file.
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let temp = dir.join(format!(
        ".key.txt.{}.{}.new",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let temp_name = temp.display().to_string();

    let result = make_and_check(ctx, &temp_name).await;
    let published = result.and_then(|()| {
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| fail(ctx.sink, format!("could not protect the new key: {e}")))?;
        // A hard link never replaces: if another run made the key first, theirs stays.
        match std::fs::hard_link(&temp, key) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(fail(
                ctx.sink,
                format!("could not save the key to {}: {e}", key.display()),
            )),
        }
    });
    let _ = std::fs::remove_file(&temp);
    published
}

async fn make_and_check<R: Runner>(ctx: &Ctx<'_, R>, temp: &str) -> Result<(), i32> {
    let made = ctx
        .runner
        .run(&Cmd::new("age-keygen").arg("-o").arg(temp))
        .await;
    match made {
        Ok(out) if out.success() => {}
        Ok(out) => {
            return Err(fail(
                ctx.sink,
                format!("age-keygen failed: {}", out.stderr.trim()),
            ))
        }
        Err(RunError::Cancelled) => return Err(fail(ctx.sink, "Cancelled.")),
        Err(RunError::Spawn { .. }) => {
            return Err(fail(
                ctx.sink,
                "`age-keygen` is not available to VexPortal. Install the age package \
                 (environment.systemPackages = [ pkgs.age ]) and rebuild, then try again.",
            ))
        }
    }
    if public_key(ctx, temp).await.is_none() {
        return Err(fail(ctx.sink, "age-keygen made a key it cannot read back."));
    }
    Ok(())
}

/// The public half, as `age-keygen -y` derives it from the key file.
async fn public_key<R: Runner>(ctx: &Ctx<'_, R>, key: &str) -> Option<String> {
    let out = ctx
        .runner
        .run(&Cmd::new("age-keygen").arg("-y").arg(key))
        .await
        .ok()?;
    let line = out.stdout.trim();
    (out.success() && line.starts_with("age1") && !line.contains(char::is_whitespace))
        .then(|| line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::testing::{block_on, FakeRunner};
    use crate::runner::Output;
    use crate::testkit::Kit;
    use std::os::unix::fs::PermissionsExt;

    const PUBLIC: &str = "age1qyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqs3290gq";

    fn server() -> Kit {
        let kit = Kit::new();
        kit.nixos_file("vexos-variant", "vexos-server-amd\n");
        kit
    }

    fn out(stdout: &str) -> Output {
        Output {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// Stands in for `age-keygen`: `-o <file>` writes a key file, `-y` prints the public key.
    struct Keygen;

    impl Runner for Keygen {
        async fn run(&self, cmd: &Cmd) -> Result<Output, RunError> {
            if cmd.args.first().map(String::as_str) == Some("-o") {
                std::fs::write(
                    &cmd.args[1],
                    format!(
                        "# public key: {PUBLIC}
AGE-SECRET-KEY-1TOPSECRET
"
                    ),
                )
                .unwrap();
                // Like the real command: made with the process's umask, not necessarily private.
                std::fs::set_permissions(&cmd.args[1], std::fs::Permissions::from_mode(0o644))
                    .unwrap();
                return Ok(Output::default());
            }
            Ok(out(&format!(
                "{PUBLIC}
"
            )))
        }

        async fn stream(&self, cmd: &Cmd, _: &dyn crate::runner::Sink) -> Result<i32, RunError> {
            self.run(cmd).await.map(|o| o.code)
        }
    }

    #[test]
    fn a_new_key_is_made_by_age_keygen_in_a_private_folder_and_only_the_public_half_is_shown() {
        let kit = server();
        assert_eq!(block_on(init(&kit.ctx(&Keygen))), 0, "{}", kit.sink.text());
        let key = kit.root.join(KEY_FILE);
        assert_eq!(
            std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(key.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let text = kit.sink.text();
        assert!(text.contains("Generating an age key"));
        assert!(text.contains(&format!("Public key: {PUBLIC}")));
        assert!(
            !text.contains("TOPSECRET"),
            "the secret key is never printed"
        );
    }

    #[test]
    fn an_existing_key_is_kept_and_its_public_half_shown() {
        let kit = server();
        let key = kit.root.join(KEY_FILE);
        std::fs::create_dir_all(key.parent().unwrap()).unwrap();
        std::fs::write(&key, "# public key: x\nAGE-SECRET-KEY-1TOPSECRET\n").unwrap();
        let fake = FakeRunner::new().on("age-keygen", &["-y"], out(&format!("{PUBLIC}\n")));
        assert_eq!(block_on(init(&kit.ctx(&fake))), 0, "{}", kit.sink.text());
        let text = kit.sink.text();
        assert!(text.contains("already exists"));
        assert!(text.contains(&format!("Public key: {PUBLIC}")));
        assert!(text.contains("creation_rules:"));
        assert!(
            !text.contains("TOPSECRET"),
            "the secret key is never printed"
        );
        assert!(
            !fake.lines().iter().any(|l| l.contains("-o")),
            "never regenerated"
        );
        assert_eq!(
            std::fs::read_to_string(&key).unwrap(),
            "# public key: x\nAGE-SECRET-KEY-1TOPSECRET\n"
        );
    }

    #[test]
    fn a_key_age_keygen_cannot_read_is_an_error_not_a_recipient() {
        // A truncated key file whose comment still names a public key.
        let kit = server();
        let key = kit.root.join(KEY_FILE);
        std::fs::create_dir_all(key.parent().unwrap()).unwrap();
        std::fs::write(
            &key,
            format!(
                "# public key: {PUBLIC}
AGE-SECRET-KEY-1TRUNC"
            ),
        )
        .unwrap();
        let fake = FakeRunner::new().failing("age-keygen", &["-y"], 1, "bad");
        assert_eq!(block_on(init(&kit.ctx(&fake))), 1);
        assert!(!kit.sink.text().contains("Public key:"));
    }

    /// An age-keygen that writes part of a file and then fails.
    struct Dies;

    impl Runner for Dies {
        async fn run(&self, cmd: &Cmd) -> Result<Output, RunError> {
            if cmd.args.first().map(String::as_str) == Some("-o") {
                std::fs::write(&cmd.args[1], "AGE-SECRET-KEY-1HALF").unwrap();
                return Ok(Output {
                    code: 1,
                    stdout: String::new(),
                    stderr: "killed".into(),
                });
            }
            Ok(Output::default())
        }

        async fn stream(&self, cmd: &Cmd, _: &dyn crate::runner::Sink) -> Result<i32, RunError> {
            self.run(cmd).await.map(|o| o.code)
        }
    }

    #[test]
    fn a_failed_generation_leaves_nothing_a_later_run_would_take_for_a_key() {
        let kit = server();
        assert_eq!(block_on(init(&kit.ctx(&Dies))), 1);
        let key = kit.root.join(KEY_FILE);
        assert!(!key.exists());
        let leftovers: Vec<_> = std::fs::read_dir(key.parent().unwrap()).unwrap().collect();
        assert!(leftovers.is_empty(), "no partial file stays: {leftovers:?}");
        // The next run starts clean and succeeds.
        assert_eq!(block_on(init(&kit.ctx(&Keygen))), 0, "{}", kit.sink.text());
        assert!(key.exists());
    }

    #[test]
    fn two_runs_at_once_do_not_touch_each_others_files() {
        use std::sync::atomic::{AtomicBool, Ordering};
        // While the outer run is making its key, an inner run starts and fails.
        struct Nested<'a>(&'a Kit, AtomicBool);
        impl Runner for Nested<'_> {
            async fn run(&self, cmd: &Cmd) -> Result<Output, RunError> {
                if cmd.args.first().map(String::as_str) == Some("-o") {
                    // The outer key is already on disk when the inner run starts and ends.
                    std::fs::write(&cmd.args[1], "OUTER-KEY").unwrap();
                    if !self.1.swap(true, Ordering::SeqCst) {
                        let _ = block_on(init(&self.0.ctx(&Dies)));
                    }
                    return Ok(Output::default());
                }
                Ok(Output {
                    code: 0,
                    stdout: "age1abc
"
                    .into(),
                    stderr: String::new(),
                })
            }
            async fn stream(
                &self,
                cmd: &Cmd,
                _: &dyn crate::runner::Sink,
            ) -> Result<i32, RunError> {
                self.run(cmd).await.map(|o| o.code)
            }
        }
        let kit = server();
        let runner = Nested(&kit, AtomicBool::new(false));
        assert_eq!(block_on(init(&kit.ctx(&runner))), 0, "{}", kit.sink.text());
        let key = kit.root.join(KEY_FILE);
        assert_eq!(std::fs::read_to_string(&key).unwrap(), "OUTER-KEY");
        assert_eq!(
            std::fs::read_dir(key.parent().unwrap()).unwrap().count(),
            1,
            "only the key is left"
        );
    }

    #[test]
    fn a_key_made_by_someone_else_in_the_meantime_is_not_replaced() {
        let kit = server();
        let key = kit.root.join(KEY_FILE);
        std::fs::create_dir_all(key.parent().unwrap()).unwrap();
        // Another run publishes its key between our check and our link.
        struct Racer(std::path::PathBuf);
        impl Runner for Racer {
            async fn run(&self, cmd: &Cmd) -> Result<Output, RunError> {
                if cmd.args.first().map(String::as_str) == Some("-o") {
                    std::fs::write(&cmd.args[1], "OURS").unwrap();
                    std::fs::write(&self.0, "THEIRS").unwrap();
                    return Ok(Output::default());
                }
                Ok(Output {
                    code: 0,
                    stdout: "age1abc
"
                    .into(),
                    stderr: String::new(),
                })
            }
            async fn stream(
                &self,
                cmd: &Cmd,
                _: &dyn crate::runner::Sink,
            ) -> Result<i32, RunError> {
                self.run(cmd).await.map(|o| o.code)
            }
        }
        assert_eq!(
            block_on(init(&kit.ctx(&Racer(key.clone())))),
            0,
            "{}",
            kit.sink.text()
        );
        assert_eq!(std::fs::read_to_string(&key).unwrap(), "THEIRS");
    }

    #[test]
    fn a_missing_age_keygen_says_what_to_install() {
        let kit = server();
        let fake = FakeRunner::new(); // no rule: starting the command fails
        assert_eq!(block_on(init(&kit.ctx(&fake))), 1);
        assert!(kit.sink.text().contains("pkgs.age"));
        assert!(!kit.root.join(KEY_FILE).exists());
    }

    #[test]
    fn only_the_server_roles_set_up_secrets() {
        let kit = Kit::new();
        kit.nixos_file("vexos-variant", "vexos-desktop-amd\n");
        let fake = FakeRunner::new();
        assert_eq!(block_on(init(&kit.ctx(&fake))), 1);
        assert!(fake.lines().is_empty());
    }
}
