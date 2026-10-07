//! The AI assistant: setup, Claude accounts and usage, and crash-notification mutes.
//!
//! The state comes from `vexos-ai status --json`; every change goes
//! through a `just ai-*` recipe in the terminal, as the user.

use crate::app::App;
use crate::system::ai::{self, Agent, AiState};
use crate::ui::{self, actions, job_view, terminal_view, window::Window};

use adw::prelude::*;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_catalog::Format;

/// Rendered by this page's own controls rather than as rows.
const MANAGED: [&str; 6] = [
    "ai-pick",
    "ai-account-add",
    "ai-account-use",
    "ai-account-remove",
    "ai-account-mode",
    "ai-crash-mute",
];

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
    match state.agent {
        None if app.visible("ai-pick").is_some() => page.add(&setup(app, window)),
        None => {}
        Some(agent) => page.add(&current(app, window, agent)),
    }
    // Both start `vexos-ai`, which falls back to its own zenity picker while no
    // assistant is chosen; the setup group above replaces that.
    let mut skip = MANAGED.to_vec();
    if state.agent.is_none() && app.visible("ai-pick").is_some() {
        skip.extend(["agent", "diagnose"]);
    }
    for group in ui::action_groups(app, window, "ai", &skip) {
        page.add(&group);
    }
    if state.agent == Some(Agent::Claude) {
        if let Some(group) = accounts(app, window, &state) {
            page.add(&group);
        }
        if let Some(group) = switching(app, window, &state) {
            page.add(&group);
        }
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
            "The AI Assistant is installed. Its crash notifications, usage warnings and \
             theme sync start after a reboot.",
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
            "The AI Assistant is installed. Its crash notifications, usage warnings and theme \
             sync start after a reboot. Unsaved work in other applications is lost.",
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

/// First run: no assistant chosen yet.
fn setup(app: &Rc<App>, window: &Window) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Set up your assistant")
        .description("Choose the coding agent VexOS opens. You can change this later.")
        .build();
    for agent in Agent::ALL {
        let row = adw::ActionRow::builder()
            .title(agent.title())
            .subtitle(agent.description())
            .build();
        row.set_subtitle_lines(0);
        let button = gtk::Button::with_label("Choose");
        button.set_valign(gtk::Align::Center);
        if agent == Agent::Claude {
            button.add_css_class("suggested-action");
        }
        button.connect_clicked({
            let app = app.clone();
            let window = window.clone();
            move |_| pick(&app, &window, agent)
        });
        row.add_suffix(&button);
        row.set_activatable_widget(Some(&button));
        group.add(&row);
    }
    group
}

/// The chosen assistant, with Change and Open.
fn current(app: &Rc<App>, window: &Window, agent: Agent) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Your Assistant")
        .build();
    let row = adw::ActionRow::builder()
        .title(agent.title())
        .subtitle(agent.description())
        .build();
    row.set_subtitle_lines(0);
    row.add_prefix(&gtk::Image::from_icon_name("user-available-symbolic"));

    if app.visible("ai-pick").is_some() {
        let change = gtk::Button::with_label("Change…");
        change.set_valign(gtk::Align::Center);
        change.connect_clicked({
            let app = app.clone();
            let window = window.clone();
            move |_| change_dialog(&app, &window, agent)
        });
        row.add_suffix(&change);
    }
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
    group.add(&row);
    group
}

fn change_dialog(app: &Rc<App>, window: &Window, current: Agent) {
    let dialog = adw::AlertDialog::new(
        Some("Change assistant?"),
        Some(&format!(
            "You are using {}. Your sign-ins are kept either way.",
            current.title()
        )),
    );
    dialog.add_response("cancel", "Cancel");
    for agent in Agent::ALL {
        dialog.add_response(agent.id(), agent.title());
        dialog.set_response_enabled(agent.id(), agent != current);
    }
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, {
        let app = app.clone();
        let window = window.clone();
        move |_, response| {
            if let Some(agent) = Agent::ALL.into_iter().find(|a| a.id() == response) {
                pick(&app, &window, agent);
            }
        }
    });
    dialog.present(Some(window.root()));
}

/// Run `ai-pick`, and once it has saved the choice, offer to open the assistant.
fn pick(app: &Rc<App>, window: &Window, agent: Agent) {
    let Some(action) = app.catalog.action("ai-pick") else {
        return;
    };
    let mut args = HashMap::new();
    args.insert("agent".to_string(), agent.id().to_string());
    let Some(job) = terminal_view::open(app, window, action, args) else {
        return;
    };

    let offered = Cell::new(false);
    let app = Rc::downgrade(app);
    let window = window.clone();
    job.connect_changed(move |job| {
        if !job.state().succeeded() || offered.replace(true) {
            return;
        }
        let Some(app) = app.upgrade() else { return };
        // Nothing worth reading in its output: close it and ask instead.
        let dialog = job.dialog.borrow().as_ref().and_then(|d| d.upgrade());
        if let Some(dialog) = dialog {
            dialog.force_close();
        }
        if app.visible("agent").is_none() {
            return;
        }
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
    });
}

fn accounts(app: &Rc<App>, window: &Window, state: &AiState) -> Option<adw::PreferencesGroup> {
    app.visible("ai-account-use")?;
    let group = adw::PreferencesGroup::builder()
        .title("Claude Accounts")
        .description("New Claude Code sessions use the active account. Usage is as last checked.")
        .build();

    if app.visible("ai-account-add").is_some() {
        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add Claude Account")
            .valign(gtk::Align::Center)
            .build();
        add.add_css_class("flat");
        add.connect_clicked({
            let app = app.clone();
            let window = window.clone();
            move |_| actions::activate(&app, &window, "ai-account-add", HashMap::new())
        });
        group.set_header_suffix(Some(&add));
    }

    let can_remove = app.visible("ai-account-remove").is_some();
    for account in &state.accounts {
        let row = adw::ActionRow::builder()
            .title(&account.label)
            .subtitle(account.subtitle())
            .use_markup(false)
            .build();
        row.set_subtitle_lines(0);
        row.add_prefix(&gtk::Image::from_icon_name("avatar-default-symbolic"));

        if account.label == state.active {
            row.add_suffix(&ui::pill("Active", "safe", "New sessions use this account"));
        } else {
            let use_ = gtk::Button::with_label("Use");
            use_.set_valign(gtk::Align::Center);
            use_.connect_clicked({
                let app = app.clone();
                let window = window.clone();
                let label = account.label.clone();
                move |_| {
                    let args = HashMap::from([("target".to_string(), label.clone())]);
                    actions::activate(&app, &window, "ai-account-use", args)
                }
            });
            row.add_suffix(&use_);
        }
        // `main` is Claude's own default sign-in, which vexos-ai never removes.
        if can_remove && account.label != "main" {
            let remove = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Remove this account")
                .valign(gtk::Align::Center)
                .build();
            remove.add_css_class("flat");
            remove.connect_clicked({
                let app = app.clone();
                let window = window.clone();
                let label = account.label.clone();
                move |_| {
                    let args = HashMap::from([("label".to_string(), label.clone())]);
                    actions::activate(&app, &window, "ai-account-remove", args)
                }
            });
            row.add_suffix(&remove);
        }
        group.add(&row);
    }
    Some(group)
}

/// Warn only, or switch accounts too, near a threshold. Applied together, since the
/// recipe takes both.
fn switching(app: &Rc<App>, window: &Window, state: &AiState) -> Option<adw::PreferencesGroup> {
    app.visible("ai-account-mode")?;
    let group = adw::PreferencesGroup::builder()
        .title("Near a Limit")
        .description("What happens when the active account nears its session or weekly limit.")
        .build();

    let mode = adw::ComboRow::builder()
        .title("When an account nears its limit")
        .model(&gtk::StringList::new(&[
            "Notify me",
            "Switch to the account with the most headroom",
        ]))
        .selected(u32::from(state.auto_switch))
        .build();
    let threshold = adw::SpinRow::builder()
        .title("Threshold")
        .subtitle("Percent of the limit")
        .adjustment(&gtk::Adjustment::new(
            f64::from(state.threshold),
            1.0,
            100.0,
            1.0,
            5.0,
            0.0,
        ))
        .build();

    let apply = gtk::Button::with_label("Apply");
    apply.set_valign(gtk::Align::Center);
    apply.add_css_class("suggested-action");
    apply.set_sensitive(false);
    group.set_header_suffix(Some(&apply));

    let saved = (state.auto_switch, state.threshold);
    let current = {
        let mode = mode.downgrade();
        let threshold = threshold.downgrade();
        move || -> Option<(bool, u8)> {
            Some((
                mode.upgrade()?.selected() == 1,
                threshold.upgrade()?.value() as u8,
            ))
        }
    };
    let current = Rc::new(current);
    let update = {
        let apply = apply.downgrade();
        let current = current.clone();
        move || {
            if let (Some(apply), Some(now)) = (apply.upgrade(), current()) {
                apply.set_sensitive(now != saved);
            }
        }
    };
    let update = Rc::new(update);
    mode.connect_selected_notify({
        let update = update.clone();
        move |_| update()
    });
    threshold.connect_value_notify(move |_| update());

    apply.connect_clicked({
        let app = app.clone();
        let window = window.clone();
        move |_| {
            let Some((auto, pct)) = current() else { return };
            let args = HashMap::from([
                (
                    "mode".to_string(),
                    if auto { "auto" } else { "manual" }.to_string(),
                ),
                ("pct".to_string(), pct.to_string()),
            ]);
            actions::activate(&app, &window, "ai-account-mode", args);
        }
    });

    group.add(&mode);
    group.add(&threshold);
    Some(group)
}

fn crash_mutes(app: &Rc<App>, window: &Window, state: &AiState) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Crash Notifications")
        .description(
            "When a program crashes, VexOS offers to have the assistant explain why. \
             These programs are muted.",
        )
        .build();
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
