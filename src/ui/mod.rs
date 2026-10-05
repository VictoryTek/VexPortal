//! Widgets.

pub mod actions;
pub mod ansi;
pub mod arg_dialog;
pub mod job_view;
pub mod pages;
pub mod switch_dialog;
pub mod terminal_view;
pub mod window;

use crate::app::App;
use window::Window;

use adw::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_catalog::{Action, Risk};

/// Load the stylesheet from the resource bundle at the application level, so every
/// window picks it up and a theme change re-resolves the libadwaita colours it uses.
pub fn load_stylesheet() {
    let provider = gtk::CssProvider::new();
    provider.load_from_resource("/io/github/vexportal/style.css");
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Wrap a page's content in a header bar, plus the rebuild banner when files the next
/// rebuild reads have changed since the last one.
pub fn page(
    app: &Rc<App>,
    window: &Window,
    title: &str,
    content: &impl IsA<gtk::Widget>,
) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());

    if app.state.borrow().rebuild_needed && app.visible("rebuild").is_some() {
        let banner = adw::Banner::builder()
            .title("Changes are waiting for a rebuild")
            .button_label("Rebuild Now")
            .revealed(true)
            .build();
        banner.connect_button_clicked({
            let app = app.clone();
            let window = window.clone();
            move |_| actions::activate(&app, &window, "rebuild", HashMap::new())
        });
        toolbar.add_top_bar(&banner);
    }

    toolbar.set_content(Some(content));
    adw::NavigationPage::builder()
        .title(title)
        .child(&toolbar)
        .build()
}

/// One action: icon, title, the real description, risk, and the button that runs it.
///
/// `preset` answers parameters the row already knows — the service a detail page is
/// about — so the user is only asked for the rest.
pub fn action_row(
    app: &Rc<App>,
    window: &Window,
    action: &Action,
    preset: &[(&str, &str)],
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&action.title)
        .subtitle(&action.blurb)
        .use_markup(false)
        .build();
    // The blurbs are full sentences; letting them wrap is the point of having them.
    row.set_subtitle_lines(0);
    row.add_prefix(&gtk::Image::from_icon_name(&action.icon));

    let suffix = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    suffix.set_valign(gtk::Align::Center);

    if action.is_terminal() {
        let icon = gtk::Image::from_icon_name("utilities-terminal-symbolic");
        icon.add_css_class("dim-label");
        icon.set_tooltip_text(
            action
                .mode_reason
                .as_deref()
                .map(|reason| format!("Opens in VexPortal's terminal. {reason}"))
                .as_deref(),
        );
        suffix.append(&icon);
    }
    if let Some(pill) = risk_pill(action.risk) {
        suffix.append(&pill);
    }

    let preset: HashMap<String, String> = preset
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let asks = action.params.iter().any(|p| !preset.contains_key(&p.name));
    let label = match (action.is_terminal(), asks) {
        (true, _) => "Open",
        (false, true) => "Run…",
        (false, false) => "Run",
    };
    let button = gtk::Button::with_label(label);
    button.set_valign(gtk::Align::Center);
    if action.risk == Risk::Destructive {
        button.add_css_class("destructive-action");
    }
    button.connect_clicked({
        let app = app.clone();
        let window = window.clone();
        let id = action.id.clone();
        move |_| actions::activate(&app, &window, &id, preset.clone())
    });
    suffix.append(&button);

    row.add_suffix(&suffix);
    row.set_activatable_widget(Some(&button));
    row
}

/// The small coloured pill on an action row, for the risk levels worth marking.
///
/// `Medium` gets no pill. Everything in a system management tool changes the system,
/// so labelling every row "Changes system" would put the same words on almost every
/// row and leave nothing for the eye to catch. Marking only the two exceptions — the
/// ones that are safe to poke at, and the ones that are not — is what makes the mark
/// mean something.
///
/// Colour is never the only signal: each pill carries its meaning as text too, and
/// anything destructive has to pass a confirmation dialog regardless.
pub fn risk_pill(risk: Risk) -> Option<gtk::Label> {
    let (text, class, tooltip) = match risk {
        Risk::Safe => (
            "Read-only",
            "safe",
            "Reads system state without changing anything",
        ),
        Risk::Medium => return None,
        Risk::Destructive => (
            "Destructive",
            "destructive",
            "Destroys data or removes a protection — asks for confirmation first",
        ),
    };
    Some(pill(text, class, tooltip))
}

/// A rounded label: `safe` (green), `destructive` (red), `accent`, or plain.
pub fn pill(text: &str, class: &str, tooltip: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("vex-pill");
    if !class.is_empty() {
        label.add_css_class(class);
    }
    if !tooltip.is_empty() {
        label.set_tooltip_text(Some(tooltip));
    }
    label.set_valign(gtk::Align::Center);
    label
}

/// A preferences group per catalog `group`, in catalog order, for the actions on a
/// page that a page does not render itself.
pub fn action_groups(
    app: &Rc<App>,
    window: &Window,
    page: &str,
    skip: &[&str],
) -> Vec<adw::PreferencesGroup> {
    let mut groups: Vec<(String, adw::PreferencesGroup)> = Vec::new();
    for action in app.visible_on(page) {
        if skip.contains(&action.id.as_str()) {
            continue;
        }
        let index = match groups.iter().position(|(title, _)| *title == action.group) {
            Some(index) => index,
            None => {
                let group = adw::PreferencesGroup::builder()
                    .title(&action.group)
                    .build();
                groups.push((action.group.clone(), group));
                groups.len() - 1
            }
        };
        groups[index].1.add(&action_row(app, window, action, &[]));
    }
    groups.into_iter().map(|(_, group)| group).collect()
}
