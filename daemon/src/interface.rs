//! The D-Bus surface: `io.github.vexportal.Daemon1`.
//!
//! `RunOperation` starts a job, `Query` answers a read-only question, `Cancel` stops a
//! job, and two signals carry a job's output and its end. Everything the GUI can read
//! without privileges — the variant, the current generation — it reads for itself;
//! asking a root daemon to `cat` a world-readable file would be surface for no benefit.

use crate::audit;
use crate::auth;
use crate::cancel::JobHandle;
use crate::config::Config;
use crate::executor;
use crate::lifecycle::IdleTracker;
use crate::native;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;
use vexportal_catalog::validate::{self, ValidationError};
use vexportal_catalog::{Backend, Catalog};
use zbus::{fdo, interface, object_server::SignalEmitter};

/// A rebuild, an update and a kernel build is already an unusual amount to have in
/// flight; past that, a caller is looping.
const MAX_CONCURRENT_JOBS: usize = 3;

pub struct Daemon {
    catalog: Catalog,
    config: Config,
    jobs: Arc<Mutex<HashMap<String, JobHandle>>>,
    idle: Arc<Mutex<IdleTracker>>,
}

impl Daemon {
    pub fn new(catalog: Catalog, config: Config, idle: Arc<Mutex<IdleTracker>>) -> Self {
        Self {
            catalog,
            config,
            jobs: Arc::new(Mutex::new(HashMap::new())),
            idle,
        }
    }

    /// Drop handles for jobs that have finished, so `ActiveJobs` and the concurrency
    /// limit reflect what is actually running.
    async fn reap(&self) {
        self.jobs.lock().await.retain(|_, job| job.is_running());
    }
}

impl Daemon {
    /// Validate a request, authorise it, and start it as a job.
    async fn start(
        &self,
        action_id: &str,
        args: HashMap<String, String>,
        header: &zbus::message::Header<'_>,
        connection: &zbus::Connection,
    ) -> fdo::Result<String> {
        let caller = header
            .sender()
            .ok_or_else(|| fdo::Error::Failed("D-Bus message has no sender".into()))?
            .to_string();

        // Validate before authenticating: a malformed request should be rejected
        // outright rather than after making someone type a password for it.
        let answers: BTreeMap<String, String> = args.into_iter().collect();
        let invocation = validate::build(&self.catalog, action_id, &answers).map_err(|e| {
            audit::rejected(&caller, action_id, &e.to_string());
            match e {
                ValidationError::UnknownAction(_) | ValidationError::TerminalOnly(_) => {
                    fdo::Error::AccessDenied(e.to_string())
                }
                _ => fdo::Error::InvalidArgs(e.to_string()),
            }
        })?;

        // A native action must have a handler. Refuse here, before a polkit prompt, so a
        // catalog entry that got ahead of its implementation is a plain error.
        if invocation.backend == Backend::Native && !vexportal_ops::is_native(action_id) {
            audit::rejected(&caller, action_id, "native action without a handler");
            return Err(fdo::Error::NotSupported(format!(
                "`{action_id}` is not implemented yet"
            )));
        }

        for path in &invocation.must_exist {
            if !std::path::Path::new(path).exists() {
                audit::rejected(&caller, action_id, "a required file does not exist");
                return Err(fdo::Error::InvalidArgs(format!("`{path}` does not exist")));
            }
        }

        self.reap().await;
        if self.jobs.lock().await.len() >= MAX_CONCURRENT_JOBS {
            return Err(fdo::Error::LimitsExceeded(format!(
                "already running {MAX_CONCURRENT_JOBS} operations"
            )));
        }

        let action = invocation.risk.polkit_action();
        if !auth::check(connection, &caller, action)
            .await
            .map_err(fdo::Error::Failed)?
        {
            audit::rejected(&caller, action_id, "polkit denied the request");
            return Err(fdo::Error::AccessDenied(
                "Not authorized to run this operation".into(),
            ));
        }

        let job_id = Uuid::new_v4().to_string();
        let uid = uid_of(connection, &caller).await;
        audit::started(&job_id, &caller, uid, &invocation);
        self.idle.lock().await.mark_active();

        let handle = match invocation.backend {
            Backend::Native => native::spawn_job(
                job_id.clone(),
                invocation,
                &self.config,
                connection.clone(),
                uid,
            ),
            Backend::Recipe => {
                executor::spawn(job_id.clone(), invocation, &self.config, connection.clone())
            }
        };
        self.jobs.lock().await.insert(job_id.clone(), handle);

        Ok(job_id)
    }
}

#[interface(name = "io.github.vexportal.Daemon1")]
impl Daemon {
    /// Start an action by its catalog id. The catalog says whether it runs as a native
    /// operation or, until every action is native, as a `just` recipe.
    ///
    /// Returns a job id; output arrives as `JobOutput` signals and the result as
    /// `JobFinished`.
    async fn run_operation(
        &self,
        op_id: &str,
        args: HashMap<String, String>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<String> {
        self.start(op_id, args, &header, connection).await
    }

    /// The same as `RunOperation`, under the name earlier GUIs call. Removed with the
    /// recipe backend.
    async fn run_recipe(
        &self,
        recipe: &str,
        args: HashMap<String, String>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<String> {
        self.start(recipe, args, &header, connection).await
    }

    /// Answer a read-only question about the host with JSON. Changes nothing, so it needs
    /// only the read-only polkit action, which asks for no password.
    async fn query(
        &self,
        op_id: &str,
        args: HashMap<String, String>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<String> {
        let caller = header
            .sender()
            .ok_or_else(|| fdo::Error::Failed("D-Bus message has no sender".into()))?
            .to_string();

        if !vexportal_ops::is_query(op_id) {
            audit::rejected(&caller, op_id, "not a query");
            return Err(fdo::Error::AccessDenied(format!(
                "`{op_id}` is not a query VexPortal knows about"
            )));
        }
        if !auth::check(
            connection,
            &caller,
            vexportal_catalog::Risk::Safe.polkit_action(),
        )
        .await
        .map_err(fdo::Error::Failed)?
        {
            audit::rejected(&caller, op_id, "polkit denied the request");
            return Err(fdo::Error::AccessDenied(
                "Not authorized to read this".into(),
            ));
        }

        let args: vexportal_ops::Args = args.into_iter().collect();
        vexportal_ops::query(op_id, &self.config.nixos_dir, &args)
            .map_err(|e| fdo::Error::Failed(e.to_string()))
    }

    /// Stop a running job. Returns false if it had already finished.
    async fn cancel(
        &self,
        job_id: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<bool> {
        let caller = header
            .sender()
            .ok_or_else(|| fdo::Error::Failed("D-Bus message has no sender".into()))?
            .to_string();

        if !auth::check(connection, &caller, "io.github.vexportal.cancel")
            .await
            .map_err(fdo::Error::Failed)?
        {
            return Err(fdo::Error::AccessDenied("Not authorized to cancel".into()));
        }

        let jobs = self.jobs.lock().await;
        Ok(jobs.get(job_id).is_some_and(JobHandle::request_cancel))
    }

    /// Running jobs as `(job_id, recipe)`, so a GUI that was restarted can reattach.
    async fn list_jobs(&self) -> fdo::Result<Vec<(String, String)>> {
        self.reap().await;
        Ok(self
            .jobs
            .lock()
            .await
            .values()
            .map(|job| (job.job_id.clone(), job.recipe.clone()))
            .collect())
    }

    /// One line of recipe output. `stream` is 0 for stdout and 1 for stderr.
    #[zbus(signal)]
    pub async fn job_output(
        emitter: &SignalEmitter<'_>,
        job_id: &str,
        stream: u32,
        line: &str,
    ) -> zbus::Result<()>;

    /// A job has ended. `exit_code` is the recipe's, or -1 if it could not be started.
    #[zbus(signal)]
    pub async fn job_finished(
        emitter: &SignalEmitter<'_>,
        job_id: &str,
        exit_code: i32,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    /// The justfile this daemon runs, so the GUI can read the same one for its
    /// dropdowns and drift check instead of guessing.
    #[zbus(property)]
    fn justfile(&self) -> String {
        self.config.justfile.display().to_string()
    }

    #[zbus(property)]
    pub async fn active_jobs(&self) -> u32 {
        self.reap().await;
        self.jobs.lock().await.len() as u32
    }
}

/// Best-effort uid lookup for the audit record. A failure here must not block the
/// operation — polkit has already decided the real question.
async fn uid_of(connection: &zbus::Connection, caller: &str) -> Option<u32> {
    let proxy = fdo::DBusProxy::new(connection).await.ok()?;
    proxy
        .get_connection_unix_user(zbus::names::BusName::try_from(caller).ok()?)
        .await
        .ok()
}
