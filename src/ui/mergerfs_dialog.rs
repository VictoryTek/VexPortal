//! The forms for the bulk pool: create it, or add a disk to it.
//!
//! Disks are picked from the ones that are free right now, and `storage` has to be typed
//! before anything is erased. Both are a courtesy: the daemon works the free disks out
//! again and checks the typed word itself.

use crate::app::App;
use crate::ui::pool_dialog::{confirm_row, fire, intro, shell, Hook, Picker};
use crate::ui::{actions, window::Window};

use adw::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;

const FILESYSTEMS: [(&str, &str); 2] = [
    ("ext4", "ext4: the usual choice"),
    ("xfs", "XFS: good for very large files"),
];

pub fn create(app: &Rc<App>, window: &Window) {
    let sh = shell(window, "Create Bulk Pool", "Create Pool", true);
    intro(
        &sh.page,
        "Combines disks of any size into one pool at /storage. Each disk keeps its own \
         filesystem and can be read on its own. Parity disks add SnapRAID protection: each must \
         be at least as large as the largest content disk. Every disk you choose is erased.",
    );
    let hook: Hook = Rc::default();

    let labels: Vec<&str> = FILESYSTEMS.iter().map(|f| f.1).collect();
    let filesystem = adw::ComboRow::builder()
        .title("Filesystem for each disk")
        .model(&gtk::StringList::new(&labels))
        .build();
    let basics = adw::PreferencesGroup::new();
    basics.add(&filesystem);
    sh.page.add(&basics);

    let content = Picker::new("Content disks", false, &hook);
    sh.page.add(&content.group);
    let parity = Picker::new("Parity disks (optional)", false, &hook);
    sh.page.add(&parity.group);
    let confirm = confirm_row(&sh.page, "storage");

    *hook.borrow_mut() = Some(Rc::new({
        let (content, parity, confirm, run) = (
            content.clone(),
            parity.clone(),
            confirm.clone(),
            sh.run.clone(),
        );
        move || {
            let chosen = content.selected();
            let overlap = parity.selected().iter().any(|p| chosen.contains(p));
            run.set_sensitive(
                !chosen.is_empty() && !overlap && confirm.text().as_str() == "storage",
            );
        }
    }));
    confirm.connect_changed({
        let hook = hook.clone();
        move |_| fire(&hook)
    });

    sh.run.connect_clicked({
        let (app, window, dialog) = (app.clone(), window.clone(), sh.dialog.clone());
        move |_| {
            let Some(action) = app.catalog.action("mergerfs-create") else {
                return;
            };
            let kind = FILESYSTEMS[(filesystem.selected() as usize).min(FILESYSTEMS.len() - 1)].0;
            let mut args = HashMap::from([
                ("fstype".to_string(), kind.to_string()),
                ("disks".to_string(), content.selected().join(",")),
                ("confirm".to_string(), confirm.text().to_string()),
            ]);
            let parity = parity.selected();
            if !parity.is_empty() {
                args.insert("parity".to_string(), parity.join(","));
            }
            dialog.close();
            actions::proceed(&app, &window, action, args);
        }
    });
    sh.dialog.present(Some(window.root()));
}

const ROLES: [(&str, &str); 2] = [
    ("content", "Content disk: adds its space to the pool"),
    (
        "parity",
        "Parity disk: SnapRAID protection, at least as large as the largest content disk",
    ),
];

pub fn add(app: &Rc<App>, window: &Window) {
    let sh = shell(window, "Add a Disk", "Add Disk", true);
    intro(
        &sh.page,
        "The disk you choose is erased and formatted like the pool's other disks. After the \
         next rebuild, run Sync Parity if the pool has parity.",
    );
    let hook: Hook = Rc::default();

    let labels: Vec<&str> = ROLES.iter().map(|r| r.1).collect();
    let role = adw::ComboRow::builder()
        .title("What the disk is for")
        .model(&gtk::StringList::new(&labels))
        .build();
    let basics = adw::PreferencesGroup::new();
    basics.add(&role);
    sh.page.add(&basics);

    let picker = Picker::new("New disk", true, &hook);
    sh.page.add(&picker.group);
    let confirm = confirm_row(&sh.page, "storage");

    *hook.borrow_mut() = Some(Rc::new({
        let (picker, confirm, run) = (picker.clone(), confirm.clone(), sh.run.clone());
        move || {
            run.set_sensitive(picker.selected().len() == 1 && confirm.text().as_str() == "storage")
        }
    }));
    confirm.connect_changed({
        let hook = hook.clone();
        move |_| fire(&hook)
    });

    sh.run.connect_clicked({
        let (app, window, dialog) = (app.clone(), window.clone(), sh.dialog.clone());
        move |_| {
            let Some(action) = app.catalog.action("mergerfs-add") else {
                return;
            };
            let kind = ROLES[(role.selected() as usize).min(ROLES.len() - 1)].0;
            let args = HashMap::from([
                ("role".to_string(), kind.to_string()),
                ("disks".to_string(), picker.selected().join(",")),
                ("confirm".to_string(), confirm.text().to_string()),
            ]);
            dialog.close();
            actions::proceed(&app, &window, action, args);
        }
    });
    sh.dialog.present(Some(window.root()));
}
