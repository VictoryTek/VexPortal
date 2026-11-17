//! Talking to systemd over D-Bus instead of running `systemctl`.
//!
//! `systemctl start` is a thin client for the `org.freedesktop.systemd1.Manager` API.
//! Calling that directly gives structured results — a job outcome, a unit's states — in
//! place of parsing text, and no process to spawn. Used by the operations that start,
//! stop and inspect units (VPN, backups, services).

use futures_util::StreamExt;
use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait Manager {
    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn restart_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn reset_failed_unit(&self, name: &str) -> zbus::Result<()>;
    /// Unlike `GetUnit`, loads the unit if needed, so a unit that exists on disk but
    /// has never run is still found. A missing one comes back with `LoadState` of
    /// `not-found`.
    fn load_unit(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
    /// systemd only sends `JobRemoved` to clients that have subscribed.
    fn subscribe(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn job_removed(
        &self,
        id: u32,
        job: OwnedObjectPath,
        unit: String,
        result: String,
    ) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1"
)]
trait Unit {
    #[zbus(property)]
    fn load_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn sub_state(&self) -> zbus::Result<String>;
}

#[derive(Debug, thiserror::Error)]
pub enum SystemdError {
    #[error("`{0}` is not a unit name VexPortal will pass to systemd")]
    BadUnit(String),
    #[error("systemd: {0}")]
    Bus(#[from] zbus::Error),
    #[error("the job for {unit} ended without a result")]
    NoResult { unit: String },
}

/// How a start, stop or restart job ended, as systemd reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobResult {
    Done,
    Canceled,
    Timeout,
    Failed,
    Dependency,
    Skipped,
    /// A result this code does not know. Kept as text rather than guessed at.
    Other(String),
}

impl JobResult {
    pub fn parse(result: &str) -> Self {
        match result {
            "done" => Self::Done,
            "canceled" => Self::Canceled,
            "timeout" => Self::Timeout,
            "failed" => Self::Failed,
            "dependency" => Self::Dependency,
            "skipped" => Self::Skipped,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self, Self::Done)
    }
}

/// A unit's three states, as the dashboard reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitState {
    pub load: String,
    pub active: String,
    pub sub: String,
}

impl UnitState {
    pub fn exists(&self) -> bool {
        self.load != "not-found"
    }

    /// A oneshot unit that is still doing its work is `activating`, and is running.
    pub fn is_running(&self) -> bool {
        matches!(self.active.as_str(), "active" | "activating" | "reloading")
    }
}

/// Unit types the operations deal with. Anything else is a request to do something
/// this daemon has no reason to.
const UNIT_SUFFIXES: [&str; 5] = [".service", ".timer", ".target", ".mount", ".socket"];

/// A unit name is passed to systemd as one D-Bus argument, so nothing here can become a
/// command line, but a name is still held to the shape of a real one.
pub fn check_unit(name: &str) -> Result<&str, SystemdError> {
    let shaped = !name.is_empty()
        && name.len() <= 256
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | ':' | '\\'));
    if shaped && UNIT_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        Ok(name)
    } else {
        Err(SystemdError::BadUnit(name.to_string()))
    }
}

pub struct Systemd {
    connection: Connection,
    manager: ManagerProxy<'static>,
}

impl Systemd {
    pub async fn new(connection: &Connection) -> Result<Self, SystemdError> {
        let manager = ManagerProxy::new(connection).await?;
        Ok(Self {
            connection: connection.clone(),
            manager,
        })
    }

    pub async fn start(&self, unit: &str) -> Result<JobResult, SystemdError> {
        self.job(unit, Job::Start).await
    }

    pub async fn stop(&self, unit: &str) -> Result<JobResult, SystemdError> {
        self.job(unit, Job::Stop).await
    }

    pub async fn restart(&self, unit: &str) -> Result<JobResult, SystemdError> {
        self.job(unit, Job::Restart).await
    }

    pub async fn reset_failed(&self, unit: &str) -> Result<(), SystemdError> {
        Ok(self.manager.reset_failed_unit(check_unit(unit)?).await?)
    }

    pub async fn state(&self, unit: &str) -> Result<UnitState, SystemdError> {
        let path = self.manager.load_unit(check_unit(unit)?).await?;
        // Read fresh: a cached state is exactly the wrong answer for "is it running".
        let proxy = UnitProxy::builder(&self.connection)
            .path(path)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        Ok(UnitState {
            load: proxy.load_state().await?,
            active: proxy.active_state().await?,
            sub: proxy.sub_state().await?,
        })
    }

    /// systemd allows one subscription per bus connection and answers a second
    /// `Subscribe` with `AlreadySubscribed`. This connection is shared by every job, so
    /// that answer means "already done".
    async fn subscribe(&self) -> Result<(), SystemdError> {
        match self.manager.subscribe().await {
            Err(e) if is_already_subscribed(&e) => Ok(()),
            other => Ok(other?),
        }
    }

    /// Queue a job and wait for it to finish.
    async fn job(&self, unit: &str, job: Job) -> Result<JobResult, SystemdError> {
        let unit = check_unit(unit)?;
        // Listen before asking, or a quick job could finish before the listener exists
        // and the wait below would never end.
        self.subscribe().await?;
        let mut removed = self.manager.receive_job_removed().await?;

        // "replace" is what `systemctl start` uses: a queued job for the unit that
        // conflicts with this one is replaced.
        let path = match job {
            Job::Start => self.manager.start_unit(unit, "replace").await?,
            Job::Stop => self.manager.stop_unit(unit, "replace").await?,
            Job::Restart => self.manager.restart_unit(unit, "replace").await?,
        };

        while let Some(signal) = removed.next().await {
            let args = signal.args()?;
            if *args.job() == path {
                return Ok(JobResult::parse(args.result()));
            }
        }
        Err(SystemdError::NoResult {
            unit: unit.to_string(),
        })
    }
}

fn is_already_subscribed(error: &zbus::Error) -> bool {
    matches!(
        error,
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "org.freedesktop.systemd1.AlreadySubscribed"
    )
}

#[derive(Clone, Copy)]
enum Job {
    Start,
    Stop,
    Restart,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_unit_names_pass() {
        for name in [
            "vexos-killswitch.service",
            "restic-backups-vexos.timer",
            "container@app.service",
            "mnt-pool.mount",
            "multi-user.target",
            // systemd escapes a `-` inside a mount path as `\x2d`.
            "mnt-my\\x2dpool.mount",
        ] {
            assert_eq!(check_unit(name).unwrap(), name, "{name}");
        }
    }

    #[test]
    fn anything_else_is_refused() {
        for name in [
            "",
            "plex",
            "plex.service; reboot",
            "../../etc/passwd.service",
            "a b.service",
            ".hidden.service",
            "x.service\n",
            "plex.path",
            "plex.service$(id)",
        ] {
            assert!(check_unit(name).is_err(), "{name:?} should be refused");
        }
    }

    #[test]
    fn only_already_subscribed_is_swallowed() {
        let method_error = |name: &str| {
            zbus::Error::MethodError(
                zbus::names::OwnedErrorName::try_from(name.to_string()).unwrap(),
                None,
                zbus::message::Message::method_call("/", "M")
                    .unwrap()
                    .build(&())
                    .unwrap(),
            )
        };
        assert!(is_already_subscribed(&method_error(
            "org.freedesktop.systemd1.AlreadySubscribed"
        )));
        assert!(!is_already_subscribed(&method_error(
            "org.freedesktop.systemd1.NoSuchUnit"
        )));
        assert!(!is_already_subscribed(&zbus::Error::InvalidReply));
    }

    #[test]
    fn job_results_map_and_unknown_ones_survive() {
        assert!(JobResult::parse("done").is_success());
        assert_eq!(JobResult::parse("failed"), JobResult::Failed);
        assert_eq!(JobResult::parse("dependency"), JobResult::Dependency);
        assert_eq!(
            JobResult::parse("invalid"),
            JobResult::Other("invalid".into())
        );
        assert!(!JobResult::parse("timeout").is_success());
    }

    #[test]
    fn a_unit_doing_oneshot_work_counts_as_running() {
        let state = |active: &str| UnitState {
            load: "loaded".into(),
            active: active.into(),
            sub: String::new(),
        };
        assert!(state("active").is_running());
        assert!(state("activating").is_running());
        assert!(!state("inactive").is_running());
        assert!(!state("failed").is_running());
        let missing = UnitState {
            load: "not-found".into(),
            active: "inactive".into(),
            sub: "dead".into(),
        };
        assert!(!missing.exists());
    }
}
