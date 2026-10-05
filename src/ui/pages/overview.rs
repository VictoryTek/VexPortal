//! What this machine is, what it needs, and the things most often done to it.

use crate::app::App;
use crate::ui::{self, pages, switch_dialog, window::Window};

use adw::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;

pub fn build(app: &Rc<App>, window: &Window) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();

    if let Some(group) = drift_group(app) {
        page.add(&group);
    }
    page.add(&hero_group(app, window));

    if app.variant.is_some() {
        if let Some(group) = attention_group(app, window) {
            page.add(&group);
        }
        page.add(&details_group(app));
    } else {
        page.add(&not_built_group());
    }

    ui::page(app, window, "Overview", &page)
}

/// Which machine this is, and the two things people open VexPortal to do: rebuild,
/// or become something else.
fn hero_group(app: &Rc<App>, window: &Window) -> adw::PreferencesGroup {
    let state = app.state.borrow();
    let group = adw::PreferencesGroup::new();

    let hero = gtk::Box::new(gtk::Orientation::Vertical, 6);
    hero.add_css_class("vex-hero");
    hero.set_margin_bottom(12);

    let title = gtk::Label::new(Some(state.hostname.as_deref().unwrap_or("This machine")));
    title.add_css_class("title-1");
    title.set_xalign(0.0);
    hero.append(&title);

    let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    match &app.variant {
        Some(variant) => {
            let badge = gtk::Label::new(Some(variant.role.title()));
            badge.add_css_class("vex-role-badge");
            line.append(&badge);
            line.append(&ui::pill(variant.gpu_label(), "", ""));
            let raw = gtk::Label::new(Some(&variant.raw));
            raw.add_css_class("vex-variant");
            raw.set_hexpand(true);
            raw.set_xalign(1.0);
            line.append(&raw);
        }
        None => line.append(&ui::pill("Unknown role", "", "")),
    }
    hero.append(&line);
    if app.variant.is_some() {
        hero.append(&primary_buttons(app, window));
    }
    group.add(&hero);
    group
}

fn details_group(app: &Rc<App>) -> adw::PreferencesGroup {
    let state = app.state.borrow();
    let group = adw::PreferencesGroup::builder()
        .title("This Machine")
        .build();
    if let Some(generation) = state.generation {
        group.add(&info_row(
            "Generation",
            &generation.to_string(),
            "document-open-recent-symbolic",
        ));
    }
    if let Some(age) = state.lock_age_label() {
        group.add(&info_row(
            "Flake inputs updated",
            &age,
            "software-update-available-symbolic",
        ));
    }
    group
}

fn primary_buttons(app: &Rc<App>, window: &Window) -> gtk::Box {
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::Start);
    buttons.set_margin_top(18);

    if app.visible("rebuild").is_some() {
        let rebuild = pill_button("Rebuild System", "view-refresh-symbolic");
        rebuild.add_css_class("suggested-action");
        rebuild.connect_clicked({
            let app = app.clone();
            let window = window.clone();
            move |_| ui::actions::activate(&app, &window, "rebuild", HashMap::new())
        });
        buttons.append(&rebuild);
    }
    if app.visible("switch").is_some() {
        let switch = pill_button("Switch Role…", "object-select-symbolic");
        switch.connect_clicked({
            let app = app.clone();
            let window = window.clone();
            move |_| switch_dialog::present(&app, &window, switch_dialog::Mode::Switch)
        });
        buttons.append(&switch);
    }
    let activity = pill_button("Activity", "view-list-bullet-symbolic");
    activity.connect_clicked({
        let window = window.clone();
        move |_| window.show(pages::ACTIVITY)
    });
    buttons.append(&activity);
    buttons
}

/// Things that want doing, when there are any.
fn attention_group(app: &Rc<App>, window: &Window) -> Option<adw::PreferencesGroup> {
    let state = app.state.borrow();
    let group = adw::PreferencesGroup::builder()
        .title("Needs Attention")
        .build();
    let mut any = false;

    // A pending rebuild is the page banner's job, on every page.
    if state.reboot_pending {
        let row = adw::ActionRow::builder()
            .title("Reboot to finish updating")
            .subtitle(
                "A newer system is active, but this machine is still running the one it booted.",
            )
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("system-reboot-symbolic"));
        if let Some(action) = app.visible("reboot") {
            row.add_suffix(&action_button(app, window, &action.id, "Reboot"));
        }
        group.add(&row);
        any = true;
    }
    any.then_some(group)
}

fn not_built_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    let status = adw::StatusPage::builder()
        .icon_name("dialog-question-symbolic")
        .title("Not a VexOS host yet")
        .description(
            "VexPortal could not read /etc/nixos/vexos-variant, so it does not know which role \
             this machine is built as. Run `just switch` once from a terminal in the vexos-nix \
             checkout; after that this page will fill in.",
        )
        .build();
    status.set_vexpand(true);
    group.add(&status);
    group
}

/// Shown when the catalog and this host's justfile disagree — either a vexos-nix
/// change VexPortal has not caught up with, or a host that has not rebuilt since one.
fn drift_group(app: &Rc<App>) -> Option<adw::PreferencesGroup> {
    let summary = app.facts.drift_summary()?;
    let group = adw::PreferencesGroup::new();
    let row = adw::ActionRow::builder()
        .title("Some operations are unavailable")
        .subtitle(summary)
        .use_markup(false)
        .build();
    row.set_subtitle_lines(0);
    row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
    group.add(&row);
    Some(group)
}

fn info_row(title: &str, value: &str, icon: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    let label = gtk::Label::new(Some(value));
    label.add_css_class("dim-label");
    label.set_valign(gtk::Align::Center);
    label.set_selectable(true);
    row.add_suffix(&label);
    row
}

fn pill_button(label: &str, icon: &str) -> gtk::Button {
    let content = adw::ButtonContent::builder()
        .label(label)
        .icon_name(icon)
        .build();
    let button = gtk::Button::builder().child(&content).build();
    button.add_css_class("pill");
    button
}

fn action_button(app: &Rc<App>, window: &Window, id: &str, label: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.set_valign(gtk::Align::Center);
    button.connect_clicked({
        let app = app.clone();
        let window = window.clone();
        let id = id.to_string();
        move |_| ui::actions::activate(&app, &window, &id, HashMap::new())
    });
    button
}
