//! Application state, and the wiring between the GUI and the daemon.

use crate::dbus_client::{Client, Event};
use crate::job::{Job, Router};
use crate::just::JustfileFacts;
use crate::system::{ai, SystemState, Variant};
use crate::ui::{pages, window::Window};

use gtk::prelude::ApplicationExt;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_catalog::{Action, Catalog, Page, Role};

/// Everything the widgets need, shared by clone.
pub struct App {
    pub catalog: Catalog,
    pub facts: JustfileFacts,
    /// `None` when this machine is not a built VexOS host; the window then explains
    /// that instead of showing a portal that could not work.
    pub variant: Option<Variant>,
    pub state: RefCell<SystemState>,
    pub client: Client,
    router: RefCell<Router<Rc<Job>>>,
    /// Every job started this session, oldest first.
    jobs: RefCell<Vec<Rc<Job>>>,
    next_request_id: Cell<u64>,
    /// Whether the AI assistant was waiting for a reboot when last checked, so the
    /// prompt comes once, when a rebuild installs it.
    ai_reboot: Cell<bool>,
    pub window: RefCell<Option<Window>>,
}

impl App {
    /// Which role's actions to show. A machine that has never been built shows the
    /// desktop set, so the window is populated behind the "not built yet" notice
    /// rather than empty.
    pub fn role(&self) -> Role {
        self.variant.as_ref().map_or(Role::Desktop, |v| v.role)
    }

    /// An action, if it applies to this role and this host's justfile has it.
    pub fn visible(&self, id: &str) -> Option<&Action> {
        self.catalog
            .action(id)
            .filter(|a| a.applies_to(self.role()) && self.facts.is_available(a))
    }

    /// Actions to show on a page, in catalog order.
    pub fn visible_on(&self, page: &str) -> Vec<&Action> {
        self.catalog
            .on_page(page, self.role())
            .into_iter()
            .filter(|a| self.facts.is_available(a))
            .collect()
    }

    /// Sidebar entries: pages with at least one visible action. The AI Assistant page
    /// also needs the `ai` feature on in features.nix: its actions only work once the
    /// feature is built in.
    pub fn visible_pages(&self) -> Vec<&Page> {
        self.catalog
            .pages
            .iter()
            .filter(|p| p.id != "ai" || self.ai_enabled())
            .filter(|p| !self.visible_on(&p.id).is_empty())
            .collect()
    }

    /// Whether `ai` is on in features.nix, explicitly or by the module default.
    pub fn ai_enabled(&self) -> bool {
        self.state.borrow().feature("ai").is_some_and(|s| s.enabled)
    }

    pub fn refresh_state(&self) {
        *self.state.borrow_mut() = SystemState::read();
    }

    /// True once, when the AI assistant has just been built in and is waiting for a
    /// reboot to start its services.
    fn ai_reboot_became_needed(&self) -> bool {
        let now = self.ai_enabled() && ai::needs_reboot();
        !self.ai_reboot.replace(now) && now
    }

    pub fn jobs(&self) -> Vec<Rc<Job>> {
        self.jobs.borrow().clone()
    }

    /// Register a job so the Activity page and the sidebar follow it.
    pub fn track(self: &Rc<Self>, job: &Rc<Job>) {
        self.jobs.borrow_mut().push(job.clone());
        let app = Rc::downgrade(self);
        job.connect_changed(move |job| {
            let Some(app) = app.upgrade() else { return };
            let finished = !job.state().is_active();
            if finished {
                // A job that changed the variant, the generation, a feature or a
                // service has just invalidated whatever page is showing.
                app.refresh_state();
            }
            // Cloned out so the borrow ends before any page is rebuilt.
            let window = app.window.borrow().clone();
            if let Some(window) = window {
                window.jobs_changed(finished);
                if finished && app.ai_reboot_became_needed() {
                    pages::offer_ai_reboot(&app, &window);
                }
            }
        });
        if let Some(window) = self.window.borrow().as_ref() {
            window.jobs_changed(false);
        }
    }

    /// Start a daemon action. The answers have been validated against the catalog by
    /// the caller's form; the daemon validates them again regardless.
    pub fn run(self: &Rc<Self>, action: &Action, args: HashMap<String, String>) -> Rc<Job> {
        let request_id = self.next_request_id.get() + 1;
        self.next_request_id.set(request_id);

        let mut command_line = action.command_line();
        for param in &action.params {
            if let Some(value) = args.get(&param.name) {
                command_line.push(' ');
                command_line.push_str(if param.is_secret() {
                    "••••"
                } else {
                    value
                });
            }
        }

        let job = Job::new(&action.title, command_line, false);
        self.track(&job);
        self.router.borrow_mut().expect(request_id, job.clone());
        self.client.run(request_id, &action.id, args);
        job
    }

    pub fn cancel(&self, job: &Job) {
        if let Some(job_id) = job.job_id() {
            self.client.cancel(&job_id);
        }
    }

    fn handle(&self, event: Event) {
        // Route first and release the router before touching any job: a job's
        // listeners can start another job, which needs the router again.
        let deliveries = self.router.borrow_mut().route(event);
        for (job, event) in deliveries {
            job.apply(&event);
        }
    }
}

thread_local! {
    /// The running app, so a second activation and the `show-page` action reach the
    /// window that already exists instead of building another.
    static CURRENT: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// The sidebar page to open for a requested id: only a page the sidebar lists. A
/// hidden or unknown id (the AI page with the feature off) gets `None`.
fn resolve_page<'a>(requested: &str, visible: &[&'a str]) -> Option<&'a str> {
    visible.iter().copied().find(|id| *id == requested)
}

/// Raise the window and, when `requested` is given, switch to that page. An id that is
/// not a visible page opens the Overview instead. The id only picks a sidebar entry;
/// it never reaches the daemon or a command. Both `--page` and the `show-page` action
/// come through here.
pub fn show_page(requested: Option<&str>) {
    let Some(app) = CURRENT.with(|c| c.borrow().clone()) else {
        return;
    };
    let window = app.window.borrow().clone();
    let Some(window) = window else { return };
    if let Some(requested) = requested {
        let pages = app.visible_pages();
        let ids: Vec<&str> = pages.iter().map(|p| p.id.as_str()).collect();
        match resolve_page(requested, &ids) {
            Some(id) => window.show(id),
            None => {
                log::warn!("ignoring unknown or hidden page {requested:?}; opening the Overview");
                window.show(pages::OVERVIEW);
            }
        }
    }
    window.present();
}

pub fn build(application: &adw::Application) {
    // Activated again while running: raise the window rather than build a second one.
    if CURRENT.with(|c| c.borrow().is_some()) {
        show_page(None);
        return;
    }

    // A Tailscale sign-in still waiting in the browser would otherwise leave its
    // `tailscale up` running after the last window closes.
    application.connect_shutdown(|_| pages::stop_tailscale_sign_in());

    let catalog = match Catalog::load() {
        Ok(catalog) => catalog,
        Err(e) => {
            // The catalog is compiled in, so this is a build-time defect rather than
            // anything the user can act on.
            log::error!("the built-in catalog is broken: {e}");
            eprintln!("VexPortal is built with an invalid catalog: {e}");
            return;
        }
    };

    let facts = JustfileFacts::read(&catalog);
    let variant = match Variant::detect() {
        Ok(variant) => Some(variant),
        Err(e) => {
            log::warn!("could not determine this host's variant: {e}");
            None
        }
    };

    let app = Rc::new(App {
        catalog,
        facts,
        variant,
        state: RefCell::new(SystemState::read()),
        client: Client::start(),
        router: RefCell::new(Router::default()),
        jobs: RefCell::new(Vec::new()),
        next_request_id: Cell::new(0),
        // Already pending at startup is the AI page's row to show, not a prompt.
        ai_reboot: Cell::new(ai::needs_reboot()),
        window: RefCell::new(None),
    });

    let window = Window::new(application, &app);
    *app.window.borrow_mut() = Some(window.clone());

    // Daemon events arrive on a channel from the D-Bus thread; this is the only place
    // they reach a widget, and it runs on the main loop.
    let events = app.client.events.clone();
    glib::spawn_future_local({
        let app = app.clone();
        async move {
            while let Ok(event) = events.recv().await {
                app.handle(event);
            }
        }
    });

    CURRENT.with(|c| *c.borrow_mut() = Some(app));
    window.present();
}

#[cfg(test)]
mod tests {
    use super::resolve_page;

    #[test]
    fn a_visible_page_resolves_to_itself() {
        assert_eq!(resolve_page("ai", &["system", "ai"]), Some("ai"));
    }

    #[test]
    fn an_unknown_id_does_not_resolve() {
        assert_eq!(resolve_page("nope", &["system", "ai"]), None);
    }

    #[test]
    fn a_hidden_ai_page_does_not_resolve() {
        assert_eq!(resolve_page("ai", &["system", "network"]), None);
    }

    #[test]
    fn an_empty_id_does_not_resolve() {
        assert_eq!(resolve_page("", &["system", "ai"]), None);
    }
}
