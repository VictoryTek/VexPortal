//! The form that replaces `just service enable`'s questions.
//!
//! Only the questions the chosen service has are asked (the list is
//! `vexportal_ops::services::questions`, the same one the daemon checks the answers
//! against), the storage the service wants is checked for, and backups can come along.
//! Everything is then sent in one request, so the setup file is written once.

use crate::app::App;
use crate::system::services as probes;
use crate::ui::{actions, window::Window};

use adw::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_catalog::Widget;
use vexportal_ops::services::{self, Kind, Service, TierState};

/// Reads one answer out of whichever widget backs it.
enum Field {
    Switch(adw::SwitchRow),
    Text(gtk::Entry),
    Secret(gtk::PasswordEntry),
}

impl Field {
    fn value(&self) -> String {
        match self {
            Field::Switch(row) => if row.is_active() { "yes" } else { "no" }.to_string(),
            Field::Text(entry) => entry.text().trim().to_string(),
            Field::Secret(entry) => entry.text().trim().to_string(),
        }
    }
}

pub fn present(app: &Rc<App>, window: &Window, service: &'static Service) {
    if app.catalog.action("service-enable").is_none() {
        log::error!("no catalog action `service-enable`");
        return;
    }
    let (backups_on, services_text, hostname) = {
        let state = app.state.borrow();
        (
            state.service_enabled("backup"),
            state.services_text.clone(),
            state.hostname.clone().unwrap_or_default(),
        )
    };

    let page = adw::PreferencesPage::new();

    let about = adw::PreferencesGroup::builder()
        .description(glib::markup_escape_text(&format!(
            "{} Takes effect on the next rebuild.",
            service.description
        )))
        .build();
    page.add(&about);

    if let Some(tier) = service.tier {
        let row = adw::ActionRow::builder()
            .title("Storage")
            .subtitle(tier.advice(&service.name))
            .use_markup(false)
            .build();
        row.set_subtitle_lines(0);
        row.add_prefix(&gtk::Image::from_icon_name("drive-harddisk-symbolic"));
        let verdict = gtk::Label::new(Some("Checking…"));
        verdict.add_css_class("dim-label");
        verdict.set_valign(gtk::Align::Center);
        row.add_suffix(&verdict);
        let group = adw::PreferencesGroup::new();
        group.add(&row);
        page.add(&group);

        // Advice only: enabling never waits on it.
        glib::spawn_future_local(async move {
            let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
            let pools = probes::zfs_pools().await;
            verdict.set_text(match services::tier_state(tier, &mounts, &pools) {
                TierState::LocalPool => "Found on this server",
                TierState::RemoteMounted => "Remote storage is mounted",
                TierState::Missing => "Not set up yet",
            });
        });
    }

    let mut fields: Vec<(&'static str, String, bool, Field)> = Vec::new();
    let asks = services::questions(service);
    if !asks.is_empty() {
        let group = adw::PreferencesGroup::builder().title("Setup").build();
        for q in &asks {
            let title = if q.required {
                format!("{} (required)", q.label)
            } else {
                q.label.clone()
            };
            let mut help = q.help.clone();
            if q.id == "public_url" {
                if let Some(default) = services::default_url(service, &services_text, &hostname) {
                    help.push_str(&format!(" Default: {default}."));
                }
            }
            let field = match q.kind {
                Kind::Switch => {
                    let row = adw::SwitchRow::builder()
                        .title(&title)
                        .subtitle(&help)
                        .use_markup(false)
                        .active(q.default == "yes")
                        .build();
                    group.add(&row);
                    Field::Switch(row)
                }
                Kind::Text => {
                    let entry = gtk::Entry::builder()
                        .text(&q.default)
                        .hexpand(true)
                        .width_chars(26)
                        .valign(gtk::Align::Center)
                        .build();
                    group.add(&entry_row(&title, &help, &entry));
                    Field::Text(entry)
                }
                Kind::Secret => {
                    let entry = gtk::PasswordEntry::builder()
                        .show_peek_icon(true)
                        .hexpand(true)
                        .width_chars(26)
                        .valign(gtk::Align::Center)
                        .build();
                    group.add(&entry_row(&title, &help, &entry));
                    Field::Secret(entry)
                }
            };
            fields.push((q.id, q.label.clone(), q.required, field));
        }
        page.add(&group);
    }

    let with_backup = (service.name != "backup" && !backups_on).then(|| {
        let row = adw::SwitchRow::builder()
            .title("Also turn on backups")
            .subtitle(
                "Daily restic backups of this service's data to /var/lib/vexos-backup/restic-repo, with a generated password.",
            )
            .use_markup(false)
            .active(true)
            .build();
        let group = adw::PreferencesGroup::new();
        group.add(&row);
        page.add(&group);
        row
    });

    let dialog = adw::Dialog::builder()
        .title(format!("Enable {}", service.name))
        .content_width(560)
        .build();
    let header = adw::HeaderBar::new();
    let cancel = gtk::Button::with_label("Cancel");
    let enable = gtk::Button::with_label("Enable");
    enable.add_css_class("suggested-action");
    header.pack_start(&cancel);
    header.pack_end(&enable);
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
    enable.connect_clicked({
        let app = app.clone();
        let window = window.clone();
        let dialog = dialog.clone();
        move |_| {
            let mut args: HashMap<String, String> = HashMap::new();
            args.insert("service".into(), service.name.clone());
            for (id, label, required, field) in fields.iter() {
                let value = field.value();
                if value.is_empty() {
                    if *required {
                        window.alert("Missing information", &format!("`{label}` is required."));
                        return;
                    }
                    continue;
                }
                if let Some(problem) = problem_with(&app, id, &value) {
                    window.alert("Check the answer", &format!("{label}: {problem}"));
                    return;
                }
                args.insert((*id).to_string(), value);
            }
            if let Some(row) = &with_backup {
                args.insert(
                    "with_backup".into(),
                    if row.is_active() { "yes" } else { "no" }.into(),
                );
            }

            let Some(action) = app.catalog.action("service-enable") else {
                return;
            };
            dialog.close();
            actions::proceed(&app, &window, action, args);
        }
    });

    dialog.present(Some(window.root()));
}

fn entry_row(title: &str, help: &str, entry: &impl IsA<gtk::Widget>) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(help)
        .use_markup(false)
        .build();
    row.set_subtitle_lines(0);
    row.add_suffix(entry);
    row
}

/// Why `value` cannot be the answer `id`, by the same format the daemon will check, so
/// the mistake is shown here beside the field rather than as a failed job.
fn problem_with(app: &App, id: &str, value: &str) -> Option<String> {
    let action = app.catalog.action("service-enable")?;
    let param = action.params.iter().find(|p| p.name == id)?;
    match &param.widget {
        Widget::Text { format } => format.validate(value).err().map(|e| e.to_string()),
        _ => None,
    }
}
