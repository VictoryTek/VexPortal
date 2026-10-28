//! The AI assistant's per-user state, from `vexos-ai status --json`.
//!
//! All of it lives in the logged-in user's own home, so the GUI reads it as that user —
//! the daemon runs as root and would see /root's instead. `status` reads files
//! only and never goes to the network. If the command fails or speaks a schema this
//! build does not know, the files it reads are read directly instead; their paths and
//! fallbacks mirror github:VictoryTek/vexos-ai, `bin/vexos-ai.sh`. The contract is
//! that repo's `docs/contract.md`.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CLI: &str = "vexos-ai";
/// The newest `status --json` schema this build understands.
const STATUS_SCHEMA: u64 = 1;

/// Which coding agent `vexos-ai` opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    OpenCode,
}

impl Agent {
    pub const ALL: [Agent; 2] = [Agent::Claude, Agent::OpenCode];

    fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Agent::Claude),
            "opencode" => Some(Agent::OpenCode),
            _ => None,
        }
    }

    /// The value `ai-pick` takes.
    pub fn id(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::OpenCode => "opencode",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::OpenCode => "OpenCode",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Agent::Claude => "Anthropic. Sign in with a Claude subscription or API key.",
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

#[derive(Deserialize, Default)]
#[serde(default)]
struct CrashDoc {
    muted: Vec<String>,
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
        })
    }

    fn from_files() -> Self {
        let config = xdg("XDG_CONFIG_HOME", ".config").join("vexos/ai");
        Self {
            agent: first_line(&config.join("agent")).and_then(|a| Agent::parse(&a)),
            muted: dir_names(&config.join("crash-ignore")),
        }
    }
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

    #[test]
    fn reads_this_user_without_erroring() {
        AiState::read();
    }
}
