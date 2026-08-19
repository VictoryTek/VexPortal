//! Running one recipe.
//!
//! The command is built entirely from an [`Invocation`] the catalog has already
//! validated, and is passed to `just` as an argv — never through a shell — so nothing
//! a caller supplies can be interpreted as a command.

use crate::audit;
use crate::cancel::JobHandle;
use crate::config::{recipe_environment, Config};
use crate::interface::Daemon;

use log::{error, info};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use vexportal_catalog::validate::Invocation;
use zbus::Connection;

pub const OBJECT_PATH: &str = "/io/github/vexportal/Daemon";

/// Stream tags carried by the `JobOutput` signal.
pub const STREAM_STDOUT: u32 = 0;
pub const STREAM_STDERR: u32 = 1;

/// How long a cancelled job has to exit on SIGTERM before it is killed.
const GRACE: std::time::Duration = std::time::Duration::from_secs(10);

pub fn spawn(
    job_id: String,
    invocation: Invocation,
    config: &Config,
    connection: Connection,
) -> JobHandle {
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let id = job_id.clone();
    let recipe = invocation.recipe.clone();
    let config = config.clone();

    let task = tokio::spawn(async move {
        let exit_code = run(&id, invocation, &config, &connection, token).await;
        audit::finished(&id, exit_code);
        emit_finished(&connection, &id, exit_code).await;
    });

    JobHandle {
        job_id,
        recipe,
        cancel,
        task,
    }
}

async fn run(
    job_id: &str,
    invocation: Invocation,
    config: &Config,
    connection: &Connection,
    cancel: CancellationToken,
) -> i32 {
    let mut command = Command::new("just");
    command
        .arg("--justfile")
        .arg(&config.justfile)
        .arg("--working-directory")
        .arg(config.working_directory())
        .args(invocation.just_args())
        .current_dir(config.working_directory())
        .env_clear()
        .envs(recipe_environment())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if invocation.stdin.is_some() {
            Stdio::piped()
        } else {
            // Not inheriting means a recipe that still prompts reads EOF and takes its
            // default answer, rather than blocking forever on a terminal that is not there.
            Stdio::null()
        })
        // Own process group, so cancellation can reach `nix` and `nixos-rebuild` and
        // not just the `just` process at the top.
        .process_group(0);

    info!("job {job_id}: {}", invocation.audit_line());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            error!("job {job_id}: could not start `just`: {e}");
            emit_output(
                connection,
                job_id,
                STREAM_STDERR,
                &format!("VexPortal could not start `just`: {e}"),
            )
            .await;
            return -1;
        }
    };

    if let Some(secret) = invocation.stdin.as_deref() {
        if let Some(mut stdin) = child.stdin.take() {
            // A recipe's `read -rsp` consumes one line; the newline is what ends it.
            let _ = stdin.write_all(format!("{secret}\n").as_bytes()).await;
            let _ = stdin.shutdown().await;
        }
    }

    let stdout = pump(
        child.stdout.take(),
        job_id.to_string(),
        STREAM_STDOUT,
        connection.clone(),
    );
    let stderr = pump(
        child.stderr.take(),
        job_id.to_string(),
        STREAM_STDERR,
        connection.clone(),
    );

    let status = tokio::select! {
        status = child.wait() => status,
        _ = cancel.cancelled() => {
            audit::cancelled(job_id);
            emit_output(connection, job_id, STREAM_STDERR, "Cancelled — stopping…").await;
            crate::cancel::terminate(&child);
            match tokio::time::timeout(GRACE, child.wait()).await {
                Ok(status) => status,
                Err(_) => {
                    crate::cancel::kill(&child);
                    child.wait().await
                }
            }
        }
    };

    let _ = stdout.await;
    let _ = stderr.await;

    match status {
        Ok(status) => status.code().unwrap_or(-1),
        Err(e) => {
            error!("job {job_id}: waiting on `just` failed: {e}");
            -1
        }
    }
}

/// Forward one stream to `JobOutput`, a line at a time.
fn pump<R>(
    reader: Option<R>,
    job_id: String,
    stream: u32,
    connection: Connection,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(reader) = reader else { return };
        let mut lines = BufReader::new(reader).lines();
        // `next_line` splits on newlines and drops invalid UTF-8, which is what a log
        // view wants; a recipe emitting binary is not a case worth carrying.
        while let Ok(Some(line)) = lines.next_line().await {
            emit_output(&connection, &job_id, stream, &line).await;
        }
    })
}

async fn emit_output(connection: &Connection, job_id: &str, stream: u32, line: &str) {
    if let Ok(iface) = connection
        .object_server()
        .interface::<_, Daemon>(OBJECT_PATH)
        .await
    {
        let _ = Daemon::job_output(iface.signal_emitter(), job_id, stream, line).await;
    }
}

async fn emit_finished(connection: &Connection, job_id: &str, exit_code: i32) {
    if let Ok(iface) = connection
        .object_server()
        .interface::<_, Daemon>(OBJECT_PATH)
        .await
    {
        let _ = Daemon::job_finished(iface.signal_emitter(), job_id, exit_code).await;
    }
}
