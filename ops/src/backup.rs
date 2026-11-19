//! Backups: the scheduled restic run on demand, the NAS mirror jobs, and putting one
//! service's data back from a snapshot.
//!
//! The runs themselves belong to systemd units the NixOS modules define
//! (`restic-backups-main.service`, `nas-sync-<name>.service`); this starts them and
//! waits, the way an administrator would with `systemctl start --wait`.

use crate::runner::{Cmd, Output, RunError, Runner};
use crate::services::{self, parse_show, require_server, show_args, UnitInfo, ARR_META};
use crate::{capture, fail, run_error, stream, Ctx};
use std::path::{Component, Path};

const BACKUP_UNIT: &str = "restic-backups-main.service";
/// Which data paths belong to which service, written by the backup module. Under the
/// filesystem root so a test can supply its own.
const MANIFEST: &str = "etc/vexos/backup-paths.json";
/// The `restic` wrapper the backup module installs, with the repository and password
/// already filled in.
const RESTIC: &str = "restic-main";
/// Most snapshots listed; the newest come first.
const SNAPSHOTS_SHOWN: usize = 30;

/// Held while a job stops services and rewrites their data (restores and the Plex
/// archive), so two of them cannot work on the same files, or restart services under
/// each other, at once.
pub(crate) static DATA_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) fn finish(result: Result<(), i32>) -> i32 {
    result.err().unwrap_or(0)
}

/// A unit's state, or `None` when it does not exist on this host.
pub(crate) async fn unit_info<R: Runner>(
    ctx: &Ctx<'_, R>,
    unit: &str,
) -> Result<Option<UnitInfo>, i32> {
    let shown = capture(
        ctx,
        &Cmd::new("systemctl").args(show_args(&[unit.to_string()])),
    )
    .await?;
    Ok(parse_show(&shown.stdout)
        .into_iter()
        .find(|u| u.load != "not-found"))
}

/// The last lines of a unit's log, to show why it failed.
pub(crate) async fn journal_tail<R: Runner>(
    ctx: &Ctx<'_, R>,
    unit: &str,
    lines: u32,
) -> Result<(), i32> {
    let out = capture(
        ctx,
        &Cmd::new("journalctl").args(["-u", unit, "-n", &lines.to_string(), "--no-pager"]),
    )
    .await?;
    for line in out.stdout.lines().chain(out.stderr.lines()) {
        ctx.sink.info(line);
    }
    Ok(())
}

/// Start a one-shot unit and wait for it, showing its log if it fails.
async fn run_unit<R: Runner>(ctx: &Ctx<'_, R>, unit: &str, what: &str) -> Result<(), i32> {
    let code = stream(ctx, &Cmd::new("systemctl").args(["start", "--wait", unit])).await?;
    if code != 0 {
        ctx.sink.error(&format!(
            "{unit} failed (exit {code}). Last 20 lines of its log:"
        ));
        journal_tail(ctx, unit, 20).await?;
        return Err(1);
    }
    ctx.sink.info(&format!(
        "✔ {what} complete. Full log: journalctl -u {unit}"
    ));
    Ok(())
}

pub async fn backup_now<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    finish(backup_now_inner(ctx).await)
}

async fn backup_now_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    if unit_info(ctx, BACKUP_UNIT).await?.is_none() {
        return Err(fail(
            ctx.sink,
            format!("{BACKUP_UNIT} does not exist. Enable the backup service and rebuild first."),
        ));
    }
    run_unit(ctx, BACKUP_UNIT, "Backup run").await
}

pub async fn nas_sync_now<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    finish(nas_sync_now_inner(ctx).await)
}

async fn nas_sync_now_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let Some(name) = ctx.args.get("name") else {
        return Err(fail(ctx.sink, "a job name is required"));
    };
    let unit = format!("nas-sync-{name}.service");
    if unit_info(ctx, &unit).await?.is_none() {
        return Err(fail(
            ctx.sink,
            format!(
                "{unit} does not exist. Add vexos.server.nasSync.jobs.{name} in server-services.nix \
                 (with vexos.server.nasSync.enable = true) and rebuild."
            ),
        ));
    }
    run_unit(ctx, &unit, &format!("Sync '{name}'")).await
}

/// The job units in `systemctl list-unit-files` output, one per line, unit name first.
pub fn nas_sync_units(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|unit| unit.starts_with("nas-sync-") && unit.ends_with(".service"))
        .map(str::to_string)
        .collect()
}

pub async fn nas_sync_status<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    finish(nas_sync_status_inner(ctx).await)
}

async fn nas_sync_status_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    // The job names are the host's own (server-services.nix), so ask systemd for them.
    let listing = capture(
        ctx,
        &Cmd::new("systemctl").args(["list-unit-files", "nas-sync-*.service", "--no-legend"]),
    )
    .await?;
    let units = nas_sync_units(&listing.stdout);
    if units.is_empty() {
        ctx.sink.info(
            "No nas-sync jobs configured. Set vexos.server.nasSync.enable = true and add at least \
             one vexos.server.nasSync.jobs.<name> in server-services.nix, then rebuild.",
        );
        return Ok(());
    }
    ctx.sink.info("── Timers ──────────────────────────────");
    let timers = capture(
        ctx,
        &Cmd::new("systemctl").args(["list-timers", "nas-sync-*.timer", "--all", "--no-pager"]),
    )
    .await?;
    for line in timers.stdout.lines() {
        ctx.sink.info(line);
    }
    for unit in &units {
        ctx.sink
            .info(&format!("\n── {unit} ──────────────────────────"));
        // `status` exits 3 for a unit that is not running, which is normal for a timer job.
        let status = capture(
            ctx,
            &Cmd::new("systemctl").args(["status", unit, "--no-pager", "--lines=10"]),
        )
        .await?;
        for line in status.stdout.lines() {
            ctx.sink.info(line);
        }
    }
    Ok(())
}

/// The tag a service's snapshots carry, and its entry in the backup manifest: the arr
/// services share one (`arr`), every other service has its own name.
pub fn manifest_key(service: &str) -> &str {
    if ARR_META.contains(&service) {
        "arr"
    } else {
        service
    }
}

/// The systemd units behind a manifest key.
fn units_for_key(key: &str) -> Vec<String> {
    let names: &[&str] = if key == "arr" { &ARR_META } else { &[key] };
    names
        .iter()
        .filter_map(|name| services::find(name))
        .flat_map(|service| service.unit_names())
        .collect()
}

/// A path the manifest names, checked before it is handed to `restic restore --include`.
fn manifest_path_ok(path: &str) -> bool {
    path.starts_with('/')
        && path != "/"
        && !path.chars().any(char::is_control)
        && Path::new(path)
            .components()
            .all(|c| !matches!(c, Component::ParentDir))
}

/// The data paths registered for `key`, from the manifest under `root`.
pub fn manifest_paths(root: &Path, key: &str) -> Result<Vec<String>, String> {
    let file = root.join(MANIFEST);
    let text = std::fs::read_to_string(&file).map_err(|_| {
        format!(
            "{} was not found. Enable the backup service and rebuild first.",
            file.display()
        )
    })?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON: {e}", file.display()))?;
    let Some(object) = value.as_object() else {
        return Err(format!("{} is not a JSON object.", file.display()));
    };
    let Some(entry) = object.get(key) else {
        let known: Vec<&str> = object.keys().map(String::as_str).collect();
        return Err(format!(
            "'{key}' has no backup paths registered. Services with backups: {}",
            known.join(", ")
        ));
    };
    let paths: Vec<String> = entry
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|p| p.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if paths.is_empty() {
        return Err(format!("'{key}' has no backup paths registered."));
    }
    if let Some(bad) = paths.iter().find(|p| !manifest_path_ok(p)) {
        return Err(format!(
            "the backup manifest lists `{bad}`, which is not a safe place to restore into."
        ));
    }
    Ok(paths)
}

/// Run `restic-main` and capture it. A missing wrapper gets a plain explanation.
async fn restic<R: Runner>(ctx: &Ctx<'_, R>, args: &[&str]) -> Result<Output, i32> {
    match ctx
        .runner
        .run(&Cmd::new(RESTIC).args(args.iter().copied()))
        .await
    {
        Ok(output) => Ok(output),
        Err(RunError::Spawn { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Err(fail(
                ctx.sink,
                format!("{RESTIC} was not found. Enable the backup service and rebuild first."),
            ))
        }
        Err(other) => Err(run_error(ctx.sink, other)),
    }
}

/// `short-id  time  tags`, newest first, from `restic snapshots --json`.
pub fn snapshot_lines(json: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("restic gave unreadable output: {e}"))?;
    let Some(items) = value.as_array() else {
        return Err("restic gave unexpected output (not a list of snapshots).".to_string());
    };
    let mut rows: Vec<(String, String)> = items
        .iter()
        .map(|snap| {
            let text = |key: &str| snap.get(key).and_then(|v| v.as_str()).unwrap_or("");
            let id = match text("short_id") {
                "" => text("id").chars().take(8).collect(),
                short => short.to_string(),
            };
            let time = text("time");
            let when = time
                .get(..19)
                .map(|t| t.replace('T', " "))
                .unwrap_or_else(|| time.to_string());
            let tags: Vec<&str> = snap
                .get("tags")
                .and_then(|t| t.as_array())
                .map(|t| t.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            (when.clone(), format!("{id}  {when}  {}", tags.join(",")))
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(rows
        .into_iter()
        .take(SNAPSHOTS_SHOWN)
        .map(|(_, row)| row)
        .collect())
}

pub async fn snapshots<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    finish(snapshots_inner(ctx).await)
}

async fn snapshots_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let mut args = vec!["snapshots", "--json"];
    let tag;
    if let Some(name) = ctx.args.get("name") {
        tag = manifest_key(name).to_string();
        args.extend(["--tag", &tag]);
    }
    let out = restic(ctx, &args).await?;
    if out.code != 0 {
        for line in out.stderr.lines().take(10) {
            ctx.sink.error(line);
        }
        return Err(fail(
            ctx.sink,
            format!("restic snapshots exited with {}", out.code),
        ));
    }
    let rows = snapshot_lines(&out.stdout).map_err(|m| fail(ctx.sink, m))?;
    if rows.is_empty() {
        ctx.sink.info("No snapshots yet. Run a backup first.");
        return Ok(());
    }
    ctx.sink.info("ID        Time                 Tags");
    for row in &rows {
        ctx.sink.info(row);
    }
    ctx.sink
        .info("Use an ID in Restore a Service, or leave it as latest.");
    Ok(())
}

pub async fn restore_service<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    finish(restore_service_inner(ctx).await)
}

async fn restore_service_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let _only = DATA_LOCK.lock().await;
    let Some(name) = ctx.args.get("name") else {
        return Err(fail(ctx.sink, "a service is required"));
    };
    let snapshot = ctx
        .args
        .get("snapshot")
        .map(String::as_str)
        .unwrap_or("latest");
    let Some(service) = services::find(name) else {
        return Err(fail(ctx.sink, format!("unknown service `{name}`.")));
    };
    let key = manifest_key(&service.name);
    let paths = manifest_paths(ctx.root, key).map_err(|m| fail(ctx.sink, m))?;

    ctx.sink.info(&format!(
        "Restoring '{}' from snapshot '{snapshot}' (tag {key}) into:",
        service.name
    ));
    for path in &paths {
        ctx.sink.info(&format!("  {path}"));
    }
    if key != service.name {
        ctx.sink
            .info("The arr services share one backup entry, so they are restored together.");
    }

    // Find out the tool works before anything is stopped.
    let version = restic(ctx, &["version"]).await?;
    if version.code != 0 {
        return Err(fail(
            ctx.sink,
            format!("{RESTIC} does not run (exit {}).", version.code),
        ));
    }

    let wanted = units_for_key(key);
    let shown = capture(ctx, &Cmd::new("systemctl").args(show_args(&wanted))).await?;
    let running: Vec<String> = parse_show(&shown.stdout)
        .into_iter()
        .filter(|u| {
            u.load != "not-found"
                && matches!(u.active.as_str(), "active" | "activating" | "reloading")
        })
        .map(|u| format!("{}.service", u.id))
        .collect();

    let result = replace_data(ctx, snapshot, key, &paths, &running).await;

    // Bring back what was stopped, whatever happened, and even after a cancel.
    if !running.is_empty() {
        ctx.sink
            .info(&format!("Starting again: {}", running.join(" ")));
        let started = ctx
            .runner
            .cleanup(&Cmd::new("systemctl").arg("start").args(&running))
            .await;
        match started {
            Ok(out) if out.code == 0 => {}
            Ok(out) => {
                ctx.sink.error(&format!(
                    "error: could not start {} again (exit {}). Start it from its service page.",
                    running.join(" "),
                    out.code
                ));
                return Err(1);
            }
            Err(e) => {
                ctx.sink.error(&format!(
                    "error: could not start {} again: {e}. Start it from its service page.",
                    running.join(" ")
                ));
                return Err(1);
            }
        }
    }
    result?;
    ctx.sink.info(&format!(
        "✔ Restore complete for '{}'. Check it from its service page.",
        service.name
    ));
    Ok(())
}

/// Stop the service's units, then restore over its data in place.
async fn replace_data<R: Runner>(
    ctx: &Ctx<'_, R>,
    snapshot: &str,
    key: &str,
    paths: &[String],
    running: &[String],
) -> Result<(), i32> {
    if running.is_empty() {
        ctx.sink
            .info("warning: nothing for this service is running, so nothing is stopped first.");
    } else {
        ctx.sink.info(&format!("Stopping: {}", running.join(" ")));
        let code = stream(ctx, &Cmd::new("systemctl").arg("stop").args(running)).await?;
        if code != 0 {
            return Err(fail(
                ctx.sink,
                format!("systemctl stop exited with {code}; nothing was restored."),
            ));
        }
    }
    let mut cmd = Cmd::new(RESTIC).args(["restore", snapshot, "--tag", key, "--target", "/"]);
    for path in paths {
        cmd = cmd.arg("--include").arg(path);
    }
    let code = stream(ctx, &cmd).await?;
    if code != 0 {
        return Err(fail(
            ctx.sink,
            format!("restic restore exited with {code}."),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::testing::{block_on, FakeRunner};
    use crate::runner::Sink;
    use crate::testkit::Kit;

    fn out(stdout: &str) -> Output {
        Output {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn shown(units: &[(&str, &str, &str)]) -> Output {
        let blocks: Vec<String> = units
            .iter()
            .map(|(id, load, active)| {
                format!("Id={id}.service\nLoadState={load}\nActiveState={active}\nSubState=x\n")
            })
            .collect();
        out(&blocks.join("\n"))
    }

    fn server_kit(args: &[(&str, &str)]) -> Kit {
        let kit = Kit::new().with_args(args);
        kit.nixos_file("vexos-variant", "vexos-server-amd\n");
        kit
    }

    fn manifest(kit: &Kit, json: &str) {
        let dir = kit.root.join("etc/vexos");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("backup-paths.json"), json).unwrap();
    }

    // ── backup-now and nas-sync ────────────────────────────────────────────────

    #[test]
    fn backup_now_starts_the_restic_unit_and_waits_for_it() {
        let kit = server_kit(&[]);
        let fake = FakeRunner::new().ok("systemctl", &["start", "--wait"]).on(
            "systemctl",
            &["show"],
            shown(&[("restic-backups-main", "loaded", "inactive")]),
        );
        assert_eq!(
            block_on(backup_now(&kit.ctx(&fake))),
            0,
            "{}",
            kit.sink.text()
        );
        assert!(fake
            .lines()
            .contains(&"systemctl start --wait restic-backups-main.service".to_string()));
        assert!(kit.sink.text().contains("Backup run complete"));
    }

    #[test]
    fn backup_now_without_the_unit_starts_nothing() {
        let kit = server_kit(&[]);
        let fake = FakeRunner::new().on(
            "systemctl",
            &["show"],
            shown(&[("restic-backups-main", "not-found", "inactive")]),
        );
        assert_ne!(block_on(backup_now(&kit.ctx(&fake))), 0);
        assert!(!fake.lines().iter().any(|l| l.contains("start")));
        assert!(kit.sink.text().contains("Enable the backup service"));
    }

    #[test]
    fn a_failed_backup_shows_the_log_and_fails_the_job() {
        let kit = server_kit(&[]);
        let fake = FakeRunner::new()
            .failing("systemctl", &["start", "--wait"], 1, "")
            .on("journalctl", &[], out("restic: repository locked"))
            .on(
                "systemctl",
                &["show"],
                shown(&[("restic-backups-main", "loaded", "failed")]),
            );
        assert_ne!(block_on(backup_now(&kit.ctx(&fake))), 0);
        assert!(kit.sink.text().contains("repository locked"));
    }

    #[test]
    fn backups_are_for_server_roles_only() {
        let kit = Kit::new();
        kit.nixos_file("vexos-variant", "vexos-desktop-amd\n");
        let fake = FakeRunner::new();
        assert_ne!(block_on(backup_now(&kit.ctx(&fake))), 0);
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn a_nas_sync_job_keeps_the_case_the_host_named_it_with() {
        let kit = server_kit(&[("name", "Media-Mirror")]);
        let fake = FakeRunner::new().ok("systemctl", &["start", "--wait"]).on(
            "systemctl",
            &["show"],
            shown(&[("nas-sync-Media-Mirror", "loaded", "inactive")]),
        );
        assert_eq!(
            block_on(nas_sync_now(&kit.ctx(&fake))),
            0,
            "{}",
            kit.sink.text()
        );
        assert!(fake
            .lines()
            .contains(&"systemctl start --wait nas-sync-Media-Mirror.service".to_string()));
    }

    #[test]
    fn an_unknown_nas_sync_job_says_how_to_add_it() {
        let kit = server_kit(&[("name", "tv")]);
        let fake = FakeRunner::new().on(
            "systemctl",
            &["show"],
            shown(&[("nas-sync-tv", "not-found", "inactive")]),
        );
        assert_ne!(block_on(nas_sync_now(&kit.ctx(&fake))), 0);
        assert!(kit.sink.text().contains("vexos.server.nasSync.jobs.tv"));
    }

    #[test]
    fn nas_sync_jobs_are_found_from_the_unit_list() {
        let listing = "nas-sync-tv.service          static  -\n\
                       nas-sync-movies.service      static  -\n\
                       nas-sync-extra.timer         enabled -\n\
                       other.service                static  -\n";
        assert_eq!(
            nas_sync_units(listing),
            ["nas-sync-tv.service", "nas-sync-movies.service"]
        );
        assert!(nas_sync_units("").is_empty());
    }

    #[test]
    fn nas_sync_status_says_so_when_there_are_no_jobs() {
        let kit = server_kit(&[]);
        let fake = FakeRunner::new().ok("systemctl", &["list-unit-files"]);
        assert_eq!(block_on(nas_sync_status(&kit.ctx(&fake))), 0);
        assert!(kit.sink.text().contains("No nas-sync jobs configured"));
    }

    #[test]
    fn nas_sync_status_shows_the_timers_and_each_job() {
        let kit = server_kit(&[]);
        let fake = FakeRunner::new()
            .on(
                "systemctl",
                &["list-unit-files"],
                out("nas-sync-tv.service static -\n"),
            )
            .on(
                "systemctl",
                &["list-timers"],
                out("NEXT  nas-sync-tv.timer"),
            )
            .failing("systemctl", &["status"], 3, "");
        assert_eq!(
            block_on(nas_sync_status(&kit.ctx(&fake))),
            0,
            "{}",
            kit.sink.text()
        );
        let text = kit.sink.text();
        assert!(text.contains("nas-sync-tv.timer"), "{text}");
        assert!(text.contains("── nas-sync-tv.service"), "{text}");
    }

    // ── snapshots ─────────────────────────────────────────────────────────────

    const SNAPSHOTS: &str = r#"[
      {"short_id":"aaaa1111","time":"2025-01-01T03:00:01.5+00:00","tags":["vexos","jellyfin"],"paths":["/var/lib/jellyfin"]},
      {"id":"bbbb2222cccc3333","time":"2025-01-03T03:00:02.5+00:00","tags":["vexos"]}
    ]"#;

    #[test]
    fn snapshots_list_newest_first_with_a_short_id() {
        let rows = snapshot_lines(SNAPSHOTS).unwrap();
        assert_eq!(rows[0], "bbbb2222  2025-01-03 03:00:02  vexos");
        assert_eq!(rows[1], "aaaa1111  2025-01-01 03:00:01  vexos,jellyfin");
        assert!(snapshot_lines("not json").is_err());
        assert!(snapshot_lines("{}").is_err());
        assert!(snapshot_lines("[]").unwrap().is_empty());
    }

    #[test]
    fn the_snapshot_list_is_capped() {
        let many: Vec<String> = (0..50)
            .map(|i| {
                format!(
                    r#"{{"short_id":"id{i:02}","time":"2025-01-{:02}T00:00:00+00:00"}}"#,
                    i % 28 + 1
                )
            })
            .collect();
        let rows = snapshot_lines(&format!("[{}]", many.join(","))).unwrap();
        assert_eq!(rows.len(), SNAPSHOTS_SHOWN);
    }

    #[test]
    fn listing_for_an_arr_service_filters_by_the_shared_tag() {
        let kit = server_kit(&[("name", "sonarr")]);
        let fake = FakeRunner::new().on("restic-main", &["snapshots"], out(SNAPSHOTS));
        assert_eq!(
            block_on(snapshots(&kit.ctx(&fake))),
            0,
            "{}",
            kit.sink.text()
        );
        assert_eq!(fake.lines(), ["restic-main snapshots --json --tag arr"],);
        assert!(kit.sink.text().contains("bbbb2222"));
    }

    #[test]
    fn a_missing_restic_wrapper_is_explained() {
        struct Absent;
        impl Runner for Absent {
            async fn run(&self, cmd: &Cmd) -> Result<Output, RunError> {
                Err(RunError::Spawn {
                    program: cmd.program.clone(),
                    source: std::io::Error::from(std::io::ErrorKind::NotFound),
                })
            }
            async fn stream(&self, cmd: &Cmd, _: &dyn Sink) -> Result<i32, RunError> {
                self.run(cmd).await.map(|o| o.code)
            }
        }
        let kit = server_kit(&[]);
        assert_ne!(block_on(snapshots(&kit.ctx(&Absent))), 0);
        assert!(kit.sink.text().contains("restic-main was not found"));
    }

    // ── restore-service ───────────────────────────────────────────────────────

    #[test]
    fn the_arr_services_share_a_manifest_entry() {
        for name in ["sonarr", "radarr", "lidarr", "prowlarr", "sabnzbd"] {
            assert_eq!(manifest_key(name), "arr");
        }
        assert_eq!(manifest_key("jellyfin"), "jellyfin");
        assert_eq!(units_for_key("arr").len(), 5, "{:?}", units_for_key("arr"));
        assert_eq!(units_for_key("jellyfin"), ["jellyfin.service"]);
    }

    fn restore_runner() -> FakeRunner {
        FakeRunner::new()
            .ok("restic-main", &["version"])
            .ok("restic-main", &["restore"])
            .ok("systemctl", &["stop"])
            .ok("systemctl", &["start"])
            .on(
                "systemctl",
                &["show"],
                shown(&[("jellyfin", "loaded", "active")]),
            )
    }

    #[test]
    fn restore_stops_the_service_restores_in_place_and_starts_it_again() {
        let kit = server_kit(&[("name", "jellyfin"), ("snapshot", "latest")]);
        manifest(
            &kit,
            r#"{"jellyfin":["/var/lib/jellyfin","/etc/jellyfin"]}"#,
        );
        let fake = restore_runner();
        assert_eq!(
            block_on(restore_service(&kit.ctx(&fake))),
            0,
            "{}",
            kit.sink.text()
        );
        let lines = fake.lines();
        let at = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no `{needle}` in {lines:?}"))
        };
        assert!(at("systemctl stop jellyfin.service") < at("restic-main restore"));
        assert!(at("restic-main restore") < at("systemctl start jellyfin.service"));
        assert!(lines.contains(
            &"restic-main restore latest --tag jellyfin --target / \
              --include /var/lib/jellyfin --include /etc/jellyfin"
                .to_string()
        ));
    }

    #[test]
    fn a_failed_restore_still_starts_the_service_again() {
        let kit = server_kit(&[("name", "jellyfin")]);
        manifest(&kit, r#"{"jellyfin":["/var/lib/jellyfin"]}"#);
        let fake = FakeRunner::new()
            .ok("restic-main", &["version"])
            .failing("restic-main", &["restore"], 1, "no matching snapshot")
            .ok("systemctl", &["stop"])
            .ok("systemctl", &["start"])
            .on(
                "systemctl",
                &["show"],
                shown(&[("jellyfin", "loaded", "active")]),
            );
        assert_ne!(block_on(restore_service(&kit.ctx(&fake))), 0);
        assert!(fake
            .lines()
            .contains(&"systemctl start jellyfin.service".to_string()));
    }

    #[test]
    fn a_cancelled_restore_still_starts_the_service_again() {
        /// Cancels the restore itself but, like the real runner, still allows cleanup.
        struct CancelRestore(FakeRunner);
        impl Runner for CancelRestore {
            async fn run(&self, cmd: &Cmd) -> Result<Output, RunError> {
                self.0.run(cmd).await
            }
            async fn stream(&self, cmd: &Cmd, sink: &dyn Sink) -> Result<i32, RunError> {
                if cmd.program == "restic-main" {
                    return Err(RunError::Cancelled);
                }
                self.0.stream(cmd, sink).await
            }
            async fn cleanup(&self, cmd: &Cmd) -> Result<Output, RunError> {
                self.0.run(cmd).await
            }
        }
        let kit = server_kit(&[("name", "jellyfin")]);
        manifest(&kit, r#"{"jellyfin":["/var/lib/jellyfin"]}"#);
        let runner = CancelRestore(restore_runner());
        assert_ne!(block_on(restore_service(&kit.ctx(&runner))), 0);
        assert!(runner
            .0
            .lines()
            .contains(&"systemctl start jellyfin.service".to_string()));
    }

    #[test]
    fn a_service_that_is_not_running_is_restored_without_stopping_anything() {
        let kit = server_kit(&[("name", "jellyfin")]);
        manifest(&kit, r#"{"jellyfin":["/var/lib/jellyfin"]}"#);
        let fake = FakeRunner::new()
            .ok("restic-main", &["version"])
            .ok("restic-main", &["restore"])
            .on(
                "systemctl",
                &["show"],
                shown(&[("jellyfin", "loaded", "inactive")]),
            );
        assert_eq!(
            block_on(restore_service(&kit.ctx(&fake))),
            0,
            "{}",
            kit.sink.text()
        );
        assert!(!fake
            .lines()
            .iter()
            .any(|l| l.starts_with("systemctl stop") || l.starts_with("systemctl start")));
        assert!(kit
            .sink
            .text()
            .contains("nothing for this service is running"));
    }

    #[test]
    fn restore_refuses_without_changing_anything_when_it_cannot_start() {
        // No manifest: nothing is stopped and restic is never asked to restore.
        let kit = server_kit(&[("name", "jellyfin")]);
        let fake = restore_runner();
        assert_ne!(block_on(restore_service(&kit.ctx(&fake))), 0);
        assert!(fake.calls().is_empty());

        // A service with no entry lists the ones that have one.
        let kit = server_kit(&[("name", "jellyfin")]);
        manifest(&kit, r#"{"plex":["/var/lib/plex"]}"#);
        let fake = restore_runner();
        assert_ne!(block_on(restore_service(&kit.ctx(&fake))), 0);
        assert!(fake.calls().is_empty());
        assert!(kit.sink.text().contains("plex"));

        // Unknown service.
        let kit = server_kit(&[("name", "not-a-service")]);
        manifest(&kit, "{}");
        let fake = restore_runner();
        assert_ne!(block_on(restore_service(&kit.ctx(&fake))), 0);
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn a_manifest_path_that_could_escape_is_refused() {
        for bad in ["/", "relative", "/var/../etc", "/var/lib/x\ny"] {
            let kit = server_kit(&[("name", "jellyfin")]);
            manifest(
                &kit,
                &format!(r#"{{"jellyfin":[{}]}}"#, serde_json::json!(bad)),
            );
            let fake = restore_runner();
            assert_ne!(block_on(restore_service(&kit.ctx(&fake))), 0, "{bad}");
            assert!(fake.calls().is_empty(), "{bad}");
        }
    }

    #[test]
    fn restoring_one_arr_service_stops_and_restores_the_whole_group() {
        let kit = server_kit(&[("name", "sonarr")]);
        manifest(&kit, r#"{"arr":["/var/lib/sonarr","/var/lib/radarr"]}"#);
        let fake = FakeRunner::new()
            .ok("restic-main", &["version"])
            .ok("restic-main", &["restore"])
            .ok("systemctl", &["stop"])
            .ok("systemctl", &["start"])
            .on(
                "systemctl",
                &["show"],
                shown(&[
                    ("sabnzbd", "loaded", "active"),
                    ("sonarr", "loaded", "active"),
                    ("radarr", "loaded", "inactive"),
                    ("lidarr", "not-found", "inactive"),
                ]),
            );
        assert_eq!(
            block_on(restore_service(&kit.ctx(&fake))),
            0,
            "{}",
            kit.sink.text()
        );
        let lines = fake.lines();
        assert!(
            lines.contains(&"systemctl stop sabnzbd.service sonarr.service".to_string()),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("--tag arr")));
    }
}
