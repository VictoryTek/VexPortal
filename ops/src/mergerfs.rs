//! The bulk pool: mergerfs over mixed-size disks, with optional SnapRAID parity.
//!
//! What `just mergerfs-pool` does, one operation per action. The pool lives in
//! `/etc/nixos/storage-pool.nix` in the exact format the script writes, so either path can
//! manage the other's pool. vexos-nix has one mergerfs pool per host (`/storage`), which is
//! why creating a second one is refused; disks are added to the first instead.
//!
//! Every operation that erases a disk goes through [`crate::disks`]: the disk must pass the
//! eligibility check at that moment, again right before it is formatted, and a step that
//! fails stops the job. Disks this job already mounted are unmounted again when a later
//! step fails, and the pool file is written last, so a failed job leaves nothing half
//! configured. The typed `storage` confirmation is checked by the daemon's validation
//! before any operation starts.

use crate::disks::{self, Disk, ONE_AT_A_TIME};
use crate::nixedit::{self, EditError};
use crate::runner::{Cmd, Runner};
use crate::services::require_server;
use crate::{capture, fail, fsutil, remote, stream, Ctx};
use std::path::{Path, PathBuf};

pub const FILE: &str = "storage-pool.nix";

/// Where the pool is mounted, as `modules/server/mergerfs.nix` defaults it.
pub const UNION: &str = "/storage";

pub fn path(nixos_dir: &Path) -> PathBuf {
    nixos_dir.join(FILE)
}

// ───────────────────────────────── The pool file ─────────────────────────────────

/// One disk of the pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub mount: String,
    pub uuid: String,
    pub fstype: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pool {
    /// The content disks: what mergerfs unions.
    pub content: Vec<Member>,
    /// The SnapRAID parity disks.
    pub parity: Vec<Member>,
}

impl Pool {
    pub fn members(&self) -> impl Iterator<Item = &Member> {
        self.content.iter().chain(&self.parity)
    }
}

const HAND_EDITED: &str = "storage-pool.nix was not written by VexPortal or `just mergerfs-pool` (or was edited by hand), so VexPortal will not change it.";

/// `/mnt/disk<N>` or `/mnt/parity<N>`: the only mount points the pool uses.
pub fn valid_mount(mount: &str) -> bool {
    let Some(rest) = mount.strip_prefix("/mnt/") else {
        return false;
    };
    rest.strip_prefix("disk")
        .or_else(|| rest.strip_prefix("parity"))
        .is_some_and(|n| {
            !n.is_empty()
                && n.len() <= 4
                && !n.starts_with('0')
                && n.chars().all(|c| c.is_ascii_digit())
        })
}

fn quoted(line: &str, name: &str) -> Option<String> {
    let start = line.find(&format!("{name} = \""))? + name.len() + 4;
    let rest = &line[start..];
    Some(rest[..rest.find('"')?].to_string())
}

fn member(line: &str) -> Result<Member, String> {
    let bad = || HAND_EDITED.to_string();
    let mount = quoted(line, "mountPoint").ok_or_else(bad)?;
    let device = quoted(line, "device").ok_or_else(bad)?;
    let fstype = quoted(line, "fsType").ok_or_else(bad)?;
    let uuid = device
        .strip_prefix("/dev/disk/by-uuid/")
        .ok_or_else(bad)?
        .to_string();
    let uuid_ok = !uuid.is_empty()
        && uuid.len() <= 64
        && uuid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !valid_mount(&mount) || !uuid_ok || !matches!(fstype.as_str(), "ext4" | "xfs") {
        return Err(bad());
    }
    Ok(Member {
        mount,
        uuid,
        fstype,
    })
}

/// The lines that decide what NixOS does: everything but comments and blank lines.
fn significant(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

/// The pool a file describes. A file that is not exactly what [`render`] would write for
/// that pool is refused rather than guessed at, so a hand-edited file is never rewritten.
pub fn parse(text: &str) -> Result<Pool, String> {
    #[derive(PartialEq)]
    enum In {
        Nothing,
        Content,
        Parity,
    }
    let mut pool = Pool::default();
    let mut state = In::Nothing;
    for line in significant(text) {
        if line.starts_with("branches = [") {
            state = In::Content;
        } else if line.starts_with("parityDisks = [") {
            state = In::Parity;
        } else if line.starts_with("];") {
            state = In::Nothing;
        } else if state != In::Nothing && line.starts_with('{') {
            let one = member(line)?;
            match state {
                In::Content => pool.content.push(one),
                _ => pool.parity.push(one),
            }
        }
    }
    if pool.content.is_empty() || significant(text) != significant(&render(&pool, "")) {
        return Err(HAND_EDITED.to_string());
    }
    Ok(pool)
}

fn entry(m: &Member) -> String {
    format!(
        "      {{ mountPoint = \"{}\"; device = \"/dev/disk/by-uuid/{}\"; fsType = \"{}\"; }}\n",
        m.mount, m.uuid, m.fstype
    )
}

/// The file for `pool`, in the format `just mergerfs-pool` writes.
pub fn render(pool: &Pool, stamp: &str) -> String {
    let mut out = format!(
        "# /etc/nixos/storage-pool.nix\n\
         # Written by VexPortal or `just mergerfs-pool` on {stamp}.\n\
         # Changing the pool again overwrites this file (the previous version is kept as .bak).\n\
         # Host-generated (per-machine disk UUIDs): do NOT commit to the vexos-nix repo.\n\
         {{ ... }}:\n\
         {{\n  vexos.server.nas.backend = \"mergerfs\";\n\n\
         \x20 vexos.server.storage.mergerfs = {{\n    enable = true;\n    branches = [\n"
    );
    pool.content.iter().for_each(|m| out.push_str(&entry(m)));
    out.push_str("    ];\n  };\n");
    if !pool.parity.is_empty() {
        out.push_str(
            "\n  vexos.server.storage.snapraid = {\n    enable = true;\n    parityDisks = [\n",
        );
        pool.parity.iter().for_each(|m| out.push_str(&entry(m)));
        out.push_str("    ];\n  };\n");
    }
    out.push_str("}\n");
    out
}

/// The next free `/mnt/<prefix><N>`.
pub fn next_mount(pool: &Pool, prefix: &str) -> String {
    let highest = pool
        .members()
        .filter_map(|m| m.mount.strip_prefix(&format!("/mnt/{prefix}")))
        .filter_map(|n| n.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("/mnt/{prefix}{}", highest + 1)
}

/// `(mount point, size, used)` in bytes from `df -B1 --output=target,size,used`.
pub fn parse_df(text: &str) -> Vec<(String, u64, u64)> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace().rev();
            let used = words.next()?.parse().ok()?;
            let size = words.next()?.parse().ok()?;
            let mount = words.rev().collect::<Vec<_>>().join(" ");
            (!mount.is_empty()).then_some((mount, size, used))
        })
        .collect()
}

pub const DF_ARGS: [&str; 3] = ["-B1", "--output=target,size,used", "--"];

// ───────────────────────────── Reading and saving the file ─────────────────────────────

fn arg<'a, R: Runner>(ctx: &'a Ctx<'_, R>, name: &str) -> Option<&'a str> {
    ctx.args
        .get(name)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
}

fn finish(result: Result<(), i32>) -> i32 {
    result.err().unwrap_or(0)
}

/// The configured pool, or `None` if there is none.
fn read_pool<R: Runner>(ctx: &Ctx<'_, R>) -> Result<Option<Pool>, i32> {
    let file = path(ctx.nixos_dir);
    if !file.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&file)
        .map_err(|e| fail(ctx.sink, format!("could not read {FILE}: {e}")))?;
    parse(&text).map(Some).map_err(|e| fail(ctx.sink, e))
}

fn require_pool<R: Runner>(ctx: &Ctx<'_, R>) -> Result<Pool, i32> {
    read_pool(ctx)?.ok_or_else(|| {
        fail(
            ctx.sink,
            "no bulk pool is set up on this host. Create one first.",
        )
    })
}

/// Write the pool file. `before` is the pool the operation started from: an existing file
/// is only replaced while it still describes exactly that, so nobody's change is lost.
async fn save<R: Runner>(
    ctx: &Ctx<'_, R>,
    before: Option<&Pool>,
    after: &Pool,
) -> Result<(), String> {
    let file = path(ctx.nixos_dir);
    let text = render(after, &fsutil::now_stamp());
    match before {
        Some(before) => {
            nixedit::edit_file(ctx.runner, &file, |current| {
                if parse(current).as_ref() != Ok(before) {
                    return Err(EditError::Invalid(format!(
                        "{FILE} changed while this was running, so nothing was written."
                    )));
                }
                Ok(Some(text.clone()))
            })
            .await
            .map_err(|e| format!("could not update {FILE}: {e}"))?;
        }
        None => {
            // `nixedit::edit_file` checks the text parses before it replaces a file; a new
            // file gets the same check before it is created.
            let parsed = ctx
                .runner
                .run(
                    &Cmd::new("nix-instantiate")
                        .args(["--parse", "-"])
                        .stdin(text.clone().into_bytes()),
                )
                .await
                .map_err(|e| format!("could not check {FILE}: {e}"))?;
            if !parsed.success() {
                return Err(format!(
                    "{FILE} would not be valid Nix, so nothing was written:\n{}",
                    parsed.stderr.trim()
                ));
            }
            fsutil::create_new(&file, text.as_bytes(), 0o644)
                .map_err(|e| format!("could not create {FILE}: {e}"))?;
        }
    }
    track(ctx, true).await;
    Ok(())
}

/// `git+file://` builds only see files git tracks. Keep the index in step with the file,
/// as the script does, so a rebuild neither drops the pool nor brings a destroyed one back.
/// Skipped when `/etc/nixos` is not a repository.
async fn track<R: Runner>(ctx: &Ctx<'_, R>, present: bool) {
    let dir = ctx.nixos_dir.display().to_string();
    let git = |args: &[&str]| {
        Cmd::new("git")
            .args(["-C", dir.as_str()])
            .args(args.iter().copied())
    };
    let Ok(probe) = ctx.runner.run(&git(&["rev-parse", "--git-dir"])).await else {
        return;
    };
    if !probe.success() {
        return;
    }
    let step = if present {
        git(&["add", "-f", "--", FILE])
    } else {
        let Ok(known) = ctx
            .runner
            .run(&git(&["ls-files", "--error-unmatch", "--", FILE]))
            .await
        else {
            return;
        };
        if !known.success() {
            return;
        }
        git(&["rm", "-q", "--cached", "--", FILE])
    };
    if !matches!(ctx.runner.run(&step).await, Ok(out) if out.success()) {
        ctx.sink.error(&format!(
            "warning: could not update git's record of {FILE}; the next rebuild tidies it."
        ));
    }
}

// ─────────────────────────────── Disks and mounts ───────────────────────────────

/// Refuse early when a tool the job needs is not installed, before anything is erased.
fn need<R: Runner>(ctx: &Ctx<'_, R>, tools: &[&str]) -> Result<(), i32> {
    for tool in tools {
        if !remote::tool_present(ctx.root, tool) {
            return Err(fail(
                ctx.sink,
                format!("`{tool}` is not installed on this host, so nothing was changed."),
            ));
        }
    }
    Ok(())
}

fn mkfs_tool(fstype: &str) -> String {
    format!("mkfs.{fstype}")
}

fn rebuild_reminder<R: Runner>(ctx: &Ctx<'_, R>) {
    ctx.sink.info("");
    ctx.sink.info(
        "Rebuild so the boot-time mounts match storage-pool.nix. Until then this holds until the next reboot.",
    );
}

/// Whether `mount` is a mount point. `mountpoint` exits 0 for one and 32 for a directory
/// that is not; a directory that does not exist yet is not mounted either. Any other result
/// is an error, never "not mounted": this decides whether a disk may be erased.
async fn mounted<R: Runner>(ctx: &Ctx<'_, R>, mount: &str) -> Result<bool, i32> {
    let probe = capture(ctx, &Cmd::new("mountpoint").args(["-q", mount])).await?;
    match probe.code {
        0 => Ok(true),
        32 => Ok(false),
        // Exit 1 is also what a path that does not exist gives. Only a directory that is
        // positively reported missing counts, not one that merely cannot be looked at.
        1 if matches!(
            std::fs::metadata(ctx.root.join(mount.trim_start_matches('/'))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound
        ) =>
        {
            Ok(false)
        }
        code => Err(fail(
            ctx.sink,
            format!(
                "could not tell whether {mount} is mounted (`mountpoint` exited {code}: {}), so nothing was changed.",
                probe.stderr.trim()
            ),
        )),
    }
}

/// Refuse before anything is erased when a mount point is already in use: a disk mounted on
/// top of another hides it, and a mount the host's own configuration declares would be
/// declared twice at the next rebuild. A disk taken out of the pool stays mounted until
/// someone unmounts it, so its number can come up again.
async fn refuse_mounted<R: Runner>(ctx: &Ctx<'_, R>, mounts: &[String]) -> Result<(), i32> {
    for mount in mounts {
        if mounted(ctx, mount).await? {
            return Err(fail(
                ctx.sink,
                format!(
                    "{mount} is already mounted, so nothing was erased. Unmount it first (`umount {mount}`), \
                     or if something else uses that name, rename that mount."
                ),
            ));
        }
    }
    Ok(())
}

/// Unmount what a failed job had mounted. A job that was cancelled is still tidied.
async fn unmount_all<R: Runner>(ctx: &Ctx<'_, R>, mounts: &[String]) {
    for mount in mounts.iter().rev() {
        let done = ctx.runner.cleanup(&Cmd::new("umount").arg(mount)).await;
        if !matches!(done, Ok(out) if out.success()) {
            ctx.sink
                .error(&format!("warning: could not unmount {mount}."));
        }
    }
}

/// Whether `disk` can still be erased, by the same name and kernel name as chosen.
async fn still_free<R: Runner>(ctx: &Ctx<'_, R>, disk: &Disk) -> Result<(), i32> {
    let now = disks::eligible_now(ctx, &[]).await?;
    if now
        .iter()
        .any(|d| d.by_id == disk.by_id && d.kernel == disk.kernel)
    {
        return Ok(());
    }
    Err(fail(
        ctx.sink,
        format!(
            "{} is no longer a disk that can be erased (it changed while the job was running). Nothing more was erased.",
            disk.by_id
        ),
    ))
}

/// Erase `disk`, make a filesystem on it, mount it at `mount`, and give its UUID.
/// `mounts` gets `mount` as soon as it is mounted, so a failure later can undo it.
async fn prepare<R: Runner>(
    ctx: &Ctx<'_, R>,
    disk: &Disk,
    mount: &str,
    fstype: &str,
    mounts: &mut Vec<String>,
) -> Result<String, i32> {
    ctx.sink.info(&format!("{} → {mount}", disk.by_id));
    disks::wipe(ctx, &[disk], &[]).await?;
    still_free(ctx, disk).await?;

    let device = format!("/dev/{}", disk.kernel);
    let mkfs = match fstype {
        "xfs" => Cmd::new(mkfs_tool("xfs")).args(["-f", &device]),
        _ => Cmd::new(mkfs_tool("ext4")).args(["-F", &device]),
    };
    if stream(ctx, &mkfs).await? != 0 {
        return Err(fail(ctx.sink, format!("could not format {}.", disk.by_id)));
    }

    // Under the daemon's root in tests, `/` otherwise.
    std::fs::create_dir_all(ctx.root.join(mount.trim_start_matches('/')))
        .map_err(|e| fail(ctx.sink, format!("could not create {mount}: {e}")))?;
    if stream(ctx, &Cmd::new("mount").args([&device, mount])).await? != 0 {
        return Err(fail(
            ctx.sink,
            format!("could not mount {} at {mount}.", disk.by_id),
        ));
    }
    mounts.push(mount.to_string());

    let probe = capture(
        ctx,
        &Cmd::new("blkid").args(["-p", "-s", "UUID", "-o", "value", &device]),
    )
    .await?;
    let uuid = probe.stdout.trim().to_string();
    if !probe.success() || uuid.is_empty() {
        return Err(fail(
            ctx.sink,
            format!(
                "could not read the new filesystem's UUID on {}.",
                disk.by_id
            ),
        ));
    }
    ctx.sink.info(&format!("✔ {mount} mounted (UUID {uuid})"));
    Ok(uuid)
}

fn fstype_arg<R: Runner>(ctx: &Ctx<'_, R>) -> Result<&'static str, i32> {
    match arg(ctx, "fstype").unwrap_or("ext4") {
        "ext4" => Ok("ext4"),
        "xfs" => Ok("xfs"),
        _ => Err(fail(ctx.sink, "the filesystem must be ext4 or xfs.")),
    }
}

// ──────────────────────────────────── Operations ────────────────────────────────────

pub async fn create<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let _only = ONE_AT_A_TIME.lock().await;
    let mut mounts = Vec::new();
    let result = create_inner(ctx, &mut mounts).await;
    if result.is_err() {
        unmount_all(ctx, &mounts).await;
    }
    finish(result)
}

async fn create_inner<R: Runner>(ctx: &Ctx<'_, R>, mounts: &mut Vec<String>) -> Result<(), i32> {
    require_server(ctx)?;
    let fstype = fstype_arg(ctx)?;
    need(ctx, &["wipefs", "sgdisk", "blkid", &mkfs_tool(fstype)])?;
    if read_pool(ctx)?.is_some() {
        return Err(fail(
            ctx.sink,
            "a bulk pool is already set up. vexos-nix has one pool per host, at /storage: \
             use Add Disk to grow it, or destroy it first.",
        ));
    }

    let available = disks::eligible_now(ctx, &[]).await?;
    let content =
        disks::pick(&available, arg(ctx, "disks").unwrap_or("")).map_err(|e| fail(ctx.sink, e))?;
    let parity = match arg(ctx, "parity") {
        Some(ids) => disks::pick(&available, ids).map_err(|e| fail(ctx.sink, e))?,
        None => Vec::new(),
    };
    if let Some(shared) = parity
        .iter()
        .find(|p| content.iter().any(|c| c.kernel == p.kernel))
    {
        return Err(fail(
            ctx.sink,
            format!(
                "{} is chosen as both a content and a parity disk.",
                shared.by_id
            ),
        ));
    }
    let largest = content.iter().map(|d| d.size).max().unwrap_or(0);
    if let Some(small) = parity.iter().find(|p| p.size < largest) {
        return Err(fail(
            ctx.sink,
            format!(
                "parity disk {} ({}) is smaller than the largest content disk ({}). SnapRAID needs it to be at least as large. Nothing was erased.",
                small.by_id,
                disks::size_label(small.size),
                disks::size_label(largest)
            ),
        ));
    }
    if parity.is_empty() {
        ctx.sink
            .error("warning: no parity disks, so the pool has NO redundancy.");
    }

    let wanted: Vec<String> = (1..=content.len())
        .map(|n| format!("/mnt/disk{n}"))
        .chain((1..=parity.len()).map(|n| format!("/mnt/parity{n}")))
        .collect();
    refuse_mounted(ctx, &wanted).await?;

    let mut pool = Pool::default();
    for (index, disk) in content.iter().enumerate() {
        let mount = format!("/mnt/disk{}", index + 1);
        let uuid = prepare(ctx, disk, &mount, fstype, mounts).await?;
        pool.content.push(Member {
            mount,
            uuid,
            fstype: fstype.into(),
        });
    }
    for (index, disk) in parity.iter().enumerate() {
        let mount = format!("/mnt/parity{}", index + 1);
        let uuid = prepare(ctx, disk, &mount, fstype, mounts).await?;
        pool.parity.push(Member {
            mount,
            uuid,
            fstype: fstype.into(),
        });
    }

    save(ctx, None, &pool)
        .await
        .map_err(|e| fail(ctx.sink, e))?;
    ctx.sink.info(&format!(
        "Bulk pool configured: {} content disk(s), {} parity disk(s).",
        pool.content.len(),
        pool.parity.len()
    ));
    rebuild_reminder(ctx);
    if !pool.parity.is_empty() {
        ctx.sink
            .info("After the rebuild, run Sync Parity to build the first parity.");
    }
    Ok(())
}

pub async fn add<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let _only = ONE_AT_A_TIME.lock().await;
    let mut mounts = Vec::new();
    let result = add_inner(ctx, &mut mounts).await;
    if result.is_err() {
        unmount_all(ctx, &mounts).await;
    }
    finish(result)
}

async fn add_inner<R: Runner>(ctx: &Ctx<'_, R>, mounts: &mut Vec<String>) -> Result<(), i32> {
    require_server(ctx)?;
    let parity = match arg(ctx, "role") {
        Some("content") => false,
        Some("parity") => true,
        _ => return Err(fail(ctx.sink, "choose content or parity.")),
    };
    let before = require_pool(ctx)?;
    let fstype = match before.content[0].fstype.as_str() {
        "xfs" => "xfs",
        _ => "ext4",
    };
    need(ctx, &["wipefs", "sgdisk", "blkid", &mkfs_tool(fstype)])?;

    let available = disks::eligible_now(ctx, &[]).await?;
    let chosen =
        disks::pick(&available, arg(ctx, "disks").unwrap_or("")).map_err(|e| fail(ctx.sink, e))?;
    let [disk] = chosen.as_slice() else {
        return Err(fail(ctx.sink, "choose exactly one disk."));
    };

    if parity {
        let mut largest = 0;
        for member in &before.content {
            let device = format!("/dev/disk/by-uuid/{}", member.uuid);
            // A content disk that is missing has no size to compare.
            largest = largest.max(disks::size_of(ctx, &device).await?.unwrap_or(0));
        }
        if disk.size < largest {
            return Err(fail(
                ctx.sink,
                format!(
                    "{} ({}) is smaller than the largest content disk ({}). SnapRAID needs a parity disk at least as large. Nothing was erased.",
                    disk.by_id,
                    disks::size_label(disk.size),
                    disks::size_label(largest)
                ),
            ));
        }
    } else {
        // Parity has to hold the largest content disk, so a bigger one is refused too.
        let mut smallest: Option<u64> = None;
        for member in &before.parity {
            let device = format!("/dev/disk/by-uuid/{}", member.uuid);
            if let Some(size) = disks::size_of(ctx, &device).await? {
                smallest = Some(smallest.map_or(size, |s| s.min(size)));
            }
        }
        if let Some(limit) = smallest.filter(|limit| disk.size > *limit) {
            return Err(fail(
                ctx.sink,
                format!(
                    "{} ({}) is larger than a parity disk ({}), which SnapRAID needs to hold the \
                     largest content disk. Add a bigger parity disk first. Nothing was erased.",
                    disk.by_id,
                    disks::size_label(disk.size),
                    disks::size_label(limit)
                ),
            ));
        }
    }

    let mount = next_mount(&before, if parity { "parity" } else { "disk" });
    refuse_mounted(ctx, std::slice::from_ref(&mount)).await?;
    let uuid = prepare(ctx, disk, &mount, fstype, mounts).await?;
    let mut after = before.clone();
    let list = if parity {
        &mut after.parity
    } else {
        &mut after.content
    };
    list.push(Member {
        mount: mount.clone(),
        uuid,
        fstype: fstype.into(),
    });
    save(ctx, Some(&before), &after)
        .await
        .map_err(|e| fail(ctx.sink, e))?;

    ctx.sink.info(&format!("Added {} as {mount}.", disk.by_id));
    rebuild_reminder(ctx);
    if parity || !after.parity.is_empty() {
        ctx.sink
            .info("After the rebuild, run Sync Parity to bring the parity up to date.");
    }
    Ok(())
}

pub async fn remove<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let _only = ONE_AT_A_TIME.lock().await;
    finish(remove_inner(ctx).await)
}

async fn remove_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let before = require_pool(ctx)?;
    let Some(mount) = arg(ctx, "mount") else {
        return Err(fail(ctx.sink, "choose a disk to remove."));
    };
    let is_content = before.content.iter().any(|m| m.mount == mount);
    let is_parity = before.parity.iter().any(|m| m.mount == mount);
    if !is_content && !is_parity {
        return Err(fail(
            ctx.sink,
            format!("`{mount}` is not a disk of the pool."),
        ));
    }
    if is_content && before.content.len() == 1 {
        return Err(fail(
            ctx.sink,
            "that is the last content disk. Destroy the pool instead.",
        ));
    }

    let mut after = before.clone();
    after.content.retain(|m| m.mount != mount);
    after.parity.retain(|m| m.mount != mount);
    save(ctx, Some(&before), &after)
        .await
        .map_err(|e| fail(ctx.sink, e))?;

    ctx.sink.info(&format!("{mount} removed from the pool."));
    if is_content {
        ctx.sink.info(&format!(
            "Its files stop appearing in {UNION} once this is applied. The data stays on the disk; \
             it was not erased."
        ));
        if !before.parity.is_empty() {
            ctx.sink.info(
                "SnapRAID numbers data disks by position. After the rebuild, run Sync Parity and \
                 check its output for renumbering warnings.",
            );
        }
    } else {
        ctx.sink
            .info("The pool now has less parity protection than before.");
    }
    rebuild_reminder(ctx);
    ctx.sink.info(&format!(
        "If it is still mounted afterwards: umount {mount}"
    ));
    Ok(())
}

pub async fn destroy<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let _only = ONE_AT_A_TIME.lock().await;
    let mut unmounted = Vec::new();
    let result = destroy_inner(ctx, &mut unmounted).await;
    if result.is_err() && !unmounted.is_empty() {
        // The configuration is unchanged, so the pool should still be serving: put back
        // what this job took down, the disks before the pool that is built on them.
        ctx.sink
            .info("Putting back what was unmounted, since the pool is still configured.");
        for mount in unmounted.iter().rev() {
            let back = ctx.runner.cleanup(&Cmd::new("mount").arg(mount)).await;
            if !matches!(back, Ok(out) if out.success()) {
                ctx.sink.error(&format!(
                    "warning: could not mount {mount} again. A rebuild or a reboot mounts it."
                ));
            }
        }
    }
    finish(result)
}

async fn destroy_inner<R: Runner>(
    ctx: &Ctx<'_, R>,
    unmounted: &mut Vec<String>,
) -> Result<(), i32> {
    require_server(ctx)?;
    let pool = require_pool(ctx)?;
    let file = path(ctx.nixos_dir);
    let text = std::fs::read_to_string(&file)
        .map_err(|e| fail(ctx.sink, format!("could not read {FILE}: {e}")))?;

    let targets = std::iter::once(UNION).chain(pool.members().map(|m| m.mount.as_str()));
    for mount in targets {
        if !mounted(ctx, mount).await? {
            continue;
        }
        if stream(ctx, &Cmd::new("umount").arg(mount)).await? != 0 {
            return Err(fail(
                ctx.sink,
                format!(
                    "could not unmount {mount}. Stop whatever is using it (try `lsof +D {mount}`) and run this again. \
                     The configuration was not changed."
                ),
            ));
        }
        unmounted.push(mount.to_string());
        ctx.sink.info(&format!("✔ {mount} unmounted"));
    }

    fsutil::backup(&file).map_err(|e| fail(ctx.sink, format!("could not back up {FILE}: {e}")))?;
    let removed = nixedit::replace_if_unchanged(&file, &text, None)
        .await
        .map_err(|e| fail(ctx.sink, format!("could not remove {FILE}: {e}")))?;
    if !removed {
        return Err(fail(
            ctx.sink,
            format!("{FILE} changed while the pool was being destroyed, so it was left alone."),
        ));
    }
    track(ctx, false).await;

    ctx.sink.info(&format!(
        "Bulk pool removed from the configuration (a copy is kept as {FILE}.bak). The disks keep \
         their data."
    ));
    rebuild_reminder(ctx);
    ctx.sink
        .info("Until you rebuild, the old mounts are still declared and return at the next boot.");
    Ok(())
}

pub async fn sync<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    finish(snapraid(ctx, "sync").await)
}

pub async fn scrub<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    finish(snapraid(ctx, "scrub").await)
}

/// `snapraid sync` or `snapraid scrub`: hours long, safe to stop, and resumes next time.
async fn snapraid<R: Runner>(ctx: &Ctx<'_, R>, verb: &str) -> Result<(), i32> {
    require_server(ctx)?;
    let pool = require_pool(ctx)?;
    if pool.parity.is_empty() {
        return Err(fail(
            ctx.sink,
            "the pool has no parity disks, so SnapRAID is not set up.",
        ));
    }
    need(ctx, &["snapraid"])?;
    if !ctx.root.join("etc/snapraid.conf").exists() {
        return Err(fail(
            ctx.sink,
            "/etc/snapraid.conf was not found. Rebuild first so SnapRAID is configured.",
        ));
    }
    ctx.sink
        .info("This can take hours. Cancelling stops it safely; run it again to carry on.");
    match stream(ctx, &Cmd::new("snapraid").arg(verb)).await? {
        0 => Ok(()),
        code => Err(fail(
            ctx.sink,
            format!("snapraid {verb} stopped with exit code {code}."),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::testing::{block_on, FakeRunner};
    use crate::runner::Output;
    use crate::testkit::Kit;

    fn member(mount: &str, uuid: &str) -> Member {
        Member {
            mount: mount.into(),
            uuid: uuid.into(),
            fstype: "ext4".into(),
        }
    }

    fn two_and_one() -> Pool {
        Pool {
            content: vec![
                member("/mnt/disk1", "AAAA-1"),
                member("/mnt/disk2", "BBBB-2"),
            ],
            parity: vec![member("/mnt/parity1", "CCCC-3")],
        }
    }

    /// What `create-mergerfs-pool.sh` writes, comments aside.
    const SCRIPT_FILE: &str = r#"# /etc/nixos/storage-pool.nix
# GENERATED by scripts/mergerfs-pool.sh on 2026-01-01 00:00:00.
{ ... }:
{
  vexos.server.nas.backend = "mergerfs";

  vexos.server.storage.mergerfs = {
    enable = true;
    branches = [
      { mountPoint = "/mnt/disk1"; device = "/dev/disk/by-uuid/AAAA-1"; fsType = "ext4"; }
      { mountPoint = "/mnt/disk2"; device = "/dev/disk/by-uuid/BBBB-2"; fsType = "ext4"; }
    ];
  };

  vexos.server.storage.snapraid = {
    enable = true;
    parityDisks = [
      { mountPoint = "/mnt/parity1"; device = "/dev/disk/by-uuid/CCCC-3"; fsType = "ext4"; }
    ];
  };
}
"#;

    #[test]
    fn the_file_the_script_writes_is_read_and_written_back_the_same() {
        let pool = parse(SCRIPT_FILE).unwrap();
        assert_eq!(pool, two_and_one());
        assert_eq!(significant(&render(&pool, "now")), significant(SCRIPT_FILE));
    }

    #[test]
    fn a_pool_without_parity_has_no_snapraid_block() {
        let mut pool = two_and_one();
        pool.parity.clear();
        let text = render(&pool, "now");
        assert!(!text.contains("snapraid"));
        assert_eq!(parse(&text).unwrap(), pool);
    }

    #[test]
    fn a_file_somebody_edited_is_refused() {
        for edited in [
            SCRIPT_FILE.replace("enable = true;", "enable = true;\n    extra = 1;"),
            SCRIPT_FILE.replace("/mnt/disk2", "/data/two"),
            SCRIPT_FILE.replace("ext4", "btrfs"),
            SCRIPT_FILE.replace("/dev/disk/by-uuid/AAAA-1", "/dev/sda1"),
            "{ ... }: { }".to_string(),
            String::new(),
        ] {
            assert!(parse(&edited).is_err(), "{edited}");
        }
    }

    #[test]
    fn mounts_are_numbered_past_the_highest() {
        let mut pool = two_and_one();
        assert_eq!(next_mount(&pool, "disk"), "/mnt/disk3");
        assert_eq!(next_mount(&pool, "parity"), "/mnt/parity2");
        pool.content.remove(0);
        assert_eq!(next_mount(&pool, "disk"), "/mnt/disk3");
        assert_eq!(next_mount(&Pool::default(), "disk"), "/mnt/disk1");
    }

    #[test]
    fn df_output_is_read_by_mount_point() {
        let text = "Mounted on        1B-blocks          Used\n\
                    /storage      12000000000000 4000000000000\n\
                    /mnt/disk1     4000000000000 1000000000000\n";
        let rows = parse_df(text);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[1],
            (
                "/mnt/disk1".to_string(),
                4_000_000_000_000,
                1_000_000_000_000
            )
        );
        assert!(parse_df("").is_empty());
    }

    // ── a scripted server ───────────────────────────────────────────────────────

    const LSBLK: &str = r#"{"blockdevices":[
      {"name":"nvme0n1","type":"disk","size":500107862016,"model":"WD","tran":"nvme","mountpoints":[null],
       "children":[{"name":"nvme0n1p1","type":"part","size":1,"mountpoints":["/"]}]},
      {"name":"sda","type":"disk","size":4000787030016,"model":"ST4","tran":"sata","mountpoints":[null]},
      {"name":"sdb","type":"disk","size":8001574060032,"model":"ST8","tran":"sata","mountpoints":[null]},
      {"name":"sdc","type":"disk","size":8001574060032,"model":"ST8","tran":"sata","mountpoints":[null]},
      {"name":"sdd","type":"disk","size":2000398934016,"model":"ST2","tran":"sata","mountpoints":[null]}]}"#;

    fn out(stdout: &str) -> Output {
        Output {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// A server with four blank disks, the system disk, and every tool installed.
    fn server() -> Kit {
        let kit = Kit::new();
        kit.nixos_file("vexos-variant", "vexos-server-amd");
        let by_id = kit.root.join("dev/disk/by-id");
        std::fs::create_dir_all(&by_id).unwrap();
        for (name, id) in [
            ("sda", "ata-A"),
            ("sdb", "ata-B"),
            ("sdc", "ata-C"),
            ("sdd", "ata-D"),
            ("nvme0n1", "nvme-S"),
        ] {
            std::fs::write(kit.root.join(&format!("dev/{name}")), "").unwrap();
            std::os::unix::fs::symlink(kit.root.join(&format!("dev/{name}")), by_id.join(id))
                .unwrap();
        }
        let bin = kit.root.join("run/current-system/sw/bin");
        std::fs::create_dir_all(&bin).unwrap();
        for tool in [
            "wipefs",
            "sgdisk",
            "blkid",
            "mkfs.ext4",
            "mkfs.xfs",
            "snapraid",
        ] {
            std::fs::write(bin.join(tool), "").unwrap();
        }
        kit
    }

    /// Make the pool's disks resolvable by UUID, as udev does.
    fn link_uuids(kit: &Kit, pairs: &[(&str, &str)]) {
        let by_uuid = kit.root.join("dev/disk/by-uuid");
        std::fs::create_dir_all(&by_uuid).unwrap();
        for (uuid, disk) in pairs {
            std::os::unix::fs::symlink(kit.root.join(&format!("dev/{disk}")), by_uuid.join(uuid))
                .unwrap();
        }
    }

    fn runner() -> FakeRunner {
        FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out(""))
            .on_arg("blkid", "/dev/sda", out("UUID-SDA\n"))
            .on_arg("blkid", "/dev/sdb", out("UUID-SDB\n"))
            .on_arg("blkid", "/dev/sdc", out("UUID-SDC\n"))
            .on_arg("blkid", "/dev/sdd", out("UUID-SDD\n"))
            .ok("wipefs", &[])
            .ok("sgdisk", &[])
            .ok("mkfs.ext4", &[])
            .ok("mkfs.xfs", &[])
            .ok("mount", &[])
            .ok("umount", &[])
            .ok("nix-instantiate", &[])
            .ok("snapraid", &[])
            .ok("git", &[])
            .failing("mountpoint", &[], 32, "")
    }

    fn pos(lines: &[String], needle: &str) -> usize {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no `{needle}` in {lines:#?}"))
    }

    fn written(kit: &Kit) -> Pool {
        parse(&std::fs::read_to_string(path(&kit.nixos)).unwrap()).unwrap()
    }

    #[test]
    fn creating_a_pool_erases_formats_mounts_then_writes_the_file_last() {
        let kit = server().with_args(&[
            ("fstype", "ext4"),
            ("disks", "ata-A,ata-D"),
            ("parity", "ata-B"),
            ("confirm", "storage"),
        ]);
        let r = runner();
        assert_eq!(block_on(create(&kit.ctx(&r))), 0, "{}", kit.sink.text());
        let lines = r.lines();
        let wipe = pos(&lines, "wipefs -a /dev/sda");
        let zap = pos(&lines, "sgdisk --zap-all /dev/sda");
        let mkfs = pos(&lines, "mkfs.ext4 -F /dev/sda");
        let mount = pos(&lines, "mount /dev/sda /mnt/disk1");
        assert!(wipe < zap && zap < mkfs && mkfs < mount);
        assert!(lines.iter().any(|l| l == "mount /dev/sdd /mnt/disk2"));
        assert!(lines.iter().any(|l| l == "mount /dev/sdb /mnt/parity1"));
        // The disk that was not chosen is untouched.
        assert!(!lines.iter().any(|l| l.contains("/dev/sdc")));
        let pool = written(&kit);
        assert_eq!(pool.content.len(), 2);
        assert_eq!(pool.content[0].uuid, "UUID-SDA");
        assert_eq!(pool.content[1].mount, "/mnt/disk2");
        assert_eq!(pool.parity[0].uuid, "UUID-SDB");
        assert!(lines
            .iter()
            .any(|l| l.contains("add -f -- storage-pool.nix")));
    }

    #[test]
    fn xfs_disks_are_made_with_mkfs_xfs() {
        let kit = server().with_args(&[("fstype", "xfs"), ("disks", "ata-A")]);
        let r = runner();
        assert_eq!(block_on(create(&kit.ctx(&r))), 0, "{}", kit.sink.text());
        assert!(r.lines().iter().any(|l| l == "mkfs.xfs -f /dev/sda"));
        assert_eq!(written(&kit).content[0].fstype, "xfs");
    }

    #[test]
    fn a_second_pool_is_refused_and_nothing_is_touched() {
        let kit = server().with_args(&[("disks", "ata-A")]);
        kit.nixos_file(FILE, SCRIPT_FILE);
        let r = runner();
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("already set up"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
    }

    #[test]
    fn a_hand_edited_file_stops_every_operation_before_anything_runs() {
        let kit = server().with_args(&[("disks", "ata-A"), ("role", "content")]);
        kit.nixos_file(
            FILE,
            "{ ... }: { vexos.server.storage.mergerfs.enable = true; }",
        );
        let r = runner();
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert_eq!(block_on(add(&kit.ctx(&r))), 1);
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
    }

    #[test]
    fn a_parity_disk_smaller_than_the_largest_content_disk_is_refused() {
        // sdb and sdc are 8 TB, sda is 4 TB.
        let kit = server().with_args(&[("disks", "ata-B"), ("parity", "ata-A")]);
        let r = runner();
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert!(kit
            .sink
            .text()
            .contains("smaller than the largest content disk"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
        assert!(!path(&kit.nixos).exists());
    }

    #[test]
    fn a_disk_chosen_twice_or_not_free_is_refused_before_anything_is_erased() {
        for (disks, parity) in [
            ("ata-A", "ata-A"),
            ("nvme-S", ""),
            ("ata-A,ata-A", ""),
            ("", ""),
        ] {
            let mut pairs = vec![("disks", disks)];
            if !parity.is_empty() {
                pairs.push(("parity", parity));
            }
            let kit = server().with_args(&pairs);
            let r = runner();
            assert_eq!(block_on(create(&kit.ctx(&r))), 1, "{disks} {parity}");
            assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
        }
    }

    #[test]
    fn a_missing_tool_is_reported_before_any_disk_is_erased() {
        let kit = server().with_args(&[("fstype", "xfs"), ("disks", "ata-A")]);
        std::fs::remove_file(kit.root.join("run/current-system/sw/bin/mkfs.xfs")).unwrap();
        let r = runner();
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("mkfs.xfs"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
    }

    #[test]
    fn a_failed_step_unmounts_what_this_job_mounted_and_writes_no_file() {
        let kit = server().with_args(&[("disks", "ata-A,ata-D")]);
        // The second disk cannot be formatted.
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out(""))
            .on_arg("blkid", "/dev/sda", out("UUID-SDA\n"))
            .failing("mkfs.ext4", &["-F", "/dev/sdd"], 1, "boom")
            .ok("mkfs.ext4", &[])
            .ok("wipefs", &[])
            .ok("sgdisk", &[])
            .ok("mount", &[])
            .ok("umount", &[])
            .failing("mountpoint", &[], 32, "");
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        let lines = r.lines();
        assert!(lines.iter().any(|l| l == "mount /dev/sda /mnt/disk1"));
        assert!(lines.iter().any(|l| l == "umount /mnt/disk1"));
        assert!(!path(&kit.nixos).exists());
    }

    #[test]
    fn a_pool_file_that_cannot_be_written_unmounts_the_disks() {
        let kit = server().with_args(&[("disks", "ata-A")]);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out(""))
            .on_arg("blkid", "/dev/sda", out("UUID-SDA\n"))
            .failing("nix-instantiate", &[], 1, "syntax error")
            .ok("mkfs.ext4", &[])
            .ok("wipefs", &[])
            .ok("sgdisk", &[])
            .ok("mount", &[])
            .ok("umount", &[])
            .failing("mountpoint", &[], 32, "");
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert!(r.lines().iter().any(|l| l == "umount /mnt/disk1"));
        assert!(!path(&kit.nixos).exists());
    }

    #[test]
    fn adding_a_content_disk_takes_the_next_mount_and_keeps_the_rest() {
        let kit = server().with_args(&[("role", "content"), ("disks", "ata-D")]);
        kit.nixos_file(FILE, SCRIPT_FILE);
        let r = runner();
        assert_eq!(block_on(add(&kit.ctx(&r))), 0, "{}", kit.sink.text());
        assert!(r.lines().iter().any(|l| l == "mount /dev/sdd /mnt/disk3"));
        let pool = written(&kit);
        assert_eq!(pool.content.len(), 3);
        assert_eq!(pool.content[2].uuid, "UUID-SDD");
        assert_eq!(pool.parity, two_and_one().parity);
        assert!(kit.nixos.join("storage-pool.nix.bak").exists());
    }

    #[test]
    fn a_parity_disk_must_be_as_large_as_the_largest_content_disk() {
        let kit = server().with_args(&[("role", "parity"), ("disks", "ata-A")]);
        kit.nixos_file(FILE, SCRIPT_FILE);
        // The pool's content disks are sdb and sdc (8 TB); sda is 4 TB.
        link_uuids(&kit, &[("AAAA-1", "sdb"), ("BBBB-2", "sdc")]);
        let r = runner();
        assert_eq!(block_on(add(&kit.ctx(&r))), 1);
        assert!(kit
            .sink
            .text()
            .contains("smaller than the largest content disk"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
        assert_eq!(written(&kit), two_and_one());
    }

    #[test]
    fn a_big_enough_parity_disk_is_added() {
        let kit = server().with_args(&[("role", "parity"), ("disks", "ata-C")]);
        kit.nixos_file(FILE, SCRIPT_FILE);
        link_uuids(&kit, &[("AAAA-1", "sda"), ("BBBB-2", "sdd")]);
        let r = runner();
        assert_eq!(block_on(add(&kit.ctx(&r))), 0, "{}", kit.sink.text());
        let pool = written(&kit);
        assert_eq!(pool.parity.len(), 2);
        assert_eq!(pool.parity[1].mount, "/mnt/parity2");
    }

    #[test]
    fn adding_more_than_one_disk_or_with_no_pool_is_refused() {
        let kit = server().with_args(&[("role", "content"), ("disks", "ata-A,ata-D")]);
        kit.nixos_file(FILE, SCRIPT_FILE);
        let r = runner();
        assert_eq!(block_on(add(&kit.ctx(&r))), 1);
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));

        let kit = server().with_args(&[("role", "content"), ("disks", "ata-A")]);
        assert_eq!(block_on(add(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("no bulk pool"));
    }

    #[test]
    fn removing_a_disk_edits_the_file_and_erases_nothing() {
        let kit = server().with_args(&[("mount", "/mnt/disk2")]);
        kit.nixos_file(FILE, SCRIPT_FILE);
        let r = runner();
        assert_eq!(block_on(remove(&kit.ctx(&r))), 0, "{}", kit.sink.text());
        let pool = written(&kit);
        assert_eq!(pool.content, vec![member("/mnt/disk1", "AAAA-1")]);
        assert!(!r
            .lines()
            .iter()
            .any(|l| l.starts_with("wipefs") || l.starts_with("mkfs") || l.starts_with("umount")));
    }

    #[test]
    fn the_last_content_disk_and_unknown_disks_cannot_be_removed() {
        let mut one = two_and_one();
        one.content.pop();
        let kit = server().with_args(&[("mount", "/mnt/disk1")]);
        kit.nixos_file(FILE, &render(&one, "now"));
        let r = runner();
        assert_eq!(block_on(remove(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("last content disk"));

        let kit = server().with_args(&[("mount", "/mnt/disk9")]);
        kit.nixos_file(FILE, SCRIPT_FILE);
        assert_eq!(block_on(remove(&kit.ctx(&r))), 1);
        assert_eq!(written(&kit), two_and_one());
    }

    #[test]
    fn a_parity_disk_can_be_removed_even_when_it_is_the_last() {
        let kit = server().with_args(&[("mount", "/mnt/parity1")]);
        kit.nixos_file(FILE, SCRIPT_FILE);
        let r = runner();
        assert_eq!(block_on(remove(&kit.ctx(&r))), 0, "{}", kit.sink.text());
        assert!(written(&kit).parity.is_empty());
    }

    #[test]
    fn destroying_unmounts_then_removes_the_file_and_keeps_a_copy() {
        let kit = server();
        kit.nixos_file(FILE, SCRIPT_FILE);
        let r = FakeRunner::new()
            .ok("mountpoint", &[])
            .ok("umount", &[])
            .ok("git", &[]);
        assert_eq!(block_on(destroy(&kit.ctx(&r))), 0, "{}", kit.sink.text());
        let lines = r.lines();
        let union = pos(&lines, "umount /storage");
        assert!(union < pos(&lines, "umount /mnt/disk1"));
        assert!(lines.iter().any(|l| l == "umount /mnt/parity1"));
        assert!(!path(&kit.nixos).exists());
        assert_eq!(
            std::fs::read_to_string(kit.nixos.join("storage-pool.nix.bak")).unwrap(),
            SCRIPT_FILE
        );
        assert!(lines
            .iter()
            .any(|l| l.contains("rm -q --cached -- storage-pool.nix")));
        // Nothing is erased.
        assert!(!lines.iter().any(|l| l.starts_with("wipefs")));
    }

    #[test]
    fn a_pool_that_will_not_unmount_is_left_configured() {
        let kit = server();
        kit.nixos_file(FILE, SCRIPT_FILE);
        let r =
            FakeRunner::new()
                .ok("mountpoint", &[])
                .failing("umount", &[], 32, "target is busy");
        assert_eq!(block_on(destroy(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("lsof +D"));
        assert!(path(&kit.nixos).exists());
    }

    #[test]
    fn a_busy_disk_puts_back_what_destroy_already_unmounted() {
        let kit = server();
        kit.nixos_file(FILE, SCRIPT_FILE);
        // /storage and disk1 unmount, then disk2 is busy.
        let r = FakeRunner::new()
            .ok("mountpoint", &[])
            .failing("umount", &["/mnt/disk2"], 32, "target is busy")
            .ok("umount", &[])
            .ok("mount", &[]);
        assert_eq!(block_on(destroy(&kit.ctx(&r))), 1);
        let lines = r.lines();
        // Back in the opposite order: the disk before the pool built on it.
        let at = |line: &str| lines.iter().position(|l| l == line).unwrap();
        assert!(at("mount /mnt/disk1") < at("mount /storage"), "{lines:#?}");
        assert!(!lines.iter().any(|l| l == "mount /mnt/disk2"));
        assert!(path(&kit.nixos).exists());
    }

    #[test]
    fn a_content_disk_bigger_than_a_parity_disk_is_refused() {
        // Parity is sdd (2 TB); sdb is 8 TB. Content is sda.
        let mut pool = two_and_one();
        pool.content.pop();
        let kit = server().with_args(&[("role", "content"), ("disks", "ata-B")]);
        kit.nixos_file(FILE, &render(&pool, "now"));
        link_uuids(&kit, &[("AAAA-1", "sda"), ("CCCC-3", "sdd")]);
        let r = runner();
        assert_eq!(block_on(add(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("larger than a parity disk"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
        assert_eq!(written(&kit), pool);
    }

    #[test]
    fn a_mount_point_already_in_use_is_refused_before_anything_is_erased() {
        // Create: the host already mounts something at /mnt/disk1.
        let kit = server().with_args(&[("disks", "ata-A")]);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out(""))
            .ok("mountpoint", &["-q", "/mnt/disk1"]);
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("/mnt/disk1 is already mounted"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
        assert!(!path(&kit.nixos).exists());

        // Add: a disk that was removed from the pool is still mounted at /mnt/disk2, so
        // the next number is the same one.
        let mut pool = two_and_one();
        pool.content.pop();
        let kit = server().with_args(&[("role", "content"), ("disks", "ata-D")]);
        kit.nixos_file(FILE, &render(&pool, "now"));
        link_uuids(&kit, &[("AAAA-1", "sda"), ("CCCC-3", "sdb")]);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out(""))
            .ok("mountpoint", &["-q", "/mnt/disk2"]);
        assert_eq!(block_on(add(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("/mnt/disk2 is already mounted"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
        assert_eq!(written(&kit), pool);
    }

    #[test]
    fn a_mount_check_that_errors_stops_the_job_but_a_missing_directory_does_not() {
        // The directory exists and `mountpoint` fails for another reason: nothing is erased.
        let kit = server().with_args(&[("disks", "ata-A")]);
        std::fs::create_dir_all(kit.root.join("mnt/disk1")).unwrap();
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out(""))
            .failing("mountpoint", &[], 1, "Input/output error");
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert!(kit
            .sink
            .text()
            .contains("could not tell whether /mnt/disk1"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));

        // A probe killed by a signal is an error even when the directory is missing.
        let kit = server().with_args(&[("disks", "ata-A")]);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out(""))
            .failing("mountpoint", &[], -1, "");
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("could not tell whether"));
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));

        // The directory is not there yet (`mountpoint` exits 1 for that): not mounted.
        let kit = server().with_args(&[("disks", "ata-A")]);
        let r = FakeRunner::new()
            .on("lsblk", &[], out(LSBLK))
            .on("zpool", &["list"], out(""))
            .on_arg("blkid", "/dev/sda", out("UUID-SDA\n"))
            .failing("mountpoint", &[], 1, "No such file or directory")
            .ok("wipefs", &[])
            .ok("sgdisk", &[])
            .ok("mkfs.ext4", &[])
            .ok("mount", &[])
            .ok("nix-instantiate", &[])
            .ok("git", &[]);
        assert_eq!(block_on(create(&kit.ctx(&r))), 0, "{}", kit.sink.text());
    }

    #[test]
    fn sync_and_scrub_need_parity_the_tool_and_its_config() {
        let kit = server();
        let r = runner();
        // No pool.
        assert_eq!(block_on(sync(&kit.ctx(&r))), 1);
        // A pool with no parity.
        let mut bare = two_and_one();
        bare.parity.clear();
        kit.nixos_file(FILE, &render(&bare, "now"));
        assert_eq!(block_on(sync(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("no parity disks"));
        // Parity, but no snapraid.conf yet.
        kit.nixos_file(FILE, SCRIPT_FILE);
        assert_eq!(block_on(scrub(&kit.ctx(&r))), 1);
        assert!(kit.sink.text().contains("snapraid.conf"));
        assert!(!r.lines().iter().any(|l| l.starts_with("snapraid")));
    }

    #[test]
    fn sync_and_scrub_run_snapraid_and_report_its_failure() {
        let kit = server();
        kit.nixos_file(FILE, SCRIPT_FILE);
        std::fs::create_dir_all(kit.root.join("etc")).unwrap();
        std::fs::write(kit.root.join("etc/snapraid.conf"), "").unwrap();
        let r = runner();
        assert_eq!(block_on(sync(&kit.ctx(&r))), 0, "{}", kit.sink.text());
        assert_eq!(block_on(scrub(&kit.ctx(&r))), 0);
        assert!(r.lines().contains(&"snapraid sync".to_string()));
        assert!(r.lines().contains(&"snapraid scrub".to_string()));

        let failing = FakeRunner::new().failing("snapraid", &[], 1, "disk error");
        assert_eq!(block_on(sync(&kit.ctx(&failing))), 1);
        assert!(kit.sink.text().contains("exit code 1"));
    }

    #[test]
    fn a_host_that_is_not_a_server_is_refused() {
        let kit = server().with_args(&[("disks", "ata-A")]);
        kit.nixos_file("vexos-variant", "vexos-desktop-amd");
        let r = runner();
        assert_eq!(block_on(create(&kit.ctx(&r))), 1);
        assert!(!r.lines().iter().any(|l| l.starts_with("wipefs")));
    }
}
