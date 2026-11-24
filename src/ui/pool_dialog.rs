//! The forms for ZFS pools: create one, add disks, replace or attach a disk, import one.
//!
//! The disks are picked from a list of the ones that are free right now. The list is a
//! courtesy: the daemon works it out again when it is about to erase anything, and
//! refuses a disk that is not on it. Anything that erases a disk also asks for the pool's
//! name to be typed, which the daemon checks too.

use crate::app::App;
use crate::system::disks;
use crate::ui::{actions, window::Window};

use adw::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_ops::zfs::{valid_name, valid_storage_id, Layout};

pub(crate) type Hook = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

pub(crate) fn fire(hook: &Hook) {
    let f = hook.borrow().clone();
    if let Some(f) = f {
        f();
    }
}

/// A list of the free disks to tick, one or several.
pub(crate) struct Picker {
    pub(crate) group: adw::PreferencesGroup,
    rows: Rc<RefCell<Vec<(String, gtk::CheckButton)>>>,
}

impl Picker {
    pub(crate) fn new(title: &str, single: bool, changed: &Hook) -> Rc<Self> {
        let group = adw::PreferencesGroup::builder()
            .title(title)
            .description(
                "Only disks with nothing mounted and in no pool are listed. \
                 Every disk you choose is erased.",
            )
            .build();
        let waiting = adw::ActionRow::builder()
            .title("Looking for disks…")
            .build();
        waiting.add_css_class("dim-label");
        group.add(&waiting);

        let picker = Rc::new(Self {
            group: group.clone(),
            rows: Rc::default(),
        });
        glib::spawn_future_local({
            let picker = picker.clone();
            let changed = changed.clone();
            async move {
                let found = disks::eligible().await;
                picker.group.remove(&waiting);
                let list = match found {
                    Ok(list) if !list.is_empty() => list,
                    Ok(_) => {
                        let row = adw::ActionRow::builder()
                            .title("No free disks found")
                            .subtitle("Every disk is mounted, in a pool, or has no hardware name.")
                            .build();
                        picker.group.add(&row);
                        return;
                    }
                    Err(why) => {
                        picker
                            .group
                            .add(&adw::ActionRow::builder().title(why).build());
                        return;
                    }
                };
                let mut first: Option<gtk::CheckButton> = None;
                for disk in list {
                    let check = gtk::CheckButton::new();
                    if single {
                        match &first {
                            Some(head) => check.set_group(Some(head)),
                            None => first = Some(check.clone()),
                        }
                    }
                    let mut detail =
                        format!("{} · {} {}", disk.size_label(), disk.model, disk.transport);
                    if let Some(note) = disk.note {
                        detail.push_str(&format!(" · {note}"));
                    }
                    let row = adw::ActionRow::builder()
                        .title(&disk.by_id)
                        .subtitle(detail.trim())
                        .use_markup(false)
                        .build();
                    row.add_prefix(&check);
                    row.set_activatable_widget(Some(&check));
                    check.connect_toggled({
                        let changed = changed.clone();
                        move |_| fire(&changed)
                    });
                    picker.group.add(&row);
                    picker.rows.borrow_mut().push((disk.by_id.clone(), check));
                }
            }
        });
        picker
    }

    pub(crate) fn selected(&self) -> Vec<String> {
        self.rows
            .borrow()
            .iter()
            .filter(|(_, check)| check.is_active())
            .map(|(id, _)| id.clone())
            .collect()
    }
}

pub(crate) struct Shell {
    pub(crate) dialog: adw::Dialog,
    pub(crate) page: adw::PreferencesPage,
    pub(crate) run: gtk::Button,
}

pub(crate) fn shell(window: &Window, title: &str, run_label: &str, destructive: bool) -> Shell {
    let page = adw::PreferencesPage::new();
    let run = gtk::Button::with_label(run_label);
    run.add_css_class(if destructive {
        "destructive-action"
    } else {
        "suggested-action"
    });
    run.set_sensitive(false);
    let cancel = gtk::Button::with_label("Cancel");
    let header = adw::HeaderBar::new();
    header.pack_start(&cancel);
    header.pack_end(&run);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&page));
    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(600)
        .content_height(640)
        .child(&toolbar)
        .build();
    cancel.connect_clicked({
        let dialog = dialog.clone();
        move |_| {
            dialog.close();
        }
    });
    let _ = window;
    Shell { dialog, page, run }
}

pub(crate) fn intro(page: &adw::PreferencesPage, text: &str) {
    let group = adw::PreferencesGroup::new();
    let row = adw::ActionRow::builder()
        .title(text)
        .use_markup(false)
        .build();
    row.set_title_lines(0);
    group.add(&row);
    page.add(&group);
}

pub(crate) fn confirm_row(page: &adw::PreferencesPage, pool: &str) -> adw::EntryRow {
    let entry = adw::EntryRow::builder()
        .title(format!("Type {pool} to confirm"))
        .build();
    let group = adw::PreferencesGroup::new();
    group.add(&entry);
    page.add(&group);
    entry
}

// ── Create ────────────────────────────────────────────────────────────────────

const TOPOLOGIES: [(&str, &str, &str); 6] = [
    (
        "single",
        "No protection",
        "All disks added together. If any one fails, all data is lost. For testing, or data backed up elsewhere.",
    ),
    (
        "mirror",
        "Mirror (2 or more disks)",
        "Every disk is a full copy: space of one disk, survives all but one failing. Good for VMs.",
    ),
    (
        "raidz1",
        "RAID-Z1 (3 or more)",
        "Space of N−1 disks, survives one disk failing.",
    ),
    (
        "raidz2",
        "RAID-Z2 (4 or more)",
        "Space of N−2 disks, survives two disks failing.",
    ),
    (
        "raidz3",
        "RAID-Z3 (5 or more)",
        "Space of N−3 disks, survives three disks failing.",
    ),
    (
        "raid10",
        "Striped mirrors, RAID10 (4 or more, an even number)",
        "Half the space, survives one failure in each pair. Good for VMs.",
    ),
];

fn disk_count_ok(layout: &str, count: usize, exactly_one_for_single: bool) -> bool {
    let Some(layout) = Layout::parse(layout) else {
        return false;
    };
    let enough = count >= layout.min_disks();
    let even = layout != Layout::Raid10 || count.is_multiple_of(2);
    let one = !(exactly_one_for_single && layout == Layout::Single) || count == 1;
    enough && even && one
}

pub fn create(app: &Rc<App>, window: &Window) {
    let sh = shell(window, "Create ZFS Pool", "Create Pool", true);
    let hook: Hook = Rc::default();

    let name = adw::EntryRow::builder()
        .title("Pool name")
        .text("tank")
        .build();
    let labels: Vec<&str> = TOPOLOGIES.iter().map(|t| t.1).collect();
    let topology = adw::ComboRow::builder()
        .title("How the disks are combined")
        .model(&gtk::StringList::new(&labels))
        .selected(1)
        .build();
    let hint = adw::ActionRow::builder().build();
    hint.set_title_lines(0);
    hint.add_css_class("dim-label");
    let dataset = adw::SwitchRow::builder()
        .title("Create a child dataset for VM disks")
        .subtitle("Makes <pool>/vm, the usual place for Proxmox VM disks.")
        .active(true)
        .build();
    let storage = adw::EntryRow::builder()
        .title("Proxmox storage ID (optional)")
        .build();
    storage.set_tooltip_text(Some(
        "Adds the pool to Proxmox under this name, if Proxmox is running.",
    ));

    let basics = adw::PreferencesGroup::builder().title("Pool").build();
    basics.add(&name);
    basics.add(&topology);
    basics.add(&hint);
    basics.add(&dataset);
    basics.add(&storage);
    sh.page.add(&basics);

    let picker = Picker::new("Disks", false, &hook);
    sh.page.add(&picker.group);
    let confirm = confirm_row(&sh.page, "the pool name");

    let selected_topology = {
        let topology = topology.clone();
        move || TOPOLOGIES[(topology.selected() as usize).min(TOPOLOGIES.len() - 1)].0
    };
    let update = {
        let (name, storage, confirm, run) = (
            name.clone(),
            storage.clone(),
            confirm.clone(),
            sh.run.clone(),
        );
        let (picker, hint) = (picker.clone(), hint.clone());
        let selected_topology = selected_topology.clone();
        move || {
            let topo = selected_topology();
            hint.set_title(TOPOLOGIES.iter().find(|t| t.0 == topo).map_or("", |t| t.2));
            let typed = name.text().trim().to_string();
            let storage_ok = storage.text().is_empty() || valid_storage_id(&storage.text());
            run.set_sensitive(
                valid_name(&typed).is_ok()
                    && storage_ok
                    && disk_count_ok(topo, picker.selected().len(), false)
                    && confirm.text().as_str() == typed,
            );
        }
    };
    *hook.borrow_mut() = Some(Rc::new(update));
    for entry in [&name, &storage, &confirm] {
        entry.connect_changed({
            let hook = hook.clone();
            move |_| fire(&hook)
        });
    }
    topology.connect_selected_notify({
        let hook = hook.clone();
        move |_| fire(&hook)
    });
    fire(&hook);

    sh.run.connect_clicked({
        let (app, window, dialog) = (app.clone(), window.clone(), sh.dialog.clone());
        move |_| {
            let Some(action) = app.catalog.action("zfs-create") else {
                return;
            };
            let typed = name.text().trim().to_string();
            let mut args = HashMap::from([
                ("name".to_string(), typed.clone()),
                ("topology".to_string(), selected_topology().to_string()),
                ("disks".to_string(), picker.selected().join(",")),
                (
                    "dataset".to_string(),
                    if dataset.is_active() { "yes" } else { "no" }.to_string(),
                ),
                ("confirm".to_string(), confirm.text().to_string()),
            ]);
            if !storage.text().is_empty() {
                args.insert("storage_id".to_string(), storage.text().to_string());
            }
            dialog.close();
            actions::proceed(&app, &window, action, args);
        }
    });
    sh.dialog.present(Some(window.root()));
}

// ── Add, replace, attach ──────────────────────────────────────────────────────

const ADD_LAYOUTS: [(&str, &str); 4] = [
    (
        "mirror",
        "Mirror (2 or more disks): adds one disk of space, every disk a full copy",
    ),
    ("raidz1", "RAID-Z1 (3 or more): survives one failure"),
    ("raidz2", "RAID-Z2 (4 or more): survives two failures"),
    ("single", "A single disk: adds its space but NO protection"),
];

/// Add disks to a pool. Permanent: ZFS cannot remove them again.
pub fn add(app: &Rc<App>, window: &Window, pool: &str) {
    let sh = shell(window, &format!("Add Disks to {pool}"), "Add Disks", true);
    intro(
        &sh.page,
        "This is permanent: ZFS cannot remove the disks again. Use the same style as the pool's \
         existing disks; ZFS refuses a mismatch and nothing is erased in that case.",
    );
    let hook: Hook = Rc::default();
    let labels: Vec<&str> = ADD_LAYOUTS.iter().map(|l| l.1).collect();
    let layout = adw::ComboRow::builder()
        .title("How the new disks are added")
        .model(&gtk::StringList::new(&labels))
        .build();
    let group = adw::PreferencesGroup::new();
    group.add(&layout);
    sh.page.add(&group);
    let picker = Picker::new("Disks", false, &hook);
    sh.page.add(&picker.group);
    let confirm = confirm_row(&sh.page, pool);

    let chosen = {
        let layout = layout.clone();
        move || ADD_LAYOUTS[(layout.selected() as usize).min(ADD_LAYOUTS.len() - 1)].0
    };
    *hook.borrow_mut() = Some(Rc::new({
        let (picker, confirm, run, pool, chosen) = (
            picker.clone(),
            confirm.clone(),
            sh.run.clone(),
            pool.to_string(),
            chosen.clone(),
        );
        move || {
            run.set_sensitive(
                disk_count_ok(chosen(), picker.selected().len(), true)
                    && confirm.text().as_str() == pool,
            )
        }
    }));
    confirm.connect_changed({
        let hook = hook.clone();
        move |_| fire(&hook)
    });
    layout.connect_selected_notify({
        let hook = hook.clone();
        move |_| fire(&hook)
    });

    sh.run.connect_clicked({
        let (app, window, dialog, pool) = (
            app.clone(),
            window.clone(),
            sh.dialog.clone(),
            pool.to_string(),
        );
        move |_| {
            let Some(action) = app.catalog.action("zfs-add") else {
                return;
            };
            let args = HashMap::from([
                ("pool".to_string(), pool.clone()),
                ("layout".to_string(), chosen().to_string()),
                ("disks".to_string(), picker.selected().join(",")),
                ("confirm".to_string(), confirm.text().to_string()),
            ]);
            dialog.close();
            actions::proceed(&app, &window, action, args);
        }
    });
    sh.dialog.present(Some(window.root()));
}

/// Replace a disk (`zfs-replace`, naming `old`) or attach a copy of one (`zfs-attach`,
/// naming `existing`): one new disk, erased.
pub fn one_disk(app: &Rc<App>, window: &Window, replace: bool, pool: &str, device: &str) {
    let (id, key, title, label) = if replace {
        ("zfs-replace", "old", "Replace a Disk", "Replace")
    } else {
        ("zfs-attach", "existing", "Attach a Mirror Disk", "Attach")
    };
    let short = device.rsplit('/').next().unwrap_or(device);
    let sh = shell(window, title, label, true);
    intro(
        &sh.page,
        &if replace {
            format!(
                "{short} in {pool} will be replaced by the disk you choose, which must be at \
                 least as large. The pool copies its data onto the new disk."
            )
        } else {
            format!(
                "The disk you choose becomes another full copy of {short} in {pool}, and must \
                 be at least as large. Space does not grow."
            )
        },
    );
    let hook: Hook = Rc::default();
    let picker = Picker::new("New disk", true, &hook);
    sh.page.add(&picker.group);
    let confirm = confirm_row(&sh.page, pool);
    *hook.borrow_mut() = Some(Rc::new({
        let (picker, confirm, run, pool) = (
            picker.clone(),
            confirm.clone(),
            sh.run.clone(),
            pool.to_string(),
        );
        move || run.set_sensitive(picker.selected().len() == 1 && confirm.text().as_str() == pool)
    }));
    confirm.connect_changed({
        let hook = hook.clone();
        move |_| fire(&hook)
    });

    sh.run.connect_clicked({
        let (app, window, dialog) = (app.clone(), window.clone(), sh.dialog.clone());
        let (pool, device) = (pool.to_string(), device.to_string());
        move |_| {
            let Some(action) = app.catalog.action(id) else {
                return;
            };
            let args = HashMap::from([
                ("pool".to_string(), pool.clone()),
                (key.to_string(), device.clone()),
                ("disks".to_string(), picker.selected().join(",")),
                ("confirm".to_string(), confirm.text().to_string()),
            ]);
            dialog.close();
            actions::proceed(&app, &window, action, args);
        }
    });
    sh.dialog.present(Some(window.root()));
}

// ── Import ────────────────────────────────────────────────────────────────────

pub fn import(app: &Rc<App>, window: &Window) {
    let sh = shell(window, "Import ZFS Pool", "Import", false);
    intro(
        &sh.page,
        "Bring back a pool after a reinstall or after moving its disks. It is also saved to \
         import at every boot.",
    );
    let find = gtk::Button::with_label("Find Importable Pools");
    find.set_valign(gtk::Align::Center);
    let pool = adw::EntryRow::builder().title("Pool name").build();
    pool.add_suffix(&find);
    let force = adw::SwitchRow::builder()
        .title("Force the import")
        .subtitle("Only if the disks are not in use by another machine.")
        .build();
    let group = adw::PreferencesGroup::new();
    group.add(&pool);
    group.add(&force);
    sh.page.add(&group);

    find.connect_clicked({
        let (app, window) = (app.clone(), window.clone());
        move |_| actions::activate(&app, &window, "zfs-import-list", HashMap::new())
    });
    pool.connect_changed({
        let run = sh.run.clone();
        move |entry| run.set_sensitive(valid_name(entry.text().trim()).is_ok())
    });
    sh.run.connect_clicked({
        let (app, window, dialog) = (app.clone(), window.clone(), sh.dialog.clone());
        move |_| {
            let Some(action) = app.catalog.action("zfs-import") else {
                return;
            };
            let args = HashMap::from([
                ("pool".to_string(), pool.text().trim().to_string()),
                (
                    "force".to_string(),
                    if force.is_active() { "yes" } else { "no" }.to_string(),
                ),
            ]);
            dialog.close();
            actions::proceed(&app, &window, action, args);
        }
    });
    sh.dialog.present(Some(window.root()));
}
