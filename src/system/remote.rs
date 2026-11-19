//! Finding servers and shares to attach, and seeing how an attached share is doing.
//!
//! All of it runs as the user, like the tools do in a terminal: `avahi-browse`,
//! `showmount` and `smbclient` only ask questions of the network, and `findmnt` and
//! `systemctl` read this machine. What their output means is `vexportal_ops::remote`,
//! which is tested without a network. A tool that is not installed yet, or does not
//! answer in time, gives an empty answer and the form falls back to typing.

use crate::system::services::capture_within;
use std::time::Duration;
use vexportal_ops::remote::{parse_avahi, parse_exports, parse_shares, Host};

/// A scan of the local network ends on its own in a second or two; this is the limit.
const SCAN: Duration = Duration::from_secs(12);
/// Listing a share can wait on a slow NAS.
const LIST: Duration = Duration::from_secs(30);
/// Asking this machine something.
const LOCAL: Duration = Duration::from_secs(5);

fn installed(tool: &str) -> bool {
    glib::find_program_in_path(tool).is_some()
}

/// This machine's own IPv4 addresses, so it does not offer itself.
async fn own_addresses() -> Vec<String> {
    capture_within(
        "ip",
        &["-4".into(), "-o".into(), "addr".into(), "show".into()],
        LOCAL,
    )
    .await
    .map(|out| {
        out.lines()
            .filter_map(|l| l.split_whitespace().nth(3))
            .map(|a| a.split('/').next().unwrap_or(a).to_string())
            .collect()
    })
    .unwrap_or_default()
}

/// Servers on the local network that offer this protocol (`nfs` or `cifs`).
pub async fn find_hosts(protocol: &str) -> Vec<Host> {
    if !installed("avahi-browse") {
        return Vec::new();
    }
    let service = if protocol == "nfs" {
        "_nfs._tcp"
    } else {
        "_smb._tcp"
    };
    let own = own_addresses().await;
    capture_within(
        "avahi-browse",
        &["-t".into(), "-r".into(), "-p".into(), service.into()],
        SCAN,
    )
    .await
    .map(|out| parse_avahi(&out, &own))
    .unwrap_or_default()
}

/// The NFS exports a server offers.
pub async fn nfs_exports(server: &str) -> Vec<String> {
    if !installed("showmount") {
        return Vec::new();
    }
    capture_within(
        "showmount",
        &["-e".into(), "--no-headers".into(), server.into()],
        LIST,
    )
    .await
    .map(|out| parse_exports(&out))
    .unwrap_or_default()
}

/// The SMB shares a server offers to this login. The password goes through a private
/// file in the user's runtime folder (never the command line), removed straight after.
pub async fn smb_shares(server: &str, user: &str, password: &str) -> Vec<String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if !installed("smbclient") {
        return Vec::new();
    }
    let dir = glib::user_runtime_dir();
    let file = dir.join(format!("vexportal-smb-{}", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&file)
        .and_then(|mut f| {
            f.write_all(format!("username = {user}\npassword = {password}\n").as_bytes())
        });
    if written.is_err() {
        let _ = std::fs::remove_file(&file);
        return Vec::new();
    }
    let shares = capture_within(
        "smbclient",
        &[
            "-L".into(),
            format!("//{server}"),
            "-g".into(),
            "-A".into(),
            file.to_string_lossy().into_owned(),
        ],
        LIST,
    )
    .await
    .map(|out| parse_shares(&out))
    .unwrap_or_default();
    let _ = std::fs::remove_file(&file);
    shares
}

/// How an attached share is doing right now, found without touching the mount point: a
/// read of an automount path would mount it, and block on an offline server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountState {
    Mounted,
    /// The mount unit is waiting for the first use.
    Ready,
    /// Not in the running system yet: needs a rebuild.
    NotApplied,
}

impl MountState {
    pub fn label(self) -> &'static str {
        match self {
            MountState::Mounted => "Mounted",
            MountState::Ready => "Ready (mounts on first use)",
            MountState::NotApplied => "Not applied yet (rebuild)",
        }
    }
}

pub async fn mount_state(mount: &str) -> MountState {
    let findmnt = capture_within(
        "findmnt",
        &[
            "-rn".into(),
            "-t".into(),
            "nfs,nfs4,cifs".into(),
            "--mountpoint".into(),
            mount.into(),
        ],
        LOCAL,
    )
    .await;
    if findmnt.is_some_and(|out| !out.trim().is_empty()) {
        return MountState::Mounted;
    }
    let unit = capture_within(
        "systemd-escape",
        &["-p".into(), "--suffix=automount".into(), mount.into()],
        LOCAL,
    )
    .await
    .map(|u| u.trim().to_string())
    .filter(|u| u.ends_with(".automount"));
    if let Some(unit) = unit {
        let active = capture_within("systemctl", &["is-active".into(), unit], LOCAL).await;
        if active.is_some_and(|a| a.trim() == "active") {
            return MountState::Ready;
        }
    }
    MountState::NotApplied
}
