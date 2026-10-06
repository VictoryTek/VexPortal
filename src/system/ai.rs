//! The AI assistant's per-user state, from the files `vexos-ai` keeps.
//!
//! All of it lives in the logged-in user's own home, so the GUI reads it directly —
//! the daemon runs as root and would see /root's instead. The paths and fallbacks
//! mirror vexos-nix's `pkgs/vexos-ai/vexos-ai.sh`.

use serde::Deserialize;
use std::path::{Path, PathBuf};

const CLI: &str = "vexos-ai";
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

/// `usage-<label>.json`, as `vexos-ai`'s `probe_usage` writes it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub ok: bool,
    pub session: f64,
    pub weekly: f64,
    pub why: String,
}

impl Usage {
    /// "Session 12% · weekly 40%", or why it is unknown.
    pub fn summary(&self) -> String {
        if self.ok {
            format!(
                "Session {}% · weekly {}%",
                self.session.floor(),
                self.weekly.floor()
            )
        } else if self.why.is_empty() {
            "Usage unknown".to_string()
        } else {
            format!("Usage unknown: {}", self.why)
        }
    }
}

#[derive(Debug, Clone)]
pub struct Account {
    pub label: String,
    /// `None` until `vexos-ai` has cached a reading for this account.
    pub usage: Option<Usage>,
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

impl AiState {
    pub fn read() -> Self {
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
