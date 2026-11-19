//! Running other programs.
//!
//! Every operation that has to call a real tool (`git`, `nix-instantiate`, `zpool`…)
//! goes through [`Runner`]. A command is an argv, never a shell line, so nothing a
//! caller supplies can be interpreted as a command. The daemon provides the real
//! runner; tests use [`testing::FakeRunner`].

use std::future::Future;
use std::path::PathBuf;

/// One command: a program and its arguments.
#[derive(Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    /// Written to the child's stdin, then closed. Holds passwords for the few tools that
    /// read them from stdin, so it is never shown by `Debug`.
    pub stdin: Option<Vec<u8>>,
    pub cwd: Option<PathBuf>,
}

impl Cmd {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            stdin: None,
            cwd: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// For the job log: the command as a person would read it. Not for execution.
    pub fn display(&self) -> String {
        let mut line = self.program.clone();
        for arg in &self.args {
            line.push(' ');
            line.push_str(arg);
        }
        if self.stdin.is_some() {
            line.push_str(" <stdin>");
        }
        line
    }
}

impl std::fmt::Debug for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cmd")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("stdin", &self.stdin.as_ref().map(|_| "<redacted>"))
            .field("cwd", &self.cwd)
            .finish()
    }
}

/// What a finished command left behind.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Output {
    /// The exit status, or -1 if the process was killed by a signal.
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == 0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("could not start `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cancelled")]
    Cancelled,
}

/// Which stream a line of output came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Where an operation's progress goes: the job log the GUI shows.
pub trait Sink: Send + Sync {
    fn line(&self, stream: Stream, text: &str);

    fn info(&self, text: &str) {
        self.line(Stream::Stdout, text);
    }

    fn error(&self, text: &str) {
        self.line(Stream::Stderr, text);
    }
}

pub trait Runner: Send + Sync {
    /// Run to completion and capture the output. For short probes whose result an
    /// operation reads, such as `git status` or `nix-instantiate --parse`.
    fn run(&self, cmd: &Cmd) -> impl Future<Output = Result<Output, RunError>> + Send;

    /// Run to completion, sending each line of output to `sink` as it appears.
    /// Returns the exit status. For long work the user watches, such as a rebuild.
    fn stream(
        &self,
        cmd: &Cmd,
        sink: &dyn Sink,
    ) -> impl Future<Output = Result<i32, RunError>> + Send;

    /// Run a short command that puts back what the job changed, such as restarting a
    /// service the job stopped. Unlike [`Runner::run`] it still runs after the job was
    /// cancelled, because that is exactly when it is needed. Only for undoing; never for
    /// doing more work.
    fn cleanup(&self, cmd: &Cmd) -> impl Future<Output = Result<Output, RunError>> + Send {
        self.run(cmd)
    }
}

pub mod testing {
    //! Fakes for tests, here rather than under `cfg(test)` so the daemon's tests can use
    //! them too.

    use super::*;
    use std::sync::Mutex;

    /// Drive a future to completion. The fakes never actually wait, so a poll loop is
    /// all the tests need and the crate takes no async runtime for them.
    pub fn block_on<F: Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, Waker};
        let mut future = std::pin::pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
        }
    }

    struct Rule {
        program: String,
        prefix: Vec<String>,
        contains: Option<String>,
        output: Output,
    }

    /// A runner that answers from a script and records what it was asked.
    #[derive(Default)]
    pub struct FakeRunner {
        rules: Mutex<Vec<Rule>>,
        calls: Mutex<Vec<Cmd>>,
    }

    impl FakeRunner {
        pub fn new() -> Self {
            Self::default()
        }

        /// Answer any command for `program` whose arguments start with `prefix`. The
        /// first matching rule wins, so put the specific ones first.
        pub fn on(self, program: &str, prefix: &[&str], output: Output) -> Self {
            self.rules.lock().unwrap().push(Rule {
                program: program.to_string(),
                prefix: prefix.iter().map(|s| s.to_string()).collect(),
                contains: None,
                output,
            });
            self
        }

        /// Answer any command for `program` that has an argument containing `needle`.
        /// For telling apart calls whose only difference is inside one long argument.
        pub fn on_arg(self, program: &str, needle: &str, output: Output) -> Self {
            self.rules.lock().unwrap().push(Rule {
                program: program.to_string(),
                prefix: Vec::new(),
                contains: Some(needle.to_string()),
                output,
            });
            self
        }

        pub fn ok(self, program: &str, prefix: &[&str]) -> Self {
            self.on(program, prefix, Output::default())
        }

        pub fn failing(self, program: &str, prefix: &[&str], code: i32, stderr: &str) -> Self {
            self.on(
                program,
                prefix,
                Output {
                    code,
                    stdout: String::new(),
                    stderr: stderr.to_string(),
                },
            )
        }

        /// Every command run so far, in order.
        pub fn calls(&self) -> Vec<Cmd> {
            self.calls.lock().unwrap().clone()
        }

        /// The calls as `program arg arg` strings, which is easier to assert on.
        pub fn lines(&self) -> Vec<String> {
            self.calls().iter().map(Cmd::display).collect()
        }

        fn answer(&self, cmd: &Cmd) -> Result<Output, RunError> {
            self.calls.lock().unwrap().push(cmd.clone());
            let rules = self.rules.lock().unwrap();
            rules
                .iter()
                .find(|rule| {
                    rule.program == cmd.program
                        && cmd.args.starts_with(rule.prefix.as_slice())
                        && rule
                            .contains
                            .as_ref()
                            .is_none_or(|needle| cmd.args.iter().any(|a| a.contains(needle)))
                })
                .map(|rule| rule.output.clone())
                // An unscripted command is a test that forgot a rule: fail it loudly
                // rather than pretend it succeeded.
                .ok_or_else(|| RunError::Spawn {
                    program: cmd.program.clone(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("FakeRunner has no rule for `{}`", cmd.display()),
                    ),
                })
        }
    }

    impl Runner for FakeRunner {
        async fn run(&self, cmd: &Cmd) -> Result<Output, RunError> {
            self.answer(cmd)
        }

        async fn stream(&self, cmd: &Cmd, sink: &dyn Sink) -> Result<i32, RunError> {
            let output = self.answer(cmd)?;
            for line in output.stdout.lines() {
                sink.line(Stream::Stdout, line);
            }
            for line in output.stderr.lines() {
                sink.line(Stream::Stderr, line);
            }
            Ok(output.code)
        }
    }

    /// A sink that keeps what it was given.
    #[derive(Default)]
    pub struct CollectSink {
        lines: Mutex<Vec<(Stream, String)>>,
    }

    impl CollectSink {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn lines(&self) -> Vec<(Stream, String)> {
            self.lines.lock().unwrap().clone()
        }

        pub fn text(&self) -> String {
            self.lines()
                .into_iter()
                .map(|(_, line)| line)
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    impl Sink for CollectSink {
        fn line(&self, stream: Stream, text: &str) {
            self.lines.lock().unwrap().push((stream, text.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn stdin_is_never_shown_by_debug_or_display() {
        let cmd = Cmd::new("tool").arg("--flag").stdin("hunter2");
        assert!(!format!("{cmd:?}").contains("hunter2"));
        assert!(!cmd.display().contains("hunter2"));
        assert_eq!(cmd.display(), "tool --flag <stdin>");
    }

    #[test]
    fn the_fake_matches_on_an_argument_prefix_and_records_calls() {
        let runner = FakeRunner::new().on(
            "git",
            &["status"],
            Output {
                code: 0,
                stdout: "clean\n".into(),
                stderr: String::new(),
            },
        );
        let out = block_on(runner.run(&Cmd::new("git").args(["status", "--short"]))).unwrap();
        assert_eq!(out.stdout, "clean\n");
        assert_eq!(runner.lines(), ["git status --short"]);
    }

    #[test]
    fn the_fake_fails_loudly_on_an_unscripted_command() {
        let runner = FakeRunner::new();
        let err = block_on(runner.run(&Cmd::new("rm").arg("-rf"))).unwrap_err();
        assert!(err.to_string().contains("no rule"), "{err}");
    }

    #[test]
    fn the_fake_streams_both_outputs_to_the_sink() {
        let runner = FakeRunner::new().on(
            "tool",
            &[],
            Output {
                code: 3,
                stdout: "a\nb\n".into(),
                stderr: "oops\n".into(),
            },
        );
        let sink = CollectSink::new();
        let code = block_on(runner.stream(&Cmd::new("tool"), &sink)).unwrap();
        assert_eq!(code, 3);
        assert_eq!(
            sink.lines(),
            [
                (Stream::Stdout, "a".to_string()),
                (Stream::Stdout, "b".to_string()),
                (Stream::Stderr, "oops".to_string()),
            ]
        );
    }
}
