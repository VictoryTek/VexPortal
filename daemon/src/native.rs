//! Running native operations.
//!
//! The counterpart of [`crate::executor`] for actions whose catalog `backend` is
//! `native`: instead of `just`, the daemon calls into `vexportal-ops`. This module
//! supplies what that crate leaves out: a [`Runner`] that really spawns programs, and
//! the job plumbing that turns an operation's output into `JobOutput` signals.

use crate::audit;
use crate::cancel::JobHandle;
use crate::config::{tool_environment, Config};
use crate::executor::{emit_finished, emit_output, STREAM_STDERR, STREAM_STDOUT};

use log::{error, info};
use std::collections::BTreeMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vexportal_catalog::validate::Invocation;
use vexportal_ops::runner::{Cmd, Output, RunError, Runner, Sink, Stream};
use vexportal_ops::Secret;
use zbus::Connection;

/// How long a cancelled command has to exit on SIGTERM before it is killed.
const GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Spawns real programs for an operation, and stops them if the job is cancelled.
pub struct TokioRunner {
    cancel: CancellationToken,
}

impl TokioRunner {
    pub fn new(cancel: CancellationToken) -> Self {
        Self { cancel }
    }
}

/// Kills a command's whole process group if the future running it is dropped before the
/// command was collected. `kill_on_drop` alone reaches only the command itself, and the
/// tools it started would carry on after the job was reported finished.
struct GroupGuard {
    pgid: Option<i32>,
}

impl GroupGuard {
    fn new(child: &Child) -> Self {
        Self {
            pgid: child.id().map(|pid| pid as i32),
        }
    }

    /// The command was collected normally; there is nothing left to clean up.
    fn disarm(&mut self) {
        self.pgid = None;
    }

    /// SIGKILL whatever is left in the group, then stand down. The group id stays
    /// valid while any member lives, so this cannot hit an unrelated process.
    fn kill_group(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            // ESRCH just means everything already exited.
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill_group();
    }
}

fn spawn(cmd: &Cmd) -> Result<(Child, GroupGuard), RunError> {
    let mut command = Command::new(&cmd.program);
    command
        .args(&cmd.args)
        .env_clear()
        .envs(tool_environment())
        .stdin(if cmd.stdin.is_some() {
            Stdio::piped()
        } else {
            // A tool that still prompts reads EOF and fails, rather than blocking on a
            // terminal that is not there.
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group, so cancelling reaches the tools a tool starts.
        .process_group(0)
        .kill_on_drop(true);
    if let Some(dir) = &cmd.cwd {
        command.current_dir(dir);
    }
    let mut child = command.spawn().map_err(|source| RunError::Spawn {
        program: cmd.program.clone(),
        source,
    })?;
    let guard = GroupGuard::new(&child);
    feed_stdin(&mut child, cmd);
    Ok((child, guard))
}

/// Write stdin from its own task, so a tool that fills its output pipes before it has
/// read all its input cannot deadlock against us, and cancelling is never stuck behind
/// the write.
fn feed_stdin(child: &mut Child, cmd: &Cmd) {
    if let (Some(bytes), Some(mut stdin)) = (cmd.stdin.clone(), child.stdin.take()) {
        tokio::spawn(async move {
            // A tool that exits early closes the pipe; its exit code says what went wrong.
            let _ = stdin.write_all(&bytes).await;
            let _ = stdin.shutdown().await;
        });
    }
}

/// Stop a cancelled child: SIGTERM to its group, SIGKILL if it ignores that, and a last
/// SIGKILL for anything it started that outlived it.
async fn stop(child: &mut Child, guard: &mut GroupGuard) {
    crate::cancel::terminate(child);
    if tokio::time::timeout(GRACE, child.wait()).await.is_err() {
        crate::cancel::kill(child);
        let _ = child.wait().await;
    }
    guard.kill_group();
}

/// Lines from a pipe. Invalid UTF-8 becomes U+FFFD rather than an error, because a read
/// error would end the loop, drop the pipe, and kill the tool with SIGPIPE on its next
/// write. Everything read so far is kept in `buf`, so a read that is dropped half-way
/// (by a `select!`) loses nothing.
struct Lines<R> {
    reader: BufReader<R>,
    buf: Vec<u8>,
}

impl<R: tokio::io::AsyncRead + Unpin> Lines<R> {
    fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            buf: Vec::new(),
        }
    }

    /// The next line, or `None` at the end of the stream.
    async fn next(&mut self) -> Option<String> {
        loop {
            let read = self
                .reader
                .read_until(b'\n', &mut self.buf)
                .await
                .unwrap_or(0);
            if read == 0 {
                // End of stream; a last line with no newline is still a line.
                return (!self.buf.is_empty()).then(|| self.take());
            }
            if self.buf.ends_with(b"\n") {
                return Some(self.take());
            }
        }
    }

    fn take(&mut self) -> String {
        if self.buf.ends_with(b"\n") {
            self.buf.pop();
        }
        if self.buf.ends_with(b"\r") {
            self.buf.pop();
        }
        let line = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        line
    }
}

impl Runner for TokioRunner {
    async fn run(&self, cmd: &Cmd) -> Result<Output, RunError> {
        // Once cancelled, start nothing: a tool killed a moment after it started can
        // leave half-done state, such as a stale git index lock.
        if self.cancel.is_cancelled() {
            return Err(RunError::Cancelled);
        }
        let (mut child, mut guard) = spawn(cmd)?;

        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let read = async {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            tokio::join!(
                async {
                    if let Some(s) = stdout.as_mut() {
                        let _ = s.read_to_end(&mut out).await;
                    }
                },
                async {
                    if let Some(s) = stderr.as_mut() {
                        let _ = s.read_to_end(&mut err).await;
                    }
                }
            );
            (out, err)
        };

        let (out, err) = tokio::select! {
            output = read => output,
            _ = self.cancel.cancelled() => {
                stop(&mut child, &mut guard).await;
                return Err(RunError::Cancelled);
            }
        };

        // Both pipes closing does not mean the command has exited; it can close them
        // and carry on, so the wait is raced against cancel too.
        let status = tokio::select! {
            status = child.wait() => status,
            _ = self.cancel.cancelled() => {
                stop(&mut child, &mut guard).await;
                return Err(RunError::Cancelled);
            }
        };
        guard.disarm();
        let status = status.map_err(|source| RunError::Spawn {
            program: cmd.program.clone(),
            source,
        })?;
        Ok(Output {
            code: status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
        })
    }

    async fn stream(&self, cmd: &Cmd, sink: &dyn Sink) -> Result<i32, RunError> {
        if self.cancel.is_cancelled() {
            return Err(RunError::Cancelled);
        }
        let (mut child, mut guard) = spawn(cmd)?;

        let mut out: Option<Lines<ChildStdout>> = child.stdout.take().map(Lines::new);
        let mut err: Option<Lines<ChildStderr>> = child.stderr.take().map(Lines::new);

        // A stream is set to `None` once it reaches the end.
        while out.is_some() || err.is_some() {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    stop(&mut child, &mut guard).await;
                    return Err(RunError::Cancelled);
                }
                line = async { out.as_mut().unwrap().next().await }, if out.is_some() => {
                    match line {
                        Some(line) => sink.line(Stream::Stdout, &line),
                        None => out = None,
                    }
                }
                line = async { err.as_mut().unwrap().next().await }, if err.is_some() => {
                    match line {
                        Some(line) => sink.line(Stream::Stderr, &line),
                        None => err = None,
                    }
                }
            }
        }

        tokio::select! {
            status = child.wait() => {
                guard.disarm();
                let status = status.map_err(|source| RunError::Spawn {
                    program: cmd.program.clone(),
                    source,
                })?;
                Ok(status.code().unwrap_or(-1))
            }
            _ = self.cancel.cancelled() => {
                stop(&mut child, &mut guard).await;
                Err(RunError::Cancelled)
            }
        }
    }
}

/// Hands an operation's output to the task that emits it as `JobOutput` signals.
struct ChannelSink(mpsc::UnboundedSender<(u32, String)>);

impl Sink for ChannelSink {
    fn line(&self, stream: Stream, text: &str) {
        let tag = match stream {
            Stream::Stdout => STREAM_STDOUT,
            Stream::Stderr => STREAM_STDERR,
        };
        let _ = self.0.send((tag, text.to_string()));
    }
}

pub fn spawn_job(
    job_id: String,
    invocation: Invocation,
    config: &Config,
    connection: Connection,
) -> JobHandle {
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let id = job_id.clone();
    let action = invocation.action.clone();
    let config = config.clone();

    let task = tokio::spawn(async move {
        let exit_code = run(&id, invocation, &config, &connection, token).await;
        audit::finished(&id, exit_code);
        emit_finished(&connection, &id, exit_code).await;
    });

    JobHandle {
        job_id,
        recipe: action,
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
    info!("job {job_id}: {}", invocation.audit_line());

    let (tx, mut rx) = mpsc::unbounded_channel::<(u32, String)>();
    // Emitting is async and a `Sink` is not, so lines go through a channel.
    let emitter = {
        let connection = connection.clone();
        let job_id = job_id.to_string();
        tokio::spawn(async move {
            while let Some((stream, line)) = rx.recv().await {
                emit_output(&connection, &job_id, stream, &line).await;
            }
        })
    };
    let sink = ChannelSink(tx);

    let code = execute(job_id, &invocation, config, &sink, cancel).await;

    drop(sink);
    let _ = emitter.await;
    code
}

/// Run the operation and return its exit code.
///
/// Cancellation is not raced against the operation here. Dropping an operation in the
/// middle of a tool call would cut short the runner's own shutdown of the tool's
/// process group. Instead the runner sees the cancel, stops the tool and reports it,
/// and the operation returns.
async fn execute(
    job_id: &str,
    invocation: &Invocation,
    config: &Config,
    sink: &dyn Sink,
    cancel: CancellationToken,
) -> i32 {
    let runner = TokioRunner::new(cancel.clone());
    let secrets: BTreeMap<String, Secret> = invocation
        .secrets
        .iter()
        .map(|(name, secret)| (name.clone(), Secret::new(secret.expose())))
        .collect();
    let ctx = vexportal_ops::Ctx {
        runner: &runner,
        nixos_dir: &config.nixos_dir,
        root: std::path::Path::new("/"),
        sink,
        args: &invocation.named,
        secrets: &secrets,
        // Filled in once an operation needs it; the interface already looks it up for
        // the audit record.
        caller_uid: None,
    };

    let code = match vexportal_ops::run(&invocation.action, &ctx).await {
        Some(code) => code,
        None => {
            error!(
                "job {job_id}: `{}` has no native handler",
                invocation.action
            );
            sink.error(&format!("`{}` has no native handler", invocation.action));
            -1
        }
    };

    // A cancel that arrives after the work succeeded does not undo it, so it is only
    // reported when something did not finish.
    if cancel.is_cancelled() && code != 0 {
        audit::cancelled(job_id);
        sink.error("Cancelled");
        return -1;
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use vexportal_catalog::{Backend, Catalog};
    use vexportal_ops::fsutil::testing::TempDir;
    use vexportal_ops::runner::testing::CollectSink;

    fn sh(script: &str) -> Cmd {
        // Only shell builtins, so the tests do not depend on what is on the fixed PATH.
        Cmd::new("sh").args(["-c", script])
    }

    /// Alive means running or sleeping: a zombie is dead, it is only waiting to be
    /// collected by whoever adopted it.
    fn is_alive(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => !stat
                .rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z'),
            Err(_) => false,
        }
    }

    async fn dies_soon(pid: i32) -> bool {
        for _ in 0..40 {
            if !is_alive(pid) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }

    /// A script that starts a background busy loop, records its pid, then busy-loops
    /// itself: two processes in one group, neither of which exits on its own.
    fn spinner(pidfile: &Path) -> Cmd {
        sh(&format!(
            "(while :; do :; done) & echo $! > '{}'; while :; do :; done",
            pidfile.display()
        ))
    }

    async fn child_pid(pidfile: &Path) -> i32 {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(pidfile) {
                if let Ok(pid) = text.trim().parse() {
                    return pid;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("the script never wrote its child's pid");
    }

    #[test]
    fn the_catalog_and_the_ops_crate_agree_on_what_is_native() {
        let catalog = Catalog::load().unwrap();
        let native: Vec<&str> = catalog
            .actions
            .iter()
            .filter(|a| a.backend == Backend::Native)
            .map(|a| a.id.as_str())
            .collect();
        for id in &native {
            assert!(
                vexportal_ops::is_native(id),
                "`{id}` is marked native in the catalog but has no handler"
            );
        }
        for id in vexportal_ops::NATIVE_OPERATIONS {
            assert!(
                native.contains(id),
                "`{id}` has a handler but the catalog does not mark it native"
            );
        }
    }

    #[tokio::test]
    async fn run_captures_output_and_the_exit_code() {
        let runner = TokioRunner::new(CancellationToken::new());
        let out = runner
            .run(&sh("echo out; echo err >&2; exit 3"))
            .await
            .unwrap();
        assert_eq!(out.code, 3);
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.stderr, "err\n");
    }

    #[tokio::test]
    async fn stdin_reaches_the_child_and_a_missing_stdin_is_empty() {
        let runner = TokioRunner::new(CancellationToken::new());
        let out = runner
            .run(&sh("read line; echo got:$line").stdin("secret-value\n"))
            .await
            .unwrap();
        assert_eq!(out.stdout, "got:secret-value\n");

        // Without stdin the child sees EOF at once instead of waiting on a terminal.
        let out = runner.run(&sh("read line || echo eof")).await.unwrap();
        assert_eq!(out.stdout, "eof\n");
    }

    #[tokio::test]
    async fn input_and_output_larger_than_a_pipe_do_not_deadlock() {
        // 200 KB in and 200 KB out, each several times a pipe buffer. Writing all the
        // input before reading any output would hang here.
        let runner = TokioRunner::new(CancellationToken::new());
        let input = "xxx\n".repeat(50_000);
        let out = runner
            .run(&sh("while read line; do echo \"$line\"; done").stdin(input.clone()))
            .await
            .unwrap();
        assert_eq!(out.stdout.len(), input.len());
    }

    #[tokio::test]
    async fn the_environment_is_not_inherited() {
        std::env::set_var("VEXPORTAL_TEST_LEAK", "yes");
        let runner = TokioRunner::new(CancellationToken::new());
        let out = runner
            .run(&sh("echo \"[${VEXPORTAL_TEST_LEAK-unset}]\""))
            .await
            .unwrap();
        assert_eq!(out.stdout, "[unset]\n");
    }

    #[tokio::test]
    async fn stream_sends_each_line_to_the_sink_as_it_arrives() {
        let runner = TokioRunner::new(CancellationToken::new());
        let sink = CollectSink::new();
        let code = runner
            .stream(&sh("echo one; echo two >&2; echo three; exit 2"), &sink)
            .await
            .unwrap();
        assert_eq!(code, 2);
        let lines = sink.lines();
        assert!(lines.contains(&(Stream::Stdout, "one".into())));
        assert!(lines.contains(&(Stream::Stdout, "three".into())));
        assert!(lines.contains(&(Stream::Stderr, "two".into())));
    }

    #[tokio::test]
    async fn stream_survives_output_that_is_not_utf8() {
        // One bad byte must not end the read: that would close the pipe and the tool's
        // next write would kill it. `after` arriving and a clean exit prove it lived.
        let runner = TokioRunner::new(CancellationToken::new());
        let sink = CollectSink::new();
        let code = runner
            .stream(
                &sh("printf 'a\\377b\\n'; echo after; printf 'no newline'"),
                &sink,
            )
            .await
            .unwrap();
        assert_eq!(code, 0);
        let lines: Vec<String> = sink.lines().into_iter().map(|(_, l)| l).collect();
        assert_eq!(lines, ["a\u{fffd}b", "after", "no newline"]);
    }

    #[tokio::test]
    async fn nothing_is_started_once_the_job_is_cancelled() {
        let dir = TempDir::new();
        let marker = dir.join("ran");
        let token = CancellationToken::new();
        token.cancel();
        let runner = TokioRunner::new(token);
        let cmd = sh(&format!("echo > '{}'", marker.display()));

        assert!(matches!(runner.run(&cmd).await, Err(RunError::Cancelled)));
        assert!(matches!(
            runner.stream(&cmd, &CollectSink::new()).await,
            Err(RunError::Cancelled)
        ));
        assert!(!marker.exists(), "the command ran after the cancel");
    }

    #[tokio::test]
    async fn a_program_that_is_not_there_is_a_spawn_error() {
        let runner = TokioRunner::new(CancellationToken::new());
        let err = runner
            .run(&Cmd::new("vexportal-no-such-program"))
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::Spawn { .. }), "{err}");
    }

    #[tokio::test]
    async fn cancelling_kills_the_command_and_what_it_started() {
        let dir = TempDir::new();
        let pidfile = dir.join("pid");
        let token = CancellationToken::new();
        let runner = TokioRunner::new(token.clone());
        let cmd = spinner(&pidfile);

        let (result, pid) = tokio::join!(runner.run(&cmd), async {
            let pid = child_pid(&pidfile).await;
            assert!(is_alive(pid), "the background child should be running");
            token.cancel();
            pid
        });

        assert!(matches!(result, Err(RunError::Cancelled)));
        assert!(dies_soon(pid).await, "the command's child outlived the job");
    }

    #[tokio::test]
    async fn cancelling_a_stream_kills_what_it_started_too() {
        let dir = TempDir::new();
        let pidfile = dir.join("pid");
        let token = CancellationToken::new();
        let runner = TokioRunner::new(token.clone());
        let sink = CollectSink::new();
        let cmd = spinner(&pidfile);

        let (result, pid) = tokio::join!(runner.stream(&cmd, &sink), async {
            let pid = child_pid(&pidfile).await;
            token.cancel();
            pid
        });

        assert!(matches!(result, Err(RunError::Cancelled)));
        assert!(dies_soon(pid).await, "the command's child outlived the job");
    }

    #[tokio::test]
    async fn cancelling_works_after_the_command_closes_its_output() {
        // Both pipes reach end-of-file while the command is still running.
        let token = CancellationToken::new();
        let runner = TokioRunner::new(token.clone());
        let cmd = sh("exec >&- 2>&-; while :; do :; done");

        let (result, _) = tokio::join!(runner.run(&cmd), async {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            token.cancel();
        });

        assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
    }

    #[tokio::test]
    async fn dropping_a_run_in_flight_still_kills_the_whole_group() {
        // What happens if something above the runner gives up on it: the future is
        // dropped mid-call. The guard, not the runner's own cleanup, has to do the work.
        let dir = TempDir::new();
        let pidfile = dir.join("pid");
        let runner = TokioRunner::new(CancellationToken::new());

        let started = std::time::Instant::now();
        let gave_up = tokio::time::timeout(
            std::time::Duration::from_millis(400),
            runner.run(&spinner(&pidfile)),
        )
        .await;
        assert!(
            gave_up.is_err(),
            "the spinner should still have been running"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));

        let pid = child_pid(&pidfile).await;
        assert!(
            dies_soon(pid).await,
            "the command's child outlived the drop"
        );
    }

    fn feature_enable() -> Invocation {
        let catalog = Catalog::load().unwrap();
        vexportal_catalog::validate::build(
            &catalog,
            "feature-enable",
            &[("feature".to_string(), "gaming".to_string())]
                .into_iter()
                .collect(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn an_operation_without_a_handler_fails_and_says_so() {
        let sink = CollectSink::new();

        let code = execute(
            "job",
            &feature_enable(),
            &Config::default(),
            &sink,
            CancellationToken::new(),
        )
        .await;

        assert_eq!(code, -1);
        assert!(sink.text().contains("no native handler"));
    }

    #[tokio::test]
    async fn a_cancelled_job_reports_failure() {
        let token = CancellationToken::new();
        token.cancel();
        let sink = CollectSink::new();

        let code = execute("job", &feature_enable(), &Config::default(), &sink, token).await;

        assert_eq!(code, -1);
        assert!(sink.text().contains("Cancelled"));
    }
}
