//! Server services: every module VexOS offers, which ones are on, and a page each.

use crate::app::App;
use crate::ui::{self, window::Window};

use adw::prelude::*;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_catalog::drift::ServiceEntry;

/// A heading, its group, and each row with the lowercase text search matches on.
type ServiceGroup = (String, adw::PreferencesGroup, Vec<(adw::ActionRow, String)>);

/// Per-service actions, which live on each service's own page — and the catalog
/// listing, which this page already is.
const PER_SERVICE: [&str; 8] = [
    "service-available",
    "service-info",
    "service-status",
    "service-restart",
    "service-enable",
    "service-disable",
    "plex-pass-enable",
    "plex-pass-disable",
];

pub fn build(app: &Rc<App>, window: &Window) -> adw::NavigationPage {
    let services = catalog(app);
    let state = app.state.borrow();
    let enabled = services
        .iter()
        .filter(|s| state.settings.service_enabled(&s.name))
        .count();
    drop(state);

    let page = adw::PreferencesPage::new();
    page.set_description(&format!(
        "{enabled} of {} services enabled on this server.",
        services.len()
    ));

    for group in ui::action_groups(app, window, "services", &PER_SERVICE) {
        page.add(&group);
    }

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search services")
        .hexpand(true)
        .build();
    let search_group = adw::PreferencesGroup::new();
    search_group.add(&search);
    page.add(&search_group);

    // Enabled services first, then the catalog's own groups in its order.
    let mut groups: Vec<ServiceGroup> = Vec::new();
    let state = app.state.borrow();
    for (on, service) in services
        .iter()
        .filter(|s| state.settings.service_enabled(&s.name))
        .map(|s| (true, s))
        .chain(
            services
                .iter()
                .filter(|s| !state.settings.service_enabled(&s.name))
                .map(|s| (false, s)),
        )
    {
        let heading = if on {
            "Enabled"
        } else {
            service.group.as_str()
        };
        let index = match groups.iter().position(|(title, _, _)| title == heading) {
            Some(index) => index,
            None => {
                let group = adw::PreferencesGroup::builder().title(heading).build();
                groups.push((heading.to_string(), group, Vec::new()));
                groups.len() - 1
            }
        };
        let row = service_row(app, window, service, on);
        groups[index].1.add(&row);
        let haystack = format!("{} {}", service.name, service.description).to_lowercase();
        groups[index].2.push((row, haystack));
    }
    drop(state);

    let groups = Rc::new(groups);
    for (_, group, _) in groups.iter() {
        page.add(group);
    }
    search.connect_search_changed({
        let groups = groups.clone();
        move |entry| {
            let needle = entry.text().to_lowercase();
            for (_, group, rows) in groups.iter() {
                let mut any = false;
                for (row, haystack) in rows {
                    let show = needle.is_empty() || haystack.contains(needle.trim());
                    row.set_visible(show);
                    any |= show;
                }
                group.set_visible(any);
            }
        }
    });

    ui::page(app, window, "Services", &page)
}

/// The services this justfile offers, grouped and described; falls back to the bare
/// name list if the justfile has no `_service_catalog`.
fn catalog(app: &App) -> Vec<ServiceEntry> {
    if !app.facts.service_catalog.is_empty() {
        return app.facts.service_catalog.clone();
    }
    app.facts
        .server_services
        .iter()
        .map(|name| ServiceEntry {
            group: "Services".to_string(),
            name: name.clone(),
            description: String::new(),
        })
        .collect()
}

fn service_row(app: &Rc<App>, window: &Window, service: &ServiceEntry, on: bool) -> adw::ActionRow {
    // Plain text: the justfile's descriptions contain `&`, which is not valid markup.
    let row = adw::ActionRow::builder()
        .title(&service.name)
        .subtitle(&service.description)
        .use_markup(false)
        .activatable(true)
        .build();
    if on {
        row.add_suffix(&ui::pill(
            "Enabled",
            "safe",
            "Enabled in server-services.nix",
        ));
    }
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    row.connect_activated({
        let app = app.clone();
        let window = window.clone();
        let service = service.clone();
        move |_| window.push(&detail(&app, &window, &service))
    });
    row
}

/// One service: how to reach it, whether it is healthy, and turning it on or off.
fn detail(app: &Rc<App>, window: &Window, service: &ServiceEntry) -> adw::NavigationPage {
    let state = app.state.borrow();
    let on = state.settings.service_enabled(&service.name);
    let plex_pass = state.settings.plex_pass();
    drop(state);

    let page = adw::PreferencesPage::new();
    let name = service.name.as_str();
    let preset = [("service", name)];

    let about = adw::PreferencesGroup::builder()
        .title(name)
        .description(glib::markup_escape_text(&service.description))
        .build();
    about.set_header_suffix(Some(&if on {
        ui::pill("Enabled", "safe", "Enabled in server-services.nix")
    } else {
        ui::pill("Not enabled", "", "")
    }));
    page.add(&about);

    if on {
        for id in ["service-info", "service-status", "service-restart"] {
            if let Some(action) = app.visible(id) {
                about.add(&ui::action_row(app, window, action, &preset));
            }
        }
    } else if let Some(action) = app.visible("service-enable") {
        about.add(&ui::action_row(app, window, action, &preset));
    }

    if on && name == "plex" && app.visible("plex-pass-enable").is_some() {
        let group = adw::PreferencesGroup::builder().title("Plex").build();
        let row = adw::SwitchRow::builder()
            .title("Plex Pass Hardware Transcoding")
            .subtitle("Needs a Plex Pass subscription. Takes effect on the next rebuild.")
            .active(plex_pass)
            .build();
        // Set while the switch is moved back after a failure, so that move is not
        // taken as a new request.
        let reverting = Rc::new(Cell::new(false));
        row.connect_active_notify({
            let app = app.clone();
            let window = window.clone();
            move |row| {
                if reverting.get() {
                    return;
                }
                let on = row.is_active();
                let reverting = reverting.clone();
                row.set_sensitive(false);
                let row = row.downgrade();
                ui::actions::run_quietly(
                    &app,
                    &window,
                    if on {
                        "plex-pass-enable"
                    } else {
                        "plex-pass-disable"
                    },
                    HashMap::new(),
                    "Plex Pass setting saved for the next rebuild",
                    move |ok| {
                        if let Some(row) = row.upgrade() {
                            row.set_sensitive(true);
                            if !ok {
                                reverting.set(true);
                                row.set_active(!on);
                                reverting.set(false);
                            }
                        }
                    },
                );
            }
        });
        group.add(&row);
        page.add(&group);
    }

    let mut more = Vec::new();
    if on {
        if let Some(action) = app.visible("restore-service") {
            more.push(ui::action_row(app, window, action, &[("name", name)]));
        }
        if let Some(action) = app.visible("service-disable") {
            more.push(ui::action_row(app, window, action, &preset));
        }
    }
    if !more.is_empty() {
        let group = adw::PreferencesGroup::builder().title("Manage").build();
        for row in more {
            group.add(&row);
        }
        page.add(&group);
    }

    ui::page(app, window, name, &page)
}
