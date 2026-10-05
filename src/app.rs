//! Application state, and the wiring between the GUI and the daemon.

use crate::dbus_client::{Client, Event};
use crate::job::{Job, Router};
use crate::just::JustfileFacts;
use crate::system::{SystemState, Variant};
use crate::ui::window::Window;

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

    /// Sidebar entries: pages with at least one visible action.
    pub fn visible_pages(&self) -> Vec<&Page> {
        self.catalog
            .pages
            .iter()
            .filter(|p| !self.visible_on(&p.id).is_empty())
            .collect()
    }

    pub fn refresh_state(&self) {
        *self.state.borrow_mut() = SystemState::read();
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
            if !job.state().is_active() {
                // A job that changed the variant, the generation, a feature or a
                // service has just invalidated whatever page is showing.
                app.refresh_state();
            }
            // Cloned out so the borrow ends before any page is rebuilt.
            let window = app.window.borrow().clone();
            if let Some(window) = window {
                window.jobs_changed(!job.state().is_active());
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

pub fn build(application: &adw::Application) {
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

    window.present();
}
