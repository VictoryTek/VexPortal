//! The path from a click to a running job: ask for what is missing, confirm, then run
//! — in the daemon or in the built-in terminal, whichever the catalog says.

use crate::app::App;
use crate::job::{Job, JobState};
use crate::ui::{arg_dialog, job_view, terminal_view, window::Window};

use adw::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_catalog::{Action, Risk};

/// Start an action from a button: collect any parameters `preset` does not answer,
/// confirm, run, and show the result.
pub fn activate(app: &Rc<App>, window: &Window, id: &str, preset: HashMap<String, String>) {
    let Some(action) = app.catalog.action(id) else {
        log::error!("no catalog action `{id}`");
        return;
    };
    if action.params.iter().any(|p| !preset.contains_key(&p.name)) {
        arg_dialog::present(app, window, action, preset);
    } else {
        proceed(app, window, action, preset);
    }
}

/// Confirm if the catalog asks for it, then start and show the job.
pub fn proceed(app: &Rc<App>, window: &Window, action: &Action, args: HashMap<String, String>) {
    let id = action.id.clone();
    confirm(
        window,
        action,
        {
            let app = app.clone();
            let window = window.clone();
            move || {
                let Some(action) = app.catalog.action(&id) else {
                    return;
                };
                if action.is_terminal() {
                    terminal_view::open(&app, &window, action, args);
                } else {
                    let job = app.run(action, args);
                    job_view::present(&app, &window, &job);
                }
            }
        },
        || {},
    );
}

/// Run a daemon action behind a control — a switch, a combo — without opening a log.
/// The control shows progress itself; the outcome arrives as a toast, with the log one
/// click away. `done` gets whether it succeeded, including `false` for a cancelled
/// confirmation, so the control can put itself back.
pub fn run_quietly(
    app: &Rc<App>,
    window: &Window,
    id: &str,
    args: HashMap<String, String>,
    success: &str,
    done: impl Fn(bool) + 'static,
) {
    let Some(action) = app.catalog.action(id) else {
        done(false);
        return;
    };
    let done = Rc::new(done);
    let success = success.to_string();
    let id = action.id.clone();
    confirm(
        window,
        action,
        {
            let app = app.clone();
            let window = window.clone();
            let done = done.clone();
            move || {
                let Some(action) = app.catalog.action(&id) else {
                    return;
                };
                let job = app.run(action, args);
                let weak_job = Rc::downgrade(&job);
                let app = Rc::downgrade(&app);
                let window = window.clone();
                let done = done.clone();
                let success = success.clone();
                job.connect_changed(move |job| {
                    let state = job.state();
                    if state.is_active() {
                        return;
                    }
                    done(state.succeeded());
                    let (Some(app), Some(job)) = (app.upgrade(), weak_job.upgrade()) else {
                        return;
                    };
                    let message = match &state {
                        JobState::Finished(0) => success.clone(),
                        JobState::Declined => format!("{}: not authorized", job.title),
                        other => format!("{} — {}", job.title, other.label().to_lowercase()),
                    };
                    toast_with_details(&app, &window, &message, &job, state.succeeded());
                });
            }
        },
        {
            let done = done.clone();
            move || done(false)
        },
    );
}

fn toast_with_details(app: &Rc<App>, window: &Window, message: &str, job: &Rc<Job>, quiet: bool) {
    let toast = adw::Toast::new(message);
    if !quiet {
        toast.set_button_label(Some("Details"));
        toast.set_timeout(8);
        let app = app.clone();
        let window = window.clone();
        let job = job.clone();
        toast.connect_button_clicked(move |_| job_view::present(&app, &window, &job));
    }
    window.add_toast(toast);
}

/// Ask before anything with a `confirm` message — every destructive action has one,
/// in either lane — and run `yes` or `no` with the answer.
fn confirm(
    window: &Window,
    action: &Action,
    yes: impl FnOnce() + 'static,
    no: impl FnOnce() + 'static,
) {
    let Some(body) = action.confirm.clone() else {
        yes();
        return;
    };
    // A response arrives once, but the handler has to be `Fn`.
    let yes = RefCell::new(Some(yes));
    let no = RefCell::new(Some(no));

    let dialog = adw::AlertDialog::new(Some(&format!("{}?", action.title)), Some(&body));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("run", &action.title);
    dialog.set_response_appearance(
        "run",
        if action.risk == Risk::Destructive {
            adw::ResponseAppearance::Destructive
        } else {
            adw::ResponseAppearance::Suggested
        },
    );
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, response| {
        if response == "run" {
            if let Some(yes) = yes.borrow_mut().take() {
                yes();
            }
        } else if let Some(no) = no.borrow_mut().take() {
            no();
        }
    });
    dialog.present(Some(window.root()));
}
