//! Looking at disks and ZFS pools without privilege.
//!
//! `lsblk`, `zpool list` and `zpool status` answer an ordinary user, so the pickers and the
//! pool list need no daemon. The list of disks that may be erased is only a courtesy: the
//! daemon works it out again when it is about to erase anything. What the tools' output
//! means is `vexportal_ops::{disks, zfs}`, tested without a machine.

use crate::system::services::capture_within;
use std::path::Path;
use std::time::Duration;
use vexportal_ops::disks::{self, Disk, Inputs, LSBLK_ARGS};
use vexportal_ops::zfs::{self, Leaf, PoolRow, LIST_ARGS};

/// Asking this machine something.
const LOCAL: Duration = Duration::from_secs(8);

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// The imported pools. `None` if ZFS is not installed (or did not answer).
pub async fn pools() -> Option<Vec<PoolRow>> {
    let out = capture_within("zpool", &owned(&LIST_ARGS), LOCAL).await?;
    Some(zfs::parse_pool_list(&out))
}

/// The disks of one pool.
pub async fn leaves(pool: &str) -> Vec<Leaf> {
    capture_within("zpool", &owned(&["status", "-P", pool]), LOCAL)
        .await
        .map(|out| zfs::leaves(&out, pool))
        .unwrap_or_default()
}

/// The disks that may be erased right now, or why that could not be worked out.
pub async fn eligible() -> Result<Vec<Disk>, String> {
    let listed = capture_within("lsblk", &owned(&LSBLK_ARGS), LOCAL)
        .await
        .ok_or("lsblk did not answer, so the disks cannot be listed.")?;
    let tree = disks::parse_lsblk(&listed)?;

    let mut members = Vec::new();
    for pool in pools().await.unwrap_or_default() {
        for leaf in leaves(&pool.name).await {
            members.extend(disks::kernel_name(Path::new("/"), &leaf.device));
        }
    }
    let links = disks::read_links(Path::new("/"));
    let claimed = disks::config_claims(Path::new("/etc/nixos"));
    Ok(disks::eligible(&Inputs {
        tree: &tree,
        links: &links,
        pool_members: &members,
        claimed: &claimed,
    }))
}
