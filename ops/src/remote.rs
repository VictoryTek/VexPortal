//! Shares from another host (NFS or SMB), mounted here.
//!
//! The configuration lives in `/etc/nixos/storage-remote.nix`, one line per share between
//! two marker comments. `just remote-storage` reads and writes the same file in the same
//! shape, so either path can manage what the other attached.
//!
//! The parsers for what the network tools print are here too, so they are tested
//! without a network; the GUI runs the tools and hands the text to them.

use crate::features::role_of;
use crate::nixedit::{self, EditError, NixValue};
use crate::runner::{Cmd, Runner};
use crate::services::{ensure_private_dir, secrets_dir};
use crate::system::read_variant;
use crate::{capture, fail, fsutil, Ctx};
use std::path::{Path, PathBuf};

pub const FILE: &str = "storage-remote.nix";
pub const BEGIN: &str = "# >>> vexos-remote-entries >>>";
pub const END: &str = "# <<< vexos-remote-entries <<<";

/// Where the media group starts looking for a free number when nothing is set yet.
const FIRST_MEDIA_GID: u32 = 1500;

pub fn path(nixos_dir: &Path) -> PathBuf {
    nixos_dir.join(FILE)
}

/// One attached share, as the file describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `nfs` or `cifs`.
    pub kind: String,
    pub server: String,
    pub export: String,
    pub mount_point: String,
    pub credentials_file: Option<String>,
    pub gid: Option<u32>,
    pub uid: Option<u32>,
}

/// The text after `name = "`, up to the closing quote. `name` must start a word, so
/// `server` does not match inside something longer.
fn quoted_field(line: &str, name: &str) -> Option<String> {
    let needle = format!("{name} = \"");
    let mut from = 0;
    while let Some(found) = line[from..].find(&needle) {
        let at = from + found;
        let boundary = line[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        if boundary {
            let rest = &line[at + needle.len()..];
            return rest.find('"').map(|end| rest[..end].to_string());
        }
        from = at + needle.len();
    }
    None
}

fn number_field(line: &str, name: &str) -> Option<u32> {
    let needle = format!("{name} = ");
    let mut from = 0;
    while let Some(found) = line[from..].find(&needle) {
        let at = from + found;
        let boundary = line[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        if boundary {
            let digits: String = line[at + needle.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            return digits.parse().ok();
        }
        from = at + needle.len();
    }
    None
}

fn parse_entry(line: &str) -> Option<Entry> {
    Some(Entry {
        kind: quoted_field(line, "type")?,
        server: quoted_field(line, "server")?,
        export: quoted_field(line, "export")?,
        mount_point: quoted_field(line, "mountPoint")?,
        credentials_file: quoted_field(line, "credentialsFile"),
        gid: number_field(line, "gid"),
        uid: number_field(line, "uid"),
    })
}

/// The shares between the markers. Anything outside them is not managed here.
pub fn entries(text: &str) -> Vec<Entry> {
    let mut inside = false;
    let mut found = Vec::new();
    for line in text.lines() {
        if line.contains(BEGIN) {
            inside = true;
        } else if line.contains(END) {
            inside = false;
        } else if inside && !line.trim_start().starts_with('#') {
            found.extend(parse_entry(line));
        }
    }
    found
}

/// A file with no shares in it yet.
pub fn template() -> String {
    format!(
        "# /etc/nixos/storage-remote.nix\n\
         # Managed by VexPortal; `just remote-storage` reads and writes the same lines.\n\
         # Entries between the markers are managed for you.\n\
         # Host-generated: do not commit it to the vexos-nix repo. CIFS logins live in\n\
         # /etc/nixos/secrets and are referenced by path only.\n\
         {{ ... }}:\n\
         {{\n  vexos.storage.remote = [\n    {BEGIN}\n    {END}\n  ];\n}}\n"
    )
}

/// The line for one share. Every value is quoted by the same code that quotes the other
/// files, so nothing a person typed can end the string.
pub fn render_entry(entry: &Entry) -> Result<String, EditError> {
    let q = |name: &str, value: &str| NixValue::str(value).render(name);
    let mut line = format!(
        "      {{ type = {}; server = {}; export = {}; mountPoint = {};",
        q("type", &entry.kind)?,
        q("server", &entry.server)?,
        q("export", &entry.export)?,
        q("mountPoint", &entry.mount_point)?,
    );
    if let Some(file) = &entry.credentials_file {
        line.push_str(&format!(
            " credentialsFile = {};",
            q("credentialsFile", file)?
        ));
    }
    if let Some(gid) = entry.gid {
        line.push_str(&format!(" gid = {gid};"));
    }
    if let Some(uid) = entry.uid {
        line.push_str(&format!(" uid = {uid};"));
    }
    line.push_str(" }");
    Ok(line)
}

/// `text` with `line` added before the end marker.
pub fn add_entry(text: &str, line: &str) -> Result<String, String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let begin = lines.iter().position(|l| l.contains(BEGIN));
    let end = lines.iter().position(|l| l.contains(END));
    match (begin, end) {
        (Some(b), Some(e)) if b < e => {
            let mut out = String::new();
            for (i, l) in lines.iter().enumerate() {
                if i == e {
                    out.push_str(line);
                    out.push('\n');
                }
                out.push_str(l);
            }
            Ok(out)
        }
        _ => Err(format!(
            "{FILE} has no `{BEGIN}` / `{END}` block to add to. Put the markers back, or move the file aside."
        )),
    }
}

/// `text` without the share mounted at `mount`, or `None` if there is no such share.
pub fn remove_entry(text: &str, mount: &str) -> Option<String> {
    let mut inside = false;
    let mut removed = false;
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        if line.contains(BEGIN) {
            inside = true;
        } else if line.contains(END) {
            inside = false;
        } else if inside
            && !line.trim_start().starts_with('#')
            && parse_entry(line).is_some_and(|e| e.mount_point == mount)
        {
            removed = true;
            continue;
        }
        out.push_str(line);
    }
    removed.then_some(out)
}

/// A name that is safe in a path or a file name: letters, digits, dot, underscore and
/// dash, with runs of anything else squeezed to one dash.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_') {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '.') {
        "share".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `/mnt/<server>/<share>`, so shares from different servers never collide and group by
/// server. A bare address becomes `nas-1-2-3-4`.
pub fn derive_mount(server: &str, export: &str) -> String {
    let name = if server.chars().all(|c| c.is_ascii_digit() || c == '.') {
        format!("nas-{}", server.replace('.', "-"))
    } else {
        server.split('.').next().unwrap_or(server).to_string()
    };
    let share = export
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    format!("/mnt/{}/{}", slug(&name), slug(share))
}

/// Why `gid` cannot be the media group's number, if it cannot. `existing` is the number
/// the other shares already use (they share one group); `group_name` is who owns `gid`
/// on this machine today.
pub fn gid_problem(gid: u32, existing: Option<u32>, group_name: Option<&str>) -> Option<String> {
    if gid == 0 {
        return Some("group 0 is root".into());
    }
    if let Some(other) = existing.filter(|e| *e != gid) {
        return Some(format!(
            "your other shares already use group {other} (all shares share one group)"
        ));
    }
    match group_name {
        Some(name) if !name.is_empty() && name != "media" => Some(format!(
            "group {gid} is already used on this machine by '{name}'"
        )),
        _ => None,
    }
}

// ── What the network tools print ──────────────────────────────────────────────

/// A server found over mDNS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub address: String,
    pub label: String,
    pub name: String,
}

/// `avahi-browse -t -r -p` lines for one service type, without this machine's own
/// addresses. Avahi writes odd bytes of a name as `\DDD` (decimal).
pub fn parse_avahi(output: &str, own: &[String]) -> Vec<Host> {
    let mut seen: Vec<String> = Vec::new();
    let mut hosts = Vec::new();
    for line in output.lines() {
        let f: Vec<&str> = line.split(';').collect();
        if f.len() < 9 || f[0] != "=" || f[2] != "IPv4" {
            continue;
        }
        let address = f[7];
        if own.iter().any(|a| a == address) || seen.iter().any(|a| a == address) {
            continue;
        }
        seen.push(address.to_string());
        let name = unescape_avahi(f[3]);
        let short = f[6].split('.').next().unwrap_or("").to_string();
        hosts.push(Host {
            address: address.to_string(),
            label: format!("{name} ({address})"),
            name: short,
        });
    }
    hosts
}

fn unescape_avahi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 4 <= bytes.len()
            && bytes[i + 1..i + 4].iter().all(u8::is_ascii_digit)
        {
            let n: u32 = text[i + 1..i + 4].parse().unwrap_or(63);
            out.push(u8::try_from(n).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `showmount -e --no-headers` output: the export paths.
pub fn parse_exports(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|first| first.starts_with('/'))
        .map(str::to_string)
        .collect()
}

/// `smbclient -L -g` output: disk shares, without the hidden administrative ones.
pub fn parse_shares(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|l| {
            let mut f = l.split('|');
            (f.next()? == "Disk").then_some(())?;
            let name = f.next()?;
            (!name.ends_with('$') && !name.is_empty()).then(|| name.to_string())
        })
        .collect()
}

/// Whether `name` is installed, looking where the daemon's tools are found.
fn tool_present(root: &Path, name: &str) -> bool {
    [
        "run/wrappers/bin",
        "run/current-system/sw/bin",
        "usr/bin",
        "bin",
    ]
    .iter()
    .any(|dir| root.join(dir).join(name).exists())
}

// ── attach ────────────────────────────────────────────────────────────────────

fn finish(result: Result<(), i32>) -> i32 {
    result.err().unwrap_or(0)
}

fn require_role<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    let variant = read_variant(ctx.nixos_dir).unwrap_or_default();
    match role_of(&variant) {
        Some("desktop" | "htpc" | "server" | "headless-server") => Ok(()),
        _ => Err(fail(
            ctx.sink,
            format!(
                "remote shares are not available on this role yet (this host is {}). \
                 Supported: desktop, htpc, server, headless-server.",
                if variant.is_empty() {
                    "unknown"
                } else {
                    &variant
                }
            ),
        )),
    }
}

fn yes<R: Runner>(ctx: &Ctx<'_, R>, name: &str, default: bool) -> bool {
    match ctx.args.get(name).map(String::as_str) {
        Some("yes") => true,
        Some("no") => false,
        _ => default,
    }
}

fn credentials_path<R: Runner>(ctx: &Ctx<'_, R>, mount: &str) -> PathBuf {
    secrets_dir(ctx.nixos_dir).join(format!("remote-{}-credentials", slug(mount)))
}

/// What a failed attach has to put back: the login file it wrote.
struct Undo {
    path: Option<PathBuf>,
    previous: Option<Vec<u8>>,
}

impl Undo {
    fn run(&self) {
        let Some(path) = &self.path else { return };
        match &self.previous {
            Some(bytes) => {
                let _ = fsutil::write_atomic(path, bytes, 0o600);
            }
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// One attach or detach at a time: they share the configuration file and the login files,
/// and each reads the file before it decides.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn attach<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let _only = ONE_AT_A_TIME.lock().await;
    finish(attach_inner(ctx).await)
}

async fn attach_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_role(ctx)?;
    let arg = |name: &str| {
        ctx.args
            .get(name)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
    };
    let Some(kind) = arg("protocol").filter(|p| matches!(*p, "nfs" | "cifs")) else {
        return Err(fail(ctx.sink, "the protocol must be nfs or cifs."));
    };
    let (Some(server), Some(share)) = (arg("server"), arg("share")) else {
        return Err(fail(ctx.sink, "a server and a share are required."));
    };
    let cifs = kind == "cifs";
    let export = if cifs {
        if share.contains('/') {
            return Err(fail(ctx.sink, "an SMB share name has no slash in it."));
        }
        share.to_string()
    } else {
        format!("/{}", share.trim_start_matches('/'))
    };

    let mount = match arg("mountpoint") {
        Some(m) => m.to_string(),
        None => derive_mount(server, &export),
    };
    let sane = Path::new(&mount).starts_with("/mnt")
        && Path::new(&mount) != Path::new("/mnt")
        && !mount.chars().any(char::is_control)
        && !mount.contains(['"', '\\', '$'])
        && Path::new(&mount)
            .components()
            .all(|c| !matches!(c, std::path::Component::ParentDir));
    if !sane {
        return Err(fail(
            ctx.sink,
            format!("the mount point must be a folder under /mnt, not {mount}."),
        ));
    }

    let file = path(ctx.nixos_dir);
    let existing = std::fs::read_to_string(&file).unwrap_or_default();
    let others = entries(&existing);
    if others.iter().any(|e| e.mount_point == mount) {
        return Err(fail(
            ctx.sink,
            format!(
                "{mount} is already used by an attached share. Detach it first, or choose another mount point."
            ),
        ));
    }
    let existing_gid = others.iter().find_map(|e| e.gid);

    // The login, before the share is tried: the test mount reads it from the file.
    let mut undo = Undo {
        path: None,
        previous: None,
    };
    let mut credentials = None;
    if cifs {
        let (Some(user), Some(password)) = (
            arg("username"),
            ctx.secrets.get("password").map(|p| p.expose().to_string()),
        ) else {
            return Err(fail(
                ctx.sink,
                "an SMB share needs a username and a password.",
            ));
        };
        // A login file holds each on one line; spaces inside a password are fine.
        if user.chars().any(char::is_control)
            || password.is_empty()
            || password.chars().any(char::is_control)
        {
            return Err(fail(
                ctx.sink,
                "the username and password must each be one line.",
            ));
        }
        let cred = credentials_path(ctx, &mount);
        // Two mount points can slug to one file name, and the second login would replace
        // the first share's.
        if others
            .iter()
            .any(|e| e.credentials_file.as_deref() == Some(cred.to_string_lossy().as_ref()))
        {
            return Err(fail(
                ctx.sink,
                format!(
                    "another attached share already uses the login file {}. Choose a different mount point.",
                    cred.display()
                ),
            ));
        }
        let dir = secrets_dir(ctx.nixos_dir);
        ensure_private_dir(&dir).map_err(|e| {
            fail(
                ctx.sink,
                format!("could not prepare {}: {e}", dir.display()),
            )
        })?;
        undo = Undo {
            path: Some(cred.clone()),
            previous: std::fs::read(&cred).ok(),
        };
        fsutil::write_atomic(
            &cred,
            format!("username={user}\npassword={password}\n").as_bytes(),
            0o600,
        )
        .map_err(|e| fail(ctx.sink, format!("could not write {}: {e}", cred.display())))?;
        ctx.sink.info(&format!(
            "✔ Login saved to {} (root only, not in the Nix store).",
            cred.display()
        ));
        credentials = Some(cred);
    }

    let result = attach_with(
        ctx,
        &file,
        &Settings {
            kind,
            server,
            export: &export,
            mount: &mount,
            credentials: credentials.as_deref(),
            existing_gid,
        },
    )
    .await;
    if result.is_err() {
        undo.run();
    }
    result
}

struct Settings<'a> {
    kind: &'a str,
    server: &'a str,
    export: &'a str,
    mount: &'a str,
    credentials: Option<&'a Path>,
    existing_gid: Option<u32>,
}

async fn attach_with<R: Runner>(
    ctx: &Ctx<'_, R>,
    file: &Path,
    s: &Settings<'_>,
) -> Result<(), i32> {
    let cifs = s.kind == "cifs";
    let mut detected_gid = None;
    if yes(ctx, "test_mount", true) {
        detected_gid = test_mount(ctx, s).await?;
    }

    // Who may write.
    let media = yes(ctx, "media_write", true);
    let mut gid = None;
    if media {
        let requested = ctx
            .args
            .get("gid")
            .and_then(|g| g.trim().parse::<u32>().ok());
        gid =
            Some(if cifs {
                // Only a local label: the SMB account decides what is really allowed.
                match s.existing_gid {
                    Some(g) => g,
                    None => free_gid(ctx).await?,
                }
            } else {
                match requested.or(s.existing_gid).or(detected_gid) {
                    Some(g) => g,
                    None => return Err(fail(
                        ctx.sink,
                        "could not read which group owns the share, so enter the group number, \
                         or turn off write access for media services.",
                    )),
                }
            });
        if let Some(g) = gid {
            let name = group_name(ctx, g).await?;
            if let Some(problem) = gid_problem(g, s.existing_gid, name.as_deref()) {
                return Err(fail(ctx.sink, format!("cannot use group {g}: {problem}")));
            }
        }
    }
    let uid = (cifs && yes(ctx, "owner_write", true))
        .then_some(ctx.caller_uid)
        .flatten()
        .filter(|u| *u != 0);

    let line = render_entry(&Entry {
        kind: s.kind.to_string(),
        server: s.server.to_string(),
        export: s.export.to_string(),
        mount_point: s.mount.to_string(),
        credentials_file: s.credentials.map(|p| p.to_string_lossy().into_owned()),
        gid,
        uid,
    })
    .map_err(|e| fail(ctx.sink, e))?;

    if !file.exists() {
        fsutil::create_new(file, template().as_bytes(), 0o644).map_err(|e| {
            fail(
                ctx.sink,
                format!("could not create {}: {e}", file.display()),
            )
        })?;
    }
    nixedit::edit_file(ctx.runner, file, |text| {
        add_entry(text, &line).map(Some).map_err(EditError::Invalid)
    })
    .await
    .map_err(|e| {
        fail(
            ctx.sink,
            format!("could not update {}: {e}", file.display()),
        )
    })?;

    ctx.sink.info(&format!(
        "✔ Attached {}:{} → {} (applies on the next rebuild).",
        s.server, s.export, s.mount
    ));
    ctx.sink.info(
        "The share mounts on first use (x-systemd.automount), so an offline server never blocks boot.",
    );
    if uid.is_some() {
        ctx.sink.info(
            "Your account owns the mount. The login still decides what the server really allows.",
        );
    }
    if let Some(g) = gid {
        ctx.sink.info(&format!(
            "Media services join group 'media' ({g}) on rebuild; restart the ones you run afterwards."
        ));
        if !cifs {
            ctx.sink.info(&format!(
                "NFS: the export must also let group {g} write (Mapall User/Group, or a dataset ACL). \
                 Nothing on this side can grant that."
            ));
        }
    }
    Ok(())
}

/// Mount the share on a scratch folder and look at it. `Ok(gid)` is the group that owns
/// an NFS share's root, when it could be read.
async fn test_mount<R: Runner>(ctx: &Ctx<'_, R>, s: &Settings<'_>) -> Result<Option<u32>, i32> {
    use std::os::unix::fs::MetadataExt;
    let cifs = s.kind == "cifs";
    let helper = if cifs { "mount.cifs" } else { "mount.nfs" };
    if !tool_present(ctx.root, helper) {
        ctx.sink.info(&format!(
            "warning: {helper} is not installed yet (it arrives with the rebuild), so the test mount is skipped."
        ));
        return Ok(None);
    }
    let scratch = ctx.root.join("run").join(format!(
        "vexportal-remote-test.{}.{}",
        std::process::id(),
        fsutil::random_hex(4).unwrap_or_default()
    ));
    std::fs::create_dir_all(&scratch)
        .map_err(|e| fail(ctx.sink, format!("could not make a test folder: {e}")))?;
    let target = scratch.to_string_lossy().into_owned();
    let cmd = if cifs {
        let cred = s
            .credentials
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        Cmd::new("mount").args([
            "-t".to_string(),
            "cifs".into(),
            "-o".into(),
            format!("credentials={cred}"),
            format!("//{}/{}", s.server, s.export),
            target.clone(),
        ])
    } else {
        Cmd::new("mount").args([
            "-t".to_string(),
            "nfs".into(),
            "-o".into(),
            "nfsvers=4.2,soft,timeo=50".into(),
            format!("{}:{}", s.server, s.export),
            target.clone(),
        ])
    };
    ctx.sink
        .info(&format!("Trying a test mount of {}…", s.export));
    let mounted = capture(ctx, &cmd).await;
    let result = match mounted {
        Ok(out) if out.success() => {
            let gid = (!cifs)
                .then(|| std::fs::metadata(&scratch).ok().map(|m| m.gid()))
                .flatten();
            ctx.sink.info("✔ The test mount worked.");
            Ok(gid)
        }
        Ok(out) => {
            ctx.sink.error(&format!(
                "error: the test mount failed: {}",
                out.stderr.trim()
            ));
            ctx.sink.info(
                "Nothing was written. Check the address and the login, or turn off the test to attach anyway.",
            );
            Err(1)
        }
        Err(code) => Err(code),
    };
    // Whatever happened, do not leave it mounted or the folder behind, even after a cancel.
    let _ = ctx.runner.cleanup(&Cmd::new("umount").arg(&target)).await;
    let _ = std::fs::remove_dir(&scratch);
    result
}

/// The group's name if the machine has one with this number.
async fn group_name<R: Runner>(ctx: &Ctx<'_, R>, gid: u32) -> Result<Option<String>, i32> {
    let out = capture(ctx, &Cmd::new("getent").args(["group", &gid.to_string()])).await?;
    Ok(out
        .success()
        .then(|| out.stdout.split(':').next().map(str::to_string))
        .flatten()
        .filter(|n| !n.is_empty()))
}

/// The first number from [`FIRST_MEDIA_GID`] that no group on this machine uses.
async fn free_gid<R: Runner>(ctx: &Ctx<'_, R>) -> Result<u32, i32> {
    for gid in FIRST_MEDIA_GID..FIRST_MEDIA_GID + 200 {
        if group_name(ctx, gid).await?.is_none() {
            return Ok(gid);
        }
    }
    Err(fail(ctx.sink, "no free group number found near 1500."))
}

// ── detach ────────────────────────────────────────────────────────────────────

pub async fn detach<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    let _only = ONE_AT_A_TIME.lock().await;
    finish(detach_inner(ctx).await)
}

async fn detach_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    require_role(ctx)?;
    let Some(mount) = ctx.args.get("mount").map(|m| m.trim()) else {
        return Err(fail(ctx.sink, "a mount point is required."));
    };
    let file = path(ctx.nixos_dir);
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let all = entries(&text);
    let Some(entry) = all.iter().find(|e| e.mount_point == mount).cloned() else {
        let known: Vec<&str> = all.iter().map(|e| e.mount_point.as_str()).collect();
        return Err(fail(
            ctx.sink,
            format!(
                "no attached share is mounted at {mount}. Attached: {}",
                if known.is_empty() {
                    "none".into()
                } else {
                    known.join(", ")
                }
            ),
        ));
    };

    // The configuration first: that is the part that matters. What follows is cleanup
    // of this session's state and only warns.
    nixedit::edit_file(ctx.runner, &file, |text| Ok(remove_entry(text, mount)))
        .await
        .map_err(|e| {
            fail(
                ctx.sink,
                format!("could not update {}: {e}", file.display()),
            )
        })?;
    ctx.sink.info(&format!(
        "✔ Removed {}:{} from {FILE}.",
        entry.server, entry.export
    ));

    let unit = capture(
        ctx,
        &Cmd::new("systemd-escape").args(["-p", "--suffix=automount", mount]),
    )
    .await?;
    let unit = unit.stdout.trim().to_string();
    if unit.ends_with(".automount") {
        let _ = capture(ctx, &Cmd::new("systemctl").args(["stop", &unit])).await?;
    }
    let mounted = capture(
        ctx,
        &Cmd::new("findmnt").args(["-rn", "--mountpoint", mount]),
    )
    .await?;
    if mounted.success() {
        let plain = capture(ctx, &Cmd::new("umount").arg(mount)).await?;
        if plain.success() {
            ctx.sink.info(&format!("✔ Unmounted {mount}."));
        } else if capture(ctx, &Cmd::new("umount").args(["-l", mount]))
            .await?
            .success()
        {
            ctx.sink.info(&format!(
                "warning: {mount} was busy, so it was detached lazily and clears when the last user closes it."
            ));
        } else {
            ctx.sink.info(&format!(
                "warning: could not unmount {mount}. Close what uses it and run `umount {mount}`; the rebuild drops the unit regardless."
            ));
        }
    } else {
        ctx.sink.info(&format!("{mount} was not mounted."));
    }

    // Only an empty folder goes, and the server's folder under /mnt with it if that is
    // now empty. `remove_dir` refuses anything with files in it.
    if Path::new(mount).starts_with("/mnt") && std::fs::remove_dir(mount).is_ok() {
        ctx.sink
            .info(&format!("✔ Removed the empty folder {mount}."));
        if let Some(parent) = Path::new(mount).parent() {
            if parent != Path::new("/mnt") && parent.starts_with("/mnt") {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }

    // The login file goes only if nothing left uses it, and only from the secrets folder.
    if let Some(cred) = &entry.credentials_file {
        let remaining = std::fs::read_to_string(&file).unwrap_or_default();
        let still_used = entries(&remaining)
            .iter()
            .any(|e| e.credentials_file.as_deref() == Some(cred));
        let ours = Path::new(cred).starts_with(secrets_dir(ctx.nixos_dir))
            && !Path::new(cred)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir));
        if !still_used && ours && std::fs::remove_file(cred).is_ok() {
            ctx.sink.info(&format!("✔ Removed the login file {cred}."));
        }
    }
    ctx.sink
        .info("Apply with a rebuild: NixOS then drops the mount unit and its fstab line.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::testing::{block_on, FakeRunner};
    use crate::runner::Output;
    use crate::testkit::Kit;
    use crate::Secret;

    fn out(stdout: &str) -> Output {
        Output {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    const NFS: &str = r#"      { type = "nfs"; server = "10.0.0.5"; export = "/tank/media"; mountPoint = "/mnt/nas/media"; gid = 1500; }"#;
    const SMB: &str = r#"      { type = "cifs"; server = "nas.local"; export = "tv"; mountPoint = "/mnt/nas/tv"; credentialsFile = "/etc/nixos/secrets/remote-mnt-nas-tv-credentials"; gid = 1500; uid = 1000; }"#;

    fn file_with(lines: &[&str]) -> String {
        let mut text = template();
        for line in lines {
            text = add_entry(&text, line).unwrap();
        }
        text
    }

    // ── the file ──────────────────────────────────────────────────────────────

    #[test]
    fn entries_are_read_between_the_markers() {
        let text = file_with(&[NFS, SMB]);
        let all = entries(&text);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].kind, "nfs");
        assert_eq!(all[0].export, "/tank/media");
        assert_eq!(all[0].gid, Some(1500));
        assert_eq!(all[0].uid, None);
        assert_eq!(all[1].kind, "cifs");
        assert_eq!(all[1].uid, Some(1000));
        assert_eq!(
            all[1].credentials_file.as_deref(),
            Some("/etc/nixos/secrets/remote-mnt-nas-tv-credentials")
        );
        assert!(entries("").is_empty());
        assert!(entries(&template()).is_empty());
    }

    #[test]
    fn a_line_the_recipe_wrote_is_read_too() {
        // The shape `just remote-storage attach` produces.
        let text = format!(
            "{{ ... }}:\n{{\n  vexos.storage.remote = [\n    {BEGIN}\n{NFS}\n    {END}\n  ];\n}}\n"
        );
        assert_eq!(entries(&text).len(), 1);
    }

    #[test]
    fn commented_lines_and_lines_outside_the_markers_are_not_shares() {
        let text = format!("{NFS}\n{BEGIN}\n# {SMB}\n{END}\n{SMB}\n");
        assert!(entries(&text).is_empty());
    }

    #[test]
    fn rendered_entries_read_back_the_same() {
        let entry = Entry {
            kind: "cifs".into(),
            server: "nas.local".into(),
            export: "tv".into(),
            mount_point: "/mnt/nas/tv".into(),
            credentials_file: Some("/etc/nixos/secrets/x".into()),
            gid: Some(1500),
            uid: Some(1000),
        };
        let line = render_entry(&entry).unwrap();
        assert_eq!(parse_entry(&line), Some(entry));
        // The line a person's typing cannot break out of.
        let hostile = Entry {
            server: "a\"; evil = \"".into(),
            ..parse_entry(&line).unwrap()
        };
        let line = render_entry(&hostile).unwrap();
        assert!(line.contains(r#"a\"; evil = \""#), "{line}");
    }

    #[test]
    fn an_entry_is_added_before_the_end_marker_and_removed_by_its_mount() {
        let text = file_with(&[NFS]);
        let added = add_entry(&text, SMB).unwrap();
        assert_eq!(entries(&added).len(), 2);
        assert!(added.find(SMB).unwrap() < added.find(END).unwrap());

        let removed = remove_entry(&added, "/mnt/nas/media").unwrap();
        let left = entries(&removed);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].mount_point, "/mnt/nas/tv");
        assert!(remove_entry(&added, "/mnt/other").is_none());
        // Text around the block is untouched.
        assert!(removed.starts_with("# /etc/nixos/storage-remote.nix"));
    }

    #[test]
    fn a_file_without_the_markers_is_refused_not_guessed_at() {
        assert!(add_entry("{ ... }: { }\n", NFS).is_err());
        assert!(add_entry(&format!("{END}\n{BEGIN}\n"), NFS).is_err());
    }

    #[test]
    fn mount_points_come_from_the_server_and_share() {
        assert_eq!(derive_mount("nas.local", "/tank/media"), "/mnt/nas/media");
        assert_eq!(derive_mount("10.0.0.5", "tv"), "/mnt/nas-10-0-0-5/tv");
        assert_eq!(derive_mount("box", "/a b/c d"), "/mnt/box/c-d");
        assert_eq!(derive_mount("box", "/"), "/mnt/box/share");
        assert_eq!(slug(".."), "share");
        assert_eq!(slug("--x--y--"), "x-y");
    }

    #[test]
    fn the_media_group_number_is_checked() {
        assert!(gid_problem(0, None, None).is_some());
        assert!(gid_problem(1500, Some(1600), None)
            .unwrap()
            .contains("1600"));
        assert!(gid_problem(1500, None, Some("wheel"))
            .unwrap()
            .contains("wheel"));
        assert!(gid_problem(1500, Some(1500), Some("media")).is_none());
        assert!(gid_problem(1500, None, None).is_none());
    }

    // ── what the tools print ──────────────────────────────────────────────────

    #[test]
    fn avahi_hosts_skip_this_machine_and_repeats() {
        let output = "+;eth0;IPv4;ignored;_smb._tcp;local\n\
            =;eth0;IPv4;My\\032NAS;_smb._tcp;local;nas.local;192.168.1.5;445;\"\"\n\
            =;eth0;IPv4;My\\032NAS;_smb._tcp;local;nas.local;192.168.1.5;445;\"\"\n\
            =;eth0;IPv6;v6;_smb._tcp;local;nas.local;fe80::1;445;\"\"\n\
            =;eth0;IPv4;me;_smb._tcp;local;me.local;192.168.1.9;445;\"\"\n";
        let hosts = parse_avahi(output, &["192.168.1.9".to_string()]);
        assert_eq!(
            hosts,
            [Host {
                address: "192.168.1.5".into(),
                label: "My NAS (192.168.1.5)".into(),
                name: "nas".into()
            }]
        );
    }

    #[test]
    fn exports_and_shares_are_read_from_the_tools_output() {
        assert_eq!(
            parse_exports("/tank/media  10.0.0.0/24\n/tank/tv *\nExport list for x:\n"),
            ["/tank/media", "/tank/tv"]
        );
        assert_eq!(
            parse_shares("Disk|media|Movies\nDisk|ADMIN$|x\nPrinter|lp|x\nIPC|IPC$|x\nDisk|tv|\n"),
            ["media", "tv"]
        );
    }

    // ── attach ────────────────────────────────────────────────────────────────

    fn kit(variant: &str, args: &[(&str, &str)]) -> Kit {
        let mut kit = Kit::new().with_args(args);
        kit.nixos_file("vexos-variant", &format!("{variant}\n"));
        kit.caller = Some(1000);
        kit.secrets.insert("password".into(), Secret::new("s3cret"));
        kit
    }

    fn fake() -> FakeRunner {
        FakeRunner::new()
            .ok("nix-instantiate", &[])
            .failing("getent", &["group"], 2, "")
            .ok("mount", &[])
            .ok("umount", &[])
    }

    fn written(kit: &Kit) -> String {
        std::fs::read_to_string(kit.nixos.join(FILE)).unwrap_or_default()
    }

    #[test]
    fn an_nfs_share_is_tested_and_added_with_the_group_it_reports() {
        let kit = kit(
            "vexos-server-amd",
            &[
                ("protocol", "nfs"),
                ("server", "10.0.0.5"),
                ("share", "tank/media"),
            ],
        );
        let helper = kit.root.join("run/current-system/sw/bin");
        std::fs::create_dir_all(&helper).unwrap();
        std::fs::write(helper.join("mount.nfs"), "").unwrap();
        let runner = fake();
        assert_eq!(
            block_on(attach(&kit.ctx(&runner))),
            0,
            "{}",
            kit.sink.text()
        );

        let all = entries(&written(&kit));
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].export, "/tank/media");
        assert_eq!(all[0].mount_point, "/mnt/nas-10-0-0-5/media");
        // The test mount's own group, read from the scratch folder (this user's).
        assert!(all[0].gid.is_some());
        let lines = runner.lines();
        assert!(
            lines
                .iter()
                .any(|l| l
                    .starts_with("mount -t nfs -o nfsvers=4.2,soft,timeo=50 10.0.0.5:/tank/media")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.starts_with("umount ")), "{lines:?}");
        // The scratch folder is gone again.
        let left: Vec<_> = std::fs::read_dir(kit.root.join("run"))
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("vexportal-remote-test")
            })
            .collect();
        assert!(left.is_empty());
    }

    #[test]
    fn an_smb_share_gets_a_private_login_file_the_owner_and_the_first_free_group() {
        use std::os::unix::fs::PermissionsExt;
        let kit = kit(
            "vexos-desktop-amd",
            &[
                ("protocol", "cifs"),
                ("server", "nas.local"),
                ("share", "tv"),
                ("username", "me"),
                ("test_mount", "no"),
            ],
        );
        let runner = fake();
        assert_eq!(
            block_on(attach(&kit.ctx(&runner))),
            0,
            "{}",
            kit.sink.text()
        );

        let cred = kit.nixos.join("secrets/remote-mnt-nas-tv-credentials");
        assert_eq!(
            std::fs::read_to_string(&cred).unwrap(),
            "username=me\npassword=s3cret\n"
        );
        assert_eq!(
            std::fs::metadata(&cred).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let all = entries(&written(&kit));
        assert_eq!(all[0].uid, Some(1000));
        assert_eq!(all[0].gid, Some(1500));
        assert_eq!(
            all[0].credentials_file.as_deref(),
            Some(cred.to_string_lossy().as_ref())
        );
        assert!(!kit.sink.text().contains("s3cret"));
    }

    #[test]
    fn a_failed_test_mount_writes_nothing_and_removes_the_new_login() {
        let kit = kit(
            "vexos-server-amd",
            &[
                ("protocol", "cifs"),
                ("server", "nas.local"),
                ("share", "tv"),
                ("username", "me"),
            ],
        );
        let helper = kit.root.join("bin");
        std::fs::create_dir_all(&helper).unwrap();
        std::fs::write(helper.join("mount.cifs"), "").unwrap();
        let runner = FakeRunner::new()
            .failing("mount", &[], 32, "mount error(13): Permission denied")
            .ok("umount", &[])
            .ok("nix-instantiate", &[]);
        assert_ne!(block_on(attach(&kit.ctx(&runner))), 0);
        assert!(!kit.nixos.join(FILE).exists());
        assert!(!kit
            .nixos
            .join("secrets/remote-mnt-nas-tv-credentials")
            .exists());
        assert!(kit.sink.text().contains("Permission denied"));
    }

    #[test]
    fn a_missing_mount_helper_skips_the_test_with_a_warning() {
        let kit = kit(
            "vexos-server-amd",
            &[
                ("protocol", "nfs"),
                ("server", "box"),
                ("share", "/data"),
                ("gid", "1500"),
            ],
        );
        let runner = fake();
        assert_eq!(
            block_on(attach(&kit.ctx(&runner))),
            0,
            "{}",
            kit.sink.text()
        );
        assert!(kit.sink.text().contains("test mount is skipped"));
        assert!(!runner.lines().iter().any(|l| l.starts_with("mount ")));
    }

    #[test]
    fn an_nfs_share_with_no_known_group_asks_for_one() {
        let kit = kit(
            "vexos-server-amd",
            &[
                ("protocol", "nfs"),
                ("server", "box"),
                ("share", "/data"),
                ("test_mount", "no"),
            ],
        );
        let runner = fake();
        assert_ne!(block_on(attach(&kit.ctx(&runner))), 0);
        assert!(kit.sink.text().contains("enter the group number"));
        assert!(!kit.nixos.join(FILE).exists());
        // Or turn media access off, and it goes through.
        let kit2 = self::kit(
            "vexos-server-amd",
            &[
                ("protocol", "nfs"),
                ("server", "box"),
                ("share", "/data"),
                ("test_mount", "no"),
                ("media_write", "no"),
            ],
        );
        assert_eq!(
            block_on(attach(&kit2.ctx(&runner))),
            0,
            "{}",
            kit2.sink.text()
        );
        assert_eq!(entries(&written(&kit2))[0].gid, None);
    }

    #[test]
    fn a_taken_mount_point_and_a_conflicting_group_are_refused() {
        let kit = kit(
            "vexos-server-amd",
            &[
                ("protocol", "nfs"),
                ("server", "box"),
                ("share", "/data"),
                ("test_mount", "no"),
                ("gid", "1500"),
            ],
        );
        kit.nixos_file(FILE, &file_with(&[r#"      { type = "nfs"; server = "x"; export = "/y"; mountPoint = "/mnt/box/data"; gid = 1500; }"#]));
        let runner = fake();
        assert_ne!(block_on(attach(&kit.ctx(&runner))), 0);
        assert!(kit.sink.text().contains("already used"));

        // A different group than the other shares use.
        let kit = self::kit(
            "vexos-server-amd",
            &[
                ("protocol", "nfs"),
                ("server", "box"),
                ("share", "/data"),
                ("test_mount", "no"),
                ("gid", "2000"),
            ],
        );
        kit.nixos_file(FILE, &file_with(&[NFS]));
        assert_ne!(block_on(attach(&kit.ctx(&runner))), 0);
        assert!(kit.sink.text().contains("1500"));
        assert_eq!(entries(&written(&kit)).len(), 1);
    }

    #[test]
    fn a_group_name_that_belongs_to_someone_else_is_refused() {
        let kit = kit(
            "vexos-server-amd",
            &[
                ("protocol", "nfs"),
                ("server", "box"),
                ("share", "/data"),
                ("test_mount", "no"),
                ("gid", "100"),
            ],
        );
        let runner = FakeRunner::new()
            .on("getent", &["group"], out("users:x:100:\n"))
            .ok("nix-instantiate", &[]);
        assert_ne!(block_on(attach(&kit.ctx(&runner))), 0);
        assert!(kit.sink.text().contains("users"));
    }

    #[test]
    fn mount_points_outside_mnt_and_other_roles_are_refused() {
        for mount in [
            "/etc",
            "/mnt",
            "/mnt/../etc",
            "/srv/x",
            "mnt/x",
            "/mnt/a\"b",
            "/mnt/a\\b",
            "/mnt/${x}",
        ] {
            let kit = kit(
                "vexos-server-amd",
                &[
                    ("protocol", "nfs"),
                    ("server", "box"),
                    ("share", "/d"),
                    ("test_mount", "no"),
                    ("media_write", "no"),
                    ("mountpoint", mount),
                ],
            );
            assert_ne!(block_on(attach(&kit.ctx(&fake()))), 0, "{mount}");
            assert!(!kit.nixos.join(FILE).exists(), "{mount}");
        }
        let kit = kit(
            "vexos-stateless-amd",
            &[("protocol", "nfs"), ("server", "box"), ("share", "/d")],
        );
        let runner = fake();
        assert_ne!(block_on(attach(&kit.ctx(&runner))), 0);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn an_smb_attach_needs_a_login() {
        let mut kit = kit(
            "vexos-server-amd",
            &[("protocol", "cifs"), ("server", "nas"), ("share", "tv")],
        );
        kit.secrets.clear();
        assert_ne!(block_on(attach(&kit.ctx(&fake()))), 0);
        let kit = self::kit(
            "vexos-server-amd",
            &[
                ("protocol", "cifs"),
                ("server", "nas"),
                ("share", "a/b"),
                ("username", "me"),
            ],
        );
        assert_ne!(block_on(attach(&kit.ctx(&fake()))), 0);
    }

    // ── detach ────────────────────────────────────────────────────────────────

    #[test]
    fn detach_edits_the_file_first_then_cleans_up_what_it_can() {
        let kit = kit("vexos-server-amd", &[("mount", "/mnt/nas/tv")]);
        let cred = kit.nixos.join("secrets/remote-mnt-nas-tv-credentials");
        std::fs::create_dir_all(cred.parent().unwrap()).unwrap();
        std::fs::write(&cred, "username=a\npassword=b\n").unwrap();
        let smb = format!(
            r#"      {{ type = "cifs"; server = "nas.local"; export = "tv"; mountPoint = "/mnt/nas/tv"; credentialsFile = "{}"; }}"#,
            cred.display()
        );
        kit.nixos_file(FILE, &file_with(&[NFS, &smb]));
        let runner = FakeRunner::new()
            .ok("nix-instantiate", &[])
            .on("systemd-escape", &[], out("mnt-nas-tv.automount\n"))
            .ok("systemctl", &["stop"])
            .on("findmnt", &[], out("/mnt/nas/tv nas:/tv cifs rw\n"))
            .ok("umount", &[]);
        assert_eq!(
            block_on(detach(&kit.ctx(&runner))),
            0,
            "{}",
            kit.sink.text()
        );

        let left = entries(&written(&kit));
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].mount_point, "/mnt/nas/media");
        assert!(
            !cred.exists(),
            "the login file should go with its last share"
        );
        let lines = runner.lines();
        assert!(
            lines.contains(&"systemctl stop mnt-nas-tv.automount".to_string()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"umount /mnt/nas/tv".to_string()),
            "{lines:?}"
        );
    }

    #[test]
    fn a_login_file_still_used_by_another_share_stays() {
        let kit = kit("vexos-server-amd", &[("mount", "/mnt/a/x")]);
        let cred = kit.nixos.join("secrets/shared");
        std::fs::create_dir_all(cred.parent().unwrap()).unwrap();
        std::fs::write(&cred, "x").unwrap();
        let entry = |m: &str| {
            format!(
                r#"      {{ type = "cifs"; server = "s"; export = "e"; mountPoint = "{m}"; credentialsFile = "{}"; }}"#,
                cred.display()
            )
        };
        kit.nixos_file(FILE, &file_with(&[&entry("/mnt/a/x"), &entry("/mnt/a/y")]));
        let runner = FakeRunner::new()
            .ok("nix-instantiate", &[])
            .on("systemd-escape", &[], out("x.automount\n"))
            .ok("systemctl", &["stop"])
            .failing("findmnt", &[], 1, "");
        assert_eq!(
            block_on(detach(&kit.ctx(&runner))),
            0,
            "{}",
            kit.sink.text()
        );
        assert!(cred.exists());
    }

    #[test]
    fn a_login_file_outside_the_secrets_folder_is_never_deleted() {
        let kit = kit("vexos-server-amd", &[("mount", "/mnt/a/x")]);
        let elsewhere = kit.root.join("precious");
        std::fs::write(&elsewhere, "keep").unwrap();
        kit.nixos_file(
            FILE,
            &file_with(&[&format!(
                r#"      {{ type = "cifs"; server = "s"; export = "e"; mountPoint = "/mnt/a/x"; credentialsFile = "{}"; }}"#,
                elsewhere.display()
            )]),
        );
        let runner = FakeRunner::new()
            .ok("nix-instantiate", &[])
            .on("systemd-escape", &[], out("x.automount\n"))
            .ok("systemctl", &["stop"])
            .failing("findmnt", &[], 1, "");
        assert_eq!(
            block_on(detach(&kit.ctx(&runner))),
            0,
            "{}",
            kit.sink.text()
        );
        assert!(elsewhere.exists());
    }

    #[test]
    fn detaching_something_that_is_not_attached_changes_nothing() {
        let kit = kit("vexos-server-amd", &[("mount", "/mnt/nope")]);
        kit.nixos_file(FILE, &file_with(&[NFS]));
        let runner = FakeRunner::new();
        assert_ne!(block_on(detach(&kit.ctx(&runner))), 0);
        assert!(runner.calls().is_empty());
        assert_eq!(entries(&written(&kit)).len(), 1);
        assert!(kit.sink.text().contains("/mnt/nas/media"));
    }

    #[test]
    fn two_mount_points_that_share_a_login_file_name_are_not_allowed() {
        let kit = kit(
            "vexos-server-amd",
            &[
                ("protocol", "cifs"),
                ("server", "nas"),
                ("share", "tv"),
                ("username", "me"),
                ("test_mount", "no"),
                ("mountpoint", "/mnt/nas-a/tv"),
            ],
        );
        let cred = kit.nixos.join("secrets/remote-mnt-nas-a-tv-credentials");
        std::fs::create_dir_all(cred.parent().unwrap()).unwrap();
        std::fs::write(&cred, "username=first\npassword=first\n").unwrap();
        kit.nixos_file(
            FILE,
            &file_with(&[&format!(
                r#"      {{ type = "cifs"; server = "x"; export = "y"; mountPoint = "/mnt/nas/a-tv"; credentialsFile = "{}"; }}"#,
                cred.display()
            )]),
        );
        assert_ne!(block_on(attach(&kit.ctx(&fake()))), 0);
        assert_eq!(
            std::fs::read_to_string(&cred).unwrap(),
            "username=first\npassword=first\n",
            "the first share's login was overwritten"
        );
    }

    #[test]
    fn a_password_with_a_space_is_kept_as_typed() {
        let mut kit = kit(
            "vexos-server-amd",
            &[
                ("protocol", "cifs"),
                ("server", "nas"),
                ("share", "Shared Documents"),
                ("username", "me"),
                ("test_mount", "no"),
            ],
        );
        kit.secrets
            .insert("password".into(), Secret::new("two words"));
        assert_eq!(
            block_on(attach(&kit.ctx(&fake()))),
            0,
            "{}",
            kit.sink.text()
        );
        let cred = kit
            .nixos
            .join("secrets/remote-mnt-nas-Shared-Documents-credentials");
        assert_eq!(
            std::fs::read_to_string(cred).unwrap(),
            "username=me\npassword=two words\n"
        );
        assert_eq!(entries(&written(&kit))[0].export, "Shared Documents");
    }

    #[test]
    fn an_attach_waits_for_the_one_before_it() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        let kit = kit(
            "vexos-server-amd",
            &[
                ("protocol", "nfs"),
                ("server", "box"),
                ("share", "/d"),
                ("test_mount", "no"),
                ("media_write", "no"),
            ],
        );
        let runner = fake();
        let held = block_on(ONE_AT_A_TIME.lock());
        let ctx = kit.ctx(&runner);
        let mut job = Box::pin(attach(&ctx));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(job.as_mut().poll(&mut cx), Poll::Pending));
        assert!(
            !kit.nixos.join(FILE).exists(),
            "it ran while another held the lock"
        );
        drop(held);
        let mut done = Poll::Pending;
        for _ in 0..5000 {
            done = job.as_mut().poll(&mut cx);
            if done.is_ready() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            matches!(done, Poll::Ready(0)),
            "{done:?} {}",
            kit.sink.text()
        );
    }
}
