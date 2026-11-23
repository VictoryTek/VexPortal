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

/// Every assistant, whether or not one is chosen: its sign-in, its model, and Use/Open.
fn assistants(app: &Rc<App>, window: &Window, chosen: Option<Agent>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Assistants")
        .description(
            "Choose the coding agent VexOS opens, and the model it uses. A model change \
             applies to the next session. Sign in inside the assistant itself.",
        )
        .build();
    let dirs = Dirs::of_user();
    for agent in Agent::ALL {
        let saved = dirs.model(agent);
        let signed_in = match dirs.signed_in(agent) {
            Some(true) => "Signed in",
            Some(false) => "Not signed in",
            None => "Sign-in not shown",
        };
        let row = adw::ExpanderRow::builder()
            .title(agent.title())
            .subtitle(format!(
                "{}\n{signed_in} · Model: {}",
                agent.description(),
                model_label(agent, saved.as_deref())
            ))
            .use_markup(false)
            .build();
        row.set_subtitle_lines(0);

        if chosen == Some(agent) {
            row.add_prefix(&gtk::Image::from_icon_name("user-available-symbolic"));
            if app.visible("agent").is_some() {
                let open = gtk::Button::with_label("Open");
                open.set_valign(gtk::Align::Center);
                open.add_css_class("suggested-action");
                open.connect_clicked({
                    let app = app.clone();
                    let window = window.clone();
                    move |_| actions::activate(&app, &window, "agent", HashMap::new())
                });
                row.add_suffix(&open);
            }
        } else if app.visible("ai-pick").is_some() {
            let use_it = gtk::Button::with_label("Use this");
            use_it.set_valign(gtk::Align::Center);
            if chosen.is_none() && agent == Agent::Claude {
                use_it.add_css_class("suggested-action");
            }
            use_it.connect_clicked({
                let app = app.clone();
                let window = window.clone();
                move |_| pick(&app, &window, agent)
            });
            row.add_suffix(&use_it);
        }
        row.add_row(&model_row(window, agent, saved));
        group.add(&row);
    }
    group
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
fn model_row(window: &Window, agent: Agent, saved: Option<String>) -> adw::ComboRow {
    let mut values: Vec<String> = choices(agent).iter().map(|(v, _)| v.to_string()).collect();
    let mut labels: Vec<String> = choices(agent).iter().map(|(_, l)| l.to_string()).collect();
    let current = saved.clone().unwrap_or_default();
    if !values.contains(&current) {
        values.push(current.clone());
        labels.push(format!("{current} (current)"));
    }
    let selected = values.iter().position(|v| *v == current).unwrap_or(0);
    let list = gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>());
    let row = adw::ComboRow::builder()
        .title("Model")
        .model(&list)
        .selected(selected as u32)
        .build();
    row.connect_selected_notify({
        let window = window.clone();
        move |row| {
            let value = values
                .get(row.selected() as usize)
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
    row
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
        .title("Crash Notifications")
        .description(
            "When a program crashes, VexOS offers to have the assistant explain why. \
             These programs are muted.",
        )
        .build();
    if app.visible("ai-crash-capture").is_some() {
        group.add(&capture_switch(app, window, state.crash_capture));
    }
    if state.muted.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No programs are muted")
            .build();
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
        .subtitle("Offers to have the assistant explain a crash. Off stops the watcher.")
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
