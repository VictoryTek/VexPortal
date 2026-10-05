//! Watching a daemon job.
//!
//! A dialog over whatever page started it, never a page pushed onto the navigation
//! stack: refreshing a page after the job changes the system can then never tear the
//! result away from the person reading it.

use crate::app::App;
use crate::job::{Job, JobState};
use crate::ui::window::Window;

use adw::prelude::*;
use std::rc::Rc;

pub fn present(app: &Rc<App>, window: &Window, job: &Rc<Job>) {
    // One dialog per job: a second click raises the first.
    if let Some(dialog) = job.dialog.borrow().as_ref().and_then(|d| d.upgrade()) {
        dialog.present(Some(window.root()));
        return;
    }

    let view = gtk::TextView::builder()
        .buffer(&job.log)
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .build();
    view.add_css_class("vex-log");

    let scroller = gtk::ScrolledWindow::builder()
        .child(&view)
        .vexpand(true)
        .build();
    scroller.add_css_class("vex-log-view");

    let spinner = adw::Spinner::new();
    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_hexpand(true);
    status.add_css_class("heading");

    let cancel = gtk::Button::with_label("Cancel");
    cancel.set_valign(gtk::Align::Center);

    let status_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    status_row.append(&spinner);
    status_row.append(&status);
    status_row.append(&cancel);

    let banner = adw::Banner::new("");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.set_margin_bottom(12);
    content.append(&status_row);
    content.append(&scroller);

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(&job.title, &job.command_line)));
    let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
    copy.set_tooltip_text(Some("Copy output"));
    header.pack_end(&copy);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&banner);
    toolbar.set_content(Some(&content));

    let dialog = adw::Dialog::builder()
        .title(&job.title)
        .content_width(760)
        .content_height(520)
        .child(&toolbar)
        .build();
    *job.dialog.borrow_mut() = Some(dialog.downgrade());

    let show = {
        let spinner = spinner.downgrade();
        let status = status.downgrade();
        let cancel = cancel.downgrade();
        let banner = banner.downgrade();
        move |job: &Job| {
            let (Some(spinner), Some(status), Some(cancel), Some(banner)) = (
                spinner.upgrade(),
                status.upgrade(),
                cancel.upgrade(),
                banner.upgrade(),
            ) else {
                return;
            };
            let state = job.state();
            spinner.set_visible(state.is_active());
            cancel.set_visible(state.is_active());
            cancel.set_sensitive(matches!(state, JobState::Running));
            status.set_text(&state.label());
            let problem = match &state {
                JobState::Finished(0) | JobState::Starting | JobState::Running => None,
                JobState::Finished(_) => {
                    Some("This did not complete. The output below says why.".to_string())
                }
                JobState::Declined => {
                    Some("This needs administrator authorization to run.".to_string())
                }
                JobState::NotStarted(message) => Some(message.clone()),
                JobState::External => None,
            };
            banner.set_revealed(problem.is_some());
            banner.set_title(problem.as_deref().unwrap_or(""));
        }
    };
    show(job);
    job.connect_changed(show);

    cancel.connect_clicked({
        let app = app.clone();
        let job = Rc::downgrade(job);
        move |button| {
            if let Some(job) = job.upgrade() {
                app.cancel(&job);
                button.set_sensitive(false);
            }
        }
    });

    copy.connect_clicked({
        let job = Rc::downgrade(job);
        let window = window.clone();
        move |_| {
            if let (Some(job), Some(display)) = (job.upgrade(), gtk::gdk::Display::default()) {
                display.clipboard().set_text(&job.text());
                window.toast("Output copied");
            }
        }
    });

    // Follow the output, but only while the reader is already at the bottom — yanking
    // the view back down while someone is reading earlier output is maddening.
    let follow = job.log.connect_changed({
        let view = view.downgrade();
        let scroller = scroller.downgrade();
        move |buffer| {
            let (Some(view), Some(scroller)) = (view.upgrade(), scroller.upgrade()) else {
                return;
            };
            let adjustment = scroller.vadjustment();
            if adjustment.value() + adjustment.page_size() < adjustment.upper() - 64.0 {
                return;
            }
            let mark = buffer.create_mark(None, &buffer.end_iter(), false);
            view.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
            buffer.delete_mark(&mark);
        }
    });
    dialog.connect_closed({
        let buffer = job.log.clone();
        let follow = std::cell::Cell::new(Some(follow));
        move |_| {
            if let Some(id) = follow.take() {
                buffer.disconnect(id);
            }
        }
    });

    dialog.present(Some(window.root()));
}
