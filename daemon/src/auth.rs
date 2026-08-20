//! polkit authorization.
//!
//! The daemon asks polkit about the *calling* process, identified by its unique bus
//! name, so authorization is decided against the real user behind the D-Bus call
//! rather than anything the caller tells us about itself.

use log::{info, warn};
use std::collections::HashMap;
use zbus::Connection;

/// Ask polkit whether `caller` may perform `action`.
///
/// `AllowUserInteraction` is set so the desktop's polkit agent can put up the password
/// prompt; without it a `auth_admin` action would simply be denied with no way for the
/// user to authenticate.
pub async fn check(connection: &Connection, caller: &str, action: &str) -> Result<bool, String> {
    let subject = (
        "system-bus-name",
        HashMap::from([("name", zbus::zvariant::Value::from(caller))]),
    );

    let polkit = PolkitAuthorityProxy::new(connection)
        .await
        .map_err(|e| format!("could not reach polkit: {e}"))?;

    const ALLOW_USER_INTERACTION: u32 = 1;
    let result = polkit
        .check_authorization(
            &subject,
            action,
            &HashMap::new(),
            ALLOW_USER_INTERACTION,
            "",
        )
        .await
        .map_err(|e| format!("polkit CheckAuthorization failed: {e}"))?;

    if result.is_authorized {
        info!("polkit allowed {action} for {caller}");
    } else {
        warn!("polkit denied {action} for {caller}");
    }
    Ok(result.is_authorized)
}

#[zbus::proxy(
    interface = "org.freedesktop.PolicyKit1.Authority",
    default_service = "org.freedesktop.PolicyKit1",
    default_path = "/org/freedesktop/PolicyKit1/Authority"
)]
trait PolkitAuthority {
    async fn check_authorization(
        &self,
        subject: &(&str, HashMap<&str, zbus::zvariant::Value<'_>>),
        action_id: &str,
        details: &HashMap<&str, &str>,
        flags: u32,
        cancellation_id: &str,
    ) -> zbus::Result<AuthorizationResult>;
}

#[derive(Debug, serde::Deserialize, zbus::zvariant::Type)]
pub struct AuthorizationResult {
    pub is_authorized: bool,
    #[allow(dead_code)]
    pub is_challenge: bool,
    #[allow(dead_code)]
    pub details: HashMap<String, String>,
}
