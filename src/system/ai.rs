//! The AI assistant's per-user state, from `vexos-ai status --json`.
//!
//! All of it lives in the logged-in user's own home, so the GUI reads it as that user —
//! the daemon runs as root and would see /root's instead. Plain `status` reads files
//! only and never goes to the network (`--refresh` does, so it is not used here). If
//! the command fails or speaks a schema this build does not know, the files it reads
//! are read directly instead; their paths and fallbacks mirror
//! github:VictoryTek/vexos-ai, `bin/vexos-ai.sh`. The contract is that repo's
//! `docs/contract.md`.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CLI: &str = "vexos-ai";
/// The newest `status --json` schema this build understands.
const STATUS_SCHEMA: u64 = 1;
/// `vexos-ai`'s own default when the threshold file is absent or invalid.
pub const DEFAULT_THRESHOLD: u8 = 95;

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

/// A per-model weekly limit, such as "Opus Weekly".
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Scoped {
    pub label: String,
    pub percent: f64,
}

/// A usage reading: `status --json`'s `usage`, or `usage-<label>.json` in the cache.
/// Unknown keys are ignored.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub ok: bool,
    /// `None` when `ok` is false.
    pub session: Option<f64>,
    pub weekly: Option<f64>,
    pub scoped: Vec<Scoped>,
    /// With `ok`: the numbers are the last good ones, and `why` says why they could
    /// not be refreshed.
    pub stale: bool,
    pub why: Option<String>,
}

impl Usage {
    /// "Session 12% · weekly 40%", marked stale with the reason when it is; or why it
    /// is unknown.
    pub fn summary(&self) -> String {
        let why = self.why.as_deref().filter(|w| !w.is_empty());
        if !self.ok {
            return match why {
                Some(why) => format!("Usage unknown: {why}"),
                None => "Usage unknown".to_string(),
            };
        }
        let percent = |p: Option<f64>| p.map_or("?".to_string(), |p| format!("{}%", p.floor()));
        let mut line = format!(
            "Session {} · weekly {}",
            percent(self.session),
            percent(self.weekly)
        );
        for limit in &self.scoped {
            line.push_str(&format!(" · {} {}%", limit.label, limit.percent.floor()));
        }
        if self.stale {
            match why {
                Some(why) => line.push_str(&format!(" (stale: {why})")),
                None => line.push_str(" (stale)"),
            }
        }
        line
    }
}

#[derive(Debug, Clone)]
pub struct Account {
    pub label: String,
    /// False only when the account's refresh token is gone.
    pub signed_in: bool,
    /// "Max 5x", "Pro"; `None` when `vexos-ai` does not know it.
    pub plan: Option<String>,
    /// `None` until `vexos-ai` has cached a reading for this account.
    pub usage: Option<Usage>,
}

impl Account {
    /// The plan, a signed-out notice if it applies, and the usage, a line each.
    pub fn subtitle(&self) -> String {
        let mut lines = Vec::new();
        if let Some(plan) = &self.plan {
            lines.push(plan.clone());
        }
        if !self.signed_in {
            lines.push("Signed out. Sign in again to use this account.".to_string());
        }
        lines.push(
            self.usage
                .as_ref()
                .map_or("No usage reading yet".to_string(), Usage::summary),
        );
        lines.join("\n")
    }
}

#[derive(Debug, Clone)]
pub struct AiState {
    /// `None` until an assistant has been chosen.
    pub agent: Option<Agent>,
    /// `main` first, then the extra accounts by name.
    pub accounts: Vec<Account>,
    pub active: String,
    pub auto_switch: bool,
    pub threshold: u8,
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
    accounts: Vec<AccountDoc>,
    active_account: Option<String>,
    switching: SwitchingDoc,
    crash: CrashDoc,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountDoc {
    label: String,
    #[serde(default = "signed_in")]
    signed_in: bool,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

fn signed_in() -> bool {
    true
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SwitchingDoc {
    mode: Option<String>,
    threshold: Option<u64>,
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

    /// `None` when the document is not schema 1 or older, or names no accounts.
    fn from_status(json: &str) -> Option<Self> {
        let doc: StatusDoc = serde_json::from_str(json).ok()?;
        if !doc.schema.is_some_and(|schema| schema <= STATUS_SCHEMA) || doc.accounts.is_empty() {
            return None;
        }
        let labels: Vec<String> = doc.accounts.iter().map(|a| a.label.clone()).collect();
        Some(Self {
            agent: doc.agent.as_deref().and_then(Agent::parse),
            active: active_account(doc.active_account, &labels),
            accounts: doc
                .accounts
                .into_iter()
                .map(|a| Account {
                    label: a.label,
                    signed_in: a.signed_in,
                    plan: a.plan,
                    usage: a.usage,
                })
                .collect(),
            auto_switch: doc.switching.mode.as_deref() == Some("auto"),
            threshold: doc
                .switching
                .threshold
                .and_then(|t| u8::try_from(t).ok())
                .filter(|t| (1..=100).contains(t))
                .unwrap_or(DEFAULT_THRESHOLD),
            muted: doc.crash.muted,
        })
    }

    fn from_files() -> Self {
        let config = xdg("XDG_CONFIG_HOME", ".config").join("vexos/ai");
        let data = xdg("XDG_DATA_HOME", ".local/share").join("vexos/ai/claude-accounts");
        let cache = xdg("XDG_CACHE_HOME", ".cache").join("vexos/ai");

        let mut extra = dir_names(&data, true);
        extra.retain(|label| label != "main");
        let mut labels = vec!["main".to_string()];
        labels.extend(extra);

        let accounts = labels
            .iter()
            .map(|label| Account {
                label: label.clone(),
                signed_in: true,
                plan: None,
                usage: std::fs::read_to_string(cache.join(format!("usage-{label}.json")))
                    .ok()
                    .and_then(|json| serde_json::from_str(&json).ok()),
            })
            .collect();

        Self {
            agent: first_line(&config.join("agent")).and_then(|a| Agent::parse(&a)),
            active: active_account(first_line(&config.join("claude-account")), &labels),
            accounts,
            auto_switch: first_line(&config.join("mode")).as_deref() == Some("auto"),
            threshold: parse_threshold(first_line(&config.join("threshold")).as_deref()),
            muted: dir_names(&config.join("crash-ignore"), false),
        }
    }
}

/// `vexos-ai` is on PATH only once the `ai` feature has been built in.
pub fn is_installed() -> bool {
    glib::find_program_in_path(CLI).is_some()
}

/// Built into the running system but not the booted one. Its user services — crash
/// notifications, usage warnings, theme sync — start with a graphical session, and
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

/// Entry names in `dir`, sorted; only directories when `dirs` is set.
fn dir_names(dir: &Path, dirs: bool) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| !dirs || e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// The saved account, if it still exists; `main` otherwise, as `vexos-ai` resolves it.
fn active_account(saved: Option<String>, labels: &[String]) -> String {
    saved
        .filter(|a| labels.contains(a))
        .unwrap_or_else(|| "main".to_string())
}

fn parse_threshold(saved: Option<&str>) -> u8 {
    saved
        .and_then(|t| t.parse::<u8>().ok())
        .filter(|t| (1..=100).contains(t))
        .unwrap_or(DEFAULT_THRESHOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_usage_reading() {
        let usage: Usage = serde_json::from_str(
            r#"{"ok":true,"session":12.7,"sessionReset":"x","weekly":40,"weeklyReset":"y"}"#,
        )
        .unwrap();
        assert_eq!(usage.summary(), "Session 12% · weekly 40%");
    }

    #[test]
    fn a_stale_reading_keeps_its_numbers_and_says_why() {
        let usage: Usage = serde_json::from_str(
            r#"{"ok":true,"session":12.0,"weekly":40.0,"stale":true,"why":"rate limited",
                "scoped":[{"label":"Opus Weekly","percent":30.5,"resetsAt":"x"}]}"#,
        )
        .unwrap();
        assert_eq!(
            usage.summary(),
            "Session 12% · weekly 40% · Opus Weekly 30% (stale: rate limited)"
        );
    }

    const STATUS: &str = r#"{
        "schema": 1, "version": "1.0.0", "installed": true, "futureKey": {"a": 1},
        "agent": "claude",
        "accounts": [
            {"label": "main", "active": false, "signedIn": true, "plan": "Max 5x",
             "inUse": false,
             "usage": {"ok": true, "session": 12.0, "weekly": 40.0, "scoped": [],
                       "stale": false, "why": null}},
            {"label": "work", "active": true, "signedIn": false, "plan": null,
             "usage": {"ok": false, "session": null, "weekly": null, "why": "no token"}}
        ],
        "activeAccount": "work",
        "switching": {"mode": "auto", "threshold": 80},
        "crash": {"capture": true, "muted": ["firefox"]}
    }"#;

    #[test]
    fn builds_the_state_from_status_json() {
        let state = AiState::from_status(STATUS).unwrap();
        assert_eq!(state.agent, Some(Agent::Claude));
        assert_eq!(state.active, "work");
        assert!(state.auto_switch);
        assert_eq!(state.threshold, 80);
        assert_eq!(state.muted, ["firefox"]);
        assert_eq!(
            state.accounts[0].subtitle(),
            "Max 5x\nSession 12% · weekly 40%"
        );
        assert_eq!(
            state.accounts[1].subtitle(),
            "Signed out. Sign in again to use this account.\nUsage unknown: no token"
        );
    }

    #[test]
    fn status_json_that_is_unknown_falls_back() {
        assert!(AiState::from_status("not json").is_none());
        assert!(AiState::from_status(r#"{"accounts":[{"label":"main"}]}"#).is_none());
        assert!(AiState::from_status(&STATUS.replace("\"schema\": 1", "\"schema\": 2")).is_none());
        assert!(AiState::from_status(r#"{"schema":1,"accounts":[]}"#).is_none());
    }

    #[test]
    fn an_unchosen_agent_and_missing_usage_are_fine() {
        let state = AiState::from_status(
            r#"{"schema":1,"agent":null,"accounts":[{"label":"main","usage":null}]}"#,
        )
        .unwrap();
        assert_eq!(state.agent, None);
        assert_eq!(state.threshold, DEFAULT_THRESHOLD);
        assert_eq!(state.accounts[0].subtitle(), "No usage reading yet");
    }

    #[test]
    fn an_unknown_reading_says_why() {
        let usage: Usage =
            serde_json::from_str(r#"{"ok":false,"why":"usage endpoint unavailable"}"#).unwrap();
        assert_eq!(usage.summary(), "Usage unknown: usage endpoint unavailable");
    }

    #[test]
    fn the_active_account_falls_back_to_main() {
        let labels = ["main".to_string(), "work".to_string()];
        assert_eq!(active_account(Some("work".into()), &labels), "work");
        assert_eq!(active_account(Some("gone".into()), &labels), "main");
        assert_eq!(active_account(None, &labels), "main");
    }

    #[test]
    fn thresholds_out_of_range_use_the_default() {
        assert_eq!(parse_threshold(Some("80")), 80);
        assert_eq!(parse_threshold(Some("0")), DEFAULT_THRESHOLD);
        assert_eq!(parse_threshold(Some("101")), DEFAULT_THRESHOLD);
        assert_eq!(parse_threshold(Some("x")), DEFAULT_THRESHOLD);
        assert_eq!(parse_threshold(None), DEFAULT_THRESHOLD);
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
        let state = AiState::read();
        assert_eq!(state.accounts[0].label, "main");
    }
}
