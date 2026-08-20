//! What this machine is, and the handful of things most often done to it.

use crate::app::App;
use crate::ui::{category_page, window::Window};

use adw::prelude::*;
use std::rc::Rc;

/// The actions worth a shortcut on the front page, in order. Anything absent from the
/// current role's catalog is skipped.
const QUICK_ACTIONS: [&str; 4] = ["rebuild", "switch", "rollback", "variant"];

pub fn build(app: &Rc<App>, window: &Window) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();

    if let Some(banner) = drift_banner(app) {
        let group = adw::PreferencesGroup::new();
        group.add(&banner);
        page.add(&group);
    }

    page.add(&identity_group(app));

    if app.variant.is_some() {
        if let Some(group) = features_group(app) {
            page.add(&group);
        }
        page.add(&quick_actions_group(app, window));
    } else {
        page.add(&not_built_group());
    }

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));

    adw::NavigationPage::builder()
        .title("Dashboard")
        .child(&toolbar)
        .build()
}

fn identity_group(app: &Rc<App>) -> adw::PreferencesGroup {
    let state = app.state.borrow();
    let group = adw::PreferencesGroup::new();
    group.set_title("This machine");

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    header.set_margin_bottom(6);
    match &app.variant {
        Some(variant) => {
            let badge = gtk::Label::new(Some(variant.role.title()));
            badge.add_css_class("vex-role-badge");
            badge.set_valign(gtk::Align::Center);
            header.append(&badge);

            let gpu = gtk::Label::new(Some(variant.gpu_label()));
            gpu.set_valign(gtk::Align::Center);
            header.append(&gpu);

            let raw = gtk::Label::new(Some(&variant.raw));
            raw.add_css_class("vex-variant");
            raw.set_valign(gtk::Align::Center);
            raw.set_hexpand(true);
            raw.set_xalign(1.0);
            header.append(&raw);
        }
        None => {
            let badge = gtk::Label::new(Some("Unknown role"));
            badge.add_css_class("vex-badge");
            header.append(&badge);
        }
    }
    group.set_header_suffix(Some(&header));

    if let Some(hostname) = &state.hostname {
        group.add(&info_row("Hostname", hostname, "computer-symbolic"));
    }
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

    if state.reboot_pending {
        let row = adw::ActionRow::builder()
            .title("Reboot pending")
            .subtitle("A newer generation is activated but this machine is still running the one it booted.")
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("system-reboot-symbolic"));
        row.set_subtitle_lines(0);
        group.add(&row);
    }

    group
}

fn features_group(app: &Rc<App>) -> Option<adw::PreferencesGroup> {
    let state = app.state.borrow();
    if state.features.is_empty() {
        return None;
    }

    let group = adw::PreferencesGroup::new();
    group.set_title("Features");
    group.set_description(Some(
        "Optional modules for this host. Changes take effect on the next rebuild.",
    ));

    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .column_spacing(6)
        .row_spacing(6)
        .margin_top(6)
        .build();
    for (name, enabled) in &state.features {
        let label = gtk::Label::new(Some(name));
        label.add_css_class(if *enabled { "vex-risk" } else { "vex-badge" });
        if *enabled {
            label.add_css_class("safe");
        }
        label.set_tooltip_text(Some(if *enabled { "Enabled" } else { "Disabled" }));
        flow.append(&label);
    }
    group.add(&flow);
    Some(group)
}

fn quick_actions_group(app: &Rc<App>, window: &Window) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("Common actions");

    let role = app.role();
    for name in QUICK_ACTIONS {
        let Some(recipe) = app.catalog.recipe(name) else {
            continue;
        };
        if !recipe.applies_to(role) || !app.facts.is_available(name) {
            continue;
        }
        group.add(&category_page::action_row(app, window, recipe));
    }
    group
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

/// Shown when the catalog and this host's justfile disagree — either a vexos-nix
/// change VexPortal has not caught up with, or a host that has not rebuilt since one.
fn drift_banner(app: &Rc<App>) -> Option<adw::Banner> {
    let banner = adw::Banner::new(&app.facts.drift_summary()?);
    banner.set_revealed(true);
    Some(banner)
}
