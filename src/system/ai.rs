//! The AI assistant's per-user state, from `vexos-ai status --json`.
//!
//! All of it lives in the logged-in user's own home, so the GUI reads it as that user —
//! the daemon runs as root and would see /root's instead. `status` reads files
//! only and never goes to the network. If the command fails or speaks a schema this
//! build does not know, the files it reads are read directly instead; their paths and
//! fallbacks mirror github:VictoryTek/vexos-ai, `bin/vexos-ai.sh`. The contract is
//! that repo's `docs/contract.md`.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const CLI: &str = "vexos-ai";
/// A settings command only reads and writes a few small files. One that has not
/// answered by now is stuck and is killed.
const DEADLINE: Duration = Duration::from_secs(20);
/// The newest `status --json` schema this build understands.
const STATUS_SCHEMA: u64 = 1;

/// Which coding agent `vexos-ai` opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
    OpenCode,
}

impl Agent {
    pub const ALL: [Agent; 3] = [Agent::Claude, Agent::Codex, Agent::OpenCode];

    /// Any other value, including one a newer `vexos-ai` saved, is not a choice.
    fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            "opencode" => Some(Agent::OpenCode),
            _ => None,
        }
    }

    /// The value `ai-pick` takes.
    pub fn id(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::OpenCode => "opencode",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
            Agent::OpenCode => "OpenCode",
        }
    }

    /// Who makes it, in a word or two.
    pub fn maker(self) -> &'static str {
        match self {
            Agent::Claude => "Anthropic",
            Agent::Codex => "OpenAI",
            Agent::OpenCode => "Open source",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Agent::Claude => "Anthropic. Sign in with a Claude subscription or API key.",
            Agent::Codex => "OpenAI. Sign in with ChatGPT or an API key.",
            Agent::OpenCode => "Open source. Claude, ChatGPT, local models and more.",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AiState {
    /// `None` until an assistant has been chosen.
    pub agent: Option<Agent>,
    /// Programs whose crash notifications are muted, by name.
    pub muted: Vec<String>,
    /// Whether the crash watcher is on, which is the default.
    pub crash_capture: bool,
}

/// `vexos-ai status --json`. Fields this build does not read are ignored, and a
/// missing one takes its default.
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct StatusDoc {
    schema: Option<u64>,
    agent: Option<String>,
    crash: CrashDoc,
}

#[derive(Deserialize)]
#[serde(default)]
struct CrashDoc {
    muted: Vec<String>,
    capture: bool,
}

impl Default for CrashDoc {
    fn default() -> Self {
        Self {
            muted: Vec::new(),
            capture: true,
        }
    }
}

impl AiState {
    /// From `vexos-ai status --json`, or from the files it reads if that is unusable.
    pub fn read() -> Self {
        Self::from_command().unwrap_or_else(Self::from_files)
    }

    fn from_command() -> Option<Self> {
        let output = Command::new(CLI)
            .args(["status", "--json"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Self::from_status(std::str::from_utf8(&output.stdout).ok()?)
    }

    /// `None` when the document is not schema 1 or older.
    fn from_status(json: &str) -> Option<Self> {
        let doc: StatusDoc = serde_json::from_str(json).ok()?;
        if doc.schema.is_none_or(|schema| schema > STATUS_SCHEMA) {
            return None;
        }
        Some(Self {
            agent: doc.agent.as_deref().and_then(Agent::parse),
            muted: doc.crash.muted,
            crash_capture: doc.crash.capture,
        })
    }

    fn from_files() -> Self {
        let config = xdg("XDG_CONFIG_HOME", ".config").join("vexos/ai");
        Self {
            agent: first_line(&config.join("agent")).and_then(|a| Agent::parse(&a)),
            muted: dir_names(&config.join("crash-ignore")),
            // The flag file exists while capture is off.
            crash_capture: !config.join("crash-capture-off").exists(),
        }
    }
}

/// Whether an action's command is `vexos-ai`'s, for the exit-code messages.
pub fn is_assistant_action(action: &str) -> bool {
    matches!(action, "agent" | "diagnose") || action.starts_with("ai-")
}

/// The `vexos-ai` command line for an assistant action, from answers the catalog has
/// validated. The settings commands are told never to ask or open a dialog: VexPortal
/// has already asked, and has no terminal to show one in.
pub fn argv(action: &str, named: &BTreeMap<String, String>) -> Option<Vec<String>> {
    let answer = |name: &str| {
        named
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    };
    let mut argv = vec![CLI.to_string()];
    let silent = ["--non-interactive", "--json"];
    match action {
        "agent" => {}
        "diagnose" => {
            argv.push("diagnose".into());
            argv.extend(answer("target").map(str::to_string));
        }
        "ai-pick" => {
            argv.extend(["pick".into(), answer("agent")?.to_string()]);
            argv.extend(silent.map(str::to_string));
        }
        "ai-crash-mute" => {
            argv.extend(["crash-mute".into(), answer("name")?.to_string()]);
            argv.extend(answer("state").map(str::to_string));
            argv.extend(silent.map(str::to_string));
        }
        "ai-crash-capture" => {
            argv.extend(["crash-capture".into(), answer("state")?.to_string()]);
            argv.extend(silent.map(str::to_string));
        }
        _ => return None,
    }
    Some(argv)
}

/// What an exit code of `vexos-ai` means, in words for the person. `subject` names the
/// assistant it was about. `None` for a code with no fixed meaning: the command's own
/// error line says more.
pub fn exit_message(action: &str, subject: &str, code: i32) -> Option<String> {
    match code {
        2 if action == "ai-pick" => Some(format!(
            "This version of vexos-ai does not support {subject} yet. Update VexOS and rebuild."
        )),
        2 => Some("That input was not valid.".into()),
        3 => Some(format!(
            "{subject} is not installed. Turn on the ai feature and rebuild."
        )),
        4 => Some("It needs input that was not given.".into()),
        _ => None,
    }
}

/// What a finished `vexos-ai` printed and how it ended.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub code: i32,
    pub stderr: String,
}

/// Run an assistant settings action as this user and wait for it. The error is a
/// sentence for the person: nothing is shown raw except the command's own one-line
/// error, which `vexos-ai` writes for people.
pub async fn run_action(action: &str, named: &BTreeMap<String, String>) -> Result<(), String> {
    let argv = argv(action, named).ok_or_else(|| format!("`{action}` has no command to run"))?;
    let subject = named
        .get("agent")
        .and_then(|a| Agent::parse(a))
        .map_or("The assistant", Agent::title);
    match run_program(&argv[0], &argv[1..]).await {
        Err(RunError::NotStarted) => {
            Err("vexos-ai is not installed. Turn on the ai feature and rebuild.".into())
        }
        Err(RunError::TimedOut) => Err("vexos-ai did not answer in time and was stopped.".into()),
        Err(RunError::Other(why)) => Err(format!("vexos-ai could not be run: {why}")),
        Ok(done) if done.code == 0 => Ok(()),
        Ok(done) => Err(exit_message(action, subject, done.code).unwrap_or_else(|| {
            let line = done.stderr.lines().next().unwrap_or("").trim();
            if line.is_empty() {
                format!("vexos-ai failed (exit code {}).", done.code)
            } else {
                line.to_string()
            }
        })),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RunError {
    NotStarted,
    TimedOut,
    Other(String),
}

/// Its environment is the user's own plus the instruction not to ask anything.
async fn run_program(program: &str, args: &[String]) -> Result<Outcome, RunError> {
    run_program_within(program, args, DEADLINE).await
}

async fn run_program_within(
    program: &str,
    args: &[String],
    deadline: Duration,
) -> Result<Outcome, RunError> {
    let launcher = gio::SubprocessLauncher::new(
        gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_PIPE,
    );
    launcher.setenv("VEXOS_AI_NONINTERACTIVE", "1", true);
    let mut argv: Vec<&std::ffi::OsStr> = vec![std::ffi::OsStr::new(program)];
    argv.extend(args.iter().map(std::ffi::OsStr::new));
    let process = launcher.spawn(&argv).map_err(|_| RunError::NotStarted)?;

    // The deadline cancels the wait itself, not only the process: a helper that inherited
    // stderr could keep the pipe open after `vexos-ai` is gone.
    let Ok(answered) =
        glib::future_with_timeout(deadline, process.communicate_utf8_future(None)).await
    else {
        process.force_exit();
        return Err(RunError::TimedOut);
    };
    let (_, stderr) = answered.map_err(|e| RunError::Other(e.to_string()))?;
    Ok(Outcome {
        // A command killed by a signal has no exit code.
        code: if process.has_exited() {
            process.exit_status()
        } else {
            -1
        },
        stderr: stderr.map(|s| s.to_string()).unwrap_or_default(),
    })
}

/// `vexos-ai` is on PATH only once the `ai` feature has been built in.
pub fn is_installed() -> bool {
    glib::find_program_in_path(CLI).is_some()
}

/// Built into the running system but not the booted one. Its user services — crash
/// notifications and theme sync — start with a graphical session, and
/// VexOS logs in automatically, so a reboot is what starts them.
pub fn needs_reboot() -> bool {
    needs_reboot_in(
        Path::new("/run/current-system"),
        Path::new("/run/booted-system"),
    )
}

fn needs_reboot_in(current: &Path, booted: &Path) -> bool {
    let bin = Path::new("sw/bin").join(CLI);
    current.join(&bin).exists() && !booted.join(&bin).exists()
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => glib::home_dir().join(fallback),
    }
}

fn first_line(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let line = contents.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// Entry names in `dir`, sorted.
fn dir_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `status --json` as `docs/contract.md` of vexos-ai documents it: the account
    /// keys are still present but fixed, and unknown keys are to be ignored.
    const STATUS: &str = r#"{
        "schema": 1, "version": "1.0.0", "installed": true, "futureKey": {"a": 1},
        "needsReboot": false,
        "agent": "claude",
        "workdir": "/etc/nixos",
        "accounts": [],
        "activeAccount": "main",
        "switching": {"mode": "manual", "threshold": 95},
        "crash": {"capture": true, "muted": ["firefox"]}
    }"#;

    #[test]
    fn builds_the_state_from_status_json() {
        let state = AiState::from_status(STATUS).unwrap();
        assert_eq!(state.agent, Some(Agent::Claude));
        assert_eq!(state.muted, ["firefox"]);
    }

    #[test]
    fn status_json_that_is_unknown_falls_back() {
        assert!(AiState::from_status("not json").is_none());
        assert!(AiState::from_status(r#"{"agent":"claude"}"#).is_none());
        assert!(AiState::from_status(&STATUS.replace("\"schema\": 1", "\"schema\": 2")).is_none());
    }

    #[test]
    fn an_unchosen_agent_is_fine() {
        let state = AiState::from_status(r#"{"schema":1,"agent":null}"#).unwrap();
        assert_eq!(state.agent, None);
        assert!(state.muted.is_empty());
    }

    #[test]
    fn agents_round_trip() {
        for agent in Agent::ALL {
            assert_eq!(Agent::parse(agent.id()), Some(agent));
        }
        assert_eq!(Agent::parse("vim"), None);
    }

    #[test]
    fn all_three_assistants_are_choices_and_anything_else_is_not() {
        assert_eq!(Agent::ALL.len(), 3);
        for (saved, agent) in [
            ("claude", Agent::Claude),
            ("codex", Agent::Codex),
            ("opencode", Agent::OpenCode),
        ] {
            let json = format!(r#"{{"schema":1,"agent":"{saved}"}}"#);
            assert_eq!(AiState::from_status(&json).unwrap().agent, Some(agent));
            assert_eq!(agent.id(), saved);
        }
        let state = AiState::from_status(r#"{"schema":1,"agent":"something-new"}"#).unwrap();
        assert_eq!(state.agent, None);
    }

    #[test]
    fn each_assistant_has_its_own_name_and_blurb() {
        let titles: Vec<&str> = Agent::ALL.iter().map(|a| a.title()).collect();
        assert_eq!(titles, ["Claude Code", "Codex", "OpenCode"]);
        for agent in Agent::ALL {
            assert!(!agent.description().is_empty());
        }
    }

    #[test]
    fn a_reboot_is_needed_only_between_install_and_boot() {
        let root = std::env::temp_dir().join(format!("vexportal-ai-{}", std::process::id()));
        let (current, booted) = (root.join("current"), root.join("booted"));
        let install = |system: &Path| {
            std::fs::create_dir_all(system.join("sw/bin")).unwrap();
            std::fs::write(system.join("sw/bin").join(CLI), "").unwrap();
        };
        assert!(!needs_reboot_in(&current, &booted));
        install(&current);
        assert!(needs_reboot_in(&current, &booted));
        install(&booted);
        assert!(!needs_reboot_in(&current, &booted));
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn named(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn strings(argv: Option<Vec<String>>) -> Vec<String> {
        argv.expect("an argv")
    }

    #[test]
    fn the_assistant_opens_with_no_options_and_diagnose_takes_its_target() {
        assert_eq!(strings(argv("agent", &named(&[]))), ["vexos-ai"]);
        assert_eq!(
            strings(argv("diagnose", &named(&[]))),
            ["vexos-ai", "diagnose"]
        );
        assert_eq!(
            strings(argv("diagnose", &named(&[("target", "rebuild")]))),
            ["vexos-ai", "diagnose", "rebuild"]
        );
    }

    #[test]
    fn the_settings_commands_never_ask_and_always_answer_in_json() {
        assert_eq!(
            strings(argv("ai-pick", &named(&[("agent", "codex")]))),
            ["vexos-ai", "pick", "codex", "--non-interactive", "--json"]
        );
        assert_eq!(
            strings(argv(
                "ai-crash-mute",
                &named(&[("name", "firefox"), ("state", "off")])
            )),
            [
                "vexos-ai",
                "crash-mute",
                "firefox",
                "off",
                "--non-interactive",
                "--json"
            ]
        );
        // The state is optional: vexos-ai mutes when it is left out.
        assert_eq!(
            strings(argv("ai-crash-mute", &named(&[("name", "firefox")]))),
            [
                "vexos-ai",
                "crash-mute",
                "firefox",
                "--non-interactive",
                "--json"
            ]
        );
        assert_eq!(
            strings(argv("ai-crash-capture", &named(&[("state", "off")]))),
            [
                "vexos-ai",
                "crash-capture",
                "off",
                "--non-interactive",
                "--json"
            ]
        );
    }

    #[test]
    fn an_action_missing_what_it_needs_has_no_command() {
        assert_eq!(argv("ai-pick", &named(&[])), None);
        assert_eq!(argv("ai-pick", &named(&[("agent", "")])), None);
        assert_eq!(argv("ai-crash-mute", &named(&[])), None);
        assert_eq!(argv("ai-crash-capture", &named(&[])), None);
        assert_eq!(argv("reboot", &named(&[])), None);
    }

    #[test]
    fn exit_codes_say_which_assistant_is_missing_or_unsupported() {
        let m = |action, code| exit_message(action, "Codex", code);
        assert!(m("ai-pick", 3).unwrap().contains("Codex is not installed"));
        assert!(m("ai-pick", 3).unwrap().contains("rebuild"));
        assert!(m("ai-pick", 2)
            .unwrap()
            .contains("does not support Codex yet"));
        // Only a pick can be refused for the assistant's name.
        assert_eq!(m("ai-crash-mute", 2).unwrap(), "That input was not valid.");
        assert!(m("agent", 4).unwrap().contains("input that was not given"));
        assert_eq!(m("ai-pick", 1), None);
        assert_eq!(m("ai-pick", 5), None);
    }

    #[test]
    fn which_actions_are_the_assistants() {
        for id in [
            "agent",
            "diagnose",
            "ai-pick",
            "ai-crash-mute",
            "ai-crash-capture",
        ] {
            assert!(is_assistant_action(id), "{id}");
        }
        assert!(!is_assistant_action("zfs-pool"));
        assert!(!is_assistant_action("ssh"));
    }

    #[test]
    fn crash_capture_is_on_unless_the_document_says_off() {
        let on = AiState::from_status(r#"{"schema":1,"crash":{"muted":[]}}"#).unwrap();
        assert!(on.crash_capture);
        let none = AiState::from_status(r#"{"schema":1}"#).unwrap();
        assert!(none.crash_capture);
        let off = AiState::from_status(r#"{"schema":1,"crash":{"capture":false}}"#).unwrap();
        assert!(!off.crash_capture);
        assert!(AiState::from_status(STATUS).unwrap().crash_capture);
    }

    /// Run a future to completion on a main context of its own, which is what the
    /// subprocess's exit watch needs.
    fn block<T>(future: impl std::future::Future<Output = T>) -> T {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| context.block_on(future))
            .unwrap()
    }

    fn sh(script: &str) -> Result<Outcome, RunError> {
        block(run_program("sh", &["-c".into(), script.into()]))
    }

    #[test]
    fn a_command_that_fails_reports_its_code_and_its_error_line() {
        let done = sh("echo 'vexos-ai: no such agent' >&2; exit 3").unwrap();
        assert_eq!(done.code, 3);
        assert_eq!(done.stderr.trim(), "vexos-ai: no such agent");
        assert_eq!(sh("exit 0").unwrap().code, 0);
    }

    #[test]
    fn a_helper_holding_the_error_pipe_cannot_hold_the_page_up() {
        let started = std::time::Instant::now();
        let args = ["-c".to_string(), "sleep 20 & exit 0".to_string()];
        let outcome = block(run_program_within("sh", &args, Duration::from_millis(500)));
        assert_eq!(outcome.unwrap_err(), RunError::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn it_is_told_never_to_ask_and_a_missing_program_is_not_started() {
        let done = sh(r#"[ "$VEXOS_AI_NONINTERACTIVE" = 1 ] || exit 9"#).unwrap();
        assert_eq!(done.code, 0);
        let missing = block(run_program("vexportal-no-such-program", &[]));
        assert_eq!(missing.unwrap_err(), RunError::NotStarted);
    }

    #[test]
    fn reads_this_user_without_erroring() {
        AiState::read();
    }
}
