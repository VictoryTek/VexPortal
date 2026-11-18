//! Server services: every module VexOS offers, which ones are on and how they are doing,
//! and a page each.
//!
//! What a service is, where to reach it and what to know about it comes from the built-in
//! table (`vexportal_ops::services`); how it is doing is asked of systemd and of the
//! service's own address as the logged-in user, so none of it needs the daemon.

use crate::app::App;
use crate::system::services as probes;
use crate::ui::{self, window::Window};

use adw::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_ops::services::{self, Health, Reach, Service};

/// A heading, its group, and each row with the lowercase text search matches on.
type ServiceGroup = (String, adw::PreferencesGroup, Vec<(adw::ActionRow, String)>);

/// Per-service actions, which live on each service's own page.
const PER_SERVICE: [&str; 8] = [
    "beszel-setup",
    "beszel-status",
    "service-restart",
    "service-enable",
    "service-disable",
    "service-set-url",
    "plex-pass-enable",
    "plex-pass-disable",
];

pub fn build(app: &Rc<App>, window: &Window) -> adw::NavigationPage {
    let all = services::all();
    let state = app.state.borrow();
    let enabled = all
        .iter()
        .filter(|s| state.service_enabled(&s.name))
        .count();
    drop(state);

    let page = adw::PreferencesPage::new();
    page.set_description(&format!(
        "{enabled} of {} services enabled on this server.",
        all.len()
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

    // Enabled services first, then the table's own groups in its order.
    let mut groups: Vec<ServiceGroup> = Vec::new();
    let mut watching: Vec<(&'static Service, gtk::Label)> = Vec::new();
    let state = app.state.borrow();
    let on = |s: &&Service| state.service_enabled(&s.name);
    for (is_on, service) in all
        .iter()
        .filter(on)
        .map(|s| (true, s))
        .chain(all.iter().filter(|s| !on(s)).map(|s| (false, s)))
    {
        let heading = if is_on {
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
        let (row, health) = service_row(app, window, service, is_on);
        if let Some(label) = health {
            watching.push((service, label));
        }
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

    // One question to systemd answers every row.
    if !watching.is_empty() {
        glib::spawn_future_local(async move {
            let units: Vec<String> = watching
                .iter()
                .flat_map(|(service, _)| service.unit_names())
                .collect();
            let infos = probes::units(&units).await;
            for (service, label) in &watching {
                if infos.is_empty() {
                    show_unknown(label);
                } else {
                    show_health(label, services::report_for(service, &infos).health);
                }
            }
        });
    }

    ui::page(app, window, "Services", &page)
}

/// A row, and for an enabled service the pill its health will be written into.
fn service_row(
    app: &Rc<App>,
    window: &Window,
    service: &'static Service,
    on: bool,
) -> (adw::ActionRow, Option<gtk::Label>) {
    // Plain text: the descriptions contain `&`, which is not valid markup.
    let row = adw::ActionRow::builder()
        .title(&service.name)
        .subtitle(&service.description)
        .use_markup(false)
        .activatable(true)
        .build();
    let pill = on.then(|| {
        let pill = ui::pill("Checking…", "", "How this service is doing right now");
        row.add_suffix(&pill);
        pill
    });
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    row.connect_activated({
        let app = app.clone();
        let window = window.clone();
        move |_| window.push(&detail(&app, &window, service))
    });
    (row, pill)
}

/// Colour and word for a service's health. Colour is never the only signal: the word is
/// always there.
fn show_health(label: &gtk::Label, health: Health) {
    label.remove_css_class("safe");
    label.remove_css_class("destructive");
    label.set_text(health.label());
    match health {
        Health::Running => label.add_css_class("safe"),
        Health::Failed | Health::Partial => label.add_css_class("destructive"),
        Health::Stopped | Health::NotBuilt => {}
    }
    label.set_tooltip_text(Some(match health {
        Health::Running => "Every systemd unit of this service is running",
        Health::Stopped => "None of this service's systemd units is running",
        Health::Failed => "A systemd unit of this service has failed",
        Health::Partial => "Some of this service's systemd units are not running",
        Health::NotBuilt => "Enabled in the configuration, but not built yet: rebuild to start it",
    }));
}

fn show_unknown(label: &gtk::Label) {
    label.remove_css_class("safe");
    label.remove_css_class("destructive");
    label.set_text("Unknown");
    label.set_tooltip_text(Some("systemd did not answer"));
}

/// One service: how to reach it, whether it is healthy, and turning it on or off.
fn detail(app: &Rc<App>, window: &Window, service: &'static Service) -> adw::NavigationPage {
    let state = app.state.borrow();
    let on = state.service_enabled(&service.name);
    let plex_pass = state.settings.plex_pass();
    drop(state);

    let page = adw::PreferencesPage::new();
    let name = service.name.as_str();
    let preset = [("service", name)];

    let about = adw::PreferencesGroup::builder()
        .title(name)
        .description(glib::markup_escape_text(&service.description))
        .build();
    let health = ui::pill(
        if on { "Checking…" } else { "Not enabled" },
        "",
        "How this service is doing right now",
    );
    about.set_header_suffix(Some(&health));
    page.add(&about);

    // Ports, addresses and what to know. `<server-ip>` is this machine.
    let notes = adw::PreferencesGroup::builder()
        .title("Access and notes")
        .build();
    for (label, text) in service.notes() {
        let row = adw::ActionRow::builder()
            .title(if label.is_empty() { "Note" } else { label })
            .subtitle(text)
            .use_markup(false)
            .subtitle_selectable(true)
            .build();
        row.set_subtitle_lines(0);
        row.set_title_lines(0);
        notes.add(&row);
    }
    page.add(&notes);

    if on {
        page.add(&status_group(service, &health));
    }

    if on {
        let group = adw::PreferencesGroup::builder().title("Service").build();
        for id in ["service-restart"] {
            if let Some(action) = app.visible(id) {
                group.add(&ui::action_row(app, window, action, &preset));
            }
        }
        if service.public_url.is_some() {
            if let Some(action) = app.visible("service-set-url") {
                group.add(&ui::action_row(app, window, action, &preset));
            }
        }
        page.add(&group);
    } else if let Some(action) = app.visible("service-enable") {
        let group = adw::PreferencesGroup::new();
        group.add(&ui::action_row(app, window, action, &preset));
        page.add(&group);
    }

    if on && name == "beszel-agent" {
        let group = adw::PreferencesGroup::builder().title("Beszel").build();
        for id in ["beszel-setup", "beszel-status"] {
            if let Some(action) = app.visible(id) {
                group.add(&ui::action_row(app, window, action, &[]));
            }
        }
        page.add(&group);
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

/// systemd's report on each unit and whether the service's addresses answer, with a
/// button to ask again. `health` is the pill in the page header, kept in step.
fn status_group(service: &'static Service, health: &gtk::Label) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Status").build();
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.add_css_class("flat");
    refresh.set_valign(gtk::Align::Center);
    refresh.set_tooltip_text(Some("Check again"));
    group.set_header_suffix(Some(&refresh));

    let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::default();
    let check = {
        let group = group.clone();
        let health = health.clone();
        let refresh = refresh.clone();
        let rows = rows.clone();
        move || {
            // A second click while a check is running would only pile up.
            if !refresh.is_sensitive() {
                return;
            }
            refresh.set_sensitive(false);
            let (group, health, refresh, rows) =
                (group.clone(), health.clone(), refresh.clone(), rows.clone());
            glib::spawn_future_local(async move {
                check_status(service, &group, &health, &rows).await;
                refresh.set_sensitive(true);
            });
        }
    };
    refresh.connect_clicked({
        let check = check.clone();
        move |_| check()
    });
    check();
    group
}

async fn check_status(
    service: &'static Service,
    group: &adw::PreferencesGroup,
    health: &gtk::Label,
    rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
) {
    let infos = probes::units(&service.unit_names()).await;
    for row in rows.borrow_mut().drain(..) {
        group.remove(&row);
    }
    let add = |title: &str, subtitle: &str| -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .use_markup(false)
            .build();
        row.set_subtitle_lines(0);
        group.add(&row);
        rows.borrow_mut().push(row.clone());
        row
    };

    if infos.is_empty() {
        show_unknown(health);
        add(
            "systemd did not answer",
            "Use the button above to try again.",
        );
        return;
    }
    let report = services::report_for(service, &infos);
    show_health(health, report.health);
    if report.units.is_empty() {
        add(
            "Not built yet",
            "It is enabled in the configuration, but none of its systemd units exist. Rebuild to build it.",
        );
        return;
    }
    for unit in &report.units {
        let detail = if unit.sub.is_empty() || unit.sub == unit.active {
            unit.active.clone()
        } else {
            format!("{} ({})", unit.active, unit.sub)
        };
        add(&format!("{}.service", unit.id), &detail);
    }

    for url in &service.health {
        let row = add(url, "Checking…");
        let verdict = match probes::probe(url).await {
            Some(Reach::Ok(code)) => format!("Answers ({code})"),
            Some(Reach::Error(code)) => format!("Answers with an error ({code})"),
            Some(Reach::Unreachable) => "Does not answer".to_string(),
            None => "Not checked: curl is not installed".to_string(),
        };
        row.set_subtitle(&verdict);
    }
}
