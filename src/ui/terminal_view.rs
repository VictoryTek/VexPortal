//! Running an interactive action in VexPortal's own terminal.
//!
//! The recipe runs as the logged-in user, exactly as if typed into a terminal: it can
//! ask its questions, `sudo` can ask for a password, and a wizard can draw its menus.
//! The command is a validated argv exec'd directly — see [`crate::terminal`].

use crate::app::App;
use crate::job::{Job, JobState};
use crate::terminal;
use crate::ui::window::Window;

use adw::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use vexportal_catalog::validate::{build_for_terminal, Invocation};
use vexportal_catalog::Action;
use vte4::prelude::*;

pub fn open(app: &Rc<App>, window: &Window, action: &Action, args: HashMap<String, String>) {
    let answers: BTreeMap<String, String> = args.into_iter().collect();
    let invocation = match build_for_terminal(&app.catalog, &action.id, &answers) {
        Ok(invocation) => invocation,
        Err(e) => {
            window.alert("Cannot start this", &e.to_string());
            return;
        }
    };

    let command_line = invocation.audit_line();
    let job = Job::new(&action.title, command_line.clone(), true);
    app.track(&job);

    let term = vte4::Terminal::new();
    term.set_hexpand(true);
    term.set_vexpand(true);
    term.set_scrollback_lines(10_000);
    term.set_scroll_on_output(true);
    term.add_css_class("vex-terminal");

    let status = gtk::Label::new(Some(
        "Answer any questions in the terminal. Administrator steps ask for your password here.",
    ));
    status.set_xalign(0.0);
    status.set_hexpand(true);
    status.set_wrap(true);
    status.add_css_class("dim-label");

    let close = gtk::Button::with_label("Close");
    close.set_valign(gtk::Align::Center);
    close.set_visible(false);
    close.add_css_class("suggested-action");

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    footer.set_margin_start(12);
    footer.set_margin_end(12);
    footer.set_margin_top(6);
    footer.set_margin_bottom(12);
    footer.append(&status);
    footer.append(&close);

    let frame = gtk::Frame::new(None);
    frame.set_child(Some(&term));
    frame.set_margin_start(12);
    frame.set_margin_end(12);
    frame.add_css_class("vex-terminal-frame");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&frame);
    content.append(&footer);

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(&action.title, &command_line)));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));

    let dialog = adw::Dialog::builder()
        .title(&action.title)
        .content_width(860)
        .content_height(560)
        .child(&toolbar)
        .build();
    *job.dialog.borrow_mut() = Some(dialog.downgrade());

    // Closing while the recipe still runs would hang up on it half-way through a
    // wizard, so ask first.
    dialog.set_can_close(false);
    dialog.connect_close_attempt({
        let window = window.clone();
        let job = Rc::downgrade(&job);
        move |dialog| {
            let running = job.upgrade().is_some_and(|j| j.state().is_active());
            if !running {
                dialog.force_close();
                return;
            }
            let ask = adw::AlertDialog::new(
                Some("Stop this operation?"),
                Some("It is still running. Closing the terminal stops it where it is."),
            );
            ask.add_response("keep", "Keep Running");
            ask.add_response("stop", "Stop and Close");
            ask.set_response_appearance("stop", adw::ResponseAppearance::Destructive);
            ask.set_default_response(Some("keep"));
            ask.set_close_response("keep");
            let dialog = dialog.clone();
            ask.connect_response(None, move |_, response| {
                if response == "stop" {
                    dialog.force_close();
                }
            });
            ask.present(Some(window.root()));
        }
    });
    dialog.connect_closed({
        let job = Rc::downgrade(&job);
        move |_| {
            if let Some(job) = job.upgrade() {
                if job.state().is_active() {
                    job.set_state(JobState::Finished(-1));
                }
            }
        }
    });
    close.connect_clicked({
        let dialog = dialog.downgrade();
        move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.force_close();
            }
        }
    });

    term.connect_child_exited({
        let job = Rc::downgrade(&job);
        let status = status.downgrade();
        let close = close.downgrade();
        move |_, wait_status| {
            let code = terminal::exit_code(wait_status);
            if let Some(job) = job.upgrade() {
                job.set_state(JobState::Finished(code));
            }
            if let (Some(status), Some(close)) = (status.upgrade(), close.upgrade()) {
                status.set_text(&if code == 0 {
                    "Finished.".to_string()
                } else {
                    format!("Stopped with exit code {code}. The output above says why.")
                });
                close.set_visible(true);
                close.grab_focus();
            }
        }
    });

    dialog.present(Some(window.root()));
    spawn(window, &term, &dialog, &job, &invocation);
    term.grab_focus();
}

fn spawn(
    window: &Window,
    term: &vte4::Terminal,
    dialog: &adw::Dialog,
    job: &Rc<Job>,
    invocation: &Invocation,
) {
    let argv = terminal::just_argv(invocation);
    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    // Added to the user's own environment, not replacing it: the recipe needs the
    // session's PATH, HOME and SSH agent. VEXOS_ASSUME_YES is deliberately absent —
    // a person is here to answer.
    let envv = ["VEXPORTAL=1"];

    let fallback = {
        let window = window.clone();
        let dialog = dialog.downgrade();
        let title = job.title.clone();
        let job = Rc::downgrade(job);
        let invocation = invocation.clone();
        move |error: glib::Error| {
            log::warn!("built-in terminal failed to start: {error}");
            let Some(job) = job.upgrade() else { return };
            if let Some(dialog) = dialog.upgrade() {
                dialog.force_close();
            }
            let argv = terminal::external_argv(&title, &invocation);
            match std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .current_dir(terminal::WORKING_DIRECTORY)
                .spawn()
            {
                Ok(_) => {
                    job.set_state(JobState::External);
                    window.toast(&format!("Opened {title} in a terminal window"));
                }
                Err(e) => {
                    job.set_state(JobState::NotStarted(e.to_string()));
                    window.alert(
                        "No terminal available",
                        &format!(
                            "VexPortal could not open a terminal ({error}; {e}). Run this \
                             from a shell instead:\n\n{}",
                            job.command_line
                        ),
                    );
                }
            }
        }
    };

    term.spawn_async(
        vte4::PtyFlags::DEFAULT,
        Some(terminal::WORKING_DIRECTORY),
        &argv_refs,
        &envv,
        glib::SpawnFlags::SEARCH_PATH,
        || {},
        -1,
        gio::Cancellable::NONE,
        move |result| {
            if let Err(error) = result {
                fallback(error);
            }
        },
    );
}
