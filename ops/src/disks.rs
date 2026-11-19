//! Which whole disks may be wiped.
//!
//! One definition, used twice: the GUI offers the disks that pass, and the daemon asks
//! again at the moment it is about to erase one, because a caller on the bus is not
//! trusted to have shown anyone the list. A disk passes only if nothing on it is mounted
//! (the root, `/boot`, `/nix`, swap and data all show up as a mount point on the disk or
//! one of its partitions), it is not part of an imported ZFS pool, and none of its
//! filesystems is one the configuration already uses.
//!
//! The parsing is pure and tested against real `lsblk` output; the functions that gather
//! the inputs are small and the daemon's go through the [`Runner`].

use crate::runner::{Cmd, Runner};
use crate::{capture, fail, zfs, Ctx};
use serde_json::Value;
use std::path::Path;

/// Only one job at a time may erase or format disks: two could pick the same one.
pub(crate) static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The `lsblk` columns this module reads. `NAME` must be among them: it is what makes the JSON
/// nest each disk's partitions and mappers under it. `KNAME` is the kernel name, which is what
/// the pool's device paths resolve to. The JSON nests each disk's partitions, so a
/// partition's mount point still reaches its disk.
pub const LSBLK_ARGS: [&str; 4] = [
    "-J",
    "-b",
    "-o",
    "NAME,KNAME,TYPE,SIZE,MODEL,TRAN,FSTYPE,UUID,MOUNTPOINTS",
];

/// One block device and what hangs off it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    pub kind: String,
    pub size: u64,
    pub model: String,
    pub transport: String,
    pub fstype: Option<String>,
    pub uuid: Option<String>,
    pub mounts: Vec<String>,
    pub children: Vec<Node>,
}

impl Node {
    /// This device and everything under it.
    pub fn all(&self) -> Vec<&Node> {
        let mut out = vec![self];
        for child in &self.children {
            out.extend(child.all());
        }
        out
    }
}

/// A disk that may be offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    /// The name under `/dev/disk/by-id`: stable, and what the daemon is told.
    pub by_id: String,
    /// The kernel name, `sda`.
    pub kernel: String,
    pub size: u64,
    pub model: String,
    pub transport: String,
    /// A warning shown beside the disk, for one that already holds something.
    pub note: Option<&'static str>,
}

impl Disk {
    pub fn path(&self) -> String {
        format!("/dev/disk/by-id/{}", self.by_id)
    }

    /// `500 GB`, as people read disk sizes.
    pub fn size_label(&self) -> String {
        size_label(self.size)
    }
}

/// Decimal units, the way disks are sold.
pub fn size_label(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Older `lsblk` prints sizes as strings, newer as numbers.
fn number(value: &Value, key: &str) -> u64 {
    match value.get(key) {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn node(value: &Value) -> Node {
    let mut mounts: Vec<String> = match value.get("mountpoints") {
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    // Before util-linux 2.37 there is one `mountpoint`.
    if let Some(one) = text(value, "mountpoint") {
        mounts.push(one);
    }
    mounts.retain(|m| !m.is_empty());
    Node {
        // The kernel name (`dm-0`), the same one `zpool status` paths resolve to; `name`
        // would be the mapper name (`cryptpool`).
        name: text(value, "kname")
            .or_else(|| text(value, "name"))
            .unwrap_or_default(),
        kind: text(value, "type").unwrap_or_default(),
        size: number(value, "size"),
        model: text(value, "model").unwrap_or_default(),
        transport: text(value, "tran").unwrap_or_default(),
        fstype: text(value, "fstype"),
        uuid: text(value, "uuid"),
        mounts,
        children: match value.get("children") {
            Some(Value::Array(list)) => list.iter().map(node).collect(),
            _ => Vec::new(),
        },
    }
}

/// The devices from `lsblk -J`.
pub fn parse_lsblk(json: &str) -> Result<Vec<Node>, String> {
    let doc: Value = serde_json::from_str(json).map_err(|e| format!("lsblk output: {e}"))?;
    let Some(Value::Array(devices)) = doc.get("blockdevices") else {
        return Err("lsblk output has no block devices".into());
    };
    Ok(devices.iter().map(node).collect())
}

/// A name that is safe to put in a path and an argv: what `/dev/disk/by-id` holds.
pub fn valid_id(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '+' | '@' | '=' | '-')
        })
}

/// The by-id links worth offering: a whole disk by a hardware name, not a partition.
fn offered(name: &str) -> bool {
    let partition = name.rfind("-part").is_some_and(|at| {
        name[at + 5..].chars().all(|c| c.is_ascii_digit()) && name.len() > at + 5
    });
    !partition
        && ["ata-", "nvme-", "scsi-", "wwn-"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// What the eligibility check needs to know about the machine.
pub struct Inputs<'a> {
    pub tree: &'a [Node],
    /// `(by-id name, kernel name)` for every link under `/dev/disk/by-id`.
    pub links: &'a [(String, String)],
    /// Kernel names of the disks or partitions that belong to an imported pool.
    pub pool_members: &'a [String],
    /// Filesystem UUIDs the configuration already uses.
    pub claimed: &'a [String],
}

/// The disks that may be erased, by by-id name.
pub fn eligible(inputs: &Inputs<'_>) -> Vec<Disk> {
    let mut out = Vec::new();
    for top in inputs.tree.iter().filter(|n| n.kind == "disk") {
        let family = top.all();
        let mounted = family.iter().any(|n| !n.mounts.is_empty());
        let in_pool = family.iter().any(|n| inputs.pool_members.contains(&n.name));
        let claimed = family.iter().any(|n| {
            n.uuid
                .as_ref()
                .is_some_and(|u| inputs.claimed.iter().any(|c| c.eq_ignore_ascii_case(u)))
        });
        if mounted || in_pool || claimed {
            continue;
        }
        // Several names point at one disk (`ata-…` and `wwn-…`): the first, alphabetically.
        let mut names: Vec<&str> = inputs
            .links
            .iter()
            .filter(|(id, kernel)| *kernel == top.name && offered(id) && valid_id(id))
            .map(|(id, _)| id.as_str())
            .collect();
        names.sort_unstable();
        let Some(by_id) = names.first() else { continue };
        // The kernel name goes into a path, so only the plain shapes are offered.
        if !top
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            continue;
        }

        let fstypes: Vec<&str> = family.iter().filter_map(|n| n.fstype.as_deref()).collect();
        let note = if fstypes.contains(&"zfs_member") {
            Some("has old ZFS labels")
        } else if !fstypes.is_empty() {
            Some("has an existing filesystem")
        } else {
            None
        };
        out.push(Disk {
            by_id: by_id.to_string(),
            kernel: top.name.clone(),
            size: top.size,
            model: top.model.clone(),
            transport: top.transport.clone(),
            note,
        });
    }
    out.sort_by(|a, b| a.by_id.cmp(&b.by_id));
    out
}

/// Every link under `<root>/dev/disk/by-id` with the kernel name it points at.
pub fn read_links(root: &Path) -> Vec<(String, String)> {
    let dir = root.join("dev/disk/by-id");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let id = entry.file_name().into_string().ok()?;
            let target = std::fs::canonicalize(entry.path()).ok()?;
            let kernel = target.file_name()?.to_str()?.to_string();
            Some((id, kernel))
        })
        .collect()
}

/// The kernel name a device path such as `/dev/disk/by-id/ata-X-part1` points at.
pub fn kernel_name(root: &Path, device: &str) -> Option<String> {
    if !device.starts_with('/') {
        return None;
    }
    let real = std::fs::canonicalize(root.join(device.trim_start_matches('/'))).ok()?;
    Some(real.file_name()?.to_str()?.to_string())
}

/// Which of `ids` (a comma-separated list of by-id names) are in `available`, in the
/// order asked. Each must be one disk that is still eligible, listed once.
pub fn pick<'a>(available: &'a [Disk], ids: &str) -> Result<Vec<&'a Disk>, String> {
    let mut chosen: Vec<&Disk> = Vec::new();
    for id in ids.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if !valid_id(id) {
            return Err(format!("`{id}` is not a disk name."));
        }
        let Some(disk) = available.iter().find(|d| d.by_id == id) else {
            return Err(format!(
                "`{id}` is not a disk that can be erased. It is mounted, part of a pool, in use by \
                 the configuration, or no longer there."
            ));
        };
        if chosen.iter().any(|d| d.kernel == disk.kernel) {
            return Err(format!("`{id}` was listed twice."));
        }
        chosen.push(disk);
    }
    if chosen.is_empty() {
        return Err("no disks were chosen.".into());
    }
    Ok(chosen)
}

/// The filesystem UUIDs the configuration refers to as `/dev/disk/by-uuid/<uuid>`, in any
/// top-level `.nix` file of `dir`. A disk the configuration mounts is not free even while it
/// happens to be unmounted.
pub fn config_claims(dir: &Path) -> Vec<String> {
    const MARK: &str = "/dev/disk/by-uuid/";
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "nix") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for piece in text.split(MARK).skip(1) {
            let uuid: String = piece
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            if !uuid.is_empty() {
                found.push(uuid);
            }
        }
    }
    found
}

/// The size in bytes of the device a path such as `/dev/disk/by-id/ata-X-part1` points at,
/// if it can be told (a pool disk that is missing has no path to follow).
pub async fn size_of<R: Runner>(ctx: &Ctx<'_, R>, device: &str) -> Result<Option<u64>, i32> {
    let Some(kernel) = kernel_name(ctx.root, device) else {
        return Ok(None);
    };
    let listed = capture(ctx, &Cmd::new("lsblk").args(LSBLK_ARGS)).await?;
    if !listed.success() {
        return Err(fail(
            ctx.sink,
            format!("lsblk failed: {}", listed.stderr.trim()),
        ));
    }
    let tree = parse_lsblk(&listed.stdout).map_err(|e| fail(ctx.sink, e))?;
    Ok(tree
        .iter()
        .flat_map(Node::all)
        .find(|n| n.name == kernel)
        .map(|n| n.size))
}

/// The disks that may be erased right now, asked of the machine itself.
///
/// `claimed` are filesystem UUIDs the configuration uses that may be unmounted at the
/// moment (a mergerfs branch whose disk is out).
pub async fn eligible_now<R: Runner>(
    ctx: &Ctx<'_, R>,
    claimed: &[String],
) -> Result<Vec<Disk>, i32> {
    let listed = capture(ctx, &Cmd::new("lsblk").args(LSBLK_ARGS)).await?;
    if !listed.success() {
        return Err(fail(
            ctx.sink,
            format!("lsblk failed: {}", listed.stderr.trim()),
        ));
    }
    let tree = parse_lsblk(&listed.stdout).map_err(|e| fail(ctx.sink, e))?;
    let links = read_links(ctx.root);
    let members = zfs::pool_members(ctx).await?;
    let mut claimed: Vec<String> = claimed.to_vec();
    claimed.extend(config_claims(ctx.nixos_dir));
    Ok(eligible(&Inputs {
        tree: &tree,
        links: &links,
        pool_members: &members,
        claimed: &claimed,
    }))
}

/// Erase the signatures on each disk: `wipefs -a`, then `sgdisk --zap-all`. A failure
/// stops everything, so a disk is never half-erased and then built on.
///
/// The disk is pinned by its kernel name, and before each command the whole eligibility
/// check runs again and must still give the same by-id name for the same kernel name. That
/// narrows the window in which something could claim the disk, or a name could be
/// retargeted, between the check and the erase; it cannot close it, because the two tools
/// open the device separately. `claimed` is as for [`eligible_now`].
pub async fn wipe<R: Runner>(
    ctx: &Ctx<'_, R>,
    disks: &[&Disk],
    claimed: &[String],
) -> Result<(), i32> {
    for disk in disks {
        let pinned = format!("/dev/{}", disk.kernel);
        for cmd in [
            Cmd::new("wipefs").arg("-a").arg(&pinned),
            Cmd::new("sgdisk").arg("--zap-all").arg(&pinned),
        ] {
            let still = eligible_now(ctx, claimed).await?;
            if !still
                .iter()
                .any(|d| d.by_id == disk.by_id && d.kernel == disk.kernel)
            {
                return Err(fail(
                    ctx.sink,
                    format!(
                        "{} is no longer a disk that can be erased (it changed while the job was running). Nothing more was erased.",
                        disk.by_id
                    ),
                ));
            }
            ctx.sink.info(&format!("$ {}", cmd.display()));
            let out = capture(ctx, &cmd).await?;
            if !out.success() {
                return Err(fail(
                    ctx.sink,
                    format!(
                        "`{}` failed on {}: {}",
                        cmd.program,
                        disk.by_id,
                        out.stderr.trim()
                    ),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host with the system on an NVMe drive (root on a partition, swap on another),
    /// a spare SATA disk with an old ZFS label, a blank one, one with a pool member
    /// partition, and a loop device.
    const LSBLK: &str = r#"{"blockdevices":[
      {"name":"nvme0n1","type":"disk","size":"500107862016","model":"WD Black","tran":"nvme",
       "fstype":null,"uuid":null,"mountpoints":[null],"children":[
         {"name":"nvme0n1p1","type":"part","size":536870912,"fstype":"vfat","uuid":"AAAA-1111","mountpoints":["/boot"]},
         {"name":"nvme0n1p2","type":"part","size":8589934592,"fstype":"swap","uuid":"s1","mountpoints":["[SWAP]"]},
         {"name":"nvme0n1p3","type":"part","size":490000000000,"fstype":"ext4","uuid":"r1","mountpoints":["/"]}]},
      {"name":"sda","type":"disk","size":4000787030016,"model":"ST4000","tran":"sata",
       "fstype":"zfs_member","uuid":"z1","mountpoints":[null]},
      {"name":"sdb","type":"disk","size":8001563222016,"model":"WDC WD80","tran":"sata",
       "fstype":null,"uuid":null,"mountpoints":[null]},
      {"name":"sdc","type":"disk","size":4000787030016,"model":"ST4000","tran":"sata","fstype":null,
       "uuid":null,"mountpoints":[null],"children":[
         {"name":"sdc1","type":"part","size":4000000000000,"fstype":"zfs_member","uuid":"z2","mountpoints":[null]}]},
      {"name":"sdd","type":"disk","size":1000204886016,"model":"Old","tran":"sata","fstype":"ext4",
       "uuid":"CLAIMED","mountpoints":[null]},
      {"name":"sde","type":"disk","size":1000204886016,"model":"Usb","tran":"usb","fstype":null,
       "uuid":null,"mountpoints":[null]},
      {"name":"loop0","type":"loop","size":100000,"fstype":"squashfs","mountpoints":["/snap/x"]}]}"#;

    fn links() -> Vec<(String, String)> {
        [
            ("nvme-WD_Black_SN750_1", "nvme0n1"),
            ("nvme-WD_Black_SN750_1-part1", "nvme0n1p1"),
            ("ata-ST4000_ZA1", "sda"),
            ("wwn-0x5000c500aaaa", "sda"),
            ("ata-WDC_WD80_B2", "sdb"),
            ("wwn-0x5000cca0bbbb", "sdb"),
            ("ata-ST4000_ZC3", "sdc"),
            ("ata-Old_D4", "sdd"),
            ("usb-Seagate_E5", "sde"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
    }

    fn run(members: &[&str], claimed: &[&str]) -> Vec<Disk> {
        let tree = parse_lsblk(LSBLK).unwrap();
        let members: Vec<String> = members.iter().map(|s| s.to_string()).collect();
        let claimed: Vec<String> = claimed.iter().map(|s| s.to_string()).collect();
        eligible(&Inputs {
            tree: &tree,
            links: &links(),
            pool_members: &members,
            claimed: &claimed,
        })
    }

    fn ids(disks: &[Disk]) -> Vec<&str> {
        disks.iter().map(|d| d.by_id.as_str()).collect()
    }

    #[test]
    fn a_disk_with_anything_mounted_on_it_is_never_offered() {
        let disks = run(&[], &[]);
        // The system disk carries /boot, swap and /, on three partitions.
        assert!(!ids(&disks).iter().any(|id| id.starts_with("nvme-")));
        assert!(!disks.iter().any(|d| d.kernel == "nvme0n1"));
    }

    #[test]
    fn only_hardware_named_whole_disks_are_offered_once_each() {
        let disks = run(&[], &["CLAIMED"]);
        // sda has two names: the alphabetically first wins. The USB disk has no accepted
        // name, and the loop device is not a disk.
        assert_eq!(
            ids(&disks),
            ["ata-ST4000_ZA1", "ata-ST4000_ZC3", "ata-WDC_WD80_B2"]
        );
    }

    #[test]
    fn a_disk_in_an_imported_pool_is_hidden_even_by_one_partition() {
        let disks = run(&["sda", "sdc1"], &["CLAIMED"]);
        assert_eq!(ids(&disks), ["ata-WDC_WD80_B2"]);
    }

    #[test]
    fn a_disk_the_configuration_already_uses_is_hidden_while_unmounted() {
        let with = run(&[], &[]);
        assert!(ids(&with).contains(&"ata-Old_D4"));
        let without = run(&[], &["CLAIMED"]);
        assert!(!ids(&without).contains(&"ata-Old_D4"));
    }

    #[test]
    fn a_disk_that_holds_something_says_so() {
        let disks = run(&[], &["CLAIMED"]);
        let note = |id: &str| disks.iter().find(|d| d.by_id == id).unwrap().note;
        assert_eq!(note("ata-ST4000_ZA1"), Some("has old ZFS labels"));
        assert_eq!(note("ata-ST4000_ZC3"), Some("has old ZFS labels"));
        assert_eq!(note("ata-WDC_WD80_B2"), None);
        let all = run(&[], &[]);
        let old = all.iter().find(|d| d.by_id == "ata-Old_D4").unwrap();
        assert_eq!(old.note, Some("has an existing filesystem"));
    }

    #[test]
    fn sizes_are_read_as_numbers_or_strings_and_shown_the_way_disks_are_sold() {
        let disks = run(&[], &["CLAIMED"]);
        let b = disks.iter().find(|d| d.by_id == "ata-WDC_WD80_B2").unwrap();
        assert_eq!(b.size, 8_001_563_222_016);
        assert_eq!(b.size_label(), "8.0 TB");
        assert_eq!(size_label(500_107_862_016), "500 GB");
        assert_eq!(size_label(512), "512 B");
        assert_eq!(size_label(1_000_204_886_016), "1.0 TB");
    }

    #[test]
    fn the_older_single_mountpoint_column_counts_too() {
        let tree = parse_lsblk(
            r#"{"blockdevices":[{"name":"sda","type":"disk","size":1,"mountpoint":"/data"}]}"#,
        )
        .unwrap();
        let none: Vec<String> = Vec::new();
        let disks = eligible(&Inputs {
            tree: &tree,
            links: &[("ata-X".to_string(), "sda".to_string())],
            pool_members: &none,
            claimed: &none,
        });
        assert!(disks.is_empty());
    }

    #[test]
    fn a_pool_on_a_mapper_device_is_matched_by_kernel_name() {
        // `name` is the mapper name; `kname` is what zpool's path resolves to.
        let tree = parse_lsblk(
            r#"{"blockdevices":[{"name":"sdq","kname":"sdq","type":"disk","size":1,
               "mountpoints":[null],"children":[{"name":"cryptpool","kname":"dm-0","type":"crypt",
               "size":1,"mountpoints":[null]}]}]}"#,
        )
        .unwrap();
        let links = [("ata-Q".to_string(), "sdq".to_string())];
        let none: Vec<String> = Vec::new();
        let members = vec!["dm-0".to_string()];
        let with = |pool: &[String]| {
            eligible(&Inputs {
                tree: &tree,
                links: &links,
                pool_members: pool,
                claimed: &none,
            })
        };
        assert!(with(&members).is_empty());
        assert_eq!(with(&none).len(), 1);
    }

    #[test]
    fn the_configurations_uuids_are_collected_from_nix_files_only() {
        let dir = crate::fsutil::testing::TempDir::new();
        dir.write(
            "hardware-configuration.nix",
            "{ fileSystems.\"/data\".device = \"/dev/disk/by-uuid/1234-ABCD\"; \n  swapDevices = [ { device = \"/dev/disk/by-uuid/dead-beef-0\"; } ]; }\n",
        );
        dir.write("notes.txt", "/dev/disk/by-uuid/ignored");
        let mut found = config_claims(dir.path());
        found.sort();
        assert_eq!(found, ["1234-ABCD", "dead-beef-0"]);
        assert!(config_claims(&dir.join("missing")).is_empty());
    }

    #[test]
    fn unreadable_lsblk_output_is_an_error_not_an_empty_list() {
        assert!(parse_lsblk("nope").is_err());
        assert!(parse_lsblk("{}").is_err());
    }

    #[test]
    fn partition_links_and_foreign_prefixes_are_not_offered() {
        assert!(offered("ata-X"));
        assert!(offered("nvme-eui.0025"));
        assert!(!offered("ata-X-part1"));
        assert!(!offered("nvme-X-part12"));
        assert!(!offered("usb-X"));
        assert!(!offered("dm-name-vg-root"));
        // A name that merely contains "-part" is a disk.
        assert!(offered("ata-Apart-part-store"));
    }

    #[test]
    fn a_pick_must_be_eligible_unique_and_well_formed() {
        let disks = run(&[], &["CLAIMED"]);
        let got = pick(&disks, "ata-ST4000_ZA1, ata-WDC_WD80_B2").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].kernel, "sdb");
        for (bad, why) in [
            ("ata-ST4000_ZA1,ata-ST4000_ZA1", "twice"),
            ("nvme-WD_Black_SN750_1", "not a disk that can be erased"),
            ("../../dev/sda", "not a disk name"),
            ("ata-X;rm", "not a disk name"),
            ("", "no disks"),
            (" , ", "no disks"),
        ] {
            let err = pick(&disks, bad).unwrap_err();
            assert!(err.contains(why), "{bad}: {err}");
        }
    }

    #[test]
    fn links_are_read_from_the_disk_directory() {
        let dir = crate::fsutil::testing::TempDir::new();
        std::fs::create_dir_all(dir.join("dev/disk/by-id")).unwrap();
        std::fs::write(dir.join("dev/sdb"), "").unwrap();
        std::os::unix::fs::symlink(
            dir.join("dev/sdb"),
            dir.join("dev/disk/by-id/ata-WDC_WD80_B2"),
        )
        .unwrap();
        assert_eq!(
            read_links(dir.path()),
            [("ata-WDC_WD80_B2".to_string(), "sdb".to_string())]
        );
        assert_eq!(
            kernel_name(dir.path(), "/dev/disk/by-id/ata-WDC_WD80_B2").as_deref(),
            Some("sdb")
        );
        assert_eq!(kernel_name(dir.path(), "1234567890"), None);
        assert_eq!(kernel_name(dir.path(), "/dev/disk/by-id/missing"), None);
    }
}
