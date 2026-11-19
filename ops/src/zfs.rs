//! ZFS pools: create, import, destroy, scrub, clear, and replace, add, attach or detach
//! disks.
//!
//! What `just zfs-pool` does, one operation per action. A pool is registered for import
//! at boot in `/etc/nixos/zfs-pools.nix`, between the same two marker comments the
//! script uses, so either path can manage the other's pools.
//!
//! Every operation that erases a disk goes through [`crate::disks`]: the disk must pass
//! the eligibility check at that moment, and a wipe that fails stops the job before
//! anything is built on the disk. The typed pool-name confirmation is checked by the
//! daemon's validation before any operation starts.

use crate::disks::{self, Disk, ONE_AT_A_TIME};
use crate::nixedit::{self, EditError};
use crate::runner::{Cmd, RunError, Runner};
use crate::services::require_server;
use crate::{capture, fail, fsutil, run_error, stream, Ctx};
use std::path::{Path, PathBuf};

pub const FILE: &str = "zfs-pools.nix";
pub const BEGIN: &str = "# >>> vexos-zfs-pools >>>";
pub const END: &str = "# <<< vexos-zfs-pools <<<";

pub fn path(nixos_dir: &Path) -> PathBuf {
    nixos_dir.join(FILE)
}

// ───────────────────────────── Names and parsers ─────────────────────────────

/// What zpool refuses as a pool name, or this module will not use: a letter first, then
/// letters, digits and `. _ : -`; not a keyword; not `c0`–`c9…`, which zpool reserves.
pub fn valid_name(name: &str) -> Result<(), String> {
    let shape = name.len() <= 200
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'));
    if !shape {
        return Err(format!(
            "`{name}` is not a valid pool name: start with a letter and use letters, digits and . _ : - only."
        ));
    }
    let lower = name.to_ascii_lowercase();
    // ZFS refuses any name that begins with one of these words, not only the word itself.
    let reserved = matches!(lower.as_str(), "log" | "cache" | "special")
        || ["mirror", "raidz", "draid", "spare"]
            .iter()
            .any(|p| lower.starts_with(p))
        || (lower.starts_with('c') && lower[1..].starts_with(|c: char| c.is_ascii_digit()));
    if reserved {
        return Err(format!("`{name}` is a name ZFS reserves. Choose another."));
    }
    Ok(())
}

/// A Proxmox storage ID.
pub fn valid_storage_id(id: &str) -> bool {
    id.len() <= 64
        && id.starts_with(|c: char| c.is_ascii_alphabetic())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A row of `zpool list -H -o name,size,alloc,free,health`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolRow {
    pub name: String,
    pub size: String,
    pub used: String,
    pub free: String,
    pub health: String,
}

pub const LIST_ARGS: [&str; 4] = ["list", "-H", "-o", "name,size,alloc,free,health"];

pub fn parse_pool_list(text: &str) -> Vec<PoolRow> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            (f.len() >= 5 && !f[0].is_empty()).then(|| PoolRow {
                name: f[0].into(),
                size: f[1].into(),
                used: f[2].into(),
                free: f[3].into(),
                health: f[4].trim().into(),
            })
        })
        .collect()
}

/// One disk of a pool, as `zpool status -P` shows it: its full path (a number for a disk
/// that has gone missing) and its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    pub device: String,
    pub state: String,
}

/// The disks in the config table of `zpool status -P <pool>`, skipping the pool itself and
/// the grouping lines (`mirror-0`, `raidz1-0`, `logs`, `replacing-1` …).
pub fn leaves(status: &str, pool: &str) -> Vec<Leaf> {
    let mut in_table = false;
    let mut out = Vec::new();
    for line in status.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() >= 2 && words[0] == "NAME" && words[1] == "STATE" {
            in_table = true;
            continue;
        }
        if in_table && words.is_empty() {
            in_table = false;
        }
        if in_table && words.len() >= 2 && words[0] != pool && !is_group(words[0]) {
            out.push(Leaf {
                device: words[0].into(),
                state: words[1].into(),
            });
        }
    }
    out
}

fn is_group(word: &str) -> bool {
    let base = word.rsplit_once('-').map_or(word, |(head, tail)| {
        if tail.chars().all(|c| c.is_ascii_digit()) && !tail.is_empty() {
            head
        } else {
            word
        }
    });
    matches!(
        base,
        "mirror" | "spare" | "replacing" | "logs" | "cache" | "spares" | "special" | "dedup"
    ) || base.starts_with("raidz")
        || base.starts_with("draid")
}

/// The pool names in `zpool import` output.
pub fn parse_importable(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.trim_start().strip_prefix("pool:"))
        .map(|rest| rest.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect()
}

/// The Proxmox storage IDs whose pool is `pool` or a dataset under it.
pub fn pve_storage_ids(config: &str, pool: &str) -> Vec<String> {
    let mut id: Option<String> = None;
    let mut found = Vec::new();
    for line in config.lines() {
        if let Some(rest) = line.strip_prefix("zfspool:") {
            id = rest.split_whitespace().next().map(str::to_string);
            continue;
        }
        if !line.is_empty() && !line.starts_with(char::is_whitespace) {
            id = None;
            continue;
        }
        let mut words = line.split_whitespace();
        if id.is_some() && words.next() == Some("pool") {
            if let Some(target) = words.next() {
                if target == pool || target.starts_with(&format!("{pool}/")) {
                    found.extend(id.take());
                }
            }
        }
    }
    found
}

// ───────────────────────────────── Layouts ─────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Single,
    Mirror,
    Raidz1,
    Raidz2,
    Raidz3,
    Raid10,
}

impl Layout {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "single" => Self::Single,
            "mirror" => Self::Mirror,
            "raidz1" => Self::Raidz1,
            "raidz2" => Self::Raidz2,
            "raidz3" => Self::Raidz3,
            "raid10" => Self::Raid10,
            _ => return None,
        })
    }

    pub fn min_disks(self) -> usize {
        match self {
            Self::Single => 1,
            Self::Mirror => 2,
            Self::Raidz1 => 3,
            Self::Raidz2 | Self::Raid10 => 4,
            Self::Raidz3 => 5,
        }
    }

    /// The vdev part of a `zpool create` or `zpool add` command.
    pub fn vdev_args(self, paths: &[String]) -> Result<Vec<String>, String> {
        if paths.len() < self.min_disks() {
            return Err(format!(
                "this layout needs at least {} disk(s); {} chosen.",
                self.min_disks(),
                paths.len()
            ));
        }
        let keyword = |word: &str| {
            std::iter::once(word.to_string())
                .chain(paths.iter().cloned())
                .collect::<Vec<_>>()
        };
        Ok(match self {
            Self::Single => paths.to_vec(),
            Self::Mirror => keyword("mirror"),
            Self::Raidz1 => keyword("raidz1"),
            Self::Raidz2 => keyword("raidz2"),
            Self::Raidz3 => keyword("raidz3"),
            Self::Raid10 => {
                if !paths.len().is_multiple_of(2) {
                    return Err("striped mirrors need an even number of disks.".into());
                }
                paths
                    .chunks(2)
                    .flat_map(|pair| ["mirror".to_string(), pair[0].clone(), pair[1].clone()])
                    .collect()
            }
        })
    }
}

// ───────────────────── The boot-time import list (zfs-pools.nix) ─────────────────────

/// The names listed between the markers.
pub fn registered(text: &str) -> Vec<String> {
    let mut inside = false;
    let mut names = Vec::new();
    for line in text.lines() {
        if line.contains(BEGIN) {
            inside = true;
        } else if line.contains(END) {
            inside = false;
        } else if inside {
            names.extend(quoted(line));
        }
    }
    names
}

/// Every `"name"` on a line.
fn quoted(line: &str) -> Vec<String> {
    line.split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// Whether everything between the markers is a quoted name: nothing else would survive a
/// rewrite of the block.
pub fn block_is_plain(text: &str) -> bool {
    let mut inside = false;
    for line in text.lines() {
        if line.contains(BEGIN) {
            inside = true;
        } else if line.contains(END) {
            inside = false;
        } else if inside {
            let outside: String = line.split('"').step_by(2).collect::<Vec<_>>().join("");
            if !outside.trim().is_empty() {
                return false;
            }
        }
    }
    true
}

/// A new file with an empty list.
pub fn template(stamp: &str) -> String {
    format!(
        "# /etc/nixos/zfs-pools.nix\n\
         # Written by VexPortal or `just zfs-pool` on {stamp}.\n\
         # Registers ZFS pools managed from either so NixOS generates a zfs-import-<pool>.service\n\
         # unit for each and imports them on every boot. Without this, a pool created by hand\n\
         # never imports on its own; see modules/zfs-server.nix.\n\
         # Host-generated: do NOT commit to the vexos-nix repo.\n\
         {{ ... }}:\n\
         {{\n  boot.zfs.extraPools = [\n    {BEGIN}\n    {END}\n  ];\n}}\n"
    )
}

/// `text` with the names between the markers replaced. `Err` if a marker is missing:
/// a file somebody rewrote by hand is left alone rather than guessed at.
pub fn with_registered(text: &str, names: &[String]) -> Result<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    let begin = lines.iter().position(|l| l.contains(BEGIN));
    let end = lines.iter().position(|l| l.contains(END));
    let (Some(begin), Some(end)) = (begin, end.filter(|e| Some(*e) > begin)) else {
        return Err(format!(
            "{FILE} has no `{BEGIN}` and `{END}` lines, so VexPortal will not edit it."
        ));
    };
    let mut out: Vec<String> = lines[..=begin].iter().map(|l| l.to_string()).collect();
    out.extend(names.iter().map(|n| format!("    \"{n}\"")));
    out.extend(lines[end..].iter().map(|l| l.to_string()));
    Ok(out.join("\n") + "\n")
}

async fn edit_registry<R: Runner>(
    ctx: &Ctx<'_, R>,
    change: impl Fn(&mut Vec<String>) -> bool,
) -> Result<bool, String> {
    let file = path(ctx.nixos_dir);
    if !file.exists() {
        fsutil::write_atomic(&file, template(&fsutil::now_stamp()).as_bytes(), 0o644)
            .map_err(|e| format!("could not create {FILE}: {e}"))?;
    }
    let mut changed = false;
    nixedit::edit_file(ctx.runner, &file, |text| {
        if !block_is_plain(text) {
            return Err(EditError::Invalid(format!(
                "{FILE} has something other than quoted pool names between its markers, so VexPortal will not edit it."
            )));
        }
        let mut names = registered(text);
        if !change(&mut names) {
            return Ok(None);
        }
        changed = true;
        with_registered(text, &names)
            .map(Some)
            .map_err(EditError::Invalid)
    })
    .await
    .map_err(|e| format!("could not update {FILE}: {e}"))?;
    Ok(changed)
}

/// Import `pool` at boot.
pub async fn register<R: Runner>(ctx: &Ctx<'_, R>, pool: &str) -> Result<(), String> {
    edit_registry(ctx, |names| {
        if names.iter().any(|n| n == pool) {
            false
        } else {
            names.push(pool.to_string());
            true
        }
    })
    .await
    .map(|_| ())
}

/// Stop importing `pool` at boot. `Ok(false)` if it was not listed.
pub async fn unregister<R: Runner>(ctx: &Ctx<'_, R>, pool: &str) -> Result<bool, String> {
    if !path(ctx.nixos_dir).exists() {
        return Ok(false);
    }
    edit_registry(ctx, |names| {
        let before = names.len();
        names.retain(|n| n != pool);
        names.len() != before
    })
    .await
}

// ───────────────────────────── Reading the machine ─────────────────────────────

/// The imported pools. `None` if ZFS is not installed here.
async fn list_pools<R: Runner>(ctx: &Ctx<'_, R>) -> Result<Option<Vec<PoolRow>>, i32> {
    match ctx.runner.run(&Cmd::new("zpool").args(LIST_ARGS)).await {
        Ok(out) if out.success() => Ok(Some(parse_pool_list(&out.stdout))),
        Ok(out) => Err(fail(
            ctx.sink,
            format!("zpool list failed: {}", out.stderr.trim()),
        )),
        Err(RunError::Spawn { .. }) => Ok(None),
        Err(e) => Err(run_error(ctx.sink, e)),
    }
}

async fn imported<R: Runner>(ctx: &Ctx<'_, R>) -> Result<Vec<String>, i32> {
    match list_pools(ctx).await? {
        Some(rows) => Ok(rows.into_iter().map(|r| r.name).collect()),
        None => Err(fail(
            ctx.sink,
            "ZFS is not set up on this host: `zpool` was not found. modules/zfs-server.nix must be active.",
        )),
    }
}

/// Kernel names of the disks and partitions that belong to an imported pool.
pub async fn pool_members<R: Runner>(ctx: &Ctx<'_, R>) -> Result<Vec<String>, i32> {
    let Some(rows) = list_pools(ctx).await? else {
        return Ok(Vec::new());
    };
    let mut members = Vec::new();
    for row in rows {
        let status = capture(ctx, &Cmd::new("zpool").args(["status", "-P", &row.name])).await?;
        if !status.success() {
            // A pool whose disks cannot be read must not let its disks look free.
            return Err(fail(
                ctx.sink,
                format!(
                    "could not read the disks of pool `{}`, so no disk can be checked: {}",
                    row.name,
                    status.stderr.trim()
                ),
            ));
        }
        for leaf in leaves(&status.stdout, &row.name) {
            members.extend(disks::kernel_name(ctx.root, &leaf.device));
        }
    }
    Ok(members)
}

fn arg<'a, R: Runner>(ctx: &'a Ctx<'_, R>, name: &str) -> Option<&'a str> {
    ctx.args
        .get(name)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
}

/// The `pool` answer, which must be one that is imported.
async fn existing_pool<R: Runner>(ctx: &Ctx<'_, R>) -> Result<String, i32> {
    let Some(pool) = arg(ctx, "pool") else {
        return Err(fail(ctx.sink, "a pool is required."));
    };
    let pools = imported(ctx).await?;
    if !pools.iter().any(|p| p == pool) {
        return Err(fail(
            ctx.sink,
            format!(
                "`{pool}` is not an imported pool. Imported: {}.",
                if pools.is_empty() {
                    "none".to_string()
                } else {
                    pools.join(", ")
                }
            ),
        ));
    }
    Ok(pool.to_string())
}

/// A disk of `pool`, named by an answer. It must be one `zpool status` lists now, so the
/// caller can never point a command at anything else.
async fn pool_disk<R: Runner>(
    ctx: &Ctx<'_, R>,
    pool: &str,
    name: &str,
    what: &str,
) -> Result<String, i32> {
    let Some(device) = arg(ctx, name) else {
        return Err(fail(ctx.sink, format!("{what} is required.")));
    };
    let status = capture(ctx, &Cmd::new("zpool").args(["status", "-P", pool])).await?;
    if !status.success() {
        return Err(fail(
            ctx.sink,
            format!(
                "could not read the disks of `{pool}`: {}",
                status.stderr.trim()
            ),
        ));
    }
    if !leaves(&status.stdout, pool)
        .iter()
        .any(|l| l.device == device)
    {
        return Err(fail(
            ctx.sink,
            format!("`{device}` is not a disk of pool `{pool}`."),
        ));
    }
    Ok(device.to_string())
}

/// The one new disk, from the `disks` answer.
async fn one_new_disk<R: Runner>(ctx: &Ctx<'_, R>) -> Result<Disk, i32> {
    let available = disks::eligible_now(ctx, &[]).await?;
    let chosen =
        disks::pick(&available, arg(ctx, "disks").unwrap_or("")).map_err(|e| fail(ctx.sink, e))?;
    match chosen.as_slice() {
        [one] => Ok((*one).clone()),
        _ => Err(fail(ctx.sink, "choose exactly one disk.")),
    }
}

/// What ZFS keeps back on a whole disk (the 8 MiB reserved partition, the GPT and alignment),
/// rounded up.
const PARTITION_OVERHEAD: u64 = 16 * 1024 * 1024;

/// Refuse a new disk that is smaller than the one it stands in for, before it is erased.
/// A disk that is missing has no size to compare, and ZFS then decides.
async fn too_small<R: Runner>(ctx: &Ctx<'_, R>, new: &Disk, old: &str) -> Result<(), i32> {
    if let Some(needed) = disks::size_of(ctx, old).await? {
        // ZFS partitions a whole disk and keeps some of it back, so a new disk as big as the
        // old partition would still come out too small.
        if new.size < needed + PARTITION_OVERHEAD {
            return Err(fail(
                ctx.sink,
                format!(
                    "{} ({}) is smaller than the disk it would stand in for ({}). Nothing was erased.",
                    new.by_id,
                    disks::size_label(new.size),
                    disks::size_label(needed)
                ),
            ));
        }
    }
    Ok(())
}

fn finish(result: Result<(), i32>) -> i32 {
    result.err().unwrap_or(0)
}

fn rebuild_reminder<R: Runner>(ctx: &Ctx<'_, R>) {
    ctx.sink.info("");
    ctx.sink.info(
        "Rebuild so NixOS's boot-time import list matches. Until then the change affects this boot only.",
    );
}

async fn show_status<R: Runner>(ctx: &Ctx<'_, R>, pool: &str) -> Result<(), i32> {
    stream(ctx, &Cmd::new("zpool").args(["status", pool]))
        .await
        .map(|_| ())
}

/// Proxmox is "running" when its cluster filesystem is mounted.
async fn proxmox_running<R: Runner>(ctx: &Ctx<'_, R>) -> bool {
    matches!(
        ctx.runner
            .run(&Cmd::new("mountpoint").args(["-q", "/etc/pve"]))
            .await,
        Ok(out) if out.success()
    )
}

// ──────────────────────────────── Operations ────────────────────────────────

pub async fn create<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let _only = ONE_AT_A_TIME.lock().await;
    finish(create_inner(ctx).await)
}

async fn create_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let Some(name) = arg(ctx, "name") else {
        return Err(fail(ctx.sink, "a pool name is required."));
    };
    valid_name(name).map_err(|e| fail(ctx.sink, e))?;
    let Some(layout) = arg(ctx, "topology").and_then(Layout::parse) else {
        return Err(fail(ctx.sink, "choose how the disks are combined."));
    };
    let storage_id = arg(ctx, "storage_id");
    if storage_id.is_some_and(|id| !valid_storage_id(id)) {
        return Err(fail(ctx.sink, "that is not a valid Proxmox storage ID."));
    }
    if imported(ctx).await?.iter().any(|p| p == name) {
        return Err(fail(
            ctx.sink,
            format!("a pool named `{name}` already exists. Choose a different name."),
        ));
    }

    let available = disks::eligible_now(ctx, &[]).await?;
    let chosen =
        disks::pick(&available, arg(ctx, "disks").unwrap_or("")).map_err(|e| fail(ctx.sink, e))?;
    let paths: Vec<String> = chosen.iter().map(|d| d.path()).collect();
    let vdevs = layout.vdev_args(&paths).map_err(|e| fail(ctx.sink, e))?;

    // Pools made now may not import on the next boot without a host id.
    let hostid_empty = std::fs::metadata(ctx.root.join("etc/hostid")).is_ok_and(|m| m.len() == 0)
        || !ctx.root.join("etc/hostid").exists();
    let hostid_set =
        std::fs::read_to_string(ctx.nixos_dir.join("configuration.nix")).is_ok_and(|t| {
            t.lines()
                .any(|l| l.trim_start().starts_with("networking.hostId"))
        });
    if hostid_empty && !hostid_set {
        ctx.sink.error(
            "warning: /etc/hostid is empty and networking.hostId is not set, so this pool may not \
             import on the next boot.",
        );
    }

    ctx.sink.info(&format!("Creating pool {name} on:"));
    for disk in &chosen {
        ctx.sink.info(&format!(
            "  {}  ({} {})",
            disk.by_id,
            disk.size_label(),
            disk.model
        ));
    }
    disks::wipe(ctx, &chosen, &[]).await?;

    let create = Cmd::new("zpool")
        .args([
            "create",
            "-f",
            "-o",
            "ashift=12",
            "-O",
            "compression=lz4",
            "-O",
            "atime=off",
            "-O",
            "xattr=sa",
            "-O",
            "acltype=posixacl",
            "-O",
        ])
        .arg(format!("mountpoint=/{name}"))
        .arg(name)
        .args(vdevs);
    if stream(ctx, &create).await? != 0 {
        if let Ok(log) = ctx
            .runner
            .run(&Cmd::new("journalctl").args(["-k", "--since", "2 minutes ago", "--no-pager"]))
            .await
        {
            ctx.sink.info("── recent kernel log ──");
            let lines: Vec<&str> = log.stdout.lines().collect();
            for line in &lines[lines.len().saturating_sub(50)..] {
                ctx.sink.info(line);
            }
        }
        return Err(fail(ctx.sink, "zpool create failed (see the log above)."));
    }
    ctx.sink.info(&format!("Pool {name} created."));

    let mut problem = false;
    if let Err(why) = register(ctx, name).await {
        problem = true;
        ctx.sink.error(&format!(
            "error: the pool exists but is not saved for import at boot: {why}"
        ));
    }

    let mut target = name.to_string();
    if arg(ctx, "dataset") != Some("no") {
        let child = format!("{name}/vm");
        if stream(ctx, &Cmd::new("zfs").args(["create", &child])).await? == 0 {
            target = child;
        } else {
            ctx.sink
                .error(&format!("warning: could not create the dataset {child}."));
        }
    }

    if let Some(id) = storage_id {
        if proxmox_running(ctx).await {
            let add = Cmd::new("pvesm").args([
                "add",
                "zfspool",
                id,
                "--pool",
                &target,
                "--content",
                "images,rootdir",
                "--sparse",
                "1",
            ]);
            match stream(ctx, &add).await {
                Ok(0) => ctx
                    .sink
                    .info(&format!("Proxmox storage `{id}` added (pool {target}).")),
                Ok(_) | Err(_) => ctx.sink.error(&format!(
                    "warning: could not add Proxmox storage `{id}`. Add it in Datacenter → Storage (pool {target})."
                )),
            }
        } else {
            ctx.sink.error(&format!(
                "warning: Proxmox is not running here. Add the storage later in Datacenter → Storage (pool {target})."
            ));
        }
    }

    stream(ctx, &Cmd::new("zfs").args(["list", "-r", name])).await?;
    rebuild_reminder(ctx);
    if problem {
        return Err(1);
    }
    Ok(())
}

pub async fn destroy<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let _only = ONE_AT_A_TIME.lock().await;
    finish(destroy_inner(ctx).await)
}

async fn destroy_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let pool = existing_pool(ctx).await?;
    let storage = std::fs::read_to_string(ctx.root.join("etc/pve/storage.cfg")).unwrap_or_default();
    let ids = pve_storage_ids(&storage, &pool);

    show_status(ctx, &pool).await?;
    if stream(ctx, &Cmd::new("zpool").args(["destroy", &pool])).await? != 0 {
        return Err(fail(
            ctx.sink,
            format!(
                "zpool destroy failed. If the pool is busy, stop the VMs and services using it \
                 (try `lsof +D /{pool}`) and run this again."
            ),
        ));
    }
    ctx.sink.info(&format!("Pool {pool} destroyed."));

    let mut problem = false;
    if !ids.is_empty() {
        if proxmox_running(ctx).await {
            for id in &ids {
                match stream(ctx, &Cmd::new("pvesm").args(["remove", id])).await {
                    Ok(0) => ctx.sink.info(&format!("Proxmox storage `{id}` removed.")),
                    _ => ctx.sink.error(&format!(
                        "warning: could not remove Proxmox storage `{id}`. Remove it in Datacenter → Storage."
                    )),
                }
            }
        } else {
            ctx.sink.error(&format!(
                "warning: Proxmox is not running here. Remove storage {} later in Datacenter → Storage.",
                ids.join(", ")
            ));
        }
    }
    match unregister(ctx, &pool).await {
        Ok(true) => ctx.sink.info(&format!("{pool} removed from {FILE}.")),
        Ok(false) => ctx.sink.info(&format!("{pool} was not listed in {FILE}.")),
        Err(why) => {
            problem = true;
            ctx.sink.error(&format!("error: {why}"));
        }
    }
    rebuild_reminder(ctx);
    if problem {
        return Err(1);
    }
    Ok(())
}

/// Read-only: what `zpool import` finds on the connected disks.
pub async fn import_list<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    finish(import_list_inner(ctx).await)
}

async fn import_list_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    imported(ctx).await?;
    let out = capture(
        ctx,
        &Cmd::new("zpool").args(["import", "-d", "/dev/disk/by-id"]),
    )
    .await?;
    let text = format!("{}{}", out.stdout, out.stderr);
    for line in text.lines() {
        ctx.sink.info(line);
    }
    let names = parse_importable(&text);
    ctx.sink.info("");
    if names.is_empty() {
        ctx.sink
            .info("No importable pools were found on the connected disks.");
    } else {
        ctx.sink
            .info(&format!("Importable pools: {}", names.join(", ")));
        ctx.sink.info("Type one of these names in Import Pool.");
    }
    Ok(())
}

pub async fn import<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let _only = ONE_AT_A_TIME.lock().await;
    finish(import_inner(ctx).await)
}

async fn import_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let Some(pool) = arg(ctx, "pool") else {
        return Err(fail(ctx.sink, "a pool name is required."));
    };
    valid_name(pool).map_err(|e| fail(ctx.sink, e))?;
    if imported(ctx).await?.iter().any(|p| p == pool) {
        return Err(fail(ctx.sink, format!("`{pool}` is already imported.")));
    }
    let force = arg(ctx, "force") == Some("yes");
    let mut cmd = Cmd::new("zpool").args(["import", "-d", "/dev/disk/by-id"]);
    if force {
        cmd = cmd.arg("-f");
    }
    if stream(ctx, &cmd.arg(pool)).await? != 0 {
        let hint = if force {
            ""
        } else {
            " If the disks are not in use by another machine, run Import again with Force on."
        };
        return Err(fail(ctx.sink, format!("could not import `{pool}`.{hint}")));
    }
    ctx.sink.info(&format!("Pool {pool} imported."));
    if let Err(why) = register(ctx, pool).await {
        return Err(fail(
            ctx.sink,
            format!("the pool is imported but is not saved for import at boot: {why}"),
        ));
    }
    ctx.sink.info(&format!("{pool} registered in {FILE}."));
    rebuild_reminder(ctx);
    Ok(())
}

pub async fn replace<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let _only = ONE_AT_A_TIME.lock().await;
    finish(replace_inner(ctx).await)
}

async fn replace_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let pool = existing_pool(ctx).await?;
    let old = pool_disk(ctx, &pool, "old", "the disk to replace").await?;
    let status = capture(ctx, &Cmd::new("zpool").args(["status", "-P", &pool])).await?;
    if matches!(disk_parent(&status.stdout, &pool, &old), Some(Some(g)) if g.starts_with("cache") || g.starts_with("spares"))
    {
        return Err(fail(
            ctx.sink,
            "a cache disk or an unused spare cannot be replaced; remove it from the pool instead. Nothing was erased.",
        ));
    }
    let new = one_new_disk(ctx).await?;
    too_small(ctx, &new, &old).await?;
    ctx.sink.info(&format!(
        "Replacing {old} with {} (it will be erased).",
        new.by_id
    ));
    disks::wipe(ctx, &[&new], &[]).await?;
    let cmd = Cmd::new("zpool").args(["replace", &pool, &old, &new.path()]);
    if stream(ctx, &cmd).await? != 0 {
        return Err(fail(ctx.sink, "zpool replace failed (see the log above)."));
    }
    ctx.sink
        .info("Replace started: ZFS is copying the data to the new disk (resilvering).");
    show_status(ctx, &pool).await
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The first vdev of a pool and how wide it is, from `zpool status -P`: what `zpool add`
/// compares a new layout against. `None` if it cannot be told (RAID-Z3, dRAID, odd output).
pub fn pool_shape(status: &str) -> Option<(Layout, usize)> {
    let mut in_table = false;
    let mut seen_pool = false;
    let mut first: Option<(Layout, usize)> = None;
    let mut group_indent = 0;
    let mut child_indent: Option<usize> = None;
    for line in status.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() >= 2 && words[0] == "NAME" && words[1] == "STATE" {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        if words.is_empty() {
            break;
        }
        if !seen_pool {
            seen_pool = true;
            continue;
        }
        let word = words[0];
        match first {
            None => {
                first = match word {
                    w if w.starts_with("mirror") => Some((Layout::Mirror, 0)),
                    w if w.starts_with("raidz1") => Some((Layout::Raidz1, 0)),
                    w if w.starts_with("raidz2") => Some((Layout::Raidz2, 0)),
                    w if w.starts_with("raidz") || w.starts_with("draid") => return None,
                    w if w.starts_with('/') || w.chars().all(|c| c.is_ascii_digit()) => {
                        return Some((Layout::Single, 1))
                    }
                    _ => return None,
                };
                group_indent = indent(line);
            }
            Some((layout, ref mut width)) => {
                // The group's own disks are indented under it; the next group or section
                // starts at the same depth.
                if indent(line) <= group_indent {
                    return Some((layout, *width));
                }
                // Only the group's own members count: a `replacing-0` inside a mirror is one
                // member, whatever it holds.
                let child = *child_indent.get_or_insert(indent(line));
                if indent(line) == child {
                    *width += 1;
                }
            }
        }
    }
    first
}

/// Where a disk sits in `zpool status -P`: `Some(None)` for a disk of its own at the top
/// of the pool, `Some(Some(group))` for one inside a group such as `mirror-0` or `raidz1-0`,
/// `None` if the disk is not listed.
pub fn disk_parent(status: &str, pool: &str, device: &str) -> Option<Option<String>> {
    let mut in_table = false;
    let mut stack: Vec<(usize, String)> = Vec::new();
    for line in status.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() >= 2 && words[0] == "NAME" && words[1] == "STATE" {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        if words.is_empty() {
            break;
        }
        let depth = indent(line);
        while stack.last().is_some_and(|(d, _)| *d >= depth) {
            stack.pop();
        }
        if words[0] == device {
            return Some(match stack.last() {
                Some((_, parent)) if parent != pool => Some(parent.clone()),
                _ => None,
            });
        }
        stack.push((depth, words[0].to_string()));
    }
    None
}

/// Whether `zpool add`'s complaint is only about what is already on the new disks. Every
/// line about a device must say it carries a filesystem, label or pool membership; a
/// device that is busy or cannot be opened is not that, and nothing else may appear
/// besides the usual headings.
fn signature_complaint(text: &str) -> bool {
    let mut about_a_device = false;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line.contains("/dev/") {
            let stale = ["contains a ", "contains an ", "is part of "]
                .iter()
                .any(|p| line.contains(p));
            if !stale || line.contains("busy") {
                return false;
            }
            about_a_device = true;
        } else if !(line.contains("invalid vdev specification")
            || line.contains("use '-f'")
            || line.contains("override the following errors"))
        {
            return false;
        }
    }
    about_a_device
}

pub async fn add<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let _only = ONE_AT_A_TIME.lock().await;
    finish(add_inner(ctx).await)
}

async fn add_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let pool = existing_pool(ctx).await?;
    let layout = arg(ctx, "layout").and_then(Layout::parse).filter(|l| {
        matches!(
            l,
            Layout::Single | Layout::Mirror | Layout::Raidz1 | Layout::Raidz2
        )
    });
    let Some(layout) = layout else {
        return Err(fail(
            ctx.sink,
            "choose mirror, RAID-Z1, RAID-Z2 or a single disk.",
        ));
    };
    let available = disks::eligible_now(ctx, &[]).await?;
    let chosen =
        disks::pick(&available, arg(ctx, "disks").unwrap_or("")).map_err(|e| fail(ctx.sink, e))?;
    if layout == Layout::Single && chosen.len() != 1 {
        return Err(fail(ctx.sink, "a single disk is added one at a time."));
    }
    let paths: Vec<String> = chosen.iter().map(|d| d.path()).collect();
    let vdevs = layout.vdev_args(&paths).map_err(|e| fail(ctx.sink, e))?;

    // The preview runs without `-f`, exactly as the real command will, and before
    // anything is erased. Leftover labels on the new disks would also stop it, but they
    // are about to be wiped; a layout that does not match the pool would stop the real
    // command after the wipe, so that one is refused here.
    let preview = Cmd::new("zpool")
        .args(["add", "-n", &pool])
        .args(vdevs.clone());
    ctx.sink.info(&format!("$ {}", preview.display()));
    let out = capture(ctx, &preview).await?;
    let said = format!("{}{}", out.stdout, out.stderr);
    for line in said.lines() {
        ctx.sink.info(line);
    }
    // A failed preview authorises nothing unless it failed only because the new disks still
    // carry old signatures, which the wipe removes. Anything else, and any layout that
    // differs in style from the pool, stops here, before anything is erased.
    let mismatch = !out.success() && said.contains("mismatched replication level");
    let status = capture(ctx, &Cmd::new("zpool").args(["status", "-P", &pool])).await?;
    let shape = pool_shape(&status.stdout);
    let wrong_style = shape.is_some_and(|(style, _)| style != layout);
    // ZFS also compares how wide two groups are, but only after it has looked at old
    // signatures, so a preview stopped by those cannot be trusted on that point.
    let wrong_width = layout != Layout::Single && shape.is_some_and(|(_, w)| w != chosen.len());
    if mismatch || wrong_style || (!out.success() && wrong_width) {
        return Err(fail(
            ctx.sink,
            "this layout does not match the pool's existing disks, so ZFS would refuse it. \
             Nothing was changed.",
        ));
    }
    if !out.success() {
        if !signature_complaint(&said) {
            return Err(fail(
                ctx.sink,
                "ZFS rejected this layout (see above). Nothing was erased.",
            ));
        }
        // It stopped at the old signatures, so the layout was never compared with the
        // pool. Only a layout that can be compared here goes ahead.
        if shape.is_none() {
            return Err(fail(
                ctx.sink,
                "ZFS stopped at old signatures on the new disks, and this pool's layout cannot be \
                 checked here, so nothing was erased. Wipe the disks yourself and try again.",
            ));
        }
    }

    disks::wipe(ctx, &chosen, &[]).await?;
    let real = Cmd::new("zpool").args(["add", &pool]).args(vdevs);
    if stream(ctx, &real).await? != 0 {
        return Err(fail(
            ctx.sink,
            "zpool add failed, usually because the layout does not match the pool's existing disks.",
        ));
    }
    ctx.sink.info(&format!("Disks added to {pool}."));
    stream(ctx, &Cmd::new("zpool").args(["list", &pool]))
        .await
        .map(|_| ())
}

pub async fn attach<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let _only = ONE_AT_A_TIME.lock().await;
    finish(attach_inner(ctx).await)
}

async fn attach_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let pool = existing_pool(ctx).await?;
    let existing = pool_disk(ctx, &pool, "existing", "the disk to copy").await?;
    // Only a disk on its own, or one in a mirror, can be copied. ZFS rejects the rest after
    // the new disk has been erased, so it is refused first.
    let status = capture(ctx, &Cmd::new("zpool").args(["status", "-P", &pool])).await?;
    match disk_parent(&status.stdout, &pool, &existing) {
        Some(None) => {}
        // `logs`, `special` and `dedup` only name a class: a disk under one is a vdev of the
        // pool that ZFS can mirror.
        Some(Some(group))
            if ["mirror", "logs", "special", "dedup"]
                .iter()
                .any(|p| group.starts_with(p)) => {}
        _ => {
            return Err(fail(
                ctx.sink,
                "a mirror copy can only be attached to a single disk or a disk in a mirror, \
                 not to this one. Nothing was erased.",
            ))
        }
    }
    let new = one_new_disk(ctx).await?;
    too_small(ctx, &new, &existing).await?;
    ctx.sink.info(&format!(
        "Attaching {} as a copy of {existing} (it will be erased).",
        new.by_id
    ));
    disks::wipe(ctx, &[&new], &[]).await?;
    let cmd = Cmd::new("zpool").args(["attach", &pool, &existing, &new.path()]);
    if stream(ctx, &cmd).await? != 0 {
        return Err(fail(ctx.sink, "zpool attach failed (see the log above)."));
    }
    ctx.sink
        .info("Attach started: ZFS is copying the data to the new disk (resilvering).");
    show_status(ctx, &pool).await
}

pub async fn detach<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let _only = ONE_AT_A_TIME.lock().await;
    finish(detach_inner(ctx).await)
}

async fn detach_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let pool = existing_pool(ctx).await?;
    let disk = pool_disk(ctx, &pool, "disk", "the disk to detach").await?;
    if stream(ctx, &Cmd::new("zpool").args(["detach", &pool, &disk])).await? != 0 {
        return Err(fail(
            ctx.sink,
            "zpool detach failed. Only a disk in a mirror can be detached.",
        ));
    }
    ctx.sink.info(&format!("{disk} detached from {pool}."));
    show_status(ctx, &pool).await
}

pub async fn scrub<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    finish(scrub_inner(ctx).await)
}

async fn scrub_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let pool = existing_pool(ctx).await?;
    let stop = arg(ctx, "action") == Some("stop");
    let mut cmd = Cmd::new("zpool").arg("scrub");
    if stop {
        cmd = cmd.arg("-s");
    }
    if stream(ctx, &cmd.arg(&pool)).await? != 0 {
        return Err(fail(
            ctx.sink,
            if stop {
                "could not stop the scrub. Is one running?"
            } else {
                "could not start the scrub."
            },
        ));
    }
    ctx.sink.info(&format!(
        "Scrub {} on {pool}.",
        if stop { "stopped" } else { "started" }
    ));
    show_status(ctx, &pool).await
}

pub async fn clear<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    finish(clear_inner(ctx).await)
}

async fn clear_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let pool = existing_pool(ctx).await?;
    if stream(ctx, &Cmd::new("zpool").args(["clear", &pool])).await? != 0 {
        return Err(fail(ctx.sink, "zpool clear failed."));
    }
    ctx.sink.info(&format!("Error counters cleared on {pool}."));
    show_status(ctx, &pool).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::testing::{block_on, FakeRunner};
    use crate::runner::Output;
    use crate::testkit::Kit;

    const STATUS: &str = "  pool: tank\n state: ONLINE\nconfig:\n\n\
        \tNAME                                  STATE     READ WRITE CKSUM\n\
        \ttank                                  ONLINE       0     0     0\n\
        \t  mirror-0                            ONLINE       0     0     0\n\
        \t    /dev/disk/by-id/ata-A-part1       ONLINE       0     0     0\n\
        \t    /dev/disk/by-id/ata-B-part1       ONLINE       0     0     0\n\
        \tlogs\n\
        \t  /dev/disk/by-id/nvme-L-part1        ONLINE       0     0     0\n\
        \t  replacing-1                         DEGRADED     0     0     0\n\
        \t    1234567890123                     UNAVAIL      0     0     0  was /dev/x\n\
        \nerrors: No known data errors\n";

    #[test]
    fn pool_names_follow_zpools_rules() {
        for ok in ["tank", "Tank-1", "a.b_c:d", "data2"] {
            assert!(valid_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "1tank", "-t", "mirror", "RAIDZ2", "log", "draid2", "c0", "c1disk", "a b", "a/b",
            "tank;x", "spare", "special",
        ] {
            assert!(valid_name(bad).is_err(), "{bad}");
        }
        assert!(valid_name("cache1").is_ok());
        assert!(valid_name("cache").is_err());
        assert!(
            valid_storage_id("vm-store") && !valid_storage_id("-x") && !valid_storage_id("a b")
        );
    }

    #[test]
    fn the_pool_table_is_read_from_scripted_output() {
        let rows =
            parse_pool_list("tank\t3.62T\t1.2T\t2.4T\tONLINE\nbulk\t10T\t9T\t1T\tDEGRADED\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].name, "bulk");
        assert_eq!(rows[1].health, "DEGRADED");
        assert!(parse_pool_list("").is_empty());
    }

    #[test]
    fn a_pools_disks_skip_the_pool_and_the_grouping_lines() {
        let found = leaves(STATUS, "tank");
        let devices: Vec<&str> = found.iter().map(|l| l.device.as_str()).collect();
        assert_eq!(
            devices,
            [
                "/dev/disk/by-id/ata-A-part1",
                "/dev/disk/by-id/ata-B-part1",
                "/dev/disk/by-id/nvme-L-part1",
                "1234567890123"
            ]
        );
        assert_eq!(found[3].state, "UNAVAIL");
        assert!(leaves("no config here", "tank").is_empty());
    }

    #[test]
    fn importable_pools_and_proxmox_entries_are_found() {
        let text = "   pool: tank\n     id: 1\n  state: ONLINE\n\n   pool: old-pool\n     id: 2\n";
        assert_eq!(parse_importable(text), ["tank", "old-pool"]);
        let cfg = "dir: local\n\tpath /var/lib/vz\n\nzfspool: vm-store\n\tpool tank/vm\n\tcontent images\n\n\
                   zfspool: other\n\tpool tankish\n\nzfspool: whole\n\tpool tank\n";
        assert_eq!(pve_storage_ids(cfg, "tank"), ["vm-store", "whole"]);
        assert!(pve_storage_ids(cfg, "nope").is_empty());
    }

    #[test]
    fn layouts_build_the_vdev_arguments() {
        let d: Vec<String> = ["/a", "/b", "/c", "/d"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            Layout::Single.vdev_args(&d).unwrap(),
            ["/a", "/b", "/c", "/d"]
        );
        assert_eq!(
            Layout::Mirror.vdev_args(&d[..2]).unwrap(),
            ["mirror", "/a", "/b"]
        );
        assert_eq!(Layout::Raidz2.vdev_args(&d).unwrap()[0], "raidz2");
        assert_eq!(
            Layout::Raid10.vdev_args(&d).unwrap(),
            ["mirror", "/a", "/b", "mirror", "/c", "/d"]
        );
        assert!(Layout::Raid10.vdev_args(&d[..3]).is_err());
        assert!(Layout::Raidz1.vdev_args(&d[..2]).is_err());
        assert!(Layout::Mirror.vdev_args(&d[..1]).is_err());
        assert_eq!(Layout::parse("raidz3"), Some(Layout::Raidz3));
        assert_eq!(Layout::parse("stripe"), None);
    }

    #[test]
    fn reserved_prefixes_are_refused_not_only_the_exact_words() {
        for bad in [
            "mirror-backup",
            "raidz-data",
            "spare1",
            "draid3x",
            "Mirror2",
        ] {
            assert!(valid_name(bad).is_err(), "{bad}");
        }
        assert!(valid_name("mirrors").is_err());
        assert!(valid_name("tank").is_ok() && valid_name("logs").is_ok());
    }

    #[test]
    fn every_name_on_a_registry_line_is_read_and_odd_content_is_not_rewritten() {
        let text = template("t").replace(
            "    # <<< vexos-zfs-pools <<<",
            "    \"tank\" \"bulk\"
    # <<< vexos-zfs-pools <<<",
        );
        assert_eq!(registered(&text), ["tank", "bulk"]);
        assert!(block_is_plain(&text));
        let odd = text.replace("\"bulk\"", "\"bulk\" ++ extra");
        assert!(!block_is_plain(&odd));
        assert!(block_is_plain(&template("t")));
    }

    #[test]
    fn a_pools_first_vdev_shape_is_read_from_status() {
        assert_eq!(pool_shape(STATUS), Some((Layout::Mirror, 2)));
        let single = "\tNAME STATE\n\ttank ONLINE\n\t  /dev/disk/by-id/ata-A ONLINE\n\n";
        assert_eq!(pool_shape(single), Some((Layout::Single, 1)));
        let z2 = "\tNAME STATE\n\ttank ONLINE\n\t  raidz2-0 ONLINE\n\t    /dev/sda ONLINE\n\t    /dev/sdb ONLINE\n\t    /dev/sdc ONLINE\n\t    /dev/sdd ONLINE\n\n";
        assert_eq!(pool_shape(z2), Some((Layout::Raidz2, 4)));
        let z3 = z2.replace("raidz2", "raidz3");
        assert_eq!(pool_shape(&z3), None);
        assert_eq!(pool_shape("nothing"), None);
    }

    #[test]
    fn a_disks_place_in_the_pool_is_read_from_indentation() {
        let dev = "/dev/disk/by-id/ata-A-part1";
        assert_eq!(
            disk_parent(STATUS, "tank", dev),
            Some(Some("mirror-0".to_string()))
        );
        assert_eq!(
            disk_parent(STATUS, "tank", "/dev/disk/by-id/nvme-L-part1"),
            Some(Some("logs".to_string()))
        );
        let single = "\tNAME STATE\n\ttank ONLINE\n\t  /dev/disk/by-id/ata-A ONLINE\n\n";
        assert_eq!(
            disk_parent(single, "tank", "/dev/disk/by-id/ata-A"),
            Some(None)
        );
        assert_eq!(disk_parent(STATUS, "tank", "/dev/nope"), None);
    }

    #[test]
    fn only_stale_signature_complaints_count_as_such() {
        assert!(signature_complaint(
            "invalid vdev specification\nuse '-f' to override the following errors:\n/dev/sdb contains a filesystem of type 'ext4'"
        ));
        assert!(signature_complaint(
            "/dev/sdb is part of potentially active pool 'old'"
        ));
        assert!(!signature_complaint(
            "/dev/sdb is in use and contains a ext4 filesystem busy"
        ));
        assert!(!signature_complaint(
            "cannot open /dev/sdb: Device or resource busy"
        ));
        assert!(!signature_complaint(
            "/dev/sdb contains a filesystem\n/dev/sdc is busy"
        ));
        assert!(!signature_complaint("permission denied"));
        assert!(!signature_complaint(""));
    }

    #[test]
    fn the_registry_keeps_everything_outside_the_markers() {
        let text = template("2026-11-19 12:00:00");
        assert!(registered(&text).is_empty());
        let one = with_registered(&text, &["tank".to_string()]).unwrap();
        assert_eq!(registered(&one), ["tank"]);
        assert!(one.contains("{ ... }:"));
        let two = with_registered(&one, &["tank".to_string(), "bulk".to_string()]).unwrap();
        assert_eq!(registered(&two), ["tank", "bulk"]);
        assert_eq!(with_registered(&two, &[]).unwrap(), text);
        assert!(with_registered("{ ... }: { }", &["x".to_string()]).is_err());
    }

    // ── a scripted server ───────────────────────────────────────────────────────

    const LSBLK: &str = r#"{"blockdevices":[
      {"name":"nvme0n1","type":"disk","size":500107862016,"model":"WD","tran":"nvme","mountpoints":[null],
       "children":[{"name":"nvme0n1p1","type":"part","size":1,"mountpoints":["/"]}]},
      {"name":"sda","type":"disk","size":4000787030016,"model":"ST4","tran":"sata","mountpoints":[null]},
      {"name":"sdb","type":"disk","size":4000787030016,"model":"ST4","tran":"sata","mountpoints":[null]},
      {"name":"sdc","type":"disk","size":4000787030016,"model":"ST4","tran":"sata","mountpoints":[null]}]}"#;

    fn out(stdout: &str) -> Output {
        Output {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// A server with three blank disks and the system disk, and no pools.
    fn server() -> Kit {
        let kit = Kit::new();
        kit.nixos_file("vexos-variant", "vexos-server-amd");
        let by_id = kit.root.join("dev/disk/by-id");
        std::fs::create_dir_all(&by_id).unwrap();
        for (name, id) in [
            ("sda", "ata-A"),
            ("sdb", "ata-B"),
            ("sdc", "ata-C"),
            ("nvme0n1", "nvme-S"),
        ] {
            std::fs::write(kit.root.join(&format!("dev/{name}")), "").unwrap();
            std::os::unix::fs::symlink(kit.root.join(&format!("dev/{name}")), by_id.join(id))
                .unwrap();
        }
        kit
    }

    fn runner() -> FakeRunner {
        FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out(""))
            .ok("wipefs", &[])
            .ok("sgdisk", &[])
            .ok("nix-instantiate", &[])
            .ok("zpool", &["create"])
            .ok("zpool", &["status"])
            .ok("zfs", &[])
            .failing("mountpoint", &[], 1, "")
    }

    fn pos(lines: &[String], needle: &str) -> usize {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no `{needle}` in {lines:#?}"))
    }

    #[test]
    fn creating_a_pool_wipes_then_creates_then_registers_it() {
        let kit = server().with_args(&[
            ("name", "tank"),
            ("topology", "mirror"),
            ("disks", "ata-A,ata-B"),
            ("confirm", "tank"),
        ]);
        let r = runner();
        assert_eq!(block_on(create(&kit.ctx(&r))), 0, "{}", kit.sink.text());
        let lines = r.lines();
        let wipe = pos(&lines, "wipefs -a /dev/sdb");
        let zap = pos(&lines, "sgdisk --zap-all /dev/sdb");
        let create_at = pos(&lines, "zpool create");
        assert!(wipe < zap && zap < create_at);
        assert_eq!(
            lines[create_at],
            "zpool create -f -o ashift=12 -O compression=lz4 -O atime=off -O xattr=sa \
             -O acltype=posixacl -O mountpoint=/tank tank mirror /dev/disk/by-id/ata-A /dev/disk/by-id/ata-B"
        );
        assert!(lines.iter().any(|l| l == "zfs create tank/vm"));
        let file = std::fs::read_to_string(kit.nixos.join(FILE)).unwrap();
        assert_eq!(registered(&file), ["tank"]);
        // The disk that was not chosen is untouched.
        assert!(!lines.iter().any(|l| l.contains("ata-C")));
    }

    #[test]
    fn a_disk_that_is_not_eligible_is_refused_before_anything_is_erased() {
        let kit = server().with_args(&[
            ("name", "tank"),
            ("topology", "single"),
            ("disks", "nvme-S"),
            ("confirm", "tank"),
        ]);
        let r = runner();
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("not a disk that can be erased"));
        assert!(!r
            .lines()
            .iter()
            .any(|l| l.starts_with("wipefs") || l.starts_with("zpool create")));
    }

    #[test]
    fn a_disk_in_an_imported_pool_is_refused() {
        let kit = server().with_args(&[
            ("name", "newpool"),
            ("topology", "single"),
            ("disks", "ata-A"),
            ("confirm", "newpool"),
        ]);
        let status = "\tNAME STATE\n\tbulk ONLINE\n\t  /dev/disk/by-id/ata-A ONLINE\n\n";
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out("bulk\t1T\t1G\t1T\tONLINE\n"))
            .on("zpool", &["status"], out(status));
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
    }

    #[test]
    fn a_wipe_that_fails_stops_the_job_before_the_pool_is_built() {
        let kit = server().with_args(&[
            ("name", "tank"),
            ("topology", "single"),
            ("disks", "ata-A"),
            ("confirm", "tank"),
        ]);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out(""))
            .failing("wipefs", &[], 1, "device busy");
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("device busy"));
        assert!(!r.lines().iter().any(|l| l.starts_with("zpool create")));
        assert!(!kit.nixos.join(FILE).exists());
    }

    #[test]
    fn an_existing_name_a_bad_name_and_a_wrong_role_are_refused() {
        let r = FakeRunner::new().on("lsblk", &[], out(LSBLK)).on(
            "zpool",
            &["list"],
            out("tank\t1T\t1G\t1T\tONLINE\n"),
        );
        for (name, why) in [("tank", "already exists"), ("mirror", "reserves")] {
            let kit =
                server().with_args(&[("name", name), ("topology", "single"), ("disks", "ata-A")]);
            assert_eq!(block_on(create(&kit.ctx(&r))), 1);
            assert!(kit.sink.text().contains(why), "{name}: {}", kit.sink.text());
        }
        let kit =
            Kit::new().with_args(&[("name", "x"), ("topology", "single"), ("disks", "ata-A")]);
        kit.nixos_file("vexos-variant", "vexos-desktop-amd");
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("server"));
    }

    #[test]
    fn proxmox_storage_is_added_only_when_proxmox_runs() {
        let args = [
            ("name", "tank"),
            ("topology", "single"),
            ("disks", "ata-A"),
            ("storage_id", "vm-store"),
        ];
        let kit = server().with_args(&args);
        let r = runner();
        block_on(create(&kit.ctx(&r)));
        assert!(!r.lines().iter().any(|l| l.starts_with("pvesm")));
        assert!(kit.sink.text().contains("Proxmox is not running"));

        let kit = server().with_args(&args);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out(""))
            .ok("mountpoint", &[])
            .ok("wipefs", &[])
            .ok("sgdisk", &[])
            .ok("nix-instantiate", &[])
            .ok("zpool", &["create"])
            .ok("zfs", &[])
            .ok("pvesm", &[]);
        assert_eq!(block_on(create(&kit.ctx(&r))), 0, "{}", kit.sink.text());
        assert!(r.lines().iter().any(|l| l
            == "pvesm add zfspool vm-store --pool tank/vm --content images,rootdir --sparse 1"));
    }

    fn with_pool(kit: &Kit) -> FakeRunner {
        let _ = kit;
        FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out("tank\t3T\t1T\t2T\tONLINE\n"))
            .on("zpool", &["status"], out(STATUS))
            .ok("wipefs", &[])
            .ok("sgdisk", &[])
            .ok("nix-instantiate", &[])
            .ok("zpool", &["add"])
            .ok("zpool", &["replace"])
            .ok("zpool", &["attach"])
            .ok("zpool", &["detach"])
            .ok("zpool", &["destroy"])
            .ok("zpool", &["scrub"])
            .ok("zpool", &["clear"])
            .failing("mountpoint", &[], 1, "")
    }

    #[test]
    fn destroying_a_pool_unregisters_it_and_removes_its_proxmox_entry_when_it_can() {
        let kit = server().with_args(&[("pool", "tank"), ("confirm", "tank")]);
        let file =
            with_registered(&template("t"), &["tank".to_string(), "bulk".to_string()]).unwrap();
        kit.nixos_file(FILE, &file);
        std::fs::create_dir_all(kit.root.join("etc/pve")).unwrap();
        std::fs::write(
            kit.root.join("etc/pve/storage.cfg"),
            "zfspool: vm-store\n\tpool tank/vm\n",
        )
        .unwrap();
        let r = with_pool(&kit);
        assert_eq!(block_on(destroy(&kit.ctx(&r))), 0, "{}", kit.sink.text());
        assert!(r.lines().iter().any(|l| l == "zpool destroy tank"));
        assert!(kit.sink.text().contains("Proxmox is not running"));
        let after = std::fs::read_to_string(kit.nixos.join(FILE)).unwrap();
        assert_eq!(registered(&after), ["bulk"]);
    }

    #[test]
    fn destroying_a_pool_that_is_not_imported_does_nothing() {
        let kit = server().with_args(&[("pool", "ghost"), ("confirm", "ghost")]);
        let r = with_pool(&kit);
        assert_eq!(block_on(destroy(&kit.ctx(&r))), 1);
        assert!(!r.lines().iter().any(|l| l.contains("destroy")));
    }

    #[test]
    fn replace_and_attach_only_accept_a_disk_the_pool_really_has() {
        for op in ["replace", "attach"] {
            let (key, word) = if op == "replace" {
                ("old", "replace")
            } else {
                ("existing", "attach")
            };
            let kit = server().with_args(&[
                ("pool", "tank"),
                (key, "/dev/disk/by-id/ata-ELSEWHERE-part1"),
                ("disks", "ata-C"),
            ]);
            let r = with_pool(&kit);
            let code = if op == "replace" {
                block_on(replace(&kit.ctx(&r)))
            } else {
                block_on(attach(&kit.ctx(&r)))
            };
            assert_eq!(code, 1, "{op}");
            assert!(kit.sink.text().contains("is not a disk of pool"), "{op}");
            assert!(
                !r.lines()
                    .iter()
                    .any(|l| l.starts_with("wipefs") || l.contains(word)),
                "{op}"
            );
        }
    }

    #[test]
    fn replacing_a_disk_wipes_only_the_new_one_then_replaces() {
        let kit = server().with_args(&[
            ("pool", "tank"),
            ("old", "/dev/disk/by-id/ata-B-part1"),
            ("disks", "ata-C"),
        ]);
        let r = with_pool(&kit);
        assert_eq!(block_on(replace(&kit.ctx(&r))), 0, "{}", kit.sink.text());
        let lines = r.lines();
        assert!(pos(&lines, "wipefs -a /dev/sdc") < pos(&lines, "zpool replace"));
        assert_eq!(
            lines[pos(&lines, "zpool replace")],
            "zpool replace tank /dev/disk/by-id/ata-B-part1 /dev/disk/by-id/ata-C"
        );
        // A missing disk is named by its number.
        let kit = server().with_args(&[
            ("pool", "tank"),
            ("old", "1234567890123"),
            ("disks", "ata-C"),
        ]);
        let r = with_pool(&kit);
        assert_eq!(block_on(replace(&kit.ctx(&r))), 0, "{}", kit.sink.text());
    }

    #[test]
    fn adding_disks_is_refused_before_the_wipe_when_the_layout_does_not_match() {
        let kit = server().with_args(&[("pool", "tank"), ("layout", "single"), ("disks", "ata-C")]);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out("tank\t3T\t1T\t2T\tONLINE\n"))
            .on("zpool", &["status"], out(STATUS))
            .failing(
                "zpool",
                &["add", "-n"],
                1,
                "invalid vdev specification\nmismatched replication level: pool uses mirror and new vdev is disk",
            );
        assert_eq!(block_on(add(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("does not match"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
    }

    #[test]
    fn adding_disks_previews_with_the_flags_it_runs_then_wipes_then_adds() {
        let kit = server().with_args(&[
            ("pool", "tank"),
            ("layout", "mirror"),
            ("disks", "ata-B,ata-C"),
        ]);
        let r = with_pool(&kit);
        assert_eq!(block_on(add(&kit.ctx(&r))), 0, "{}", kit.sink.text());
        let lines = r.lines();
        let preview = pos(&lines, "zpool add -n tank mirror");
        let wipe = pos(&lines, "wipefs");
        let real = lines
            .iter()
            .position(|l| l.starts_with("zpool add tank"))
            .unwrap();
        assert!(preview < wipe && wipe < real);
        assert_eq!(
            lines[real].trim_start_matches("zpool add tank "),
            lines[preview].trim_start_matches("zpool add -n tank "),
            "preview and real command differ"
        );
        assert!(!lines[real].contains(" -f"));
    }

    #[test]
    fn a_preview_that_failed_for_another_reason_does_not_authorise_the_wipe() {
        let kit = server().with_args(&[
            ("pool", "tank"),
            ("layout", "mirror"),
            ("disks", "ata-B,ata-C"),
        ]);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on(
                "zpool",
                &["list"],
                out("tank	3T	1T	2T	ONLINE
"),
            )
            .on("zpool", &["status"], out(STATUS))
            .failing("zpool", &["add", "-n"], 1, "cannot open: permission denied");
        assert_eq!(block_on(add(&kit.ctx(&r))), 1);
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));

        // Old labels on the new disks are what the wipe is for, so that one goes ahead.
        let kit = server().with_args(&[
            ("pool", "tank"),
            ("layout", "mirror"),
            ("disks", "ata-B,ata-C"),
        ]);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on(
                "zpool",
                &["list"],
                out("tank	3T	1T	2T	ONLINE
"),
            )
            .on("zpool", &["status"], out(STATUS))
            .failing(
                "zpool",
                &["add", "-n"],
                1,
                "/dev/disk/by-id/ata-B contains a filesystem of type ext4",
            )
            .ok("wipefs", &[])
            .ok("sgdisk", &[])
            .ok("zpool", &["add"])
            .ok("zpool", &["list"]);
        assert_eq!(block_on(add(&kit.ctx(&r))), 0, "{}", kit.sink.text());
        assert!(r.lines().iter().any(|l| l.starts_with("wipefs")));
    }

    #[test]
    fn a_style_that_differs_from_the_pool_is_refused_even_when_the_preview_is_silent() {
        let kit = server().with_args(&[
            ("pool", "tank"),
            ("layout", "raidz1"),
            ("disks", "ata-A,ata-B,ata-C"),
        ]);
        let r = with_pool(&kit);
        assert_eq!(block_on(add(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("does not match"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
    }

    #[test]
    fn a_pool_whose_disks_cannot_be_read_hides_nothing() {
        let kit = server().with_args(&[
            ("name", "tank"),
            ("topology", "single"),
            ("disks", "ata-A"),
            ("confirm", "tank"),
        ]);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on(
                "zpool",
                &["list"],
                out("bulk	1T	1G	1T	ONLINE
"),
            )
            .failing("zpool", &["status"], 1, "cannot open pool");
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("could not read the disks"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
    }

    #[test]
    fn a_disk_that_is_no_longer_free_is_not_wiped() {
        let kit = server();
        let mounted = LSBLK.replace(
            r#"{"name":"sda","type":"disk","size":4000787030016,"model":"ST4","tran":"sata","mountpoints":[null]}"#,
            r#"{"name":"sda","type":"disk","size":4000787030016,"model":"ST4","tran":"sata","mountpoints":["/data"]}"#,
        );
        assert_ne!(mounted, LSBLK);
        let disk = Disk {
            by_id: "ata-A".into(),
            kernel: "sda".into(),
            size: 1,
            model: String::new(),
            transport: String::new(),
            note: None,
        };
        let r = FakeRunner::new()
            .on("lsblk", &[], out(&mounted))
            .on("zpool", &["list"], out(""))
            .ok("wipefs", &[])
            .ok("sgdisk", &[]);
        assert_eq!(block_on(disks::wipe(&kit.ctx(&r), &[&disk], &[])), Err(1));
        assert!(kit
            .sink
            .text()
            .contains("no longer a disk that can be erased"));
        assert!(!r
            .lines()
            .iter()
            .any(|l| l.starts_with("wipefs") || l.starts_with("sgdisk")));
    }

    #[test]
    fn the_lsblk_columns_keep_each_disks_partitions_nested_under_it() {
        let columns = disks::LSBLK_ARGS[3];
        assert!(columns.starts_with("NAME,") && columns.contains("KNAME"));
    }

    #[test]
    fn a_disk_the_configuration_mounts_is_not_offered_even_while_unmounted() {
        let kit = server().with_args(&[
            ("name", "tank"),
            ("topology", "single"),
            ("disks", "ata-A"),
            ("confirm", "tank"),
        ]);
        kit.nixos_file(
            "hardware-configuration.nix",
            "{ fileSystems.\"/data\".device = \"/dev/disk/by-uuid/Claimed-1\"; }\n",
        );
        let claimed = LSBLK.replace(
            r#"{"name":"sda","type":"disk","size":4000787030016,"model":"ST4","tran":"sata","mountpoints":[null]}"#,
            r#"{"name":"sda","type":"disk","size":4000787030016,"model":"ST4","tran":"sata","uuid":"claimed-1","mountpoints":[null]}"#,
        );
        assert_ne!(claimed, LSBLK);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(&claimed))
            .on("zpool", &["list"], out(""))
            .ok("wipefs", &[]);
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("not a disk that can be erased"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
    }

    #[test]
    fn a_preview_stopped_by_old_signatures_still_cannot_hide_a_mirror_of_another_width() {
        // The pool's mirror is 2 wide; three disks with old filesystems are offered.
        let kit = server().with_args(&[
            ("pool", "tank"),
            ("layout", "mirror"),
            ("disks", "ata-A,ata-B,ata-C"),
        ]);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out("tank\t3T\t1T\t2T\tONLINE\n"))
            .on("zpool", &["status"], out(STATUS))
            .failing(
                "zpool",
                &["add", "-n"],
                1,
                "/dev/disk/by-id/ata-A contains a filesystem of type 'ext4'",
            )
            .ok("wipefs", &[]);
        assert_eq!(block_on(add(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("does not match"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));

        // A pool whose layout cannot be compared is not erased on a stopped preview.
        let z3 = "\tNAME STATE\n\ttank ONLINE\n\t  raidz3-0 ONLINE\n\t    /dev/disk/by-id/gone-1 ONLINE\n\n";
        let kit = server().with_args(&[
            ("pool", "tank"),
            ("layout", "raidz1"),
            ("disks", "ata-A,ata-B,ata-C"),
        ]);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out("tank\t3T\t1T\t2T\tONLINE\n"))
            .on("zpool", &["status"], out(z3))
            .failing(
                "zpool",
                &["add", "-n"],
                1,
                "/dev/disk/by-id/ata-A contains a filesystem of type 'ext4'",
            )
            .ok("wipefs", &[]);
        assert_eq!(block_on(add(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("cannot be checked"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
    }

    #[test]
    fn a_mirror_copy_is_not_attached_to_a_raidz_member_and_nothing_is_erased() {
        let z1 = "\tNAME STATE\n\ttank ONLINE\n\t  raidz1-0 ONLINE\n\t    /dev/disk/by-id/ata-A-part1 ONLINE\n\t    /dev/disk/by-id/ata-B-part1 ONLINE\n\t    /dev/disk/by-id/ata-C-part1 ONLINE\n\n";
        let kit = server().with_args(&[
            ("pool", "tank"),
            ("existing", "/dev/disk/by-id/ata-A-part1"),
            ("disks", "ata-C"),
        ]);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out("tank\t3T\t1T\t2T\tONLINE\n"))
            .on("zpool", &["status"], out(z1))
            .ok("wipefs", &[]);
        assert_eq!(block_on(attach(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("only be attached"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
    }

    #[test]
    fn a_new_disk_smaller_than_the_one_it_replaces_is_refused_before_the_wipe() {
        // sdb1 is a 5 TB partition of the pool's disk; the spare is 4 TB.
        let big = LSBLK.replace(
            r#"{"name":"sdb","type":"disk","size":4000787030016,"model":"ST4","tran":"sata","mountpoints":[null]}"#,
            r#"{"name":"sdb","type":"disk","size":5000787030016,"model":"ST5","tran":"sata","mountpoints":[null],"children":[{"name":"sdb1","type":"part","size":5000000000000,"mountpoints":[null]}]}"#,
        );
        assert_ne!(big, LSBLK);
        for op in ["replace", "attach"] {
            let key = if op == "replace" { "old" } else { "existing" };
            let kit = server().with_args(&[
                ("pool", "tank"),
                (key, "/dev/disk/by-id/ata-B-part1"),
                ("disks", "ata-C"),
            ]);
            std::fs::write(kit.root.join("dev/sdb1"), "").unwrap();
            std::os::unix::fs::symlink(
                kit.root.join("dev/sdb1"),
                kit.root.join("dev/disk/by-id/ata-B-part1"),
            )
            .unwrap();
            let r = FakeRunner::new()
                .on("lsblk", &[], out(&big))
                .on("zpool", &["list"], out("tank\t3T\t1T\t2T\tONLINE\n"))
                .on("zpool", &["status"], out(STATUS))
                .ok("wipefs", &[]);
            let code = if op == "replace" {
                block_on(replace(&kit.ctx(&r)))
            } else {
                block_on(attach(&kit.ctx(&r)))
            };
            assert_eq!(code, 1, "{op}");
            assert!(
                kit.sink.text().contains("is smaller than"),
                "{op}: {}",
                kit.sink.text()
            );
            assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")), "{op}");
        }
    }

    #[test]
    fn a_replacement_inside_a_mirror_counts_as_one_member() {
        let status = "\tNAME STATE\n\ttank ONLINE\n\t  mirror-0 ONLINE\n\t    replacing-0 DEGRADED\n\t      /dev/disk/by-id/ata-A ONLINE\n\t      /dev/disk/by-id/ata-N ONLINE\n\t    /dev/disk/by-id/ata-B ONLINE\n\tlogs\n\t  /dev/disk/by-id/ata-L ONLINE\n\n";
        assert_eq!(pool_shape(status), Some((Layout::Mirror, 2)));
    }

    #[test]
    fn a_stopped_preview_also_cannot_hide_a_raidz_of_another_width() {
        let z1 = "\tNAME STATE\n\ttank ONLINE\n\t  raidz1-0 ONLINE\n\t    /dev/disk/by-id/g-1 ONLINE\n\t    /dev/disk/by-id/g-2 ONLINE\n\t    /dev/disk/by-id/g-3 ONLINE\n\t    /dev/disk/by-id/g-4 ONLINE\n\n";
        let kit = server().with_args(&[
            ("pool", "tank"),
            ("layout", "raidz1"),
            ("disks", "ata-A,ata-B,ata-C"),
        ]);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out("tank\t3T\t1T\t2T\tONLINE\n"))
            .on("zpool", &["status"], out(z1))
            .failing(
                "zpool",
                &["add", "-n"],
                1,
                "/dev/disk/by-id/ata-A contains a filesystem of type 'ext4'",
            )
            .ok("wipefs", &[]);
        assert_eq!(block_on(add(&kit.ctx(&r))), 1);
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
    }

    #[test]
    fn cache_and_spare_disks_are_not_replaced_and_log_disks_can_be_mirrored() {
        let status = "\tNAME STATE\n\ttank ONLINE\n\t  /dev/disk/by-id/ata-A-part1 ONLINE\n\tcache\n\t  /dev/disk/by-id/ata-K-part1 ONLINE\n\tspares\n\t  /dev/disk/by-id/ata-S-part1 AVAIL\n\tlogs\n\t  /dev/disk/by-id/ata-L-part1 ONLINE\n\n";
        for old in ["/dev/disk/by-id/ata-K-part1", "/dev/disk/by-id/ata-S-part1"] {
            let kit = server().with_args(&[("pool", "tank"), ("old", old), ("disks", "ata-C")]);
            let r = FakeRunner::new()
                .on("lsblk", &[], out(LSBLK))
                .on("zpool", &["list"], out("tank\t3T\t1T\t2T\tONLINE\n"))
                .on("zpool", &["status"], out(status))
                .ok("wipefs", &[]);
            assert_eq!(block_on(replace(&kit.ctx(&r))), 1, "{old}");
            assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")), "{old}");
        }
        let kit = server().with_args(&[
            ("pool", "tank"),
            ("existing", "/dev/disk/by-id/ata-L-part1"),
            ("disks", "ata-C"),
        ]);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out("tank\t3T\t1T\t2T\tONLINE\n"))
            .on("zpool", &["status"], out(status))
            .ok("wipefs", &[])
            .ok("sgdisk", &[])
            .ok("zpool", &["attach"]);
        assert_eq!(block_on(attach(&kit.ctx(&r))), 0, "{}", kit.sink.text());
    }

    #[test]
    fn a_new_disk_exactly_as_big_as_the_old_partition_is_still_too_small() {
        // The spare is 4,000,787,030,016 bytes; so is the pool's partition.
        let same = LSBLK.replace(
            r#"{"name":"sdb","type":"disk","size":4000787030016,"model":"ST4","tran":"sata","mountpoints":[null]}"#,
            r#"{"name":"sdb","type":"disk","size":4100787030016,"model":"ST4","tran":"sata","mountpoints":[null],"children":[{"name":"sdb1","type":"part","size":4000787030016,"mountpoints":[null]}]}"#,
        );
        assert_ne!(same, LSBLK);
        let kit = server().with_args(&[
            ("pool", "tank"),
            ("old", "/dev/disk/by-id/ata-B-part1"),
            ("disks", "ata-C"),
        ]);
        std::fs::write(kit.root.join("dev/sdb1"), "").unwrap();
        std::os::unix::fs::symlink(
            kit.root.join("dev/sdb1"),
            kit.root.join("dev/disk/by-id/ata-B-part1"),
        )
        .unwrap();
        let r = FakeRunner::new()
            .on("lsblk", &[], out(&same))
            .on("zpool", &["list"], out("tank\t3T\t1T\t2T\tONLINE\n"))
            .on("zpool", &["status"], out(STATUS))
            .ok("wipefs", &[]);
        assert_eq!(block_on(replace(&kit.ctx(&r))), 1);
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
    }

    #[test]
    fn a_single_disk_is_added_one_at_a_time_and_raid10_is_not_an_add_layout() {
        for (layout, disks) in [
            ("single", "ata-B,ata-C"),
            ("raid10", "ata-A,ata-B,ata-C,ata-D"),
        ] {
            let kit = server().with_args(&[("pool", "tank"), ("layout", layout), ("disks", disks)]);
            let r = with_pool(&kit);
            assert_eq!(block_on(add(&kit.ctx(&r))), 1, "{layout}");
            assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
        }
    }

    #[test]
    fn detach_scrub_and_clear_run_their_one_command() {
        let kit = server().with_args(&[("pool", "tank"), ("disk", "/dev/disk/by-id/ata-B-part1")]);
        let r = with_pool(&kit);
        assert_eq!(block_on(detach(&kit.ctx(&r))), 0);
        assert!(r
            .lines()
            .iter()
            .any(|l| l == "zpool detach tank /dev/disk/by-id/ata-B-part1"));

        let kit = server().with_args(&[("pool", "tank"), ("action", "stop")]);
        let r = with_pool(&kit);
        assert_eq!(block_on(scrub(&kit.ctx(&r))), 0);
        assert!(r.lines().iter().any(|l| l == "zpool scrub -s tank"));
        let kit = server().with_args(&[("pool", "tank")]);
        let r = with_pool(&kit);
        assert_eq!(block_on(scrub(&kit.ctx(&r))), 0);
        assert!(r.lines().iter().any(|l| l == "zpool scrub tank"));
        assert_eq!(block_on(clear(&kit.ctx(&r))), 0);
        assert!(r.lines().iter().any(|l| l == "zpool clear tank"));
    }

    #[test]
    fn importing_a_pool_registers_it_and_force_adds_the_flag() {
        let kit = server().with_args(&[("pool", "oldpool"), ("force", "yes")]);
        let r = FakeRunner::new()
            .on("zpool", &["list"], out(""))
            .ok("zpool", &["import"])
            .ok("nix-instantiate", &[]);
        assert_eq!(block_on(import(&kit.ctx(&r))), 0, "{}", kit.sink.text());
        assert!(r
            .lines()
            .iter()
            .any(|l| l == "zpool import -d /dev/disk/by-id -f oldpool"));
        let file = std::fs::read_to_string(kit.nixos.join(FILE)).unwrap();
        assert_eq!(registered(&file), ["oldpool"]);

        let kit = server().with_args(&[("pool", "oldpool")]);
        let r = FakeRunner::new().on("zpool", &["list"], out("")).failing(
            "zpool",
            &["import"],
            1,
            "cannot import: pool may be in use",
        );
        assert_eq!(block_on(import(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("Force"));
        assert!(!kit.nixos.join(FILE).exists());
    }

    #[test]
    fn the_import_list_names_what_it_found() {
        let kit = server();
        let r = FakeRunner::new().on("zpool", &["list"], out("")).on(
            "zpool",
            &["import"],
            out("   pool: tank\n     id: 7\n"),
        );
        assert_eq!(block_on(import_list(&kit.ctx(&r))), 0);
        assert!(kit.sink.text().contains("Importable pools: tank"));
    }

    #[test]
    fn without_zfs_installed_the_job_says_so() {
        let kit = server().with_args(&[("pool", "tank")]);
        let r = FakeRunner::new();
        assert_eq!(block_on(clear(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("zpool"));
    }

    #[test]
    fn a_hand_edited_registry_is_left_alone() {
        let kit =
            server().with_args(&[("name", "tank"), ("topology", "single"), ("disks", "ata-A")]);
        kit.nixos_file(FILE, "{ ... }: { boot.zfs.extraPools = [ \"mine\" ]; }\n");
        let r = runner();
        // The pool is made, but the job fails loudly and the file is untouched.
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("will not edit"));
        let file = std::fs::read_to_string(kit.nixos.join(FILE)).unwrap();
        assert!(file.contains("\"mine\"") && !file.contains("tank"));
    }
}
