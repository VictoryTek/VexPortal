//! Talking to vexportal-daemon.
//!
//! zbus is async and GTK is not, so the connection lives on a tokio runtime in its own
//! thread. The GUI sends [`Command`]s down one channel and reads [`Event`]s off
//! another with `glib::spawn_future_local`, which keeps every widget touch on the main
//! thread without the GUI ever blocking on D-Bus.

use log::{error, warn};
use std::collections::HashMap;
use std::thread;

#[derive(Debug)]
pub enum Command {
    Run {
        /// Correlates the eventual `Started` or `Failed` with the run view that asked.
        request_id: u64,
        recipe: String,
        args: HashMap<String, String>,
    },
    Cancel {
        job_id: String,
    },
}

#[derive(Debug, Clone)]
pub enum Event {
    /// The daemon accepted a request and gave it a job id.
    Started {
        request_id: u64,
        job_id: String,
    },
    /// The request never started: validation, polkit, or the daemon being unreachable.
    Failed {
        request_id: u64,
        message: String,
    },
    Output {
        job_id: String,
        stream: u32,
        line: String,
    },
    Finished {
        job_id: String,
        exit_code: i32,
    },
}

/// True when a request failed because the user dismissed the polkit prompt. That is a
/// decision, not a fault, and the GUI says so rather than showing an error.
pub fn is_declined(message: &str) -> bool {
    message.contains("Not authorized") || message.contains("dismissed")
}

#[derive(Clone)]
pub struct Client {
    commands: async_channel::Sender<Command>,
    pub events: async_channel::Receiver<Event>,
}

impl Client {
    /// Start the D-Bus thread. Connecting is deferred to the first command so that
    /// launching VexPortal does not activate a root daemon before it is needed.
    pub fn start() -> Self {
        let (command_tx, command_rx) = async_channel::unbounded::<Command>();
        let (event_tx, event_rx) = async_channel::unbounded::<Event>();

        thread::Builder::new()
            .name("vexportal-dbus".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        error!("could not start the D-Bus runtime: {e}");
                        return;
                    }
                };
                runtime.block_on(serve(command_rx, event_tx));
            })
            .expect("spawning the D-Bus thread should not fail");

        Client {
            commands: command_tx,
            events: event_rx,
        }
    }

    pub fn run(&self, request_id: u64, recipe: &str, args: HashMap<String, String>) {
        self.send(Command::Run {
            request_id,
            recipe: recipe.to_string(),
            args,
        });
    }

    pub fn cancel(&self, job_id: &str) {
        self.send(Command::Cancel {
            job_id: job_id.to_string(),
        });
    }

    fn send(&self, command: Command) {
        if let Err(e) = self.commands.send_blocking(command) {
            error!("the D-Bus thread is gone: {e}");
        }
    }
}

async fn serve(commands: async_channel::Receiver<Command>, events: async_channel::Sender<Event>) {
    let connection = match zbus::Connection::system().await {
        Ok(connection) => connection,
        Err(e) => {
            error!("could not connect to the system bus: {e}");
            drain_with_error(
                commands,
                events,
                format!("cannot reach the system bus: {e}"),
            )
            .await;
            return;
        }
    };

    let proxy = match DaemonProxy::new(&connection).await {
        Ok(proxy) => proxy,
        Err(e) => {
            error!("could not reach vexportal-daemon: {e}");
            drain_with_error(
                commands,
                events,
                format!("vexportal-daemon is not available: {e}"),
            )
            .await;
            return;
        }
    };

    // Subscribe before any job can start, so no output is missed between the daemon
    // accepting a request and the GUI attaching to the stream.
    let output = proxy.receive_job_output().await;
    let finished = proxy.receive_job_finished().await;

    if let Ok(mut output) = output {
        let events = events.clone();
        tokio::spawn(async move {
            use futures_util::StreamExt;
            while let Some(signal) = output.next().await {
                let Ok(args) = signal.args() else { continue };
                let _ = events
                    .send(Event::Output {
                        job_id: args.job_id().to_string(),
                        stream: *args.stream(),
                        line: args.line().to_string(),
                    })
                    .await;
            }
        });
    }

    if let Ok(mut finished) = finished {
        let events = events.clone();
        tokio::spawn(async move {
            use futures_util::StreamExt;
            while let Some(signal) = finished.next().await {
                let Ok(args) = signal.args() else { continue };
                let _ = events
                    .send(Event::Finished {
                        job_id: args.job_id().to_string(),
                        exit_code: *args.exit_code(),
                    })
                    .await;
            }
        });
    }

    while let Ok(command) = commands.recv().await {
        match command {
            Command::Run {
                request_id,
                recipe,
                args,
            } => {
                let event = match proxy.run_recipe(&recipe, args).await {
                    Ok(job_id) => Event::Started { request_id, job_id },
                    Err(e) => Event::Failed {
                        request_id,
                        message: friendly(&e),
                    },
                };
                let _ = events.send(event).await;
            }
            Command::Cancel { job_id } => {
                if let Err(e) = proxy.cancel(&job_id).await {
                    warn!("cancelling {job_id} failed: {e}");
                }
            }
        }
    }
}

/// Without a daemon there is nothing to run, but the GUI still has to hear back about
/// every request it makes or its run views would sit at "starting…" forever.
async fn drain_with_error(
    commands: async_channel::Receiver<Command>,
    events: async_channel::Sender<Event>,
    message: String,
) {
    while let Ok(command) = commands.recv().await {
        if let Command::Run { request_id, .. } = command {
            let _ = events
                .send(Event::Failed {
                    request_id,
                    message: message.clone(),
                })
                .await;
        }
    }
}

/// D-Bus errors arrive as `org.freedesktop.DBus.Error.AccessDenied: …`, which is not
/// what anyone wants to read in a dialog.
fn friendly(error: &zbus::Error) -> String {
    match error {
        zbus::Error::MethodError(_, Some(message), _) => message.clone(),
        zbus::Error::MethodError(name, None, _) => name.to_string(),
        other => other.to_string(),
    }
}

#[zbus::proxy(
    interface = "io.github.vexportal.Daemon1",
    default_service = "io.github.vexportal.Daemon",
    default_path = "/io/github/vexportal/Daemon"
)]
trait Daemon {
    fn run_recipe(&self, recipe: &str, args: HashMap<String, String>) -> zbus::Result<String>;
    fn cancel(&self, job_id: &str) -> zbus::Result<bool>;
    fn list_jobs(&self) -> zbus::Result<Vec<(String, String)>>;

    #[zbus(signal)]
    fn job_output(&self, job_id: String, stream: u32, line: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn job_finished(&self, job_id: String, exit_code: i32) -> zbus::Result<()>;

    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn justfile(&self) -> zbus::Result<String>;
}
