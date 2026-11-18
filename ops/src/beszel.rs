//! The Beszel monitoring agent: give it the hub's key and token, and look at how it is
//! doing.
//!
//! The hub's "Add System" dialog shows a public key and a one-time token. The agent reads
//! both from an environment file only root can read, so setting it up is a root job; the
//! secrets go straight from the request to that file and are never logged.

use crate::runner::{Cmd, Runner};
use crate::services::{
    self, ensure_private_dir, parse_show, plain_secret, require_server, secrets_dir, services_path,
    show_args,
};
use crate::{capture, fail, stream, Ctx};
use std::path::PathBuf;

const UNIT: &str = "beszel-agent.service";
const ENV_FILE: &str = "beszel-agent-env";
/// The port the hub connects to.
const PORT: u16 = 45876;

fn env_path<R: Runner>(ctx: &Ctx<'_, R>) -> PathBuf {
    secrets_dir(ctx.nixos_dir).join(ENV_FILE)
}

fn agent_enabled<R: Runner>(ctx: &Ctx<'_, R>) -> bool {
    let text = std::fs::read_to_string(services_path(ctx.nixos_dir)).unwrap_or_default();
    services::find("beszel-agent").is_some_and(|s| services::is_enabled(&text, s))
}

/// A hub key: `ssh-ed25519 AAAA...`, one line.
fn valid_key(key: &str) -> bool {
    key.starts_with("ssh-") && !key.chars().any(char::is_control)
}

pub async fn setup<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    match setup_inner(ctx).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async fn setup_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    if !agent_enabled(ctx) {
        return Err(fail(
            ctx.sink,
            "the Beszel agent is not enabled. Enable beszel-agent and rebuild first, then set it up.",
        ));
    }
    let shown = capture(
        ctx,
        &Cmd::new("systemctl").args(show_args(&[UNIT.to_string()])),
    )
    .await?;
    if parse_show(&shown.stdout)
        .iter()
        .all(|u| u.load == "not-found")
    {
        return Err(fail(
            ctx.sink,
            format!("{UNIT} does not exist yet: the agent is enabled but not rebuilt. Rebuild, then set it up."),
        ));
    }

    let key = ctx.args.get("key").map(|k| k.trim()).unwrap_or_default();
    let token = ctx
        .secrets
        .get("token")
        .map(|t| t.expose().trim().to_string())
        .unwrap_or_default();
    if key.is_empty() || token.is_empty() {
        return Err(fail(ctx.sink, "the key and the token must both be given."));
    }
    if !valid_key(key) {
        return Err(fail(
            ctx.sink,
            "the key must be one line starting with ssh- (copy the Key from the hub's Add System dialog).",
        ));
    }
    if !plain_secret(&token) {
        return Err(fail(
            ctx.sink,
            "the token must be a single line with no spaces or control characters.",
        ));
    }

    let path = env_path(ctx);
    let existed = path.exists();
    let dir = secrets_dir(ctx.nixos_dir);
    ensure_private_dir(&dir).map_err(|e| {
        fail(
            ctx.sink,
            format!("could not prepare {}: {e}", dir.display()),
        )
    })?;
    crate::fsutil::write_atomic(
        &path,
        format!("KEY={key}\nTOKEN={token}\n").as_bytes(),
        0o600,
    )
    .map_err(|e| fail(ctx.sink, format!("could not write {}: {e}", path.display())))?;
    ctx.sink.info(&format!(
        "✓ {} {} (root only).",
        if existed { "Replaced" } else { "Wrote" },
        path.display()
    ));

    // The unit has most likely hit its start limit while the file was missing.
    let _ = capture(ctx, &Cmd::new("systemctl").args(["reset-failed", UNIT])).await?;
    let code = stream(ctx, &Cmd::new("systemctl").args(["restart", UNIT])).await?;
    let after = capture(
        ctx,
        &Cmd::new("systemctl").args(show_args(&[UNIT.to_string()])),
    )
    .await?;
    let active = parse_show(&after.stdout)
        .first()
        .map(|u| u.active.clone())
        .unwrap_or_default();
    if code != 0 || active != "active" {
        ctx.sink.error(&format!(
            "{UNIT} is {}. Last 15 journal lines:",
            if active.is_empty() {
                "unknown"
            } else {
                &active
            }
        ));
        journal(ctx, 15).await?;
        return Err(1);
    }
    ctx.sink.info(&format!("✓ {UNIT} is active."));
    ctx.sink
        .info("Now click Add System in the hub. This host should turn green within seconds.");
    Ok(())
}

async fn journal<R: Runner>(ctx: &Ctx<'_, R>, lines: u32) -> Result<(), i32> {
    let out = capture(
        ctx,
        &Cmd::new("journalctl").args(["-u", UNIT, "-n", &lines.to_string(), "--no-pager"]),
    )
    .await?;
    for line in out.stdout.lines().chain(out.stderr.lines()) {
        ctx.sink.info(line);
    }
    Ok(())
}

/// Read-only: enabled, configured, running, listening.
pub async fn status<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    match status_inner(ctx).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async fn status_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    use std::os::unix::fs::MetadataExt;
    require_server(ctx)?;
    if agent_enabled(ctx) {
        ctx.sink.info("Enabled:   yes");
    } else {
        ctx.sink
            .info("Enabled:   no (enable beszel-agent and rebuild)");
    }
    let path = env_path(ctx);
    match std::fs::metadata(&path) {
        Ok(meta) => ctx.sink.info(&format!(
            "Env file:  present ({:o}) {}",
            meta.mode() & 0o7777,
            path.display()
        )),
        Err(_) => ctx.sink.info("Env file:  missing (set the agent up)"),
    }

    let shown = capture(
        ctx,
        &Cmd::new("systemctl").args(show_args(&[UNIT.to_string()])),
    )
    .await?;
    let state = parse_show(&shown.stdout)
        .first()
        .map(|u| {
            if u.load == "not-found" {
                "not built yet".to_string()
            } else {
                u.active.clone()
            }
        })
        .unwrap_or_else(|| "unknown".into());
    ctx.sink.info(&format!("Unit:      {state}"));

    let listening = capture(
        ctx,
        &Cmd::new("ss").args(["-H", "-tln", &format!("sport = :{PORT}")]),
    )
    .await?;
    ctx.sink.info(&format!(
        "Port {PORT}: {}",
        if listening.stdout.trim().is_empty() {
            "not listening"
        } else {
            "listening"
        }
    ));
    ctx.sink.info("");
    ctx.sink.info(&format!("Last 10 journal lines of {UNIT}:"));
    journal(ctx, 10).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::testing::{block_on, FakeRunner};
    use crate::runner::Output;
    use crate::testkit::Kit;
    use crate::Secret;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIexample";

    fn out(stdout: &str) -> Output {
        Output {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn shown(load: &str, active: &str) -> Output {
        out(&format!(
            "Id={UNIT}\nLoadState={load}\nActiveState={active}\nSubState=x\n"
        ))
    }

    fn agent_kit(enabled: bool, token: &str) -> Kit {
        let mut kit = Kit::new().with_args(&[("key", KEY)]);
        kit.nixos_file("vexos-variant", "vexos-server-amd\n");
        if enabled {
            kit.nixos_file(
                "server-services.nix",
                "{\n  vexos.server.beszel-agent.enable = true;\n}\n",
            );
        }
        kit.secrets.insert("token".into(), Secret::new(token));
        kit
    }

    fn runner(after: Output) -> FakeRunner {
        // The first `show` is the existence check, the second the result.
        FakeRunner::new()
            .ok("systemctl", &["reset-failed"])
            .ok("systemctl", &["restart"])
            .on("journalctl", &[], out("agent: bad key"))
            .on("systemctl", &["show"], after)
    }

    #[test]
    fn setup_writes_a_private_env_file_and_restarts_the_agent() {
        use std::os::unix::fs::PermissionsExt;
        let kit = agent_kit(true, "tok-SECRET");
        let fake = runner(shown("loaded", "active"));
        assert_eq!(block_on(setup(&kit.ctx(&fake))), 0, "{}", kit.sink.text());

        let file = kit.nixos.join("secrets/beszel-agent-env");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("KEY={KEY}\nTOKEN=tok-SECRET\n")
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let lines = fake.lines();
        assert!(lines
            .iter()
            .any(|l| l == "systemctl reset-failed beszel-agent.service"));
        assert!(lines
            .iter()
            .any(|l| l == "systemctl restart beszel-agent.service"));
        assert!(
            !kit.sink.text().contains("tok-SECRET"),
            "not in the job log"
        );
        assert!(kit.sink.text().contains("Add System"));
    }

    #[test]
    fn setup_needs_the_agent_enabled_and_built() {
        let kit = agent_kit(false, "t");
        let fake = runner(shown("loaded", "active"));
        assert_eq!(block_on(setup(&kit.ctx(&fake))), 1);
        assert!(kit.sink.text().contains("not enabled"));
        assert!(!kit.nixos.join("secrets").exists());

        let kit = agent_kit(true, "t");
        let fake = runner(shown("not-found", "inactive"));
        assert_eq!(block_on(setup(&kit.ctx(&fake))), 1);
        assert!(kit.sink.text().contains("not rebuilt"));
        assert!(!kit.nixos.join("secrets/beszel-agent-env").exists());
    }

    #[test]
    fn a_bad_key_or_token_writes_nothing() {
        for (key, token) in [
            ("not-a-key", "t"),
            ("ssh-ed25519 AAAA\nssh-rsa BBB", "t"),
            (KEY, "two words"),
            (KEY, "line\nbreak"),
            (KEY, ""),
        ] {
            let mut k = agent_kit(true, token);
            k.args.insert("key".into(), key.into());
            let fake = runner(shown("loaded", "active"));
            assert_eq!(block_on(setup(&k.ctx(&fake))), 1, "{key:?} {token:?}");
            assert!(!k.nixos.join("secrets/beszel-agent-env").exists());
            assert!(!fake.lines().iter().any(|l| l.contains("restart")));
        }
    }

    #[test]
    fn an_agent_that_does_not_start_shows_its_journal_and_fails() {
        let kit = agent_kit(true, "t");
        let fake = runner(shown("loaded", "failed"));
        assert_eq!(block_on(setup(&kit.ctx(&fake))), 1);
        assert!(kit.sink.text().contains("failed"));
        assert!(kit.sink.text().contains("agent: bad key"));
        // The file is right, so it stays for the next try.
        assert!(kit.nixos.join("secrets/beszel-agent-env").exists());
    }

    #[test]
    fn status_reports_each_part_without_changing_anything() {
        let kit = agent_kit(true, "t");
        std::fs::create_dir_all(kit.nixos.join("secrets")).unwrap();
        std::fs::write(kit.nixos.join("secrets/beszel-agent-env"), "x").unwrap();
        let fake = FakeRunner::new()
            .on("systemctl", &["show"], shown("loaded", "active"))
            .on("ss", &["-H"], out("LISTEN 0 4096 *:45876 *:*\n"))
            .on("journalctl", &[], out("started"));
        assert_eq!(block_on(status(&kit.ctx(&fake))), 0, "{}", kit.sink.text());
        let text = kit.sink.text();
        assert!(text.contains("Enabled:   yes"));
        assert!(text.contains("Env file:  present"));
        assert!(text.contains("Unit:      active"));
        assert!(text.contains("Port 45876: listening"));
        assert!(text.contains("started"));
        assert!(!fake.lines().iter().any(|l| l.contains("restart")));
    }

    #[test]
    fn status_on_a_host_that_is_not_set_up_says_so() {
        let kit = agent_kit(false, "t");
        let fake = FakeRunner::new()
            .on("systemctl", &["show"], shown("not-found", "inactive"))
            .on("ss", &["-H"], out(""))
            .on("journalctl", &[], out(""));
        assert_eq!(block_on(status(&kit.ctx(&fake))), 0);
        let text = kit.sink.text();
        assert!(text.contains("Enabled:   no"));
        assert!(text.contains("Env file:  missing"));
        assert!(text.contains("not built yet"));
        assert!(text.contains("not listening"));
    }

    #[test]
    fn only_the_server_roles_have_a_beszel_agent() {
        let kit = Kit::new().with_args(&[("key", KEY)]);
        kit.nixos_file("vexos-variant", "vexos-desktop-amd\n");
        let fake = FakeRunner::new();
        assert_eq!(block_on(setup(&kit.ctx(&fake))), 1);
        assert_eq!(block_on(status(&kit.ctx(&fake))), 1);
        assert!(fake.lines().is_empty());
    }
}
