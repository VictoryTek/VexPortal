//! Everything started this session, newest first.

use crate::app::App;
use crate::job::JobState;
use crate::ui::{self, job_view, window::Window};

use adw::prelude::*;
use std::rc::Rc;

pub fn build(app: &Rc<App>, window: &Window) -> adw::NavigationPage {
    let jobs = app.jobs();

    let content: gtk::Widget = if jobs.is_empty() {
        adw::StatusPage::builder()
            .icon_name("view-list-bullet-symbolic")
            .title("Nothing Run Yet")
            .description("Operations you start appear here, with their output.")
            .build()
            .upcast()
    } else {
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::builder()
            .title("This Session")
            .build();
        for job in jobs.iter().rev() {
            let state = job.state();
            let age = job.started_at.elapsed().as_secs();
            let when = match age {
                0..=59 => "just now".to_string(),
                60..=3599 => format!("{} min ago", age / 60),
                _ => format!("{} h ago", age / 3600),
            };
            let row = adw::ActionRow::builder()
                .title(&job.title)
                .subtitle(format!("{} · {when}", job.command_line))
                .use_markup(false)
                .activatable(true)
                .build();

            let prefix: gtk::Widget = if state.is_active() {
                adw::Spinner::new().upcast()
            } else {
                let icon = gtk::Image::from_icon_name(match state {
                    JobState::Finished(0) | JobState::External => "object-select-symbolic",
                    JobState::Declined => "action-unavailable-symbolic",
                    _ => "dialog-error-symbolic",
                });
                icon.add_css_class(if state.succeeded() || state == JobState::External {
                    "success"
                } else {
                    "error"
                });
                icon.upcast()
            };
            row.add_prefix(&prefix);
            row.add_suffix(&ui::pill(&state.label(), "", ""));

            // A terminal job can only be reopened while its terminal still exists.
            let reopenable = !job.terminal
                || job
                    .dialog
                    .borrow()
                    .as_ref()
                    .and_then(|d| d.upgrade())
                    .is_some();
            if reopenable {
                row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                row.connect_activated({
                    let app = app.clone();
                    let window = window.clone();
                    let job = job.clone();
                    move |_| job_view::present(&app, &window, &job)
                });
            } else {
                row.set_activatable(false);
            }
            group.add(&row);
        }
        page.add(&group);
        page.upcast()
    };

    ui::page(app, window, "Activity", &content)
}
