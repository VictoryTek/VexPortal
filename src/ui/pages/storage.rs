//! Storage & Backup: the catalog's actions, plus the shares attached from other hosts
//! shown with how each is doing and a button to detach it.

use crate::app::App;
use crate::system::remote::{mount_state, MountState};
use crate::ui::{self, actions, window::Window};

use adw::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_ops::remote;

const ATTACH: &str = "remote-storage-attach";
const DETACH: &str = "remote-storage-detach";

pub fn build(app: &Rc<App>, window: &Window) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();
    let catalog_page = app.catalog.page("storage");
    if let Some(description) = catalog_page.and_then(|p| p.description.as_deref()) {
        page.set_description(description);
    }
    for group in ui::action_groups(app, window, "storage", &[ATTACH, DETACH]) {
        page.add(&group);
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
