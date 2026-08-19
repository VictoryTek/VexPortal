//! Widgets.

pub mod ansi;
pub mod arg_dialog;
pub mod category_page;
pub mod dashboard;
pub mod run_page;
pub mod window;

use adw::prelude::*;
use vexportal_catalog::Risk;

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

/// The small coloured pill on an action row, for the risk levels worth marking.
///
/// `Medium` gets no pill. Everything in a system management tool changes the system,
/// so labelling every row "Changes system" would put the same words on almost every
/// card and leave nothing for the eye to catch. Marking only the two exceptions — the
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
    let label = gtk::Label::new(Some(text));
    label.add_css_class("vex-risk");
    label.add_css_class(class);
    label.set_tooltip_text(Some(tooltip));
    label.set_valign(gtk::Align::Center);
    Some(label)
}

/// A neutral pill, for notes like "runs in a terminal".
pub fn badge(text: &str, tooltip: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("vex-badge");
    label.set_tooltip_text(Some(tooltip));
    label.set_valign(gtk::Align::Center);
    label
}
