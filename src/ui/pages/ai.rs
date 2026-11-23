//! The AI assistant: setup and crash-notification settings.
//!
//! The state comes from `vexos-ai status --json`. A setting changes by running
//! `vexos-ai` itself, silently and as the user, with no terminal: it is a few files in
//! the user's own home. Only opening the assistant needs the terminal.

use crate::app::App;
use crate::system::ai::{self, Agent, AiState};
use crate::system::ai_model::{choices, Dirs};
use crate::ui::{self, actions, job_view, window::Window};

use adw::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_catalog::Format;

/// Rendered by this page's own controls rather than as rows.
const MANAGED: [&str; 3] = ["ai-pick", "ai-crash-mute", "ai-crash-capture"];

pub fn build(app: &Rc<App>, window: &Window) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();
    if let Some(description) = app
        .catalog
        .page("ai")
        .and_then(|p| p.description.as_deref())
    {
        page.set_description(description);
    }

    // Until it is installed every action here would fail, and until a reboot its
    // background services are not running, so offer only the next step.
    let reboot = ai::needs_reboot();
    if reboot || !ai::is_installed() {
        page.add(&finish_installing(app, window, reboot));
        return ui::page(app, window, "AI Assistant", &page);
    }

    let state = AiState::read();
    page.add(&assistants(app, window, state.agent));
    // Both start `vexos-ai`, which falls back to its own zenity picker while no
    // assistant is chosen; the setup group above replaces that.
    let mut skip = MANAGED.to_vec();
    if state.agent.is_none() && app.visible("ai-pick").is_some() {
        skip.extend(["agent", "diagnose"]);
    }
    for group in ui::action_groups(app, window, "ai", &skip) {
        page.add(&group);
    }
    if app.visible("ai-crash-mute").is_some() {
        page.add(&crash_mutes(app, window, &state));
    }

    ui::page(app, window, "AI Assistant", &page)
}

fn finish_installing(app: &Rc<App>, window: &Window, reboot: bool) -> adw::PreferencesGroup {
    let (title, subtitle, icon, action, label) = if reboot {
        (
            "Reboot to finish installing",
            "The AI Assistant is installed. Its crash notifications and theme sync \
             start after a reboot.",
            "system-reboot-symbolic",
            "reboot",
            "Reboot Now",
        )
    } else {
        (
            "Rebuild to finish installing",
            "The AI Assistant is turned on, but it is installed by the next rebuild.",
            "software-update-available-symbolic",
            "rebuild",
            "Rebuild Now",
        )
    };
    let group = adw::PreferencesGroup::new();
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build();
    row.set_subtitle_lines(0);
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    if app.visible(action).is_some() {
        let button = gtk::Button::with_label(label);
        button.set_valign(gtk::Align::Center);
        button.add_css_class("suggested-action");
        button.connect_clicked({
            let app = app.clone();
            let window = window.clone();
            move |_| actions::activate(&app, &window, action, HashMap::new())
        });
        row.add_suffix(&button);
    }
    group.add(&row);
    group
}

/// Asked once, when a rebuild has just installed the assistant.
pub fn offer_reboot(app: &Rc<App>, window: &Window) {
    if app.visible("reboot").is_none() {
        return;
    }
    let ask = adw::AlertDialog::new(
        Some("Reboot to finish setting up the AI Assistant?"),
        Some(
            "The AI Assistant is installed. Its crash notifications and theme sync start \
             after a reboot. Unsaved work in other applications is lost.",
        ),
    );
    ask.add_response("later", "Later");
    ask.add_response("reboot", "Reboot Now");
    ask.set_response_appearance("reboot", adw::ResponseAppearance::Destructive);
    ask.set_default_response(Some("later"));
    ask.set_close_response("later");
    ask.connect_response(None, {
        let app = app.clone();
        let window = window.clone();
        move |_, response| {
            if response != "reboot" {
                return;
            }
            // This dialog already carries the reboot's warning; asking a second time
            // through the catalog's confirm would add nothing.
            if let Some(action) = app.catalog.action("reboot") {
                let job = app.run(action, HashMap::new());
                job_view::present(&app, &window, &job);
            }
        }
    });
    ask.present(Some(window.root()));
}

/// The three assistants as cards, whether or not one is chosen.
fn assistants(app: &Rc<App>, window: &Window, chosen: Option<Agent>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    let cards = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .min_children_per_line(1)
        .max_children_per_line(3)
        .column_spacing(14)
        .row_spacing(14)
        .build();
    let dirs = Dirs::of_user();
    for agent in Agent::ALL {
        cards.append(&card(app, window, agent, chosen, &dirs));
    }
    group.add(&cards);
    group
}

/// One assistant: its mark, its sign-in, its model, and the one thing to do next.
fn card(
    app: &Rc<App>,
    window: &Window,
    agent: Agent,
    chosen: Option<Agent>,
    dirs: &Dirs,
) -> gtk::Overlay {
    let is_chosen = chosen == Some(agent);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
    body.add_css_class("vex-agent-card");
    body.add_css_class(agent.id());
    if is_chosen {
        body.add_css_class("chosen");
    }
    body.set_tooltip_text(Some(agent.description()));

    let logo = gtk::Image::from_resource(&format!(
        "/io/github/vexportal/icons/agents/{}.svg",
        agent.id()
    ));
    logo.set_pixel_size(40);
    let tile = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    tile.add_css_class("vex-agent-logo");
    tile.set_halign(gtk::Align::Center);
    tile.append(&logo);
    body.append(&tile);

    let name = gtk::Label::new(Some(agent.title()));
    name.add_css_class("title-3");
    body.append(&name);
    let maker = gtk::Label::new(Some(agent.maker()));
    maker.add_css_class("caption");
    maker.add_css_class("dim-label");
    body.append(&maker);

    body.append(&sign_in_chip(dirs.signed_in(agent)));

    let saved = dirs.model(agent);
    body.append(&model_menu(window, agent, saved));

    if is_chosen {
        if app.visible("agent").is_some() {
            let open = gtk::Button::with_label("Open");
            open.add_css_class("suggested-action");
            open.add_css_class("pill");
            open.set_halign(gtk::Align::Center);
            open.connect_clicked({
                let app = app.clone();
                let window = window.clone();
                move |_| actions::activate(&app, &window, "agent", HashMap::new())
            });
            body.append(&open);
        }
    } else if app.visible("ai-pick").is_some() {
        let use_it = gtk::Button::with_label("Use");
        use_it.add_css_class("pill");
        use_it.set_halign(gtk::Align::Center);
        use_it.connect_clicked({
            let app = app.clone();
            let window = window.clone();
            move |_| pick(&app, &window, agent)
        });
        body.append(&use_it);
    }

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&body));
    if is_chosen {
        let badge = ui::pill("In use", "accent", "");
        badge.set_halign(gtk::Align::End);
        badge.set_valign(gtk::Align::Start);
        badge.set_margin_top(10);
        badge.set_margin_end(10);
        overlay.add_overlay(&badge);
    }
    overlay
}

/// A coloured dot and two words.
fn sign_in_chip(signed_in: Option<bool>) -> gtk::Box {
    let (class, text) = match signed_in {
        Some(true) => ("signed", "Signed in"),
        Some(false) => ("unsigned", "Not signed in"),
        None => ("unknown", "Sign-in unknown"),
    };
    let chip = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    chip.set_halign(gtk::Align::Center);
    let dot = gtk::Label::new(Some("●"));
    dot.add_css_class("vex-dot");
    dot.add_css_class(class);
    let label = gtk::Label::new(Some(text));
    label.add_css_class("caption");
    chip.append(&dot);
    chip.append(&label);
    chip
}

/// The model in words: the dropdown's label for it, else the saved name as it is.
fn model_label(agent: Agent, saved: Option<&str>) -> String {
    let Some(value) = saved else {
        return choices(agent)[0].1.to_string();
    };
    choices(agent)
        .iter()
        .find(|(v, _)| *v == value)
        .map_or_else(|| value.to_string(), |(_, label)| label.to_string())
}

/// The model dropdown. A saved name the list does not know is added, so it is never lost.
fn model_menu(window: &Window, agent: Agent, saved: Option<String>) -> gtk::DropDown {
    let mut values: Vec<String> = choices(agent).iter().map(|(v, _)| v.to_string()).collect();
    let mut labels: Vec<String> = choices(agent).iter().map(|(_, l)| l.to_string()).collect();
    let current = saved.unwrap_or_default();
    if !values.contains(&current) {
        values.push(current.clone());
        labels.push(format!("{current} (current)"));
    }
    let selected = values.iter().position(|v| *v == current).unwrap_or(0);
    let menu = gtk::DropDown::from_strings(&labels.iter().map(String::as_str).collect::<Vec<_>>());
    menu.set_selected(selected as u32);
    // A long saved name is cut short with an ellipsis instead of widening the card.
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_max_width_chars(22);
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            item.set_child(Some(&label));
        }
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let text = item
            .item()
            .and_downcast::<gtk::StringObject>()
            .map(|s| s.string())
            .unwrap_or_default();
        if let Some(label) = item.child().and_downcast::<gtk::Label>() {
            label.set_label(&text);
            label.set_tooltip_text(Some(&text));
        }
    });
    menu.set_factory(Some(&factory));
    menu.add_css_class("vex-model");
    menu.set_tooltip_text(Some("Model"));
    menu.connect_selected_notify({
        let window = window.clone();
        move |menu| {
            let value = values
                .get(menu.selected() as usize)
                .cloned()
                .unwrap_or_default();
            if value == current {
                return;
            }
            let wanted = (!value.is_empty()).then_some(value.as_str());
            match Dirs::of_user().set_model(agent, wanted) {
                Ok(()) => window.toast(&format!(
                    "{} will use {}",
                    agent.title(),
                    model_label(agent, wanted)
                )),
                Err(why) => window.alert("Cannot change the model", &why),
            }
            // The page shows what the file now says, which also undoes a refused change.
            window.state_changed();
        }
    });
    menu
}

/// Save the choice, silently and as the user, and once it is saved offer to open the
/// assistant. A failure (not installed, not supported yet) is shown as a message.
fn pick(app: &Rc<App>, window: &Window, agent: Agent) {
    let args = HashMap::from([("agent".to_string(), agent.id().to_string())]);
    actions::run_as_user(
        app,
        window,
        "ai-pick",
        args,
        &format!("{} is your assistant", agent.title()),
        {
            let app = Rc::downgrade(app);
            let window = window.clone();
            move |worked| {
                let Some(app) = app.upgrade() else { return };
                if worked && app.visible("agent").is_some() {
                    offer_open(&app, &window, agent);
                }
            }
        },
    );
}

fn offer_open(app: &Rc<App>, window: &Window, agent: Agent) {
    let ask = adw::AlertDialog::new(
        Some(&format!("Open {}?", agent.title())),
        Some(&format!(
            "{} is set up. It opens in VexPortal's terminal, working in /etc/nixos. \
             Changes it makes take effect when you rebuild.",
            agent.title()
        )),
    );
    ask.add_response("later", "Later");
    ask.add_response("open", &format!("Open {}", agent.title()));
    ask.set_response_appearance("open", adw::ResponseAppearance::Suggested);
    ask.set_default_response(Some("open"));
    ask.set_close_response("later");
    ask.connect_response(None, {
        let app = app.clone();
        let window = window.clone();
        move |_, response| {
            if response == "open" {
                actions::activate(&app, &window, "agent", HashMap::new());
            }
        }
    });
    ask.present(Some(window.root()));
}

fn crash_mutes(app: &Rc<App>, window: &Window, state: &AiState) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Crash Alerts")
        .build();
    if app.visible("ai-crash-capture").is_some() {
        group.add(&capture_switch(app, window, state.crash_capture));
    }
    if state.muted.is_empty() {
        let row = adw::ActionRow::builder().title("Nothing muted").build();
        row.add_css_class("dim-label");
        group.add(&row);
    }
    for name in &state.muted {
        let row = adw::ActionRow::builder()
            .title(name)
            .use_markup(false)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(
            "notifications-disabled-symbolic",
        ));
        let unmute = gtk::Button::with_label("Unmute");
        unmute.set_valign(gtk::Align::Center);
        // A name the recipe's argument check would refuse cannot be unmuted from here.
        let valid = Format::ProgramName.validate(name).is_ok();
        unmute.set_sensitive(valid);
        if !valid {
            unmute.set_tooltip_text(Some("This name can only be unmuted from a terminal"));
        }
        unmute.connect_clicked({
            let app = app.clone();
            let window = window.clone();
            let name = name.clone();
            move |_| {
                let args = HashMap::from([
                    ("name".to_string(), name.clone()),
                    ("state".to_string(), "off".to_string()),
                ]);
                actions::activate(&app, &window, "ai-crash-mute", args)
            }
        });
        row.add_suffix(&unmute);
        group.add(&row);
    }
    group
}

/// The crash watcher's on/off switch. Whatever the outcome, the page is rebuilt from
/// what `vexos-ai` now reports, so a refused change puts the switch back by itself.
fn capture_switch(app: &Rc<App>, window: &Window, on: bool) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Watch for crashes")
        .active(on)
        .build();
    row.connect_active_notify({
        let app = app.clone();
        let window = window.clone();
        move |row| {
            row.set_sensitive(false);
            let state = if row.is_active() { "on" } else { "off" };
            let args = HashMap::from([("state".to_string(), state.to_string())]);
            actions::run_as_user(
                &app,
                &window,
                "ai-crash-capture",
                args,
                &format!("Crash capture is {state}"),
                |_| {},
            );
        }
    });
    row
}
