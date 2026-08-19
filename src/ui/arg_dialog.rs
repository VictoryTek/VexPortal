//! The form that replaces a recipe's terminal prompts.
//!
//! Every widget is built from the catalog's parameter list, so adding an argument to a
//! recipe is a catalog edit rather than a new dialog.

use crate::app::App;
use crate::ui::{category_page, window::Window};

use adw::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_catalog::{Param, Recipe, Widget};

/// Reads one parameter's current value out of whichever widget backs it.
enum Field {
    Choice(adw::ComboRow, Vec<String>, bool),
    Text(adw::EntryRow),
    Path(Rc<std::cell::RefCell<Option<String>>>),
    Secret(adw::PasswordEntryRow),
}

impl Field {
    fn value(&self) -> String {
        match self {
            Field::Choice(row, choices, optional) => {
                let index = row.selected() as usize;
                if *optional {
                    // Index 0 is the "leave unchanged" entry, which maps to no argument.
                    if index == 0 {
                        String::new()
                    } else {
                        choices.get(index - 1).cloned().unwrap_or_default()
                    }
                } else {
                    choices.get(index).cloned().unwrap_or_default()
                }
            }
            Field::Text(row) => row.text().trim().to_string(),
            Field::Path(value) => value.borrow().clone().unwrap_or_default(),
            Field::Secret(row) => row.text().to_string(),
        }
    }
}

pub fn present(app: &Rc<App>, window: &Window, recipe: &Recipe) {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    group.set_description(Some(&recipe.blurb));

    let mut fields: Vec<(String, Field)> = Vec::new();
    for param in &recipe.params {
        let (widget, field) = build_field(app, window, param);
        group.add(&widget);
        fields.push((param.name.clone(), field));
    }
    page.add(&group);

    let dialog = adw::Dialog::builder()
        .title(&recipe.title)
        .content_width(520)
        .build();

    let header = adw::HeaderBar::new();
    let cancel = gtk::Button::with_label("Cancel");
    let run = gtk::Button::with_label("Run");
    run.add_css_class("suggested-action");
    header.pack_start(&cancel);
    header.pack_end(&run);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&page));
    dialog.set_child(Some(&toolbar));

    cancel.connect_clicked({
        let dialog = dialog.clone();
        move |_| {
            dialog.close();
        }
    });

    let fields = Rc::new(fields);
    run.connect_clicked({
        let app = app.clone();
        let window = window.clone();
        let dialog = dialog.clone();
        let recipe = recipe.name.clone();
        let fields = fields.clone();
        move |_| {
            let Some(recipe) = app.catalog.recipe(&recipe) else {
                return;
            };
            let args: HashMap<String, String> = fields
                .iter()
                .map(|(name, field)| (name.clone(), field.value()))
                .filter(|(_, value)| !value.is_empty())
                .collect();

            // Check here rather than letting the daemon reject it: a missing required
            // field should point at the field, not come back as a D-Bus error.
            if let Some(missing) = recipe
                .params
                .iter()
                .find(|p| p.required && !args.contains_key(&p.name))
            {
                let alert = adw::AlertDialog::new(
                    Some("Missing information"),
                    Some(&format!("`{}` is required.", missing.label)),
                );
                alert.add_response("ok", "OK");
                alert.present(Some(window.root()));
                return;
            }

            dialog.close();
            category_page::confirm_then_run(&app, &window, recipe, args);
        }
    });

    dialog.present(Some(window.root()));
}

fn build_field(app: &Rc<App>, window: &Window, param: &Param) -> (gtk::Widget, Field) {
    match &param.widget {
        Widget::Choice { choices } => choice_row(param, choices.clone()),
        Widget::ChoiceDynamic { source, extra } => {
            let choices = app.facts.choices(*source, extra);
            choice_row(param, choices)
        }
        Widget::Text { format } => {
            let row = adw::EntryRow::builder().title(&param.label).build();
            if let Some(default) = &param.default {
                row.set_text(default);
            }
            let help = match &param.help {
                Some(help) => help.clone(),
                None => format!("Expected {}.", format.expectation()),
            };
            row.set_tooltip_text(Some(&help));
            (row.clone().upcast(), Field::Text(row))
        }
        Widget::Path { .. } => path_row(param, window),
        Widget::Secret => {
            let row = adw::PasswordEntryRow::builder().title(&param.label).build();
            if let Some(help) = &param.help {
                row.set_tooltip_text(Some(help));
            }
            (row.clone().upcast(), Field::Secret(row))
        }
    }
}

fn choice_row(param: &Param, choices: Vec<String>) -> (gtk::Widget, Field) {
    let optional = !param.required;
    let mut entries: Vec<String> = Vec::new();
    if optional {
        entries.push("Leave unchanged".to_string());
    }
    entries.extend(choices.iter().cloned());

    let model = gtk::StringList::new(&entries.iter().map(String::as_str).collect::<Vec<_>>());
    let row = adw::ComboRow::builder()
        .title(&param.label)
        .model(&model)
        .build();
    if let Some(help) = &param.help {
        row.set_subtitle(help);
    }
    if let Some(default) = &param.default {
        if let Some(index) = entries.iter().position(|e| e == default) {
            row.set_selected(index as u32);
        }
    }
    (row.upcast_ref::<gtk::Widget>().clone(), Field::Choice(row, choices, optional))
}

fn path_row(param: &Param, window: &Window) -> (gtk::Widget, Field) {
    let must_exist = matches!(param.widget, Widget::Path { must_exist: true });
    let chosen: Rc<std::cell::RefCell<Option<String>>> = Rc::new(std::cell::RefCell::new(None));

    let row = adw::ActionRow::builder()
        .title(&param.label)
        .subtitle(if must_exist {
            "No file chosen"
        } else {
            "Default location"
        })
        .build();
    if let Some(help) = &param.help {
        row.set_subtitle(help);
    }

    let button = gtk::Button::with_label("Choose…");
    button.set_valign(gtk::Align::Center);
    row.add_suffix(&button);
    row.set_activatable_widget(Some(&button));

    button.connect_clicked({
        let chosen = chosen.clone();
        let row = row.clone();
        let root = window.root().clone();
        let title = param.label.clone();
        move |_| {
            let dialog = gtk::FileDialog::builder().title(&title).build();
            let chosen = chosen.clone();
            let row = row.clone();
            let finish = move |result: Result<gtk::gio::File, glib::Error>| {
                let Ok(file) = result else { return };
                let Some(path) = file.path() else { return };
                let display = path.display().to_string();
                row.set_subtitle(&display);
                *chosen.borrow_mut() = Some(display);
            };
            if must_exist {
                dialog.open(Some(&root), gtk::gio::Cancellable::NONE, finish);
            } else {
                dialog.save(Some(&root), gtk::gio::Cancellable::NONE, finish);
            }
        }
    });

    // Paths only ever come from the file chooser, so they are absolute by
    // construction — and the daemon validates them again regardless.
    (row.upcast(), Field::Path(chosen))
}
