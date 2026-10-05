//! The PIA VPN's live state, from the `vexos-vpn` CLI.
//!
//! `vexos-vpn status --json` and `regions --json` work without root for the users
//! group, so the GUI asks directly instead of going through the daemon. Both run as
//! async subprocesses; the window never waits on them.

use serde::Deserialize;
use std::ffi::OsStr;

const VPN_CLI: &str = "vexos-vpn";

/// `vexos-vpn status --json`. Every field is defaulted: a newer vexos-vpn adding or
/// dropping one should degrade a row, not the page.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VpnStatus {
    /// `connected`, `connecting`, `disconnected` or `error`.
    pub state: String,
    pub region_name: String,
    pub server_cn: String,
    pub protocol: String,
    pub killswitch: bool,
    pub killswitch_mode: String,
    pub region_setting: String,
    pub protocol_setting: String,
    pub logged_in: bool,
    pub last_error: String,
}

impl VpnStatus {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn is_connected(&self) -> bool {
        self.state == "connected"
    }

    /// "Connected to Netherlands", "Disconnected".
    pub fn headline(&self) -> String {
        match self.state.as_str() {
            "connected" if !self.region_name.is_empty() => {
                format!("Connected to {}", self.region_name)
            }
            "connected" => "Connected".to_string(),
            "connecting" => "Connecting…".to_string(),
            "error" => "Connection failed".to_string(),
            _ => "Disconnected".to_string(),
        }
    }
}

/// One entry of `vexos-vpn regions --json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Region {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub latency_s: Option<f64>,
}

impl Region {
    pub fn parse_list(json: &str) -> Result<Vec<Self>, serde_json::Error> {
        serde_json::from_str(json)
    }
}

/// What the VPN page can show.
#[derive(Debug, Clone)]
pub enum Probe<T> {
    /// `vexos-vpn` is not on PATH: the `vpn` feature is off or not yet rebuilt.
    NotInstalled,
    /// It ran but did not produce usable output; the message says why.
    Unavailable(String),
    Ready(T),
}

pub fn is_installed() -> bool {
    glib::find_program_in_path(VPN_CLI).is_some()
}

pub async fn status() -> Probe<VpnStatus> {
    probe(&["status", "--json"], VpnStatus::parse).await
}

pub async fn regions() -> Probe<Vec<Region>> {
    probe(&["regions", "--json"], Region::parse_list).await
}

async fn probe<T>(args: &[&str], parse: impl Fn(&str) -> Result<T, serde_json::Error>) -> Probe<T> {
    if !is_installed() {
        return Probe::NotInstalled;
    }
    let mut argv: Vec<&OsStr> = vec![OsStr::new(VPN_CLI)];
    argv.extend(args.iter().map(OsStr::new));

    let process = match gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE,
    ) {
        Ok(process) => process,
        Err(e) => return Probe::Unavailable(e.to_string()),
    };
    let (stdout, stderr) = match process.communicate_utf8_future(None).await {
        Ok(output) => output,
        Err(e) => return Probe::Unavailable(e.to_string()),
    };
    if !process.is_successful() {
        let message = stderr.map(|s| s.trim().to_string()).unwrap_or_default();
        return Probe::Unavailable(if message.is_empty() {
            format!("`{VPN_CLI} {}` failed", args.join(" "))
        } else {
            message
        });
    }
    match parse(stdout.as_deref().unwrap_or("")) {
        Ok(value) => Probe::Ready(value),
        Err(e) => Probe::Unavailable(format!("unexpected output from {VPN_CLI}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_connected_status() {
        let status = VpnStatus::parse(
            r#"{"state":"connected","region":"nl_amsterdam","region_name":"Netherlands",
               "server_cn":"amsterdam404","protocol":"wireguard","interface":"pia",
               "killswitch":true,"killswitch_mode":"always","rx_bytes":10,"tx_bytes":5,
               "protocol_setting":"wireguard","region_setting":"auto",
               "logged_in":true,"credentials_editable":true,"autoconnect":false}"#,
        )
        .unwrap();
        assert!(status.is_connected());
        assert!(status.killswitch);
        assert_eq!(status.headline(), "Connected to Netherlands");
        assert_eq!(status.region_setting, "auto");
    }

    #[test]
    fn a_sparse_status_still_parses() {
        let status = VpnStatus::parse(r#"{"state":"disconnected"}"#).unwrap();
        assert!(!status.is_connected());
        assert_eq!(status.headline(), "Disconnected");
    }

    #[test]
    fn parses_regions() {
        let regions = Region::parse_list(
            r#"[{"id":"nl_amsterdam","name":"Netherlands","country":"NL","port_forward":true,"latency_s":0.021},
                {"id":"us_east","name":"US East","country":"US","port_forward":false,"latency_s":null}]"#,
        )
        .unwrap();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[1].latency_s, None);
    }
}
