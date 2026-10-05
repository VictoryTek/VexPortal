//! Build, generations, bootloader, identity and power.

use crate::app::App;
use crate::ui::{self, switch_dialog, window::Window};

use adw::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;

/// Rendered here rather than as plain action rows.
const CUSTOM: [&str; 3] = ["switch", "build", "set-hostname"];

pub fn build(app: &Rc<App>, window: &Window) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();
    let mut identity = identity_group(app, window);

    for group in ui::action_groups(app, window, "system", &CUSTOM) {
        let title = group.title();
        if title == "Build" {
            for (id, mode) in [
                ("switch", switch_dialog::Mode::Switch),
                ("build", switch_dialog::Mode::TestBuild),
            ] {
                if let Some(action) = app.visible(id) {
                    group.add(&dialog_row(app, window, action, mode));
                }
            }
        }
        // Identity sits just before Power, matching the catalog's order.
        if title == "Power" {
            if let Some(identity) = identity.take() {
                page.add(&identity);
            }
        }
        page.add(&group);
    }
    if let Some(identity) = identity {
        page.add(&identity);
    }

    ui::page(app, window, "System", &page)
}

/// The hostname, edited in place.
fn identity_group(app: &Rc<App>, window: &Window) -> Option<adw::PreferencesGroup> {
    let action = app.visible("set-hostname")?;
    let group = adw::PreferencesGroup::builder()
        .title("Identity")
        .description(&action.blurb)
        .build();
    let row = adw::EntryRow::builder()
        .title("Hostname")
        .text(app.state.borrow().hostname.as_deref().unwrap_or(""))
        .show_apply_button(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(&action.icon));
    row.connect_apply({
        let app = app.clone();
        let window = window.clone();
        move |row| {
            let name = row.text().trim().to_string();
            let mut args = HashMap::new();
            args.insert("name".to_string(), name.clone());
            ui::actions::run_quietly(
                &app,
                &window,
                "set-hostname",
                args,
                &format!("Hostname changed to {name}"),
                |_| {},
            );
        }
    });
    group.add(&row);
    Some(group)
}

/// A row that opens the role/GPU dialog instead of a generic form.
fn dialog_row(
    app: &Rc<App>,
    window: &Window,
    action: &vexportal_catalog::Action,
    mode: switch_dialog::Mode,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&action.title)
        .subtitle(&action.blurb)
        .activatable(true)
        .build();
    row.set_subtitle_lines(0);
    row.add_prefix(&gtk::Image::from_icon_name(&action.icon));
    if let Some(pill) = ui::risk_pill(action.risk) {
        row.add_suffix(&pill);
    }
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    row.connect_activated({
        let app = app.clone();
        let window = window.clone();
        move |_| switch_dialog::present(&app, &window, mode)
    });
    row
}
