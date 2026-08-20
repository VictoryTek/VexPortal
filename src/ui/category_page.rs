//! A category's action cards, and what happens when one is pressed.

use crate::app::App;
use crate::ui::{arg_dialog, badge, risk_pill, run_page::RunPage, window::Window};

use adw::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_catalog::{Recipe, Risk};

pub fn build(app: &Rc<App>, window: &Window, category_id: &str) -> adw::NavigationPage {
    let category = app.catalog.category(category_id);
    let title = category.map_or("VexOS", |c| c.title.as_str());

    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    if let Some(description) = category.and_then(|c| c.description.as_deref()) {
        group.set_description(Some(description));
    }

    for recipe in app.visible_in(category_id) {
        group.add(&action_row(app, window, recipe));
    }
    page.add(&group);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));

    adw::NavigationPage::builder()
        .title(title)
        .child(&toolbar)
        .build()
}

/// One action: icon, title, the real description, risk, and the button that runs it.
pub fn action_row(app: &Rc<App>, window: &Window, recipe: &Recipe) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&recipe.title)
        .subtitle(&recipe.blurb)
        .build();
    // The blurbs are full sentences; letting them wrap is the point of having them.
    row.set_subtitle_lines(0);
    row.add_prefix(&gtk::Image::from_icon_name(&recipe.icon));

    let suffix = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    suffix.set_valign(gtk::Align::Center);

    if recipe.terminal {
        suffix.append(&badge(
            "Terminal",
            "This operation asks questions as it goes, so it opens in a terminal window",
        ));
    }
    if recipe.needs_upstream {
        suffix.append(&badge(
            "Needs vexos-nix update",
            "This recipe still stops for a prompt. Until vexos-nix honours VEXOS_ASSUME_YES \
             it will either take the prompt's default answer or stop with an error — it \
             cannot hang, and it cannot answer for you.",
        ));
    }
    if let Some(pill) = risk_pill(recipe.risk) {
        suffix.append(&pill);
    }

    let button = gtk::Button::with_label(if recipe.terminal { "Open" } else { "Run" });
    button.set_valign(gtk::Align::Center);
    if recipe.risk == Risk::Destructive {
        button.add_css_class("destructive-action");
    } else if recipe.risk == Risk::Medium {
        button.add_css_class("suggested-action");
    }
    button.connect_clicked({
        let app = app.clone();
        let window = window.clone();
        let recipe = recipe.name.clone();
        move |_| activate(&app, &window, &recipe)
    });
    suffix.append(&button);

    row.add_suffix(&suffix);
    row.set_activatable_widget(Some(&button));
    row
}

/// The path from a click to a running job: collect arguments, confirm, then run.
fn activate(app: &Rc<App>, window: &Window, recipe_name: &str) {
    let Some(recipe) = app.catalog.recipe(recipe_name) else {
        return;
    };

    if recipe.terminal {
        open_in_terminal(app, window, recipe);
        return;
    }

    if recipe.params.is_empty() {
        confirm_then_run(app, window, recipe, HashMap::new());
    } else {
        arg_dialog::present(app, window, recipe);
    }
}

/// Confirm anything destructive or explicitly flagged, then start it.
pub fn confirm_then_run(
    app: &Rc<App>,
    window: &Window,
    recipe: &Recipe,
    args: HashMap<String, String>,
) {
    let Some(body) = recipe.confirm.clone() else {
        start(app, window, recipe, args);
        return;
    };

    let dialog = adw::AlertDialog::new(Some(&recipe.title), Some(&body));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("run", &recipe.title);
    dialog.set_response_appearance(
        "run",
        if recipe.risk == Risk::Destructive {
            adw::ResponseAppearance::Destructive
        } else {
            adw::ResponseAppearance::Suggested
        },
    );
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    dialog.connect_response(None, {
        let app = app.clone();
        let window = window.clone();
        let recipe = recipe.name.clone();
        move |_, response| {
            if response != "run" {
                return;
            }
            if let Some(recipe) = app.catalog.recipe(&recipe) {
                start(&app, &window, recipe, args.clone());
            }
        }
    });
    dialog.present(Some(window.root()));
}

fn start(app: &Rc<App>, window: &Window, recipe: &Recipe, args: HashMap<String, String>) {
    let page = RunPage::new(app, window, recipe);
    window.push(page.navigation_page());
    app.run(recipe, args, page);
}

/// The escape hatch for recipes that hold a real conversation — the storage wizards
/// and `enable <service>`. Rather than pretend a form can drive them, hand the user a
/// terminal already sitting in the right place.
fn open_in_terminal(app: &Rc<App>, window: &Window, recipe: &Recipe) {
    let command = format!("just {}", recipe.name);
    let launched = spawn_terminal(&command);
    if launched {
        window.toast(&format!("Opened a terminal running `{command}`"));
    } else {
        let dialog = adw::AlertDialog::new(
            Some("No terminal available"),
            Some(&format!(
                "VexPortal could not find a terminal to open. Run this from a shell instead:\n\n\
                 cd /etc/nixos && {command}"
            )),
        );
        dialog.add_response("ok", "OK");
        dialog.present(Some(window.root()));
    }
    let _ = app;
}

fn spawn_terminal(command: &str) -> bool {
    // In order of preference: the GNOME default on a VexOS desktop, then the common
    // alternatives, then `x-terminal-emulator` for anything else.
    let candidates: [(&str, Vec<String>); 4] = [
        (
            "kgx",
            vec![
                "--working-directory".into(),
                "/etc/nixos".into(),
                "--".into(),
                "bash".into(),
                "-lc".into(),
                format!("{command}; exec bash"),
            ],
        ),
        (
            "gnome-terminal",
            vec![
                "--working-directory=/etc/nixos".into(),
                "--".into(),
                "bash".into(),
                "-lc".into(),
                format!("{command}; exec bash"),
            ],
        ),
        (
            "ptyxis",
            vec![
                "--working-directory".into(),
                "/etc/nixos".into(),
                "--".into(),
                "bash".into(),
                "-lc".into(),
                format!("{command}; exec bash"),
            ],
        ),
        (
            "xterm",
            vec![
                "-e".into(),
                "bash".into(),
                "-lc".into(),
                format!("cd /etc/nixos && {command}; exec bash"),
            ],
        ),
    ];

    for (program, args) in candidates {
        if std::process::Command::new(program)
            .args(&args)
            .spawn()
            .is_ok()
        {
            return true;
        }
    }
    false
}
