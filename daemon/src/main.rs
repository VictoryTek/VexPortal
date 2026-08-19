//! vexportal-daemon — the privileged half of VexPortal.
//!
//! It owns `io.github.vexportal.Daemon` on the system bus and will run exactly one
//! program: the `just` binary, against the justfile named in its own unit file, with a
//! recipe and arguments that the VexPortal catalog has validated. There is no method
//! that takes a command line.

mod audit;
mod auth;
mod cancel;
mod config;
mod executor;
mod interface;
mod lifecycle;

use config::Config;
use interface::Daemon;
use lifecycle::IdleTracker;
use log::{error, info};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use vexportal_catalog::validate::runnable_recipes;
use vexportal_catalog::Catalog;

const BUS_NAME: &str = "io.github.vexportal.Daemon";

/// How long the daemon stays up with nothing running before exiting.
const IDLE_TIMEOUT: Duration = Duration::from_secs(180);
const IDLE_POLL: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config = match Config::from_args(std::env::args()) {
        Ok(config) => config,
        Err(e) => {
            error!("{e}");
            std::process::exit(2);
        }
    };

    let catalog = Catalog::load()?;
    info!(
        "vexportal-daemon {} starting: {} runnable recipes, justfile {}",
        env!("CARGO_PKG_VERSION"),
        runnable_recipes(&catalog).len(),
        config.justfile.display()
    );

    if !config.justfile.exists() {
        // Not fatal: the daemon is D-Bus activated and the justfile may arrive with the
        // next rebuild. Refusing to start would only turn this into a confusing
        // "service failed" instead of a clear error on the first request.
        error!(
            "{} does not exist — recipes will fail until it does",
            config.justfile.display()
        );
    }

    let idle = Arc::new(Mutex::new(IdleTracker::new(IDLE_TIMEOUT)));
    let daemon = Daemon::new(catalog, config, idle.clone());

    let connection = zbus::connection::Builder::system()?
        .name(BUS_NAME)?
        .serve_at(executor::OBJECT_PATH, daemon)?
        .build()
        .await?;

    info!("listening on {BUS_NAME}");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                info!("shutting down (SIGTERM)");
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down (interrupt)");
                break;
            }
            _ = tokio::time::sleep(IDLE_POLL) => {
                if idle.lock().await.is_idle() && !has_running_jobs(&connection).await {
                    info!("shutting down (idle)");
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Never exit out from under a rebuild: idle means idle *and* nothing in flight.
async fn has_running_jobs(connection: &zbus::Connection) -> bool {
    let Ok(iface) = connection
        .object_server()
        .interface::<_, Daemon>(executor::OBJECT_PATH)
        .await
    else {
        return false;
    };
    let daemon = iface.get().await;
    daemon.active_jobs().await > 0
}
