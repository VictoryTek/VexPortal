//! Tailscale setup, split across the two accounts it needs.
//!
//! `tailscale up` has to run as the person who will use Tailscale, because it signs in
//! their session. Letting that person run the `tailscale` command without `sudo` is a root
//! job: `tailscale set --operator=<user>`. This module does the root half and holds the
//! parsers the portal needs for the rest (what `tailscale status --json` says, and the
//! sign-in link `tailscale up` prints); the GUI runs `tailscale up` as the user.

use crate::runner::{Cmd, RunError, Runner};
use crate::{capture, fail, stream, Ctx};

/// Where Tailscale is in its life, from `tailscale status --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    Running,
    /// Not signed in. `auth_url` may hold the link to sign in with.
    NeedsLogin,
    /// Signed in, waiting for an admin to approve this machine.
    NeedsMachineAuth,
    Stopped,
    Starting,
    /// A state this code does not know, kept as text rather than guessed at.
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub backend: Backend,
    /// The sign-in link, only while one is waiting for the person.
    pub auth_url: Option<String>,
    pub tailnet: Option<String>,
    pub host: Option<String>,
    pub ips: Vec<String>,
}

/// Read `tailscale status --json`. `None` if it is not that document. Every field except
/// the state may be absent, so a newer Tailscale adding or dropping one degrades a row.
pub fn parse_status(json: &str) -> Option<Status> {
    let doc: serde_json::Value = serde_json::from_str(json).ok()?;
    let backend = match doc.get("BackendState")?.as_str()? {
        "Running" => Backend::Running,
        "NeedsLogin" => Backend::NeedsLogin,
        "NeedsMachineAuth" => Backend::NeedsMachineAuth,
        "Stopped" => Backend::Stopped,
        "Starting" => Backend::Starting,
        other => Backend::Other(other.to_string()),
    };
    let text = |pointer: &str| {
        doc.pointer(pointer)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(Status {
        backend,
        auth_url: text("/AuthURL").filter(|u| is_safe_https_url(u)),
        tailnet: text("/CurrentTailnet/Name"),
        host: text("/Self/HostName"),
        ips: doc
            .get("TailscaleIPs")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// A link safe to show and open: https, one token of printable ASCII, no quotes or
/// angle brackets. The text came from another program, so it is checked, not trusted.
pub fn is_safe_https_url(url: &str) -> bool {
    url.len() <= 2048
        && url.starts_with("https://")
        && url.len() > "https://".len()
        && url
            .chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '"' | '\'' | '<' | '>' | '`' | '\\'))
}

/// The account name in a `getent passwd` line, `name:x:uid:...`, if it belongs to `uid`.
fn user_from_passwd(line: &str, uid: u32) -> Option<String> {
    let mut fields = line.trim().split(':');
    let name = fields.next()?;
    let _password = fields.next()?;
    let found: u32 = fields.next()?.parse().ok()?;
    (found == uid && valid_username(name)).then(|| name.to_string())
}

/// A login name as Linux allows them in practice. It becomes part of one argument, so
/// anything else is refused rather than passed on.
fn valid_username(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
}

/// `tailscale-operator`: let the account that asked run `tailscale` without `sudo`.
pub async fn operator<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let sink = ctx.sink;

    // Root's own account needs no operator rights, and an operator that is root would
    // hide that the person never set theirs.
    let Some(uid) = ctx.caller_uid else {
        return fail(
            sink,
            "could not tell which account asked, so no operator was set",
        );
    };
    if uid == 0 {
        return fail(
            sink,
            "run this from your own account, not root: the operator is the person who uses Tailscale",
        );
    }

    match ctx.runner.run(&Cmd::new("tailscale").arg("version")).await {
        Ok(out) if out.success() => {}
        Ok(out) => return fail(sink, format!("tailscale: {}", out.stderr.trim())),
        Err(RunError::Cancelled) => return fail(sink, "cancelled"),
        Err(_) => {
            return fail(
                sink,
                "tailscale is not installed. Add services.tailscale.enable = true; to your NixOS \
                 configuration and rebuild.",
            )
        }
    }

    let active = match capture(
        ctx,
        &Cmd::new("systemctl").args(["is-active", "--quiet", "tailscaled"]),
    )
    .await
    {
        Ok(out) => out.success(),
        Err(code) => return code,
    };
    if !active {
        return fail(
            sink,
            "tailscaled is not running. Add services.tailscale.enable = true; to your NixOS \
             configuration and rebuild.",
        );
    }

    // NSS knows accounts that /etc/passwd does not (LDAP, systemd-homed), so ask it.
    let entry = match capture(ctx, &Cmd::new("getent").args(["passwd", &uid.to_string()])).await {
        Ok(out) if out.success() => out.stdout,
        Ok(_) => return fail(sink, format!("no account has the id {uid}")),
        Err(code) => return code,
    };
    let Some(user) = user_from_passwd(&entry, uid) else {
        return fail(
            sink,
            format!("could not read a usable account name for the id {uid}"),
        );
    };

    sink.info(&format!("Letting {user} control Tailscale without sudo…"));
    let set = Cmd::new("tailscale").args(["set".to_string(), format!("--operator={user}")]);
    match stream(ctx, &set).await {
        Ok(0) => {
            sink.info(&format!("✓ {user} is now the Tailscale operator."));
            0
        }
        Ok(code) | Err(code) => code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::testing::{block_on, FakeRunner};
    use crate::runner::Output;
    use crate::testkit::Kit;

    #[test]
    fn a_running_status_is_read() {
        let status = parse_status(
            r#"{"BackendState":"Running","AuthURL":"","TailscaleIPs":["100.64.0.1","fd7a::1"],
                "Self":{"HostName":"box","Online":true},"CurrentTailnet":{"Name":"me@example.com"}}"#,
        )
        .unwrap();
        assert_eq!(status.backend, Backend::Running);
        assert_eq!(status.auth_url, None);
        assert_eq!(status.host.as_deref(), Some("box"));
        assert_eq!(status.tailnet.as_deref(), Some("me@example.com"));
        assert_eq!(status.ips, ["100.64.0.1", "fd7a::1"]);
    }

    #[test]
    fn a_status_waiting_for_sign_in_carries_the_link() {
        let status = parse_status(
            r#"{"BackendState":"NeedsLogin","AuthURL":"https://login.tailscale.com/a/abc123"}"#,
        )
        .unwrap();
        assert_eq!(status.backend, Backend::NeedsLogin);
        assert_eq!(
            status.auth_url.as_deref(),
            Some("https://login.tailscale.com/a/abc123")
        );
    }

    #[test]
    fn an_unsafe_link_in_the_status_is_dropped() {
        for bad in [
            "http://login.tailscale.com/a/x",
            "javascript:alert(1)",
            "https://x\"onload=\"y",
            "https://a b",
            "https://",
            "https://x/\n",
        ] {
            let json = format!(
                r#"{{"BackendState":"NeedsLogin","AuthURL":{}}}"#,
                serde_json::json!(bad)
            );
            assert_eq!(parse_status(&json).unwrap().auth_url, None, "{bad:?}");
        }
    }

    #[test]
    fn odd_states_and_documents_are_handled() {
        assert_eq!(
            parse_status(r#"{"BackendState":"Whatever"}"#)
                .unwrap()
                .backend,
            Backend::Other("Whatever".into())
        );
        assert_eq!(
            parse_status(r#"{"BackendState":"Stopped"}"#)
                .unwrap()
                .backend,
            Backend::Stopped
        );
        assert!(parse_status("not json").is_none());
        assert!(parse_status("{}").is_none());
    }

    #[test]
    fn account_names_come_from_the_matching_passwd_line_only() {
        let line = "alice:x:1000:1000:Alice:/home/alice:/bin/bash\n";
        assert_eq!(user_from_passwd(line, 1000).as_deref(), Some("alice"));
        assert_eq!(user_from_passwd(line, 1001), None);
        assert_eq!(
            user_from_passwd("al ice:x:1000:1000::/:/bin/sh", 1000),
            None
        );
        assert_eq!(user_from_passwd("-flag:x:1000:1000::/:/bin/sh", 1000), None);
        assert_eq!(user_from_passwd("alice:x:abc:1::/:/bin/sh", 1000), None);
    }

    fn out(text: &str) -> Output {
        Output {
            code: 0,
            stdout: text.into(),
            stderr: String::new(),
        }
    }

    fn ready(uid: u32) -> (Kit, FakeRunner) {
        let mut kit = Kit::new();
        kit.caller = Some(uid);
        let runner = FakeRunner::new()
            .ok("tailscale", &["version"])
            .ok("systemctl", &["is-active"])
            .on(
                "getent",
                &["passwd"],
                out(&format!("alice:x:{uid}:{uid}::/home/alice:/bin/bash\n")),
            )
            .ok("tailscale", &["set"]);
        (kit, runner)
    }

    #[test]
    fn the_operator_is_the_account_that_asked() {
        let (kit, runner) = ready(1000);
        assert_eq!(block_on(operator(&kit.ctx(&runner))), 0);
        assert!(runner
            .lines()
            .contains(&"tailscale set --operator=alice".to_string()));
        assert!(runner.lines().contains(&"getent passwd 1000".to_string()));
    }

    #[test]
    fn nothing_is_set_without_a_known_non_root_caller() {
        let kit = Kit::new();
        let runner = FakeRunner::new();
        assert_eq!(block_on(operator(&kit.ctx(&runner))), 1);
        assert!(runner.calls().is_empty());

        let (kit, runner) = ready(0);
        assert_eq!(block_on(operator(&kit.ctx(&runner))), 1);
        assert!(kit.sink.text().contains("not root"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn a_missing_or_stopped_tailscale_says_how_to_get_it() {
        let (kit, _) = ready(1000);
        let missing = FakeRunner::new();
        assert_eq!(block_on(operator(&kit.ctx(&missing))), 1);
        assert!(kit.sink.text().contains("not installed"));

        let (kit, _) = ready(1000);
        let stopped = FakeRunner::new().ok("tailscale", &["version"]).failing(
            "systemctl",
            &["is-active"],
            3,
            "",
        );
        assert_eq!(block_on(operator(&kit.ctx(&stopped))), 1);
        assert!(kit.sink.text().contains("tailscaled is not running"));
        assert!(stopped
            .lines()
            .iter()
            .all(|l| !l.starts_with("tailscale set")));
    }

    #[test]
    fn a_failed_set_is_a_failed_job() {
        let (kit, _) = ready(1000);
        let runner = FakeRunner::new()
            .ok("tailscale", &["version"])
            .ok("systemctl", &["is-active"])
            .on(
                "getent",
                &["passwd"],
                out("alice:x:1000:1000::/h:/bin/sh\n"),
            )
            .failing("tailscale", &["set"], 1, "permission denied");
        assert_eq!(block_on(operator(&kit.ctx(&runner))), 1);
    }

    #[test]
    fn an_account_name_that_cannot_be_trusted_is_refused() {
        let (kit, _) = ready(1000);
        let runner = FakeRunner::new()
            .ok("tailscale", &["version"])
            .ok("systemctl", &["is-active"])
            .on(
                "getent",
                &["passwd"],
                out("al;ice:x:1000:1000::/h:/bin/sh\n"),
            )
            .ok("tailscale", &["set"]);
        assert_eq!(block_on(operator(&kit.ctx(&runner))), 1);
        assert!(runner
            .lines()
            .iter()
            .all(|l| !l.starts_with("tailscale set")));
    }
}
