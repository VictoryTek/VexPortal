//! Server services: the table of what VexOS offers, which of them are on, how healthy
//! they are, and the operations that change them.
//!
//! The table is `services.toml`, built in. It replaces the lists the justfile kept
//! (`_service_catalog`, `_service_runtime`, the public-URL table and the setup notes), so
//! the GUI and the daemon read one definition.
//!
//! What is *enabled* is a line in `/etc/nixos/server-services.nix`
//! (`vexos.server.<name>.enable = true;`), the file `just service enable` also writes, so
//! the two paths stay interchangeable. Turning a service on or off changes that file; it
//! takes effect on the next rebuild.

use crate::fsutil;
use crate::nixedit::{self, EditError, NixValue};
use crate::runner::{Cmd, Runner};
use crate::system::read_variant;
use crate::{capture, fail, features, stream, Ctx};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const SERVICES_FILE: &str = "server-services.nix";

/// A file this host did not have before. The recipe copied the full commented template;
/// the options are all documented in the module, so an empty set is enough.
const SERVICES_SEED: &str =
    "# /etc/nixos/server-services.nix\n# Server services for this VexOS host.\n{\n}\n";

const TABLE: &str = include_str!("services.toml");

/// The storage a service is best on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    /// VM disks: a local ZFS pool.
    ZfsVm,
    /// A database and many small files: a ZFS dataset.
    ZfsLive,
    /// A large media library: a mergerfs and SnapRAID pool.
    MergerfsBulk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UrlHost {
    Hostname,
    Localhost,
    /// No usable default, so the address is required.
    None,
}

/// A service whose module has an option for the address people type to reach it. Behind a
/// reverse proxy the `http://<host>:<port>` default breaks the app (rejected origins,
/// bad links), so enabling asks for it.
#[derive(Debug, Clone, Deserialize)]
pub struct PublicUrl {
    /// The option under `vexos.server.<name>.`
    pub option: String,
    pub port: u16,
    pub host: UrlHost,
    /// The module's default is an invalid placeholder guarded by a build assertion.
    pub required: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Service {
    pub name: String,
    pub group: String,
    pub description: String,
    /// The option path under `vexos.server.` when it is not the service name.
    #[serde(default)]
    option: Option<String>,
    #[serde(default)]
    pub tier: Option<Tier>,
    /// systemd units, without `.service`. Those missing on a host are skipped.
    #[serde(default)]
    pub units: Vec<String>,
    /// Addresses that should answer when the service is up.
    #[serde(default)]
    pub health: Vec<String>,
    #[serde(default)]
    pub public_url: Option<PublicUrl>,
    /// `Label: text` lines. `<server-ip>` stands for this host.
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Deserialize)]
struct Table {
    service: Vec<Service>,
}

/// Every service, in the order the Services page lists them.
pub fn all() -> &'static [Service] {
    static SERVICES: OnceLock<Vec<Service>> = OnceLock::new();
    SERVICES.get_or_init(|| {
        // The table is compiled in and a test parses it, so this cannot fail in a build
        // that passed its tests.
        toml::from_str::<Table>(TABLE)
            .expect("ops/src/services.toml is built in and checked by a test")
            .service
    })
}

pub fn find(name: &str) -> Option<&'static Service> {
    all().iter().find(|s| s.name == name)
}

pub fn names() -> Vec<&'static str> {
    all().iter().map(|s| s.name.as_str()).collect()
}

/// The arr components the `vexos.server.arr.enable` meta-flag turns on by default.
const ARR_META: [&str; 5] = ["sabnzbd", "sonarr", "radarr", "lidarr", "prowlarr"];

impl Service {
    /// `vexos.server.<option>.<leaf>`, for this service's own options.
    pub fn key(&self, leaf: &str) -> String {
        format!(
            "vexos.server.{}.{leaf}",
            self.option.as_deref().unwrap_or(&self.name)
        )
    }

    pub fn enable_key(&self) -> String {
        self.key("enable")
    }

    /// The notes as (label, text) pairs; a line with no label has an empty one.
    pub fn notes(&self) -> impl Iterator<Item = (&str, &str)> {
        self.notes.iter().map(|line| match line.split_once(": ") {
            Some((label, text)) => (label, text),
            None => ("", line.as_str()),
        })
    }

    /// The systemd unit names, as `<unit>.service`.
    pub fn unit_names(&self) -> Vec<String> {
        self.units.iter().map(|u| format!("{u}.service")).collect()
    }
}

/// Whether the service is on in `server-services.nix`.
///
/// An explicit line wins, so `= false` turns off a component the arr meta-flag would
/// otherwise enable. (The recipe called such a component enabled.)
pub fn is_enabled(text: &str, service: &Service) -> bool {
    match nixedit::get_bool(text, &service.enable_key()) {
        Some(on) => on,
        None => {
            ARR_META.contains(&service.name.as_str())
                && nixedit::get_bool(text, "vexos.server.arr.enable") == Some(true)
        }
    }
}

// ─────────────────────────────── Health ────────────────────────────────

/// One systemd unit as `systemctl show` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitInfo {
    /// Without `.service`.
    pub id: String,
    pub load: String,
    pub active: String,
    pub sub: String,
}

/// The arguments that make `systemctl show` describe each unit. A unit that does not
/// exist is still answered, with `LoadState=not-found`.
pub fn show_args(units: &[String]) -> Vec<String> {
    let mut args = vec![
        "show".to_string(),
        "--property=Id,LoadState,ActiveState,SubState".to_string(),
        "--".to_string(),
    ];
    args.extend(units.iter().cloned());
    args
}

/// Read `systemctl show` output for several units: `Key=Value` lines, a blank line
/// between units.
pub fn parse_show(output: &str) -> Vec<UnitInfo> {
    let mut units = Vec::new();
    for block in output.split("\n\n") {
        let (mut id, mut load, mut active, mut sub) = (None, None, None, None);
        for line in block.lines() {
            match line.split_once('=') {
                Some(("Id", v)) => id = Some(v.trim().to_string()),
                Some(("LoadState", v)) => load = Some(v.trim().to_string()),
                Some(("ActiveState", v)) => active = Some(v.trim().to_string()),
                Some(("SubState", v)) => sub = Some(v.trim().to_string()),
                _ => {}
            }
        }
        if let Some(id) = id {
            units.push(UnitInfo {
                id: id.strip_suffix(".service").unwrap_or(&id).to_string(),
                load: load.unwrap_or_default(),
                active: active.unwrap_or_default(),
                sub: sub.unwrap_or_default(),
            });
        }
    }
    units
}

/// How a service as a whole is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Enabled in the config but none of its units exist yet: not rebuilt.
    NotBuilt,
    Running,
    Stopped,
    Failed,
    /// Some units are up and some are not.
    Partial,
}

impl Health {
    pub fn label(self) -> &'static str {
        match self {
            Health::NotBuilt => "Not built",
            Health::Running => "Running",
            Health::Stopped => "Stopped",
            Health::Failed => "Failed",
            Health::Partial => "Partial",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub health: Health,
    /// Only the units that exist on this host.
    pub units: Vec<UnitInfo>,
}

impl Report {
    /// The units that are not up, as `unit (state)`.
    pub fn down(&self) -> Vec<String> {
        self.units
            .iter()
            .filter(|u| u.active != "active")
            .map(|u| format!("{} ({})", u.id, u.active))
            .collect()
    }
}

/// Judge a service from its units' states. Units that do not exist are skipped, because
/// some services list both a docker- and a podman- unit, or an optional one.
pub fn summarize(infos: &[UnitInfo]) -> Report {
    let units: Vec<UnitInfo> = infos
        .iter()
        .filter(|u| u.load != "not-found")
        .cloned()
        .collect();
    let health = if units.is_empty() {
        Health::NotBuilt
    } else if units.iter().any(|u| u.active == "failed") {
        Health::Failed
    } else if units.iter().all(|u| u.active == "active") {
        Health::Running
    } else if units.iter().all(|u| u.active != "active") {
        Health::Stopped
    } else {
        Health::Partial
    };
    Report { health, units }
}

/// The report for one service out of a `systemctl show` over several.
pub fn report_for(service: &Service, infos: &[UnitInfo]) -> Report {
    let mine: Vec<UnitInfo> = infos
        .iter()
        .filter(|u| service.units.contains(&u.id))
        .cloned()
        .collect();
    summarize(&mine)
}

/// The result of asking a service's address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Answered with a status below 400.
    Ok(u16),
    /// Answered with an error status.
    Error(u16),
    Unreachable,
}

/// The `curl` arguments for one probe: the status code only, certificates not checked
/// (a few services use self-signed ones), three seconds at most.
pub fn probe_args(url: &str) -> Vec<String> {
    [
        "-o",
        "/dev/null",
        "-s",
        "-k",
        "-w",
        "%{http_code}",
        "--max-time",
        "3",
        url,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Read a probe's output. curl prints `000` when nothing answered.
pub fn reach(output: &str) -> Reach {
    match output.trim().parse::<u16>() {
        Ok(0) | Err(_) => Reach::Unreachable,
        Ok(code) if code < 400 => Reach::Ok(code),
        Ok(code) => Reach::Error(code),
    }
}

// ───────────────────────────── Storage advice ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierState {
    LocalPool,
    RemoteMounted,
    Missing,
}

impl Tier {
    pub fn advice(self, service: &str) -> String {
        match self {
            Tier::ZfsVm => format!("{service} stores VM disks, so it needs a local ZFS pool."),
            Tier::ZfsLive => format!(
                "{service} stores a database and many small files, so it is best on a ZFS dataset."
            ),
            Tier::MergerfsBulk => format!(
                "{service} stores a large media library, so it is best on a mergerfs and SnapRAID pool."
            ),
        }
    }
}

/// Whether the storage a service wants is there. `mounts` is `/proc/mounts`, `pools` the
/// names `zpool list -H -o name` printed (empty if ZFS is not installed). Advice only:
/// enabling a service never waits on it.
pub fn tier_state(tier: Tier, mounts: &str, pools: &[String]) -> TierState {
    let rows = || {
        mounts.lines().filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.nth(1)?, f.next()?))
        })
    };
    let local = match tier {
        Tier::MergerfsBulk => rows().any(|(point, _)| point == "/storage"),
        Tier::ZfsLive | Tier::ZfsVm => !pools.is_empty(),
    };
    if local {
        return TierState::LocalPool;
    }
    let remote =
        tier != Tier::ZfsVm && rows().any(|(_, fstype)| matches!(fstype, "nfs" | "nfs4" | "cifs"));
    if remote {
        TierState::RemoteMounted
    } else {
        TierState::Missing
    }
}

// ───────────────────────────── Setup questions ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `yes` or `no`.
    Switch,
    Text,
    /// Never logged, written only to a file the daemon makes.
    Secret,
}

/// One thing enabling a service needs to know.
#[derive(Debug, Clone)]
pub struct Question {
    /// The answer's name, which is the catalog parameter's name too.
    pub id: &'static str,
    pub label: String,
    pub help: String,
    pub kind: Kind,
    pub required: bool,
    pub default: String,
}

fn q(
    id: &'static str,
    label: &str,
    help: &str,
    kind: Kind,
    required: bool,
    default: &str,
) -> Question {
    Question {
        id,
        label: label.into(),
        help: help.into(),
        kind,
        required,
        default: default.into(),
    }
}

/// The restic repository and its password file when backups are switched on for you.
pub const BACKUP_REPOSITORY: &str = "/var/lib/vexos-backup/restic-repo";
pub const BACKUP_PASSWORD_FILE: &str = "backup-password";

/// What to ask before turning `service` on. Every id here is a parameter of the
/// `service-enable` action (a test in the daemon keeps the two together).
pub fn questions(service: &Service) -> Vec<Question> {
    let mut asks = match service.name.as_str() {
        "plex" => vec![q(
            "plex_pass",
            "I have a Plex Pass",
            "Turns on hardware transcoding.",
            Kind::Switch,
            false,
            "no",
        )],
        "proxmox" => vec![
            q(
                "ip_address",
                "This server's IP address",
                "Required by Proxmox VE, for example 192.168.1.100.",
                Kind::Text,
                true,
                "",
            ),
            q(
                "bridge_nic",
                "Network card for the bridge",
                "The physical ethernet interface vmbr0 will use, such as enp2s0. `ip link show` lists them.",
                Kind::Text,
                true,
                "",
            ),
        ],
        "backup" => vec![
            q(
                "repository",
                "Restic repository",
                "A folder such as /mnt/backup/restic-repo, or sftp:user@host:/path.",
                Kind::Text,
                true,
                BACKUP_REPOSITORY,
            ),
            q(
                "password_file",
                "Password file",
                "Holds only the repository password. If it does not exist, a random one is made in /etc/nixos/secrets.",
                Kind::Text,
                true,
                "/etc/nixos/secrets/backup-password",
            ),
        ],
        "mediamanager" => vec![q(
            "admin_email",
            "Admin e-mail",
            "Registering with this address makes that account the admin.",
            Kind::Text,
            true,
            "",
        )],
        "cloudflare-ddns" => vec![
            q(
                "zone",
                "Cloudflare zone",
                "The domain to update, such as example.com.",
                Kind::Text,
                true,
                "",
            ),
            q(
                "subdomain",
                "Subdomain",
                "Leave empty to update the zone itself.",
                Kind::Text,
                false,
                "",
            ),
            q(
                "proxied",
                "Proxy through Cloudflare's CDN",
                "Breaks anything that is not HTTP.",
                Kind::Switch,
                false,
                "no",
            ),
            q(
                "api_token",
                "API token",
                "Needs Zone:DNS:Edit on the zone. Kept in a file only root can read.",
                Kind::Secret,
                true,
                "",
            ),
        ],
        "zigbee2mqtt" => vec![q(
            "serial_port",
            "Zigbee coordinator",
            "The serial device of your coordinator.",
            Kind::Text,
            false,
            "/dev/ttyUSB0",
        )],
        _ => Vec::new(),
    };
    if let Some(url) = &service.public_url {
        let help = if url.required {
            "The address people type to reach it, such as https://name.example.org. It has no usable default."
        } else {
            "The address people type, for use behind a reverse proxy. Leave empty to keep the default."
        };
        asks.push(q(
            "public_url",
            "Public URL",
            help,
            Kind::Text,
            url.required,
            "",
        ));
    }
    asks
}

/// The default a public URL has when none is set, to show as a hint.
pub fn default_url(service: &Service, text: &str, hostname: &str) -> Option<String> {
    let url = service.public_url.as_ref()?;
    let port = nixedit::get_int(text, &service.key("port")).unwrap_or(i64::from(url.port));
    match url.host {
        UrlHost::Localhost => Some(format!("http://localhost:{port}")),
        UrlHost::Hostname => Some(format!("http://{hostname}:{port}")),
        UrlHost::None => None,
    }
}

// ────────────────────────────── Operations ──────────────────────────────

fn secrets_dir(nixos_dir: &Path) -> PathBuf {
    nixos_dir.join("secrets")
}

fn services_path(nixos_dir: &Path) -> PathBuf {
    nixos_dir.join(SERVICES_FILE)
}

/// Server services exist only on the two server roles.
fn require_server<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    let Some(variant) = read_variant(ctx.nixos_dir) else {
        return Err(fail(
            ctx.sink,
            "/etc/nixos/vexos-variant is missing, so the role is unknown. Run Switch Role or GPU once.",
        ));
    };
    match features::role_of(&variant) {
        Some("server" | "headless-server") => Ok(()),
        _ => Err(fail(
            ctx.sink,
            format!(
                "server services are only available on the server roles. This host is {variant}."
            ),
        )),
    }
}

fn service_arg<R: Runner>(ctx: &Ctx<'_, R>) -> Result<&'static Service, i32> {
    let Some(name) = ctx.args.get("service") else {
        return Err(fail(ctx.sink, "a service is required"));
    };
    find(name).ok_or_else(|| {
        fail(
            ctx.sink,
            format!(
                "unknown service `{name}`. Available: {}",
                names().join(", ")
            ),
        )
    })
}

fn answer<'a, R: Runner>(ctx: &'a Ctx<'_, R>, id: &str) -> Option<&'a str> {
    ctx.args.get(id).map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn required<'a, R: Runner>(
    ctx: &'a Ctx<'_, R>,
    service: &Service,
    id: &str,
) -> Result<&'a str, i32> {
    answer(ctx, id).ok_or_else(|| {
        let label = questions(service)
            .into_iter()
            .find(|q| q.id == id)
            .map_or_else(|| id.to_string(), |q| q.label);
        fail(
            ctx.sink,
            format!("`{label}` is required to enable {}.", service.name),
        )
    })
}

/// `yes` or `no`; anything else is a mistake rather than a default.
fn switch<R: Runner>(ctx: &Ctx<'_, R>, id: &str) -> Result<bool, i32> {
    match answer(ctx, id) {
        None | Some("no") => Ok(false),
        Some("yes") => Ok(true),
        Some(other) => Err(fail(
            ctx.sink,
            format!("`{id}` must be yes or no, not `{other}`"),
        )),
    }
}

/// When a planned setting is skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum When {
    Always,
    /// An uncommented line for the option already exists, whatever it holds.
    Assigned,
    /// The option already holds a non-empty string.
    NonEmptyString,
}

struct Setting {
    option: String,
    value: NixValue,
    skip_if: When,
}

struct NewFile {
    path: PathBuf,
    content: Vec<u8>,
    /// Replace what is there (a token the person just typed) rather than keep it (a key
    /// that was generated earlier).
    overwrite: bool,
}

#[derive(Default)]
struct Plan {
    settings: Vec<Setting>,
    files: Vec<NewFile>,
    /// Lines for the job log.
    said: Vec<String>,
}

impl Plan {
    fn set(&mut self, option: impl Into<String>, value: NixValue, skip_if: When) {
        self.settings.push(Setting {
            option: option.into(),
            value,
            skip_if,
        });
    }
}

/// The path as a Nix string value.
fn path_value(path: &Path) -> NixValue {
    NixValue::str(path.to_string_lossy().into_owned())
}

/// Plan a secret file the module needs, unless the option already points somewhere.
fn generated(
    plan: &mut Plan,
    text: &str,
    dir: &Path,
    option: &str,
    file: &str,
    content: impl FnOnce() -> std::io::Result<String>,
) -> Result<(), String> {
    if nixedit::is_assigned(text, option) {
        return Ok(());
    }
    let path = dir.join(file);
    if !path.exists() {
        plan.files.push(NewFile {
            path: path.clone(),
            content: content()
                .map_err(|e| format!("could not make a random value: {e}"))?
                .into_bytes(),
            overwrite: false,
        });
        plan.said.push(format!("  Generated {}", path.display()));
    }
    plan.set(option, path_value(&path), When::Always);
    Ok(())
}

/// A secret that goes into an environment file: one line, no control characters, no
/// whitespace.
fn plain_secret(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn build_plan<R: Runner>(ctx: &Ctx<'_, R>, service: &Service, text: &str) -> Result<Plan, i32> {
    let mut plan = Plan::default();
    let dir = secrets_dir(ctx.nixos_dir);
    let name = service.name.as_str();
    let key = |leaf: &str| service.key(leaf);
    plan.set(service.enable_key(), NixValue::Bool(true), When::Always);
    let bad = |message: String| fail(ctx.sink, message);
    let hex = |bytes: usize| move || fsutil::random_hex(bytes);

    match name {
        "plex" if switch(ctx, "plex_pass")? => {
            plan.set(
                "vexos.server.plex.plexPass",
                NixValue::Bool(true),
                When::Always,
            );
        }
        "proxmox" => {
            plan.set(
                key("ipAddress"),
                NixValue::str(required(ctx, service, "ip_address")?),
                When::Always,
            );
            plan.set(
                key("bridgeInterface"),
                NixValue::str(required(ctx, service, "bridge_nic")?),
                When::Always,
            );
        }
        "backup" => {
            plan.set(
                key("repository"),
                NixValue::str(required(ctx, service, "repository")?),
                When::Always,
            );
            let file = PathBuf::from(required(ctx, service, "password_file")?);
            if file.is_dir() {
                return Err(bad(format!(
                    "{} is a folder, not a password file.",
                    file.display()
                )));
            }
            if !file.exists() {
                // The daemon writes a root-only file here, so it may only be made in the
                // place for secrets, not wherever a caller names.
                if !file.starts_with(&dir) || file.components().any(|c| c.as_os_str() == "..") {
                    return Err(bad(format!(
                        "{} does not exist, and a new password file can only be made inside {}.",
                        file.display(),
                        dir.display()
                    )));
                }
                let content = fsutil::random_hex(48)
                    .map_err(|e| bad(format!("could not make a random password: {e}")))?;
                plan.files.push(NewFile {
                    path: file.clone(),
                    content: format!("{content}\n").into_bytes(),
                    overwrite: false,
                });
                plan.said.push(format!(
                    "  Generated a new random password at {}",
                    file.display()
                ));
            }
            plan.set(key("passwordFile"), path_value(&file), When::Always);
        }
        "mediamanager" => {
            plan.set(
                key("adminEmails"),
                NixValue::StrList(vec![required(ctx, service, "admin_email")?.to_string()]),
                When::Always,
            );
        }
        "cloudflare-ddns" => {
            plan.set(
                key("zone"),
                NixValue::str(required(ctx, service, "zone")?),
                When::Always,
            );
            if let Some(sub) = answer(ctx, "subdomain") {
                plan.set(key("subdomain"), NixValue::str(sub), When::Always);
            }
            plan.set(
                key("proxied"),
                NixValue::Bool(switch(ctx, "proxied")?),
                When::Always,
            );

            let token = ctx
                .secrets
                .get("api_token")
                .map(|t| t.expose().trim().to_string())
                .unwrap_or_default();
            if token.is_empty() {
                return Err(bad(
                    "`API token` is required to enable cloudflare-ddns.".into()
                ));
            }
            if !plain_secret(&token) {
                return Err(bad(
                    "the API token must be a single line with no spaces or control characters."
                        .into(),
                ));
            }
            let path = dir.join("cloudflare-ddns-env");
            plan.files.push(NewFile {
                path: path.clone(),
                content: format!("API_KEY={token}\n").into_bytes(),
                overwrite: true,
            });
            plan.set(key("environmentFile"), path_value(&path), When::Always);
            plan.said.push(format!(
                "  Wrote the API token to {} (root only)",
                path.display()
            ));
        }
        "zigbee2mqtt" => {
            let port = answer(ctx, "serial_port").unwrap_or("/dev/ttyUSB0");
            if !port.starts_with("/dev/") || port.split('/').any(|p| p == "..") {
                return Err(bad(format!("`{port}` is not a device under /dev.")));
            }
            plan.set(key("serialPort"), NixValue::str(port), When::Always);
        }
        "arcane" => generated(
            &mut plan,
            text,
            &dir,
            "vexos.server.arcane.environmentFile",
            "arcane-env",
            || {
                Ok(format!(
                    "ENCRYPTION_KEY={}\nJWT_SECRET={}\n",
                    fsutil::random_hex(32)?,
                    fsutil::random_hex(32)?
                ))
            },
        )
        .map_err(bad)?,
        "searxng" => generated(
            &mut plan,
            text,
            &dir,
            "vexos.server.searxng.environmentFile",
            "searxng.env",
            || Ok(format!("SEARXNG_SECRET_KEY={}\n", hex(48)()?)),
        )
        .map_err(bad)?,
        "vexboard" => generated(
            &mut plan,
            text,
            &dir,
            "vexos.server.vexboard.secretFile",
            "vexboard-secret",
            || Ok(format!("VEXBOARD_AUTH__SECRET={}\n", hex(48)()?)),
        )
        .map_err(bad)?,
        _ => {}
    }

    if let Some(url) = &service.public_url {
        match answer(ctx, "public_url") {
            Some(given) => {
                plan.set(
                    key(&url.option),
                    NixValue::str(normalize_url(given).map_err(bad)?),
                    When::Always,
                );
            }
            None if url.required => {
                return Err(bad(format!(
                    "{name} has no usable default address, so a public URL is required."
                )));
            }
            None => {}
        }
    }

    // Backups alongside the first service, if asked for: local defaults that need no
    // further answers.
    if name != "backup"
        && switch(ctx, "with_backup")?
        && !nixedit::is_set(text, "vexos.server.backup.enable", &NixValue::Bool(true))
    {
        plan.set(
            "vexos.server.backup.enable",
            NixValue::Bool(true),
            When::Always,
        );
        plan.set(
            "vexos.server.backup.repository",
            NixValue::str(BACKUP_REPOSITORY),
            When::NonEmptyString,
        );
        let password = dir.join(BACKUP_PASSWORD_FILE);
        if !nixedit::is_assigned(text, "vexos.server.backup.passwordFile") {
            if !password.exists() {
                let content = fsutil::random_hex(48)
                    .map_err(|e| bad(format!("could not make a random password: {e}")))?;
                plan.files.push(NewFile {
                    path: password.clone(),
                    content: format!("{content}\n").into_bytes(),
                    overwrite: false,
                });
            }
            plan.set(
                "vexos.server.backup.passwordFile",
                path_value(&password),
                When::Assigned,
            );
        }
        plan.said.push(format!(
            "  + Backups also enabled (restic to {BACKUP_REPOSITORY})"
        ));
    }
    Ok(plan)
}

/// Trailing slashes dropped, and an address that is not http or https refused. The
/// daemon has already checked the characters; this keeps the value's shape.
fn normalize_url(given: &str) -> Result<String, String> {
    let trimmed = given.trim().trim_end_matches('/');
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) || trimmed.len() < 9 {
        return Err(format!("`{given}` is not an http:// or https:// address."));
    }
    Ok(trimmed.to_string())
}

fn apply(text: &str, plan: &Plan) -> Result<String, EditError> {
    let mut current = text.to_string();
    for setting in &plan.settings {
        let skip = match setting.skip_if {
            When::Always => false,
            When::Assigned => nixedit::is_assigned(&current, &setting.option),
            When::NonEmptyString => {
                nixedit::get_str(&current, &setting.option).is_some_and(|v| !v.is_empty())
            }
        };
        if !skip {
            current = nixedit::set(&current, &setting.option, &setting.value)?;
        }
    }
    Ok(current)
}

pub async fn enable<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    // One enable at a time, from planning to cleanup: two that share a generated file (the
    // backup password) must not have one delete what the other's setting refers to.
    static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _turn = ONE_AT_A_TIME.lock().await;
    match enable_inner(ctx).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async fn enable_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let service = service_arg(ctx)?;
    let path = services_path(ctx.nixos_dir);
    let existing = std::fs::read_to_string(&path).ok();
    let text = existing.as_deref().unwrap_or(SERVICES_SEED);

    // Backups may have been turned on with another service's defaults; enabling them
    // explicitly still lets the repository and password file be re-pointed.
    if service.name != "backup" && is_enabled(text, service) {
        ctx.sink
            .info(&format!("{} is already enabled.", service.name));
        if let Some(url) = &service.public_url {
            if nixedit::get_str(text, &service.key(&url.option)).is_none_or(|v| v.is_empty()) {
                ctx.sink.info(
                    "  Public URL not set: use Set Public URL if it is served behind a reverse proxy.",
                );
            }
        }
        return Ok(());
    }

    let plan = build_plan(ctx, service, text)?;

    // Everything is decided; now write. Secret files first, then the one edit that
    // refers to them, so a failed edit never leaves a setting pointing at nothing.
    // What to put back if a later step fails: the old content of a file this replaced, or
    // nothing for one it created.
    let mut made: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    let cleanup = |made: &[(PathBuf, Option<Vec<u8>>)]| {
        for (file, before) in made {
            match before {
                Some(old) => {
                    let _ = fsutil::write_atomic(file, old, 0o600);
                }
                None => {
                    let _ = std::fs::remove_file(file);
                }
            }
        }
    };
    if !plan.files.is_empty() {
        let dir = secrets_dir(ctx.nixos_dir);
        if let Err(e) = ensure_private_dir(&dir) {
            return Err(fail(
                ctx.sink,
                format!("could not prepare {}: {e}", dir.display()),
            ));
        }
    }
    for file in &plan.files {
        let before = if file.overwrite {
            std::fs::read(&file.path).ok()
        } else {
            None
        };
        let result = if file.overwrite {
            fsutil::write_atomic(&file.path, &file.content, 0o600)
        } else {
            fsutil::create_new(&file.path, &file.content, 0o600)
        };
        match result {
            Ok(()) => made.push((file.path.clone(), before)),
            // Made by an earlier run or by the person; it stays as it is.
            Err(e) if !file.overwrite && e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                cleanup(&made);
                return Err(fail(
                    ctx.sink,
                    format!("could not write {}: {e}", file.path.display()),
                ));
            }
        }
    }

    let seeded = match fsutil::create_new(&path, SERVICES_SEED.as_bytes(), 0o644) {
        Ok(()) => {
            ctx.sink.info(&format!("Created {}", path.display()));
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => {
            cleanup(&made);
            return Err(fail(
                ctx.sink,
                format!("could not create {}: {e}", path.display()),
            ));
        }
    };

    let edited =
        nixedit::edit_file(ctx.runner, &path, |current| apply(current, &plan).map(Some)).await;
    if let Err(e) = edited {
        cleanup(&made);
        if seeded {
            let _ = nixedit::replace_if_unchanged(&path, SERVICES_SEED, None).await;
        }
        return Err(fail(ctx.sink, e));
    }

    ctx.sink.info(&format!("✓ Enabled: {}", service.name));
    for line in &plan.said {
        ctx.sink.info(line);
    }
    ctx.sink.info("  → Rebuild to apply.");
    for (label, text) in service.notes() {
        if label.is_empty() {
            ctx.sink.info(&format!("  {text}"));
        } else {
            ctx.sink.info(&format!("  {label}: {text}"));
        }
    }
    Ok(())
}

/// The secrets folder: there, and for root alone.
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

pub async fn disable<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    match disable_inner(ctx).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async fn disable_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let service = service_arg(ctx)?;
    let path = services_path(ctx.nixos_dir);
    let Ok(text) = std::fs::read_to_string(&path) else {
        ctx.sink
            .info("No server-services.nix found. Nothing to disable.");
        return Ok(());
    };
    let key = service.enable_key();

    // On through the arr meta-flag, so there is no line of its own to flip: pin it off
    // explicitly, which beats the flag's default.
    let edited = if nixedit::get_bool(&text, &key) != Some(true) && is_enabled(&text, service) {
        nixedit::set_in_file(ctx.runner, &path, &key, &NixValue::Bool(false)).await
    } else {
        nixedit::disable_in_file(ctx.runner, &path, &key).await
    };
    match edited {
        Ok(nixedit::Edited::Changed) => {
            ctx.sink.info(&format!("✗ Disabled: {}", service.name));
            ctx.sink.info("  Its data is kept. → Rebuild to apply.");
            Ok(())
        }
        Ok(nixedit::Edited::Unchanged) => {
            ctx.sink
                .info(&format!("{} is already disabled.", service.name));
            Ok(())
        }
        Err(e) => Err(fail(ctx.sink, e)),
    }
}

pub async fn set_url<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    match set_url_inner(ctx).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async fn set_url_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let service = service_arg(ctx)?;
    let Some(url) = &service.public_url else {
        let with: Vec<&str> = all()
            .iter()
            .filter(|s| s.public_url.is_some())
            .map(|s| s.name.as_str())
            .collect();
        return Err(fail(
            ctx.sink,
            format!(
                "{} has no public-URL option. Services that do: {}",
                service.name,
                with.join(", ")
            ),
        ));
    };
    let path = services_path(ctx.nixos_dir);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if !is_enabled(&text, service) {
        return Err(fail(
            ctx.sink,
            format!("{} is not enabled. Enable it first.", service.name),
        ));
    }
    let given = required_url(ctx)?;
    let address = normalize_url(given).map_err(|m| fail(ctx.sink, m))?;
    let key = service.key(&url.option);
    nixedit::set_in_file(ctx.runner, &path, &key, &NixValue::str(address.clone()))
        .await
        .map_err(|e| fail(ctx.sink, e))?;
    ctx.sink.info(&format!("✓ Set {key} = {address}"));
    ctx.sink.info("  → Rebuild to apply.");
    Ok(())
}

fn required_url<'a, R: Runner>(ctx: &'a Ctx<'_, R>) -> Result<&'a str, i32> {
    answer(ctx, "url").ok_or_else(|| fail(ctx.sink, "a public URL is required"))
}

/// Restart every unit of a service in one go, clearing an earlier start-limit failure
/// first. One transaction, so systemd keeps the ordering between the units (a database
/// before the app that needs it).
pub async fn restart<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    match restart_inner(ctx).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async fn restart_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_server(ctx)?;
    let service = service_arg(ctx)?;
    let wanted = service.unit_names();
    let shown = capture(ctx, &Cmd::new("systemctl").args(show_args(&wanted))).await?;
    let present: Vec<String> = parse_show(&shown.stdout)
        .into_iter()
        .filter(|u| u.load != "not-found")
        .map(|u| format!("{}.service", u.id))
        .collect();
    if present.is_empty() {
        return Err(fail(
            ctx.sink,
            format!(
                "no systemd units for {} exist on this host. Is it enabled and rebuilt?",
                service.name
            ),
        ));
    }
    ctx.sink.info(&format!(
        "Restarting: {} ({})",
        service.name,
        present.join(" ")
    ));

    // A unit that hit its start limit refuses to start until it is reset. Nothing to
    // reset is not a problem.
    let _ = capture(
        ctx,
        &Cmd::new("systemctl").arg("reset-failed").args(&present),
    )
    .await?;

    let code = stream(ctx, &Cmd::new("systemctl").arg("restart").args(&present)).await?;
    if code != 0 {
        return Err(fail(
            ctx.sink,
            format!("systemctl restart exited with {code}"),
        ));
    }

    let after = capture(ctx, &Cmd::new("systemctl").args(show_args(&present))).await?;
    let report = summarize(&parse_show(&after.stdout));
    for unit in &report.units {
        ctx.sink.info(&format!("  {}: {}", unit.id, unit.active));
    }
    // The exit status above is the result: a oneshot service (kernel-builder, backup) that
    // finished is inactive, and that is a success. The states are for information.
    if report.health != Health::Running {
        ctx.sink
            .info(&format!("  Not running now: {}", report.down().join(", ")));
    }
    ctx.sink.info(&format!("✔ Restarted: {}", service.name));
    Ok(())
}

/// Plex Pass hardware transcoding, on or off.
pub async fn plex_pass<R: Runner>(ctx: &Ctx<'_, R>, on: bool) -> i32 {
    match plex_pass_inner(ctx, on).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async fn plex_pass_inner<R: Runner>(ctx: &Ctx<'_, R>, on: bool) -> Result<(), i32> {
    require_server(ctx)?;
    let path = services_path(ctx.nixos_dir);
    let option = "vexos.server.plex.plexPass";
    if on {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if nixedit::get_bool(&text, "vexos.server.plex.enable") != Some(true) {
            return Err(fail(ctx.sink, "Plex is not enabled. Enable it first."));
        }
        nixedit::set_in_file(ctx.runner, &path, option, &NixValue::Bool(true))
            .await
            .map_err(|e| fail(ctx.sink, e))?;
        ctx.sink.info("✓ Plex Pass hardware transcoding enabled.");
        ctx.sink.info("  → Rebuild to apply.");
        return Ok(());
    }
    if !path.exists() {
        ctx.sink
            .info("Plex Pass is already disabled (or was never set).");
        return Ok(());
    }
    match nixedit::disable_in_file(ctx.runner, &path, option).await {
        Ok(nixedit::Edited::Changed) => {
            ctx.sink.info("✓ Plex Pass hardware transcoding disabled.");
            ctx.sink.info("  → Rebuild to apply.");
            Ok(())
        }
        Ok(nixedit::Edited::Unchanged) => {
            ctx.sink
                .info("Plex Pass is already disabled (or was never set).");
            Ok(())
        }
        Err(e) => Err(fail(ctx.sink, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::testing::{block_on, FakeRunner};
    use crate::runner::Output;
    use crate::testkit::Kit;
    use crate::Secret;

    /// The names `just service available` lists today, in the justfile's order.
    const JUSTFILE_NAMES: &str = "adguard arcane attic audiobookshelf authelia backup bazarr beszel beszel-agent caddy chaptarr cloudflare-ddns cockpit code-server docker dockhand forgejo grimmory harmonia headscale home-registry homepage humidor immich jellyfin joplin kernel-builder kiji-proxy lidarr listenarr maintainerr mealie mediamanager nas netdata nextcloud nginx nginx-proxy-manager ntfy paperless papermc photoprism pihole plex podman portainer prometheus prowlarr proxmox qbittorrent radarr sabnzbd scrutiny searxng seerr sonarr sportarr syncthing tautulli tdarr traefik unbound unifi-toolkit uptime-kuma vaultwarden vexboard wishlist zigbee2mqtt";

    fn runner() -> FakeRunner {
        FakeRunner::new().ok("nix-instantiate", &["--parse"])
    }

    fn server_kit(pairs: &[(&str, &str)]) -> Kit {
        let kit = Kit::new().with_args(pairs);
        kit.nixos_file("vexos-variant", "vexos-server-amd\n");
        kit
    }

    /// Replace a kit's answers, for a test whose answers name a path inside the kit.
    fn set_args(kit: &mut Kit, pairs: &[(&str, &str)]) {
        kit.args = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    }

    fn services_text(kit: &Kit) -> String {
        std::fs::read_to_string(kit.nixos.join(SERVICES_FILE)).unwrap()
    }

    fn run_enable(kit: &Kit) -> i32 {
        block_on(enable(&kit.ctx(&runner())))
    }

    // ── the table ──

    #[test]
    fn the_table_lists_every_service_the_justfile_does() {
        let mut ours = names();
        ours.sort_unstable();
        let mut theirs: Vec<&str> = JUSTFILE_NAMES.split(' ').collect();
        theirs.sort_unstable();
        assert_eq!(ours, theirs);
        assert_eq!(all().len(), 68);
    }

    #[test]
    fn every_service_is_complete_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for s in all() {
            assert!(seen.insert(&s.name), "{} is listed twice", s.name);
            assert!(
                !s.group.is_empty() && !s.description.is_empty(),
                "{}",
                s.name
            );
            assert!(!s.units.is_empty(), "{} has no units", s.name);
            assert!(!s.notes.is_empty(), "{} has no notes", s.name);
            assert!(!s.description.contains('|'), "{}", s.name);
            for u in &s.health {
                assert!(
                    u.starts_with("http://") || u.starts_with("https://"),
                    "{}: {u}",
                    s.name
                );
            }
            for (label, text) in s.notes() {
                assert!(!text.is_empty(), "{}: empty note {label}", s.name);
            }
        }
    }

    #[test]
    fn groups_are_contiguous_so_the_page_can_list_them_in_order() {
        let mut seen: Vec<&str> = Vec::new();
        for s in all() {
            if seen.last() != Some(&s.group.as_str()) {
                assert!(!seen.contains(&s.group.as_str()), "{} is split", s.group);
                seen.push(&s.group);
            }
        }
    }

    #[test]
    fn some_services_live_under_another_option() {
        let key = |n: &str| find(n).unwrap().enable_key();
        assert_eq!(key("jellyfin"), "vexos.server.jellyfin.enable");
        assert_eq!(key("uptime-kuma"), "vexos.server.uptime-kuma.enable");
        assert_eq!(key("sonarr"), "vexos.server.arr.sonarr.enable");
        assert_eq!(key("maintainerr"), "vexos.server.arr.maintainerr.enable");
        assert_eq!(key("kernel-builder"), "vexos.server.kernelBuilder.enable");
    }

    #[test]
    fn the_public_url_services_are_the_ones_the_recipe_asked_about() {
        let mut with: Vec<&str> = all()
            .iter()
            .filter(|s| s.public_url.is_some())
            .map(|s| s.name.as_str())
            .collect();
        with.sort_unstable();
        assert_eq!(
            with,
            [
                "arcane",
                "headscale",
                "joplin",
                "mediamanager",
                "vaultwarden",
                "wishlist"
            ]
        );
        let required: Vec<&str> = all()
            .iter()
            .filter(|s| s.public_url.as_ref().is_some_and(|u| u.required))
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(required, ["headscale", "vaultwarden"]);
    }

    // ── what is enabled ──

    #[test]
    fn enabled_follows_the_enable_line_and_the_arr_meta_flag() {
        let sonarr = find("sonarr").unwrap();
        let qbit = find("qbittorrent").unwrap();
        let jelly = find("jellyfin").unwrap();
        assert!(is_enabled(
            "{\n  vexos.server.jellyfin.enable = true;\n}\n",
            jelly
        ));
        assert!(!is_enabled(
            "{\n  vexos.server.jellyfin.enable = false;\n}\n",
            jelly
        ));
        assert!(!is_enabled(
            "{\n  # vexos.server.jellyfin.enable = true;\n}\n",
            jelly
        ));
        let meta = "{\n  vexos.server.arr.enable = true;\n}\n";
        assert!(is_enabled(meta, sonarr));
        assert!(
            !is_enabled(meta, qbit),
            "only the first five follow the meta flag"
        );
        // An explicit false beats the meta flag.
        let pinned =
            "{\n  vexos.server.arr.enable = true;\n  vexos.server.arr.sonarr.enable = false;\n}\n";
        assert!(!is_enabled(pinned, sonarr));
    }

    // ── health ──

    const SHOWN: &str = "Id=a.service\nLoadState=loaded\nActiveState=active\nSubState=running\n\nId=b.service\nLoadState=loaded\nActiveState=failed\nSubState=failed\n\nId=c.service\nLoadState=not-found\nActiveState=inactive\nSubState=dead\n";

    #[test]
    fn systemctl_show_output_is_read_unit_by_unit() {
        let units = parse_show(SHOWN);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].id, "a");
        assert_eq!(units[1].active, "failed");
        assert_eq!(units[2].load, "not-found");
        assert!(parse_show("").is_empty());
    }

    fn info(id: &str, load: &str, active: &str) -> UnitInfo {
        UnitInfo {
            id: id.into(),
            load: load.into(),
            active: active.into(),
            sub: String::new(),
        }
    }

    #[test]
    fn a_service_is_judged_by_the_units_that_exist() {
        let r = summarize(&parse_show(SHOWN));
        assert_eq!(r.health, Health::Failed);
        assert_eq!(r.units.len(), 2, "the missing unit is skipped");

        let up = summarize(&[
            info("a", "loaded", "active"),
            info("b", "not-found", "inactive"),
        ]);
        assert_eq!(up.health, Health::Running);
        assert_eq!(
            summarize(&[
                info("a", "loaded", "inactive"),
                info("b", "loaded", "inactive")
            ])
            .health,
            Health::Stopped
        );
        let part = summarize(&[
            info("a", "loaded", "active"),
            info("b", "loaded", "inactive"),
        ]);
        assert_eq!(part.health, Health::Partial);
        assert_eq!(part.down(), ["b (inactive)"]);
        assert_eq!(
            summarize(&[info("a", "not-found", "inactive")]).health,
            Health::NotBuilt
        );
        assert_eq!(summarize(&[]).health, Health::NotBuilt);
    }

    #[test]
    fn one_show_call_serves_every_service_on_the_page() {
        let units = vec![
            "docker-grimmory.service".to_string(),
            "docker-grimmory-db.service".into(),
        ];
        let args = show_args(&units);
        assert_eq!(args[0], "show");
        assert!(
            args.iter().any(|a| a == "--"),
            "a unit name can never be read as an option"
        );
        let infos = vec![
            info("docker-grimmory", "loaded", "active"),
            info("docker-grimmory-db", "loaded", "active"),
            info("sonarr", "loaded", "failed"),
        ];
        assert_eq!(
            report_for(find("grimmory").unwrap(), &infos).health,
            Health::Running
        );
        assert_eq!(
            report_for(find("sonarr").unwrap(), &infos).health,
            Health::Failed
        );
        assert_eq!(
            report_for(find("jellyfin").unwrap(), &infos).health,
            Health::NotBuilt
        );
    }

    #[test]
    fn a_probe_reads_the_status_code() {
        assert_eq!(reach("200"), Reach::Ok(200));
        assert_eq!(reach("302\n"), Reach::Ok(302));
        assert_eq!(reach("503"), Reach::Error(503));
        assert_eq!(reach("000"), Reach::Unreachable);
        assert_eq!(reach(""), Reach::Unreachable);
        assert_eq!(reach("unreachable"), Reach::Unreachable);
        assert!(probe_args("http://x").contains(&"--max-time".to_string()));
    }

    #[test]
    fn storage_advice_looks_for_the_right_thing() {
        let mounts = "/dev/sda1 /storage ext4 rw 0 0\nserver:/x /mnt/nas nfs4 rw 0 0\n";
        assert_eq!(
            tier_state(Tier::MergerfsBulk, mounts, &[]),
            TierState::LocalPool
        );
        assert_eq!(
            tier_state(Tier::MergerfsBulk, "/dev/sda1 / ext4 rw 0 0\n", &[]),
            TierState::Missing
        );
        assert_eq!(
            tier_state(Tier::ZfsLive, mounts, &["tank".into()]),
            TierState::LocalPool
        );
        assert_eq!(
            tier_state(Tier::ZfsLive, mounts, &[]),
            TierState::RemoteMounted
        );
        // VM disks cannot live on a network share.
        assert_eq!(tier_state(Tier::ZfsVm, mounts, &[]), TierState::Missing);
    }

    // ── questions ──

    #[test]
    fn only_the_services_that_need_answers_ask() {
        let asks = |n: &str| {
            questions(find(n).unwrap())
                .into_iter()
                .map(|q| q.id)
                .collect::<Vec<_>>()
        };
        assert!(asks("jellyfin").is_empty());
        assert_eq!(asks("plex"), ["plex_pass"]);
        assert_eq!(asks("proxmox"), ["ip_address", "bridge_nic"]);
        assert_eq!(
            asks("cloudflare-ddns"),
            ["zone", "subdomain", "proxied", "api_token"]
        );
        assert_eq!(asks("wishlist"), ["public_url"]);
        assert_eq!(asks("mediamanager"), ["admin_email", "public_url"]);
        let vault = questions(find("vaultwarden").unwrap());
        assert!(
            vault[0].required,
            "vaultwarden cannot be enabled without a URL"
        );
        assert!(!questions(find("wishlist").unwrap())[0].required);
    }

    #[test]
    fn a_default_public_url_follows_the_port_in_the_file() {
        let wishlist = find("wishlist").unwrap();
        assert_eq!(
            default_url(wishlist, "{ }", "box").as_deref(),
            Some("http://box:3280")
        );
        assert_eq!(
            default_url(
                wishlist,
                "{\n  vexos.server.wishlist.port = 4000;\n}\n",
                "box"
            )
            .as_deref(),
            Some("http://box:4000")
        );
        assert_eq!(
            default_url(find("arcane").unwrap(), "{ }", "box").as_deref(),
            Some("http://localhost:3552")
        );
        assert_eq!(
            default_url(find("vaultwarden").unwrap(), "{ }", "box"),
            None
        );
    }

    // ── enable ──

    #[test]
    fn enabling_a_service_writes_one_line_and_nothing_else() {
        let kit = server_kit(&[("service", "jellyfin")]);
        assert_eq!(run_enable(&kit), 0, "{}", kit.sink.text());
        let text = services_text(&kit);
        assert!(text.contains("vexos.server.jellyfin.enable = true;"));
        assert!(
            !text.contains("backup"),
            "backups are only on when asked for"
        );
        assert!(!kit.nixos.join("secrets").exists());
        assert!(kit.sink.text().contains("Rebuild"));
    }

    #[test]
    fn enabling_keeps_the_rest_of_the_file_and_the_arr_option_path() {
        let kit = server_kit(&[("service", "sonarr")]);
        kit.nixos_file(
            SERVICES_FILE,
            "{\n  vexos.server.plex.enable = true; # movies\n}\n",
        );
        assert_eq!(run_enable(&kit), 0, "{}", kit.sink.text());
        let text = services_text(&kit);
        assert!(text.contains("vexos.server.plex.enable = true; # movies"));
        assert!(text.contains("vexos.server.arr.sonarr.enable = true;"));
    }

    #[test]
    fn a_service_that_is_already_on_is_left_alone() {
        let kit = server_kit(&[("service", "jellyfin")]);
        let before = "{\n  vexos.server.jellyfin.enable = true;\n}\n";
        kit.nixos_file(SERVICES_FILE, before);
        assert_eq!(run_enable(&kit), 0);
        assert_eq!(services_text(&kit), before);
        assert!(kit.sink.text().contains("already enabled"));
    }

    #[test]
    fn enabling_is_refused_off_the_server_roles_and_for_unknown_names() {
        let kit = Kit::new().with_args(&[("service", "jellyfin")]);
        kit.nixos_file("vexos-variant", "vexos-desktop-amd\n");
        assert_eq!(run_enable(&kit), 1);
        assert!(!kit.nixos.join(SERVICES_FILE).exists());

        let kit = server_kit(&[("service", "not-a-service")]);
        assert_eq!(run_enable(&kit), 1);
        assert!(kit.sink.text().contains("unknown service"));

        let kit = Kit::new().with_args(&[("service", "jellyfin")]);
        kit.nixos_file("vexos-variant", "vexos-headless-server-amd\n");
        assert_eq!(run_enable(&kit), 0, "headless servers have services too");
    }

    #[test]
    fn plex_pass_is_set_when_asked_for() {
        let kit = server_kit(&[("service", "plex"), ("plex_pass", "yes")]);
        assert_eq!(run_enable(&kit), 0, "{}", kit.sink.text());
        let text = services_text(&kit);
        assert!(text.contains("vexos.server.plex.enable = true;"));
        assert!(text.contains("vexos.server.plex.plexPass = true;"));

        let kit = server_kit(&[("service", "plex"), ("plex_pass", "no")]);
        assert_eq!(run_enable(&kit), 0);
        assert!(!services_text(&kit).contains("plexPass"));

        let kit = server_kit(&[("service", "plex"), ("plex_pass", "maybe")]);
        assert_eq!(run_enable(&kit), 1);
        assert!(!kit.nixos.join(SERVICES_FILE).exists());
    }

    #[test]
    fn a_required_answer_that_is_missing_stops_before_anything_is_written() {
        let kit = server_kit(&[("service", "proxmox"), ("ip_address", "192.168.1.5")]);
        assert_eq!(run_enable(&kit), 1);
        assert!(kit.sink.text().contains("Network card"));
        assert!(
            !kit.nixos.join(SERVICES_FILE).exists(),
            "no half-configured file"
        );

        let kit = server_kit(&[
            ("service", "proxmox"),
            ("ip_address", "192.168.1.5"),
            ("bridge_nic", "enp2s0"),
        ]);
        assert_eq!(run_enable(&kit), 0, "{}", kit.sink.text());
        let text = services_text(&kit);
        assert!(text.contains("vexos.server.proxmox.ipAddress = \"192.168.1.5\";"));
        assert!(text.contains("vexos.server.proxmox.bridgeInterface = \"enp2s0\";"));
    }

    #[test]
    fn vaultwarden_cannot_be_enabled_without_its_address() {
        let kit = server_kit(&[("service", "vaultwarden")]);
        assert_eq!(run_enable(&kit), 1);
        assert!(kit.sink.text().contains("public URL is required"));

        let kit = server_kit(&[
            ("service", "vaultwarden"),
            ("public_url", "https://vault.example.org///"),
        ]);
        assert_eq!(run_enable(&kit), 0, "{}", kit.sink.text());
        assert!(services_text(&kit)
            .contains("vexos.server.vaultwarden.domain = \"https://vault.example.org\";"));
    }

    #[test]
    fn an_address_that_is_not_http_is_refused() {
        let kit = server_kit(&[("service", "wishlist"), ("public_url", "ftp://x.example")]);
        assert_eq!(run_enable(&kit), 1);
        assert!(!kit.nixos.join(SERVICES_FILE).exists());
    }

    #[test]
    fn the_admin_email_is_written_as_a_list() {
        let kit = server_kit(&[
            ("service", "mediamanager"),
            ("admin_email", "me@example.com"),
        ]);
        assert_eq!(run_enable(&kit), 0, "{}", kit.sink.text());
        assert!(services_text(&kit)
            .contains("vexos.server.mediamanager.adminEmails = [ \"me@example.com\" ];"));
    }

    #[test]
    fn a_zigbee_device_must_be_under_dev() {
        let kit = server_kit(&[("service", "zigbee2mqtt")]);
        assert_eq!(run_enable(&kit), 0);
        assert!(services_text(&kit).contains("serialPort = \"/dev/ttyUSB0\";"));

        let kit = server_kit(&[("service", "zigbee2mqtt"), ("serial_port", "/etc/shadow")]);
        assert_eq!(run_enable(&kit), 1);
        let kit = server_kit(&[
            ("service", "zigbee2mqtt"),
            ("serial_port", "/dev/../etc/shadow"),
        ]);
        assert_eq!(run_enable(&kit), 1);
    }

    #[test]
    fn the_cloudflare_token_goes_to_a_private_file_and_nowhere_else() {
        use std::os::unix::fs::PermissionsExt;
        let mut kit = server_kit(&[
            ("service", "cloudflare-ddns"),
            ("zone", "example.com"),
            ("subdomain", "home"),
            ("proxied", "no"),
        ]);
        kit.secrets
            .insert("api_token".into(), Secret::new("tok-SECRET-123"));
        assert_eq!(run_enable(&kit), 0, "{}", kit.sink.text());

        let file = kit.nixos.join("secrets/cloudflare-ddns-env");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "API_KEY=tok-SECRET-123\n"
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(kit.nixos.join("secrets"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let text = services_text(&kit);
        assert!(
            !text.contains("SECRET-123"),
            "the token is not in the Nix file"
        );
        assert!(text.contains("vexos.server.cloudflare-ddns.zone = \"example.com\";"));
        assert!(text.contains("vexos.server.cloudflare-ddns.subdomain = \"home\";"));
        assert!(text.contains("vexos.server.cloudflare-ddns.proxied = false;"));
        assert!(text.contains(&format!("environmentFile = \"{}\";", file.display())));
        assert!(
            !kit.sink.text().contains("SECRET-123"),
            "nor in the job log"
        );
    }

    #[test]
    fn a_token_with_a_line_break_or_space_is_refused() {
        for token in ["", "two words", "line\nbreak", "tab\there", "nul\0x"] {
            let mut kit = server_kit(&[("service", "cloudflare-ddns"), ("zone", "example.com")]);
            kit.secrets.insert("api_token".into(), Secret::new(token));
            assert_eq!(run_enable(&kit), 1, "{token:?}");
            assert!(
                !kit.nixos.join("secrets/cloudflare-ddns-env").exists(),
                "{token:?}"
            );
            assert!(!kit.nixos.join(SERVICES_FILE).exists(), "{token:?}");
            assert!(!kit.sink.text().contains("two words"));
        }
    }

    #[test]
    fn a_new_token_replaces_the_old_one() {
        let mut kit = server_kit(&[("service", "cloudflare-ddns"), ("zone", "example.com")]);
        std::fs::create_dir_all(kit.nixos.join("secrets")).unwrap();
        std::fs::write(
            kit.nixos.join("secrets/cloudflare-ddns-env"),
            "API_KEY=old\n",
        )
        .unwrap();
        kit.secrets.insert("api_token".into(), Secret::new("new"));
        assert_eq!(run_enable(&kit), 0);
        assert_eq!(
            std::fs::read_to_string(kit.nixos.join("secrets/cloudflare-ddns-env")).unwrap(),
            "API_KEY=new\n"
        );
    }

    #[test]
    fn services_that_need_a_secret_get_one_generated_once() {
        let kit = server_kit(&[("service", "arcane")]);
        assert_eq!(run_enable(&kit), 0, "{}", kit.sink.text());
        let file = kit.nixos.join("secrets/arcane-env");
        let first = std::fs::read_to_string(&file).unwrap();
        assert!(first.starts_with("ENCRYPTION_KEY="));
        let lines: Vec<&str> = first.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), "ENCRYPTION_KEY=".len() + 64);
        assert_ne!(
            lines[0]["ENCRYPTION_KEY=".len()..],
            lines[1]["JWT_SECRET=".len()..],
            "two different keys"
        );
        assert!(services_text(&kit).contains("vexos.server.arcane.environmentFile"));

        // Enable it again after a disable: the same key is kept, not replaced.
        let again = server_kit(&[("service", "arcane")]);
        std::fs::create_dir_all(again.nixos.join("secrets")).unwrap();
        std::fs::write(
            again.nixos.join("secrets/arcane-env"),
            "ENCRYPTION_KEY=keep\nJWT_SECRET=keep\n",
        )
        .unwrap();
        assert_eq!(run_enable(&again), 0);
        assert_eq!(
            std::fs::read_to_string(again.nixos.join("secrets/arcane-env")).unwrap(),
            "ENCRYPTION_KEY=keep\nJWT_SECRET=keep\n"
        );

        for (service, file) in [("searxng", "searxng.env"), ("vexboard", "vexboard-secret")] {
            let kit = server_kit(&[("service", service)]);
            assert_eq!(run_enable(&kit), 0, "{service}: {}", kit.sink.text());
            assert!(kit.nixos.join("secrets").join(file).exists(), "{service}");
        }
    }

    #[test]
    fn a_secret_option_the_person_already_set_is_not_touched() {
        let kit = server_kit(&[("service", "searxng")]);
        kit.nixos_file(
            SERVICES_FILE,
            "{\n  vexos.server.searxng.environmentFile = \"/my/own\";\n}\n",
        );
        assert_eq!(run_enable(&kit), 0);
        assert!(!kit.nixos.join("secrets").exists());
        assert!(services_text(&kit).contains("environmentFile = \"/my/own\";"));
    }

    #[test]
    fn backups_come_with_the_first_service_when_asked() {
        let kit = server_kit(&[("service", "jellyfin"), ("with_backup", "yes")]);
        assert_eq!(run_enable(&kit), 0, "{}", kit.sink.text());
        let text = services_text(&kit);
        assert!(text.contains("vexos.server.backup.enable = true;"));
        assert!(text
            .contains("vexos.server.backup.repository = \"/var/lib/vexos-backup/restic-repo\";"));
        let password = kit.nixos.join("secrets/backup-password");
        assert!(password.exists());
        assert!(text.contains(&format!(
            "vexos.server.backup.passwordFile = \"{}\";",
            password.display()
        )));
        assert!(kit.sink.text().contains("Backups also enabled"));
    }

    #[test]
    fn existing_backup_settings_are_kept_when_it_comes_along() {
        let kit = server_kit(&[("service", "jellyfin"), ("with_backup", "yes")]);
        kit.nixos_file(SERVICES_FILE, "{\n  vexos.server.backup.repository = \"sftp:me@nas:/b\";\n  vexos.server.backup.passwordFile = \"/p\";\n}\n");
        assert_eq!(run_enable(&kit), 0);
        let text = services_text(&kit);
        assert!(text.contains("repository = \"sftp:me@nas:/b\";"));
        assert!(text.contains("passwordFile = \"/p\";"));
        assert!(!kit.nixos.join("secrets/backup-password").exists());

        // Already on: nothing to add.
        let kit = server_kit(&[("service", "jellyfin"), ("with_backup", "yes")]);
        kit.nixos_file(
            SERVICES_FILE,
            "{\n  vexos.server.backup.enable = true;\n}\n",
        );
        assert_eq!(run_enable(&kit), 0);
        assert!(!services_text(&kit).contains("repository"));
    }

    #[test]
    fn enabling_backup_itself_takes_a_repository_and_a_password_file() {
        let kit = server_kit(&[("service", "backup")]);
        assert_eq!(run_enable(&kit), 1, "needs a repository");

        let mut kit = server_kit(&[]);
        let password = kit.nixos.join("secrets/backup-password");
        set_args(
            &mut kit,
            &[
                ("service", "backup"),
                ("repository", "/mnt/b/repo"),
                ("password_file", password.to_str().unwrap()),
            ],
        );
        assert_eq!(run_enable(&kit), 0, "{}", kit.sink.text());
        assert!(password.exists(), "a missing password file is generated");
        assert!(std::fs::read_to_string(&password).unwrap().trim().len() >= 64);
        let text = services_text(&kit);
        assert!(text.contains("vexos.server.backup.repository = \"/mnt/b/repo\";"));
        assert!(text.contains("vexos.server.backup.enable = true;"));
    }

    #[test]
    fn a_new_password_file_can_only_be_made_in_the_secrets_folder() {
        let kit = server_kit(&[
            ("service", "backup"),
            ("repository", "/mnt/b/repo"),
            ("password_file", "/etc/cron.d/evil"),
        ]);
        assert_eq!(run_enable(&kit), 1);
        assert!(kit.sink.text().contains("only be made inside"));

        let mut kit = server_kit(&[]);
        let sneaky = format!("{}/secrets/../x", kit.nixos.display());
        set_args(
            &mut kit,
            &[
                ("service", "backup"),
                ("repository", "/mnt/b/repo"),
                ("password_file", sneaky.as_str()),
            ],
        );
        assert_eq!(run_enable(&kit), 1);
        assert!(
            kit.sink.text().contains("only be made inside"),
            "{}",
            kit.sink.text()
        );
        assert!(!kit.nixos.join(SERVICES_FILE).exists());

        // A file that is already there is only referenced, wherever it is.
        let mut kit = server_kit(&[]);
        let existing = kit.root.join("elsewhere");
        std::fs::write(&existing, "pw\n").unwrap();
        set_args(
            &mut kit,
            &[
                ("service", "backup"),
                ("repository", "/mnt/b/repo"),
                ("password_file", existing.to_str().unwrap()),
            ],
        );
        assert_eq!(run_enable(&kit), 0, "{}", kit.sink.text());
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "pw\n");
    }

    #[test]
    fn a_failed_edit_leaves_no_secret_file_and_no_seed() {
        let kit = server_kit(&[("service", "arcane")]);
        let failing = FakeRunner::new().failing("nix-instantiate", &["--parse"], 1, "syntax error");
        assert_eq!(block_on(enable(&kit.ctx(&failing))), 1);
        assert!(!kit.nixos.join("secrets/arcane-env").exists());
        assert!(
            !kit.nixos.join(SERVICES_FILE).exists(),
            "the seed is removed again"
        );
    }

    #[test]
    fn a_failed_edit_puts_back_the_token_it_replaced() {
        let mut kit = server_kit(&[("service", "cloudflare-ddns"), ("zone", "example.com")]);
        std::fs::create_dir_all(kit.nixos.join("secrets")).unwrap();
        let file = kit.nixos.join("secrets/cloudflare-ddns-env");
        std::fs::write(
            &file,
            "API_KEY=old
",
        )
        .unwrap();
        kit.secrets.insert("api_token".into(), Secret::new("new"));
        let failing = FakeRunner::new().failing("nix-instantiate", &["--parse"], 1, "syntax error");
        assert_eq!(block_on(enable(&kit.ctx(&failing))), 1);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "API_KEY=old
"
        );
    }

    // ── disable ──

    fn run_disable(kit: &Kit) -> i32 {
        block_on(disable(&kit.ctx(&runner())))
    }

    #[test]
    fn disabling_flips_the_line_and_keeps_its_comment() {
        let kit = server_kit(&[("service", "jellyfin")]);
        kit.nixos_file(
            SERVICES_FILE,
            "{\n  vexos.server.jellyfin.enable = true; # tv\n}\n",
        );
        assert_eq!(run_disable(&kit), 0);
        assert!(services_text(&kit).contains("vexos.server.jellyfin.enable = false; # tv"));
        assert!(kit.sink.text().contains("Disabled"));
    }

    #[test]
    fn disabling_an_arr_component_the_meta_flag_turned_on_pins_it_off() {
        let kit = server_kit(&[("service", "sonarr")]);
        kit.nixos_file(SERVICES_FILE, "{\n  vexos.server.arr.enable = true;\n}\n");
        assert_eq!(run_disable(&kit), 0, "{}", kit.sink.text());
        let text = services_text(&kit);
        assert!(text.contains("vexos.server.arr.sonarr.enable = false;"));
        assert!(!is_enabled(&text, find("sonarr").unwrap()));
    }

    #[test]
    fn disabling_what_is_already_off_changes_nothing() {
        let kit = server_kit(&[("service", "jellyfin")]);
        let before = "{\n  vexos.server.jellyfin.enable = false;\n}\n";
        kit.nixos_file(SERVICES_FILE, before);
        assert_eq!(run_disable(&kit), 0);
        assert_eq!(services_text(&kit), before);
        assert!(kit.sink.text().contains("already disabled"));

        let kit = server_kit(&[("service", "jellyfin")]);
        assert_eq!(run_disable(&kit), 0);
        assert!(
            !kit.nixos.join(SERVICES_FILE).exists(),
            "nothing is created to disable"
        );
    }

    // ── set-url ──

    fn run_set_url(kit: &Kit) -> i32 {
        block_on(set_url(&kit.ctx(&runner())))
    }

    #[test]
    fn set_url_writes_the_address_of_an_enabled_service() {
        let kit = server_kit(&[
            ("service", "wishlist"),
            ("url", "https://wish.example.org/"),
        ]);
        kit.nixos_file(
            SERVICES_FILE,
            "{\n  vexos.server.wishlist.enable = true;\n}\n",
        );
        assert_eq!(run_set_url(&kit), 0, "{}", kit.sink.text());
        assert!(services_text(&kit)
            .contains("vexos.server.wishlist.origin = \"https://wish.example.org\";"));
    }

    #[test]
    fn set_url_refuses_a_service_without_one_or_that_is_off() {
        let kit = server_kit(&[("service", "jellyfin"), ("url", "https://x.example")]);
        kit.nixos_file(
            SERVICES_FILE,
            "{\n  vexos.server.jellyfin.enable = true;\n}\n",
        );
        assert_eq!(run_set_url(&kit), 1);
        assert!(kit.sink.text().contains("Services that do:"));

        let kit = server_kit(&[("service", "wishlist"), ("url", "https://x.example")]);
        assert_eq!(run_set_url(&kit), 1);
        assert!(kit.sink.text().contains("not enabled"));

        let kit = server_kit(&[("service", "wishlist"), ("url", "javascript:alert(1)")]);
        kit.nixos_file(
            SERVICES_FILE,
            "{\n  vexos.server.wishlist.enable = true;\n}\n",
        );
        assert_eq!(run_set_url(&kit), 1);
    }

    // ── restart ──

    fn shown(units: &[(&str, &str, &str)]) -> Output {
        Output {
            code: 0,
            stdout: units
                .iter()
                .map(|(id, load, active)| {
                    format!("Id={id}.service\nLoadState={load}\nActiveState={active}\nSubState=x\n")
                })
                .collect::<Vec<_>>()
                .join("\n"),
            stderr: String::new(),
        }
    }

    #[test]
    fn restart_resets_then_restarts_the_units_that_exist_in_one_transaction() {
        let kit = server_kit(&[("service", "arcane")]);
        let fake = FakeRunner::new()
            .ok("systemctl", &["reset-failed"])
            .ok("systemctl", &["restart"])
            .on(
                "systemctl",
                &["show"],
                shown(&[
                    ("docker-arcane", "loaded", "active"),
                    ("podman-arcane", "not-found", "inactive"),
                ]),
            );
        // The first `show` names both units; the second only the one that exists.
        let code = block_on(restart(&kit.ctx(&fake)));
        assert_eq!(code, 0, "{}", kit.sink.text());
        let lines = fake.lines();
        assert!(
            lines
                .iter()
                .any(|l| l == "systemctl reset-failed docker-arcane.service"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l == "systemctl restart docker-arcane.service"),
            "{lines:?}"
        );
        assert!(
            lines.iter().position(|l| l.contains("reset-failed"))
                < lines.iter().position(|l| l.contains("restart docker"))
        );
        assert!(kit.sink.text().contains("Restarted: arcane"));
    }

    #[test]
    fn restart_says_so_when_nothing_is_built_yet() {
        let kit = server_kit(&[("service", "jellyfin")]);
        let fake = FakeRunner::new().on(
            "systemctl",
            &["show"],
            shown(&[("jellyfin", "not-found", "inactive")]),
        );
        assert_eq!(block_on(restart(&kit.ctx(&fake))), 1);
        assert!(kit.sink.text().contains("enabled and rebuilt"));
        assert!(!fake.lines().iter().any(|l| l.contains("restart")));
    }

    #[test]
    fn restart_trusts_the_exit_status_and_reports_a_failed_restart() {
        let kit = server_kit(&[("service", "jellyfin")]);
        let fake = FakeRunner::new()
            .on(
                "systemctl",
                &["show"],
                shown(&[("jellyfin", "loaded", "failed")]),
            )
            .ok("systemctl", &["reset-failed"])
            .ok("systemctl", &["restart"]);
        // A oneshot (backup, kernel-builder) is inactive once it has run; the restart
        // itself succeeded, so that is not a failure.
        assert_eq!(block_on(restart(&kit.ctx(&fake))), 0);
        assert!(kit.sink.text().contains("Restarted: jellyfin"));
        assert!(kit.sink.text().contains("Not running now"));

        let kit = server_kit(&[("service", "jellyfin")]);
        let fake = FakeRunner::new()
            .on(
                "systemctl",
                &["show"],
                shown(&[("jellyfin", "loaded", "active")]),
            )
            .ok("systemctl", &["reset-failed"])
            .failing("systemctl", &["restart"], 1, "Job failed");
        assert_eq!(block_on(restart(&kit.ctx(&fake))), 1);
    }

    // ── Plex Pass ──

    #[test]
    fn plex_pass_needs_plex_and_flips_one_line() {
        let kit = server_kit(&[]);
        assert_eq!(block_on(plex_pass(&kit.ctx(&runner()), true)), 1);
        assert!(kit.sink.text().contains("Plex is not enabled"));

        let kit = server_kit(&[]);
        kit.nixos_file(SERVICES_FILE, "{\n  vexos.server.plex.enable = true;\n}\n");
        assert_eq!(
            block_on(plex_pass(&kit.ctx(&runner()), true)),
            0,
            "{}",
            kit.sink.text()
        );
        assert!(services_text(&kit).contains("vexos.server.plex.plexPass = true;"));
        assert_eq!(block_on(plex_pass(&kit.ctx(&runner()), false)), 0);
        assert!(services_text(&kit).contains("vexos.server.plex.plexPass = false;"));

        let kit = server_kit(&[]);
        assert_eq!(block_on(plex_pass(&kit.ctx(&runner()), false)), 0);
        assert!(kit.sink.text().contains("already disabled"));
    }
}
