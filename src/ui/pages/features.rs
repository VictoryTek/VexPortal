//! Optional feature modules, one switch each.

use crate::app::App;
use crate::ui::{self, window::Window};

use adw::prelude::*;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

/// The switches stand in for these; the rest of the page's actions are listed below.
const SWITCHED: [&str; 3] = ["feature-enable", "feature-disable", "feature-list"];

pub fn build(app: &Rc<App>, window: &Window) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();

    if app.visible("feature-enable").is_some() && app.visible("feature-disable").is_some() {
        page.add(&switches(app, window));
    }

    let ai_enabled = {
        let state = app.state.borrow();
        let default_on = app
            .catalog
            .features
            .iter()
            .find(|f| f.name == "ai")
            .is_some_and(|f| f.default_on);
        state.settings.feature("ai", default_on).0
    };
    for group in ui::action_groups(app, window, "features", &SWITCHED) {
        // The assistant's actions only work once the feature is built in.
        if group.title() == "AI Assistant" && !ai_enabled {
            continue;
        }
        page.add(&group);
    }

    ui::page(app, window, "Features", &page)
}

fn switches(app: &Rc<App>, window: &Window) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Optional Features")
        .description(
            "Changes are saved to /etc/nixos/features.nix and take effect on the next rebuild.",
        )
        .build();

    // Only the features this justfile actually knows, when it could be read.
    let known = &app.facts.features;
    let state = app.state.borrow();
    for feature in &app.catalog.features {
        if !known.is_empty() && !known.contains(&feature.name) {
            continue;
        }
        let (enabled, defaulted) = state.settings.feature(&feature.name, feature.default_on);
        let subtitle = if defaulted && enabled {
            format!("{} On by default.", feature.description)
        } else {
            feature.description.clone()
        };
        let row = adw::SwitchRow::builder()
            .title(&feature.title)
            .subtitle(&subtitle)
            .active(enabled)
            .build();
        row.set_subtitle_lines(0);

        let spinner = adw::Spinner::new();
        spinner.set_visible(false);
        row.add_suffix(&spinner);

        // Set while the switch is moved back after a failure, so that move is not
        // taken as a new request.
        let reverting = Rc::new(Cell::new(false));
        row.connect_active_notify({
            let app = app.clone();
            let window = window.clone();
            let name = feature.name.clone();
            let title = feature.title.clone();
            let spinner = spinner.downgrade();
            move |row| {
                if reverting.get() {
                    return;
                }
                let on = row.is_active();
                row.set_sensitive(false);
                if let Some(spinner) = spinner.upgrade() {
                    spinner.set_visible(true);
                }
                let mut args = HashMap::new();
                args.insert("feature".to_string(), name.clone());
                let row = row.downgrade();
                let spinner = spinner.clone();
                let reverting = reverting.clone();
                ui::actions::run_quietly(
                    &app,
                    &window,
                    if on {
                        "feature-enable"
                    } else {
                        "feature-disable"
                    },
                    args,
                    &format!(
                        "{title} will be {} on the next rebuild",
                        if on { "enabled" } else { "disabled" }
                    ),
                    move |ok| {
                        let Some(row) = row.upgrade() else { return };
                        if let Some(spinner) = spinner.upgrade() {
                            spinner.set_visible(false);
                        }
                        row.set_sensitive(true);
                        if !ok {
                            reverting.set(true);
                            row.set_active(!on);
                            reverting.set(false);
                        }
                    },
                );
            }
        });
        group.add(&row);
    }
    group
}
