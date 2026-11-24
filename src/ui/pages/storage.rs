//! Storage & Backup: the catalog's actions, plus the shares attached from other hosts
//! shown with how each is doing and a button to detach it.

use crate::app::App;
use crate::system::disks;
use crate::system::remote::{mount_state, MountState};
use crate::ui::{self, actions, mergerfs_dialog, pool_dialog, window::Window};

use adw::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_ops::mergerfs;
use vexportal_ops::remote;
use vexportal_ops::zfs::{Leaf, PoolRow};

const ATTACH: &str = "remote-storage-attach";
const DETACH: &str = "remote-storage-detach";

/// Drawn by the ZFS Pools group below rather than as plain rows.
const ZFS: [&str; 10] = [
    "zfs-create",
    "zfs-destroy",
    "zfs-import-list",
    "zfs-import",
    "zfs-replace",
    "zfs-add",
    "zfs-attach",
    "zfs-detach",
    "zfs-scrub",
    "zfs-clear",
];

/// Drawn by the Bulk Pool group below rather than as plain rows.
const BULK: [&str; 6] = [
    "mergerfs-create",
    "mergerfs-add",
    "mergerfs-remove",
    "mergerfs-destroy",
    "mergerfs-sync",
    "mergerfs-scrub",
];

pub fn build(app: &Rc<App>, window: &Window) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();
    let catalog_page = app.catalog.page("storage");
    if let Some(description) = catalog_page.and_then(|p| p.description.as_deref()) {
        page.set_description(description);
    }
    let mut skip = vec![ATTACH, DETACH];
    skip.extend(ZFS);
    skip.extend(BULK);
    for group in ui::action_groups(app, window, "storage", &skip) {
        page.add(&group);
    }
    if app.visible("zfs-create").is_some() {
        page.add(&zfs_group(app, window));
    }
    if app.visible("mergerfs-create").is_some() {
        page.add(&bulk_group(app, window));
    }
    if let Some(attach) = app.visible(ATTACH) {
        page.add(&remote_group(app, window, attach));
    }
    ui::page(
        app,
        window,
        catalog_page.map_or("Storage & Backup", |p| &p.title),
        &page,
    )
}

/// The attach button and one row per attached share.
fn remote_group(
    app: &Rc<App>,
    window: &Window,
    attach: &vexportal_catalog::Action,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Remote Storage")
        .description("Shares from another host, mounted here. Changes apply on the next rebuild.")
        .build();
    group.add(&ui::action_row(app, window, attach, &[]));

    let text = app.state.borrow().remote_text.clone();
    let attached = remote::entries(&text);
    let can_detach = app.visible(DETACH).is_some();
    for entry in attached {
        let row = adw::ActionRow::builder()
            .title(format!(
                "{}  {}:{}",
                entry.kind.to_uppercase(),
                entry.server,
                entry.export
            ))
            .subtitle(&entry.mount_point)
            .use_markup(false)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("folder-remote-symbolic"));

        let status = gtk::Label::new(Some("Checking…"));
        status.add_css_class("dim-label");
        status.set_valign(gtk::Align::Center);
        row.add_suffix(&status);
        glib::spawn_future_local({
            let mount = entry.mount_point.clone();
            async move {
                let state = mount_state(&mount).await;
                status.set_text(state.label());
                if state == MountState::Mounted {
                    status.remove_css_class("dim-label");
                }
            }
        });

        if can_detach {
            let button = gtk::Button::with_label("Detach…");
            button.set_valign(gtk::Align::Center);
            button.add_css_class("destructive-action");
            button.connect_clicked({
                let app = app.clone();
                let window = window.clone();
                let mount = entry.mount_point.clone();
                move |_| {
                    let preset = HashMap::from([("mount".to_string(), mount.clone())]);
                    actions::activate(&app, &window, DETACH, preset);
                }
            });
            row.add_suffix(&button);
        }
        group.add(&row);
    }
    group
}

// ── ZFS pools ─────────────────────────────────────────────────────────────────

/// A row with one button.
fn button_row(
    title: &str,
    subtitle: &str,
    label: &str,
    destructive: bool,
    on_click: impl Fn() + 'static,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .use_markup(false)
        .build();
    row.set_subtitle_lines(0);
    let button = gtk::Button::with_label(label);
    button.set_valign(gtk::Align::Center);
    if destructive {
        button.add_css_class("destructive-action");
    }
    button.connect_clicked(move |_| on_click());
    row.add_suffix(&button);
    row
}

/// Create and import, then one expandable row per imported pool.
fn zfs_group(app: &Rc<App>, window: &Window) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("ZFS Pools")
        .description(
            "Pools of whole disks. Anything that erases a disk asks you to type the pool's name.",
        )
        .build();
    group.add(&button_row(
        "Create Pool",
        "Combine whole disks into a new pool. Every chosen disk is erased.",
        "Create…",
        false,
        {
            let (app, window) = (app.clone(), window.clone());
            move || pool_dialog::create(&app, &window)
        },
    ));
    if app.visible("zfs-import").is_some() {
        group.add(&button_row(
            "Import Pool",
            "Bring back a pool after a reinstall or after moving its disks.",
            "Import…",
            false,
            {
                let (app, window) = (app.clone(), window.clone());
                move || pool_dialog::import(&app, &window)
            },
        ));
    }

    let looking = adw::ActionRow::builder()
        .title("Looking for pools…")
        .build();
    looking.add_css_class("dim-label");
    group.add(&looking);
    glib::spawn_future_local({
        let (app, window, group) = (app.clone(), window.clone(), group.clone());
        async move {
            let found = disks::pools().await;
            group.remove(&looking);
            match found {
                None => {
                    let row = adw::ActionRow::builder()
                        .title("ZFS is not set up on this host")
                        .subtitle("zpool was not found.")
                        .build();
                    row.add_css_class("dim-label");
                    group.add(&row);
                }
                Some(pools) if pools.is_empty() => {
                    let row = adw::ActionRow::builder()
                        .title("No pools are imported")
                        .build();
                    row.add_css_class("dim-label");
                    group.add(&row);
                }
                Some(pools) => {
                    for pool in pools {
                        let leaves = disks::leaves(&pool.name).await;
                        group.add(&pool_row(&app, &window, &pool, &leaves));
                    }
                }
            }
        }
    });
    group
}

fn pool_row(app: &Rc<App>, window: &Window, pool: &PoolRow, leaves: &[Leaf]) -> adw::ExpanderRow {
    let row = adw::ExpanderRow::builder()
        .title(&pool.name)
        .subtitle(format!(
            "{} · {} used · {} free",
            pool.size, pool.used, pool.free
        ))
        .use_markup(false)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("drive-multidisk-symbolic"));
    let class = match pool.health.as_str() {
        "ONLINE" => "safe",
        "DEGRADED" | "FAULTED" | "UNAVAIL" | "SUSPENDED" => "destructive",
        _ => "",
    };
    row.add_suffix(&ui::pill(
        &pool.health,
        class,
        "The pool's health, from zpool",
    ));

    let name = pool.name.clone();
    let run = {
        let (app, window) = (app.clone(), window.clone());
        move |id: &'static str, pairs: &[(&str, &str)]| {
            let preset = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            actions::activate(&app, &window, id, preset);
        }
    };
    let run = Rc::new(run);

    let scrub = adw::ActionRow::builder()
        .title("Scrub")
        .subtitle("Read every block and repair bad data from the redundant copies.")
        .build();
    for (label, action) in [("Start", "start"), ("Stop", "stop")] {
        let button = gtk::Button::with_label(label);
        button.set_valign(gtk::Align::Center);
        button.connect_clicked({
            let (run, name) = (run.clone(), name.clone());
            move |_| run("zfs-scrub", &[("pool", &name), ("action", action)])
        });
        scrub.add_suffix(&button);
    }
    row.add_row(&scrub);
    row.add_row(&button_row(
        "Clear Error Counters",
        "After fixing the cause. It repairs nothing by itself.",
        "Clear…",
        false,
        {
            let (run, name) = (run.clone(), name.clone());
            move || run("zfs-clear", &[("pool", &name)])
        },
    ));
    row.add_row(&button_row(
        "Add Disks",
        "Grow the pool. Permanent: ZFS cannot remove them again.",
        "Add…",
        true,
        {
            let (app, window, name) = (app.clone(), window.clone(), name.clone());
            move || pool_dialog::add(&app, &window, &name)
        },
    ));
    row.add_row(&button_row(
        "Destroy Pool",
        "Permanently destroy the pool and everything in it.",
        "Destroy…",
        true,
        {
            let (run, name) = (run.clone(), name.clone());
            move || run("zfs-destroy", &[("pool", &name)])
        },
    ));

    for leaf in leaves {
        let short = leaf
            .device
            .rsplit('/')
            .next()
            .unwrap_or(&leaf.device)
            .to_string();
        let disk = adw::ActionRow::builder()
            .title(&short)
            .subtitle(&leaf.state)
            .use_markup(false)
            .build();
        disk.add_prefix(&gtk::Image::from_icon_name("drive-harddisk-symbolic"));
        let device = leaf.device.clone();
        for (label, kind) in [("Replace…", 0), ("Attach…", 1), ("Detach…", 2)] {
            let button = gtk::Button::with_label(label);
            button.set_valign(gtk::Align::Center);
            button.connect_clicked({
                let (app, window, run) = (app.clone(), window.clone(), run.clone());
                let (name, device) = (name.clone(), device.clone());
                move |_| match kind {
                    0 => pool_dialog::one_disk(&app, &window, true, &name, &device),
                    1 => pool_dialog::one_disk(&app, &window, false, &name, &device),
                    _ => run("zfs-detach", &[("pool", &name), ("disk", &device)]),
                }
            });
            disk.add_suffix(&button);
        }
        row.add_row(&disk);
    }
    row
}

// ── Bulk pool ─────────────────────────────────────────────────────────────────

fn dim_row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .use_markup(false)
        .build();
    row.set_title_lines(0);
    row.set_subtitle_lines(0);
    row.add_css_class("dim-label");
    row
}

fn used_of(size: u64, used: u64) -> String {
    format!(
        "{} used of {}",
        vexportal_ops::disks::size_label(used),
        vexportal_ops::disks::size_label(size)
    )
}

/// The mergerfs pool: a button to create one, or the pool with its disks.
fn bulk_group(app: &Rc<App>, window: &Window) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Bulk Pool")
        .description(
            "Disks of any size combined into one pool at /storage, with optional SnapRAID \
             parity. Anything that erases a disk asks you to type storage. Changes apply on \
             the next rebuild.",
        )
        .build();
    let text = app.state.borrow().pool_text.clone();
    if text.trim().is_empty() {
        group.add(&button_row(
            "Create Pool",
            "Combine mixed-size disks into one pool. Every chosen disk is erased.",
            "Create…",
            false,
            {
                let (app, window) = (app.clone(), window.clone());
                move || mergerfs_dialog::create(&app, &window)
            },
        ));
        return group;
    }
    match mergerfs::parse(&text) {
        Ok(pool) => group.add(&bulk_row(app, window, &pool)),
        Err(why) => group.add(&dim_row("The pool cannot be managed here", &why)),
    }
    group
}

fn bulk_row(app: &Rc<App>, window: &Window, pool: &mergerfs::Pool) -> adw::ExpanderRow {
    let row = adw::ExpanderRow::builder()
        .title(mergerfs::UNION)
        .subtitle(format!(
            "{} content disk(s), {} parity disk(s)",
            pool.content.len(),
            pool.parity.len()
        ))
        .use_markup(false)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("drive-multidisk-symbolic"));
    if pool.parity.is_empty() {
        row.add_suffix(&ui::pill(
            "No parity",
            "destructive",
            "A failed disk loses the files on it",
        ));
        row.add_row(&dim_row(
            "No parity disks",
            "A failed disk would lose its files. Add a parity disk to protect the pool.",
        ));
    }

    let run = {
        let (app, window) = (app.clone(), window.clone());
        move |id: &'static str, pairs: &[(&str, &str)]| {
            let preset = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            actions::activate(&app, &window, id, preset);
        }
    };
    let run = Rc::new(run);

    let mut labels: Vec<(String, gtk::Label)> = Vec::new();
    for (role, members) in [("Content", &pool.content), ("Parity", &pool.parity)] {
        for member in members {
            let disk = adw::ActionRow::builder()
                .title(&member.mount)
                .subtitle(format!("{role} · {} · {}", member.fstype, member.uuid))
                .use_markup(false)
                .build();
            disk.add_prefix(&gtk::Image::from_icon_name("drive-harddisk-symbolic"));
            let usage = gtk::Label::new(Some("Checking…"));
            usage.add_css_class("dim-label");
            usage.set_valign(gtk::Align::Center);
            disk.add_suffix(&usage);
            labels.push((member.mount.clone(), usage));
            let remove = gtk::Button::with_label("Remove…");
            remove.set_valign(gtk::Align::Center);
            remove.add_css_class("destructive-action");
            remove.connect_clicked({
                let (run, mount) = (run.clone(), member.mount.clone());
                move |_| run("mergerfs-remove", &[("mount", &mount)])
            });
            disk.add_suffix(&remove);
            row.add_row(&disk);
        }
    }

    row.add_row(&button_row(
        "Add Disk",
        "More space, or a parity disk. The disk is erased.",
        "Add…",
        true,
        {
            let (app, window) = (app.clone(), window.clone());
            move || mergerfs_dialog::add(&app, &window)
        },
    ));
    if !pool.parity.is_empty() {
        for (id, title, subtitle, label) in [
            (
                "mergerfs-sync",
                "Sync Parity",
                "Bring the parity up to date after files changed. Takes hours; can be stopped and resumed.",
                "Sync…",
            ),
            (
                "mergerfs-scrub",
                "Scrub",
                "Read part of the array to find silent corruption. Takes hours.",
                "Scrub…",
            ),
        ] {
            if app.visible(id).is_some() {
                row.add_row(&button_row(title, subtitle, label, false, {
                    let run = run.clone();
                    move || run(id, &[])
                }));
            }
        }
    }
    row.add_row(&button_row(
        "Destroy Pool",
        "Unmount the pool and remove it from the configuration. The disks keep their data.",
        "Destroy…",
        true,
        {
            let run = run.clone();
            move || run("mergerfs-destroy", &[])
        },
    ));

    glib::spawn_future_local({
        let row = row.clone();
        let mut mounts: Vec<String> = vec![mergerfs::UNION.to_string()];
        mounts.extend(labels.iter().map(|(m, _)| m.clone()));
        async move {
            let usage = disks::usage(&mounts).await;
            for (mount, label) in &labels {
                match usage.get(mount) {
                    Some((size, used)) => {
                        label.set_text(&used_of(*size, *used));
                        label.remove_css_class("dim-label");
                    }
                    None => label.set_text("Not mounted"),
                }
            }
            let state = match usage.get(mergerfs::UNION) {
                Some((size, used)) => used_of(*size, *used),
                None => "not mounted".to_string(),
            };
            row.set_subtitle(&format!("{} · {state}", row.subtitle()));
        }
    });
    row
}
