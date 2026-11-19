//! Plex's data directory as one portable archive: making it, and putting one back.
//!
//! `/var/lib/plex` holds the library database, watch history and the server's sign-in
//! token, so the archive is private (0600) and handed to the person who asked for it.

use crate::backup::{finish, unit_info};
use crate::runner::{Cmd, Runner};
use crate::services::{require_server, UnitInfo};
use crate::{capture, fail, fsutil, stream, Ctx};
use std::path::{Component, Path, PathBuf};

const UNIT: &str = "plex.service";
/// Where an archive is unpacked before it replaces the live data. Next to it, so the
/// final step is a rename on one filesystem.
const STAGING: &str = "plex.restore";

fn is_running(unit: &UnitInfo) -> bool {
    matches!(unit.active.as_str(), "active" | "activating" | "reloading")
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// An absolute path with no `..`, the only kind either operation accepts.
fn plain_absolute(path: &str) -> bool {
    let p = Path::new(path);
    p.is_absolute()
        && !path.chars().any(char::is_control)
        && p.components().all(|c| !matches!(c, Component::ParentDir))
}

/// Start Plex again after the job stopped it, even if the job was cancelled.
async fn start_plex<R: Runner>(ctx: &Ctx<'_, R>) -> bool {
    ctx.sink.info("Starting plex.service…");
    let started = ctx
        .runner
        .cleanup(&Cmd::new("systemctl").args(["start", UNIT]))
        .await;
    let problem = match started {
        Ok(out) if out.code == 0 => return true,
        Ok(out) => format!("exit {}", out.code),
        Err(e) => e.to_string(),
    };
    ctx.sink.error(&format!(
        "error: plex.service did not start ({problem}). Start it from its service page."
    ));
    false
}

struct Account {
    uid: u32,
    gid: u32,
    home: PathBuf,
}

/// The account that asked, as the system's account database describes it.
async fn account_of<R: Runner>(ctx: &Ctx<'_, R>, uid: u32) -> Result<Account, i32> {
    let out = capture(ctx, &Cmd::new("getent").args(["passwd", &uid.to_string()])).await?;
    let line = out.stdout.lines().next().unwrap_or("");
    let fields: Vec<&str> = line.split(':').collect();
    let parsed = (fields.len() >= 6)
        .then(|| {
            Some(Account {
                uid: fields[2].parse().ok()?,
                gid: fields[3].parse().ok()?,
                home: PathBuf::from(fields[5]),
            })
        })
        .flatten()
        .filter(|a| a.uid == uid && a.home.is_absolute());
    parsed.ok_or_else(|| {
        fail(
            ctx.sink,
            format!("could not read the account with id {uid}"),
        )
    })
}

// ── backup-plex ───────────────────────────────────────────────────────────────

pub async fn backup<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    finish(backup_inner(ctx).await)
}

async fn backup_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    // Before Plex's state is read: a job that has Plex stopped must finish, and start it
    // again, before this one decides whether to stop it.
    let _only = crate::backup::DATA_LOCK.lock().await;
    let Some(unit) = unit_info(ctx, UNIT).await? else {
        return Err(fail(
            ctx.sink,
            "plex.service does not exist. Enable Plex and rebuild first.",
        ));
    };
    let account = match ctx.caller_uid {
        Some(uid) => Some(account_of(ctx, uid).await?),
        None => None,
    };
    let live = ctx.root.join("var/lib/plex");
    let dest = destination(ctx, account.as_ref(), &live)?;

    // Created here, exclusively and private, before anything is stopped: an existing
    // file or a link planted at the name is refused, and tar then writes into a file
    // only root could have made.
    fsutil::create_new(&dest, b"", 0o600).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            fail(
                ctx.sink,
                format!("{} already exists. Choose another name.", dest.display()),
            )
        } else {
            fail(
                ctx.sink,
                format!("could not create {}: {e}", dest.display()),
            )
        }
    })?;

    let result = archive(ctx, &dest, &live, is_running(&unit)).await;
    if result.is_err() {
        // A partial archive would look like a good one.
        let _ = std::fs::remove_file(&dest);
    }
    let restarted = result?;

    if let Some(account) = &account {
        std::os::unix::fs::chown(&dest, Some(account.uid), Some(account.gid)).map_err(|e| {
            fail(
                ctx.sink,
                format!("could not hand {} to your account: {e}", dest.display()),
            )
        })?;
    }
    let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    ctx.sink.info(&format!(
        "✔ Backup complete: {} ({:.1} MiB)",
        dest.display(),
        size as f64 / (1024.0 * 1024.0)
    ));
    ctx.sink
        .info("Move this file to the new server, enable Plex there, then use Restore Plex.");
    // The archive is good, but the job failed if Plex is left down.
    if restarted {
        Ok(())
    } else {
        Err(1)
    }
}

/// Where the archive goes: the file the person named, or a dated name in their home.
fn destination<R: Runner>(
    ctx: &Ctx<'_, R>,
    account: Option<&Account>,
    live: &Path,
) -> Result<PathBuf, i32> {
    let dest = match ctx.args.get("dest") {
        Some(path) => {
            if !plain_absolute(path) {
                return Err(fail(ctx.sink, "the destination must be an absolute path."));
            }
            PathBuf::from(path)
        }
        None => {
            let Some(account) = account else {
                return Err(fail(
                    ctx.sink,
                    "could not tell which account asked, so choose a destination file.",
                ));
            };
            account
                .home
                .join(format!("plex-backup-{}.tar.gz", fsutil::now_stamp()))
        }
    };
    if dest.starts_with(live) {
        return Err(fail(
            ctx.sink,
            "the archive cannot go inside Plex's own data directory.",
        ));
    }
    match dest.parent() {
        Some(parent) if parent.is_dir() => Ok(dest),
        _ => Err(fail(
            ctx.sink,
            format!("{} is not inside an existing folder.", dest.display()),
        )),
    }
}

/// Stop Plex (if it runs), write the archive, and start Plex again.
async fn archive<R: Runner>(
    ctx: &Ctx<'_, R>,
    dest: &Path,
    live: &Path,
    was_running: bool,
) -> Result<bool, i32> {
    let written = async {
        if was_running {
            ctx.sink.info("Stopping plex.service…");
            let code = stream(ctx, &Cmd::new("systemctl").args(["stop", UNIT])).await?;
            if code != 0 {
                return Err(fail(
                    ctx.sink,
                    format!("could not stop plex.service (exit {code}); nothing was archived."),
                ));
            }
        }
        let parent = live.parent().unwrap_or(Path::new("/"));
        ctx.sink.info(&format!(
            "Archiving {} → {}",
            live.display(),
            dest.display()
        ));
        let code = stream(
            ctx,
            &Cmd::new("tar")
                .arg("czf")
                .arg(text(dest))
                .arg("-C")
                .arg(text(parent))
                .arg("plex"),
        )
        .await?;
        match code {
            0 => Ok(()),
            code => Err(fail(ctx.sink, format!("tar exited with {code}."))),
        }
    }
    .await;
    // Back up again whatever happened, and even if the stop itself was cancelled.
    let restarted = !was_running || start_plex(ctx).await;
    written?;
    Ok(restarted)
}

// ── restore-plex ──────────────────────────────────────────────────────────────

/// A device, pipe or socket in an unpacked archive. Root-run tar creates them, and the
/// chown that follows would hand a raw device to Plex.
fn find_special(dir: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::FileTypeExt;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if let Some(found) = find_special(&path) {
                return Some(found);
            }
        } else if kind.is_block_device()
            || kind.is_char_device()
            || kind.is_fifo()
            || kind.is_socket()
        {
            return Some(path);
        }
    }
    None
}

/// Check an archive listing before anything is unpacked: everything must sit under
/// `plex/`, and nothing may climb out of it.
pub fn check_layout(listing: &str) -> Result<(), String> {
    let mut under_plex = false;
    for entry in listing.lines().filter(|l| !l.is_empty()) {
        let name = entry.strip_prefix("./").unwrap_or(entry);
        let escapes = Path::new(name).is_absolute()
            || Path::new(name)
                .components()
                .any(|c| matches!(c, Component::ParentDir));
        if escapes || !(name == "plex" || name == "plex/" || name.starts_with("plex/")) {
            return Err(format!(
                "this is not a Plex backup: expected everything under plex/, found `{entry}`."
            ));
        }
        under_plex |= name.starts_with("plex/");
    }
    if under_plex {
        Ok(())
    } else {
        Err("this archive has no plex/ folder in it.".to_string())
    }
}

pub async fn restore<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    // One at a time: they share the staging folder and Plex's data.
    let _only = crate::backup::DATA_LOCK.lock().await;
    finish(restore_inner(ctx).await)
}

async fn restore_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let Some(tarball) = ctx.args.get("tarball") else {
        return Err(fail(ctx.sink, "a backup file is required"));
    };
    if !plain_absolute(tarball) {
        return Err(fail(ctx.sink, "the backup file must be an absolute path."));
    }
    let tarball = PathBuf::from(tarball);
    if !tarball.is_file() {
        return Err(fail(
            ctx.sink,
            format!("{} was not found.", tarball.display()),
        ));
    }
    let Some(unit) = unit_info(ctx, UNIT).await? else {
        return Err(fail(
            ctx.sink,
            "plex.service does not exist. Enable Plex and rebuild first, then restore.",
        ));
    };

    ctx.sink.info("Checking the archive…");
    let listing = capture(ctx, &Cmd::new("tar").arg("tzf").arg(text(&tarball))).await?;
    if listing.code != 0 {
        for line in listing.stderr.lines().take(5) {
            ctx.sink.error(line);
        }
        return Err(fail(
            ctx.sink,
            format!("{} is not a readable .tar.gz archive.", tarball.display()),
        ));
    }
    check_layout(&listing.stdout).map_err(|m| fail(ctx.sink, m))?;

    let lib = ctx.root.join("var/lib");
    let staging = lib.join(STAGING);
    let result = replace(ctx, &tarball, &lib, &staging, is_running(&unit)).await;
    // The unpacked copy has either become the live data or is not wanted.
    let _ = std::fs::remove_dir_all(&staging);
    result
}

async fn replace<R: Runner>(
    ctx: &Ctx<'_, R>,
    tarball: &Path,
    lib: &Path,
    staging: &Path,
    was_running: bool,
) -> Result<(), i32> {
    // Unpack first, with Plex still running: the long part cannot hurt anything, and
    // Plex is stopped only for the moment of the swap.
    let _ = std::fs::remove_dir_all(staging);
    std::fs::create_dir(staging).map_err(|e| {
        fail(
            ctx.sink,
            format!("could not make {}: {e}", staging.display()),
        )
    })?;
    ctx.sink
        .info(&format!("Unpacking into {} …", staging.display()));
    let code = stream(
        ctx,
        &Cmd::new("tar")
            .arg("xzf")
            .arg(text(tarball))
            .arg("-C")
            .arg(text(staging))
            .arg("--no-same-owner"),
    )
    .await?;
    if code != 0 {
        return Err(fail(
            ctx.sink,
            format!("tar exited with {code}; Plex was not touched."),
        ));
    }
    let unpacked = staging.join("plex");
    if !unpacked.is_dir() {
        return Err(fail(
            ctx.sink,
            "the archive did not unpack to a plex folder; Plex was not touched.",
        ));
    }
    if let Some(odd) = find_special(&unpacked) {
        return Err(fail(
            ctx.sink,
            format!(
                "the archive contains a device or pipe ({}); Plex was not touched.",
                odd.display()
            ),
        ));
    }
    let code = stream(
        ctx,
        &Cmd::new("chown")
            .arg("-R")
            .arg("plex:plex")
            .arg(text(&unpacked)),
    )
    .await?;
    if code != 0 {
        return Err(fail(
            ctx.sink,
            format!("chown exited with {code}; Plex was not touched."),
        ));
    }

    let swapped = async {
        if was_running {
            ctx.sink.info("Stopping plex.service…");
            let code = stream(ctx, &Cmd::new("systemctl").args(["stop", UNIT])).await?;
            if code != 0 {
                return Err(fail(
                    ctx.sink,
                    format!("could not stop plex.service (exit {code}); Plex was not touched."),
                ));
            }
        }
        swap(ctx, lib, &unpacked)
    }
    .await;
    // Plex comes back whatever happened, and even if the stop itself was cancelled.
    let restarted = start_plex(ctx).await;
    let backup = swapped?;
    if let Some(backup) = backup {
        ctx.sink.info(&format!(
            "The previous data is kept at {}. Delete it once Plex looks right.",
            backup.display()
        ));
    }
    ctx.sink
        .info("✔ Restore complete. Plex's web page is at http://<this-server>:32400/web");
    // The data is restored, but the job failed if Plex is left down.
    if restarted {
        Ok(())
    } else {
        Err(1)
    }
}

/// Move the live data aside and the unpacked copy into its place. If the second move
/// fails, the first is undone. Returns where the old data went, if there was any.
fn swap<R: Runner>(ctx: &Ctx<'_, R>, lib: &Path, unpacked: &Path) -> Result<Option<PathBuf>, i32> {
    let live = lib.join("plex");
    let mut kept = None;
    if live.exists() {
        let backup = lib.join(format!("plex.bak-{}", fsutil::now_stamp()));
        std::fs::rename(&live, &backup).map_err(|e| {
            fail(
                ctx.sink,
                format!("could not move the current data aside: {e}"),
            )
        })?;
        kept = Some(backup);
    }
    if let Err(e) = std::fs::rename(unpacked, &live) {
        if let Some(backup) = &kept {
            let _ = std::fs::rename(backup, &live);
        }
        return Err(fail(
            ctx.sink,
            format!("could not put the restored data in place: {e}. The old data is back."),
        ));
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::testing::{block_on, FakeRunner};
    use crate::runner::{Output, RunError, Sink};
    use crate::testkit::Kit;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn out(stdout: &str) -> Output {
        Output {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn plex_state(active: &str) -> Output {
        out(&format!(
            "Id=plex.service\nLoadState=loaded\nActiveState={active}\nSubState=x\n"
        ))
    }

    fn server_kit(args: &[(&str, &str)]) -> Kit {
        let mut kit = Kit::new().with_args(args);
        kit.nixos_file("vexos-variant", "vexos-server-amd\n");
        std::fs::create_dir_all(kit.root.join("var/lib/plex")).unwrap();
        std::fs::write(kit.root.join("var/lib/plex/old.txt"), "old").unwrap();
        std::fs::create_dir_all(kit.root.join("home/u")).unwrap();
        kit.caller = Some(std::fs::metadata(kit.root.path()).unwrap().uid());
        kit
    }

    /// What the system's account database says about the test's own user.
    fn passwd(kit: &Kit) -> Output {
        let meta = std::fs::metadata(kit.root.path()).unwrap();
        out(&format!(
            "u:x:{}:{}:U:{}:/bin/sh\n",
            meta.uid(),
            meta.gid(),
            kit.root.join("home/u").display()
        ))
    }

    fn backup_runner(kit: &Kit, active: &str) -> FakeRunner {
        FakeRunner::new()
            .on("systemctl", &["show"], plex_state(active))
            .on("getent", &["passwd"], passwd(kit))
            .ok("systemctl", &["stop"])
            .ok("systemctl", &["start"])
            .ok("tar", &["czf"])
    }

    // ── backup ────────────────────────────────────────────────────────────────

    #[test]
    fn backup_stops_plex_archives_it_privately_and_starts_it_again() {
        let kit = server_kit(&[]);
        let fake = backup_runner(&kit, "active");
        assert_eq!(block_on(backup(&kit.ctx(&fake))), 0, "{}", kit.sink.text());

        let lines = fake.lines();
        let at = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no `{needle}` in {lines:?}"))
        };
        assert!(at("systemctl stop plex.service") < at("tar czf"));
        assert!(at("tar czf") < at("systemctl start plex.service"));
        let tar = &lines[at("tar czf")];
        assert!(tar.contains("home/u/plex-backup-"), "{tar}");
        assert!(
            tar.ends_with(&format!("-C {} plex", kit.root.join("var/lib").display())),
            "{tar}"
        );

        let made: Vec<_> = std::fs::read_dir(kit.root.join("home/u"))
            .unwrap()
            .collect();
        assert_eq!(made.len(), 1);
        let file = made[0].as_ref().unwrap().path();
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_plex_that_was_stopped_stays_stopped() {
        let kit = server_kit(&[]);
        let fake = backup_runner(&kit, "inactive");
        assert_eq!(block_on(backup(&kit.ctx(&fake))), 0, "{}", kit.sink.text());
        assert!(!fake
            .lines()
            .iter()
            .any(|l| l.starts_with("systemctl stop") || l.starts_with("systemctl start")));
    }

    #[test]
    fn an_existing_file_is_never_overwritten_and_plex_is_left_running() {
        let kit = server_kit(&[]);
        let dest = kit.root.join("home/u/mine.tar.gz");
        std::fs::write(&dest, "precious").unwrap();
        let mut kit = kit;
        kit.args = [("dest".to_string(), dest.to_string_lossy().into_owned())]
            .into_iter()
            .collect();
        let fake = backup_runner(&kit, "active");
        assert_ne!(block_on(backup(&kit.ctx(&fake))), 0);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "precious");
        assert!(!fake.lines().iter().any(|l| l.starts_with("systemctl stop")));
    }

    #[test]
    fn a_link_planted_at_the_destination_is_not_followed() {
        let kit = server_kit(&[]);
        let target = kit.root.join("home/u/target");
        std::fs::write(&target, "untouched").unwrap();
        let link = kit.root.join("home/u/link.tar.gz");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut kit = kit;
        kit.args = [("dest".to_string(), link.to_string_lossy().into_owned())]
            .into_iter()
            .collect();
        let fake = backup_runner(&kit, "active");
        assert_ne!(block_on(backup(&kit.ctx(&fake))), 0);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
    }

    #[test]
    fn a_failed_archive_leaves_no_partial_file_and_plex_runs_again() {
        let kit = server_kit(&[]);
        let fake = FakeRunner::new()
            .on("systemctl", &["show"], plex_state("active"))
            .on("getent", &["passwd"], passwd(&kit))
            .ok("systemctl", &["stop"])
            .ok("systemctl", &["start"])
            .failing("tar", &["czf"], 2, "No space left on device");
        assert_ne!(block_on(backup(&kit.ctx(&fake))), 0);
        assert_eq!(
            std::fs::read_dir(kit.root.join("home/u")).unwrap().count(),
            0,
            "a partial archive was left"
        );
        assert!(fake
            .lines()
            .contains(&"systemctl start plex.service".to_string()));
    }

    #[test]
    fn bad_destinations_are_refused_before_plex_is_stopped() {
        for dest in [
            "relative.tar.gz".to_string(),
            "/tmp/../etc/x.tar.gz".to_string(),
            "/definitely/not/a/folder/x.tar.gz".to_string(),
        ] {
            let kit = server_kit(&[]);
            let mut kit = kit;
            kit.args = [("dest".to_string(), dest.clone())].into_iter().collect();
            let fake = backup_runner(&kit, "active");
            assert_ne!(block_on(backup(&kit.ctx(&fake))), 0, "{dest}");
            assert!(
                !fake.lines().iter().any(|l| l.starts_with("systemctl stop")),
                "{dest}"
            );
        }
        // Inside Plex's own data: tar would archive its own output.
        let kit = server_kit(&[]);
        let inside = kit.root.join("var/lib/plex/x.tar.gz");
        let mut kit = kit;
        kit.args = [("dest".to_string(), inside.to_string_lossy().into_owned())]
            .into_iter()
            .collect();
        let fake = backup_runner(&kit, "active");
        assert_ne!(block_on(backup(&kit.ctx(&fake))), 0);
    }

    // ── layout ────────────────────────────────────────────────────────────────

    #[test]
    fn only_archives_laid_out_under_plex_are_accepted() {
        check_layout("plex/\nplex/Preferences.xml\nplex/Library/a b\n").unwrap();
        check_layout("./plex/\n./plex/x\n").unwrap();
        for bad in [
            "plex/x\n../evil\n",
            "plex/x\n/etc/passwd\n",
            "etc/passwd\n",
            "plexx/y\n",
            "plex/../../etc/shadow\n",
            "",
            "plex\n",
        ] {
            assert!(check_layout(bad).is_err(), "accepted {bad:?}");
        }
    }

    // ── restore ───────────────────────────────────────────────────────────────

    /// Does what the real `tar xzf` would: puts a plex folder in the staging directory.
    struct Unpacks(FakeRunner);

    impl Unpacks {
        fn effect(&self, cmd: &Cmd) {
            if cmd.program == "tar" && cmd.args.first().map(String::as_str) == Some("xzf") {
                let at = cmd.args.iter().position(|a| a == "-C").unwrap();
                let into = PathBuf::from(&cmd.args[at + 1]);
                std::fs::create_dir_all(into.join("plex")).unwrap();
                std::fs::write(into.join("plex/Preferences.xml"), "new").unwrap();
            }
        }
    }

    impl Runner for Unpacks {
        async fn run(&self, cmd: &Cmd) -> Result<Output, RunError> {
            self.0.run(cmd).await
        }
        async fn stream(&self, cmd: &Cmd, sink: &dyn Sink) -> Result<i32, RunError> {
            self.effect(cmd);
            self.0.stream(cmd, sink).await
        }
        async fn cleanup(&self, cmd: &Cmd) -> Result<Output, RunError> {
            self.0.run(cmd).await
        }
    }

    fn restore_kit(listing: &str) -> (Kit, FakeRunner) {
        let kit = server_kit(&[]);
        let tarball = kit.root.write("backup.tar.gz", "archive");
        let mut kit = kit;
        kit.args = [(
            "tarball".to_string(),
            tarball.to_string_lossy().into_owned(),
        )]
        .into_iter()
        .collect();
        let fake = FakeRunner::new()
            .on("systemctl", &["show"], plex_state("active"))
            .on("tar", &["tzf"], out(listing))
            .ok("tar", &["xzf"])
            .ok("chown", &[])
            .ok("systemctl", &["stop"])
            .ok("systemctl", &["start"]);
        (kit, fake)
    }

    #[test]
    fn restore_unpacks_first_then_swaps_keeping_the_old_data() {
        let (kit, fake) = restore_kit("plex/\nplex/Preferences.xml\n");
        let runner = Unpacks(fake);
        assert_eq!(
            block_on(restore(&kit.ctx(&runner))),
            0,
            "{}",
            kit.sink.text()
        );

        let lib = kit.root.join("var/lib");
        assert_eq!(
            std::fs::read_to_string(lib.join("plex/Preferences.xml")).unwrap(),
            "new"
        );
        assert!(!lib.join("plex/old.txt").exists());
        let kept: Vec<_> = std::fs::read_dir(&lib)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("plex.bak-"))
            .collect();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(
            std::fs::read_to_string(lib.join(&kept[0]).join("old.txt")).unwrap(),
            "old"
        );
        assert!(!lib.join(STAGING).exists(), "the staging folder was left");

        let lines = runner.0.lines();
        let at = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no `{needle}` in {lines:?}"))
        };
        assert!(at("tar tzf") < at("tar xzf"));
        assert!(at("tar xzf") < at("chown -R plex:plex"));
        assert!(at("chown -R plex:plex") < at("systemctl stop plex.service"));
        assert!(at("systemctl stop plex.service") < at("systemctl start plex.service"));
    }

    #[test]
    fn a_bad_archive_is_refused_before_anything_is_unpacked_or_stopped() {
        let (kit, fake) = restore_kit("plex/x\n../evil\n");
        let runner = Unpacks(fake);
        assert_ne!(block_on(restore(&kit.ctx(&runner))), 0);
        let lines = runner.0.lines();
        assert!(!lines.iter().any(|l| l.contains("tar xzf")), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.starts_with("systemctl stop")),
            "{lines:?}"
        );
        assert!(kit.root.join("var/lib/plex/old.txt").exists());
    }

    #[test]
    fn an_archive_that_unpacks_to_nothing_leaves_plex_alone() {
        // The plain fake never creates files, as if tar produced no plex folder.
        let (kit, fake) = restore_kit("plex/\nplex/x\n");
        assert_ne!(block_on(restore(&kit.ctx(&fake))), 0);
        assert!(!fake.lines().iter().any(|l| l.starts_with("systemctl stop")));
        assert!(kit.root.join("var/lib/plex/old.txt").exists());
        assert!(!kit.root.join("var/lib").join(STAGING).exists());
    }

    #[test]
    fn a_missing_backup_file_or_plex_unit_changes_nothing() {
        let (mut kit, fake) = restore_kit("plex/x\n");
        kit.args = [("tarball".to_string(), "/no/such/backup.tar.gz".to_string())]
            .into_iter()
            .collect();
        assert_ne!(block_on(restore(&kit.ctx(&fake))), 0);
        assert!(fake.calls().is_empty());

        let (kit, _) = restore_kit("plex/x\n");
        let fake = FakeRunner::new().on(
            "systemctl",
            &["show"],
            out("Id=plex.service\nLoadState=not-found\nActiveState=inactive\nSubState=dead\n"),
        );
        assert_ne!(block_on(restore(&kit.ctx(&fake))), 0);
        assert!(!fake.lines().iter().any(|l| l.starts_with("tar")));
    }

    /// Cancels the `systemctl stop`, like a cancel that lands while Plex is stopping.
    struct CancelStop(FakeRunner);

    impl Runner for CancelStop {
        async fn run(&self, cmd: &Cmd) -> Result<Output, RunError> {
            self.0.run(cmd).await
        }
        async fn stream(&self, cmd: &Cmd, sink: &dyn Sink) -> Result<i32, RunError> {
            if cmd.program == "systemctl" && cmd.args.first().map(String::as_str) == Some("stop") {
                return Err(RunError::Cancelled);
            }
            self.0.stream(cmd, sink).await
        }
        async fn cleanup(&self, cmd: &Cmd) -> Result<Output, RunError> {
            self.0.run(cmd).await
        }
    }

    #[test]
    fn a_cancel_while_plex_is_stopping_still_starts_it_again() {
        let kit = server_kit(&[]);
        let runner = CancelStop(backup_runner(&kit, "active"));
        assert_ne!(block_on(backup(&kit.ctx(&runner))), 0);
        assert!(runner
            .0
            .lines()
            .contains(&"systemctl start plex.service".to_string()));

        let (kit, fake) = restore_kit("plex/\nplex/x\n");
        let runner = CancelStop(fake);
        let runner = Unpacks2(runner);
        assert_ne!(block_on(restore(&kit.ctx(&runner))), 0);
        assert!((runner.0)
            .0
            .lines()
            .contains(&"systemctl start plex.service".to_string()));
        assert!(kit.root.join("var/lib/plex/old.txt").exists());
    }

    /// Unpacks like [`Unpacks`], on top of any runner.
    struct Unpacks2(CancelStop);

    impl Runner for Unpacks2 {
        async fn run(&self, cmd: &Cmd) -> Result<Output, RunError> {
            self.0.run(cmd).await
        }
        async fn stream(&self, cmd: &Cmd, sink: &dyn Sink) -> Result<i32, RunError> {
            Unpacks(FakeRunner::new()).effect(cmd);
            self.0.stream(cmd, sink).await
        }
        async fn cleanup(&self, cmd: &Cmd) -> Result<Output, RunError> {
            self.0.cleanup(cmd).await
        }
    }

    #[test]
    fn plex_left_down_is_a_failed_job_even_though_the_work_was_done() {
        let kit = server_kit(&[]);
        let fake = FakeRunner::new()
            .on("systemctl", &["show"], plex_state("active"))
            .on("getent", &["passwd"], passwd(&kit))
            .ok("systemctl", &["stop"])
            .failing("systemctl", &["start"], 1, "")
            .ok("tar", &["czf"]);
        assert_ne!(block_on(backup(&kit.ctx(&fake))), 0);
        assert!(kit.sink.text().contains("did not start"));
        // The archive itself is kept.
        assert_eq!(
            std::fs::read_dir(kit.root.join("home/u")).unwrap().count(),
            1
        );
    }

    #[test]
    fn a_device_or_pipe_in_the_archive_is_refused_before_anything_is_stopped() {
        struct UnpacksPipe(FakeRunner);
        impl Runner for UnpacksPipe {
            async fn run(&self, cmd: &Cmd) -> Result<Output, RunError> {
                self.0.run(cmd).await
            }
            async fn stream(&self, cmd: &Cmd, sink: &dyn Sink) -> Result<i32, RunError> {
                if cmd.program == "tar" && cmd.args.first().map(String::as_str) == Some("xzf") {
                    let at = cmd.args.iter().position(|a| a == "-C").unwrap();
                    let into = PathBuf::from(&cmd.args[at + 1]).join("plex");
                    std::fs::create_dir_all(&into).unwrap();
                    let made = std::process::Command::new("mkfifo")
                        .arg(into.join("pipe"))
                        .status()
                        .unwrap();
                    assert!(made.success());
                }
                self.0.stream(cmd, sink).await
            }
        }
        let (kit, fake) = restore_kit("plex/\nplex/pipe\n");
        let runner = UnpacksPipe(fake);
        assert_ne!(block_on(restore(&kit.ctx(&runner))), 0);
        let lines = runner.0.lines();
        assert!(!lines.iter().any(|l| l.contains("chown")), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.starts_with("systemctl stop")),
            "{lines:?}"
        );
        assert!(kit.root.join("var/lib/plex/old.txt").exists());
    }

    #[test]
    fn a_backup_waits_for_the_lock_before_it_looks_at_plex() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        let kit = server_kit(&[]);
        let fake = backup_runner(&kit, "active");
        let held = block_on(crate::backup::DATA_LOCK.lock());
        let ctx = kit.ctx(&fake);
        let mut job = Box::pin(backup(&ctx));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(job.as_mut().poll(&mut cx), Poll::Pending));
        assert!(
            fake.calls().is_empty(),
            "it read Plex's state without the lock"
        );
        drop(held);
        // Other tests share the lock, so wait for it rather than expect it at once.
        let mut done = Poll::Pending;
        for _ in 0..5000 {
            done = job.as_mut().poll(&mut cx);
            if done.is_ready() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            matches!(done, Poll::Ready(0)),
            "{done:?} {}",
            kit.sink.text()
        );
    }
}
