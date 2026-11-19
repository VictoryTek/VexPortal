//! The path from a click to a running job: ask for what is missing, confirm, then run
//! — in the daemon or in the built-in terminal, whichever the catalog says.

use crate::app::App;
use crate::job::{Job, JobState};
use crate::system::ai;
use crate::ui::{arg_dialog, job_view, terminal_view, window::Window};

use adw::prelude::*;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use vexportal_catalog::validate::build_for_user;
use vexportal_catalog::{Action, Risk};

/// Start an action from a button: collect any parameters `preset` does not answer,
/// confirm, run, and show the result.
pub fn activate(app: &Rc<App>, window: &Window, id: &str, preset: HashMap<String, String>) {
    let Some(action) = app.catalog.action(id) else {
        log::error!("no catalog action `{id}`");
        return;
    };
    // Which questions Enable asks depends on the service, so it has a form of its own
    // rather than the generic one, which would list every question of every service.
    if id == "service-enable" {
        match preset
            .get("service")
            .and_then(|name| vexportal_ops::services::find(name))
        {
            Some(service) => crate::ui::service_enable::present(app, window, service),
            None => log::error!("`service-enable` needs a service"),
        }
        return;
    }
    // The attach form looks servers and shares up as it goes, which the generic one cannot.
    if id == "remote-storage-attach" {
        crate::ui::remote_attach::present(app, window);
        return;
    }
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
                } else if action.is_user() {
                    run_as_user(&app, &window, &id, args, "Done", |_| {});
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

/// Run an action that changes the user's own settings: no daemon, no terminal, no
/// administrator prompt. It is the same `vexos-ai` command a person would type, started
/// by argv as this user. A failure is a message; a success is a toast and a refreshed page.
/// `done` gets whether it worked, including `false` for a cancelled confirmation.
pub fn run_as_user(
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
    let answers: BTreeMap<String, String> = args.into_iter().collect();
    let invocation = match build_for_user(&app.catalog, id, &answers) {
        Ok(invocation) => invocation,
        Err(e) => {
            window.alert("Cannot do this", &e.to_string());
            done(false);
            return;
        }
    };
    let done = Rc::new(done);
    let title = action.title.clone();
    let success = success.to_string();
    confirm(
        window,
        action,
        {
            let window = window.clone();
            let done = done.clone();
            move || {
                glib::spawn_future_local(async move {
                    let result = ai::run_action(&invocation.action, &invocation.named).await;
                    let worked = result.is_ok();
                    match result {
                        Ok(()) => window.toast(&success),
                        Err(message) => window.alert(&title, &message),
                    }
                    done(worked);
                    // The page shows what was just changed.
                    window.state_changed();
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
