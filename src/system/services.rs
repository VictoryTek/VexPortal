//! Reading how a service is doing, without privilege.
//!
//! systemd answers `systemctl show` to anyone, and the health addresses are on this
//! machine, so none of this needs the daemon. Both run as async subprocesses; the window
//! never waits on them. What the answers mean is `vexportal_ops::services`, which is tested
//! without a systemd.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;
use vexportal_ops::services::{parse_show, probe_args, reach, show_args, Reach, UnitInfo};

/// A probe that has not finished by now is not going to. It is killed so a hung command
/// cannot hold a page's status forever.
const DEADLINE: Duration = Duration::from_secs(8);

/// Run a command and return what it printed, or `None` if it could not start, did not
/// finish in time, or its output was unreadable.
async fn capture(program: &str, args: &[String]) -> Option<String> {
    let mut argv: Vec<&std::ffi::OsStr> = vec![std::ffi::OsStr::new(program)];
    argv.extend(args.iter().map(std::ffi::OsStr::new));
    let process = gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
    )
    .ok()?;

    let finished = Rc::new(Cell::new(false));
    let timed_out = Rc::new(Cell::new(false));
    glib::spawn_future_local({
        let process = process.clone();
        let finished = finished.clone();
        let timed_out = timed_out.clone();
        async move {
            glib::timeout_future(DEADLINE).await;
            if !finished.get() {
                timed_out.set(true);
                process.force_exit();
            }
        }
    });

    let answered = process.communicate_utf8_future(None).await;
    finished.set(true);
    if timed_out.get() {
        return None;
    }
    let (stdout, _) = answered.ok()?;
    stdout.map(|s| s.to_string())
}

/// What systemd says about each of these units (`<name>.service`). A unit that does not
/// exist comes back as not found. Empty if systemd could not be asked.
pub async fn units(names: &[String]) -> Vec<UnitInfo> {
    if names.is_empty() {
        return Vec::new();
    }
    capture("systemctl", &show_args(names))
        .await
        .map(|out| parse_show(&out))
        .unwrap_or_default()
}

/// Whether an address answers. `None` if there was no way to ask (no `curl`).
pub async fn probe(url: &str) -> Option<Reach> {
    glib::find_program_in_path("curl")?;
    Some(
        capture("curl", &probe_args(url))
            .await
            .map_or(Reach::Unreachable, |out| reach(&out)),
    )
}

/// The names `zpool list -H -o name` prints; empty if ZFS is not installed.
pub async fn zfs_pools() -> Vec<String> {
    if glib::find_program_in_path("zpool").is_none() {
        return Vec::new();
    }
    capture(
        "zpool",
        &["list".into(), "-H".into(), "-o".into(), "name".into()],
    )
    .await
    .map(|out| {
        out.lines()
            .map(str::to_string)
            .filter(|l| !l.is_empty())
            .collect()
    })
    .unwrap_or_default()
}
