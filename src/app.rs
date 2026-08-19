//! Application state, and the wiring between the GUI and the daemon.

use crate::dbus_client::{self, Client, Event};
use crate::just::JustfileFacts;
use crate::system::{SystemState, Variant};
use crate::ui::run_page::RunPage;
use crate::ui::window::Window;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_catalog::{Catalog, Category, Recipe, Role};

/// Everything the widgets need, shared by clone.
pub struct App {
    pub catalog: Catalog,
    pub facts: JustfileFacts,
    /// `None` when this machine is not a built VexOS host; the window then explains
    /// that instead of showing a portal that could not work.
    pub variant: Option<Variant>,
    pub state: RefCell<SystemState>,
    pub client: Client,
    /// Run views waiting for the daemon to accept their request.
    pending: RefCell<HashMap<u64, RunPage>>,
    /// Run views attached to a job id.
    running: RefCell<HashMap<String, RunPage>>,
    next_request_id: RefCell<u64>,
    pub window: RefCell<Option<Window>>,
}

impl App {
    /// Which role's recipes to show. A machine that has never been built shows the
    /// desktop set, so the window is populated behind the "not built yet" notice
    /// rather than empty.
    pub fn role(&self) -> Role {
        self.variant.as_ref().map_or(Role::Desktop, |v| v.role)
    }

    /// Recipes to show in a category: the right role, and actually present in this
    /// host's justfile.
    pub fn visible_in(&self, category_id: &str) -> Vec<&Recipe> {
        self.catalog
            .in_category(category_id, self.role())
            .into_iter()
            .filter(|r| self.facts.is_available(&r.name))
            .collect()
    }

    /// Sidebar entries: categories with at least one visible recipe.
    pub fn visible_categories(&self) -> Vec<&Category> {
        self.catalog
            .categories
            .iter()
            .filter(|c| !self.visible_in(&c.id).is_empty())
            .collect()
    }

    pub fn refresh_state(&self) {
        *self.state.borrow_mut() = SystemState::read();
    }

    /// Start a recipe and attach `page` to the result.
    pub fn run(self: &Rc<Self>, recipe: &Recipe, args: HashMap<String, String>, page: RunPage) {
        let request_id = {
            let mut next = self.next_request_id.borrow_mut();
            *next += 1;
            *next
        };
        self.pending.borrow_mut().insert(request_id, page);
        self.client.run(request_id, &recipe.name, args);
    }

    pub fn cancel(&self, job_id: &str) {
        self.client.cancel(job_id);
    }

    fn handle(self: &Rc<Self>, event: Event) {
        match event {
            Event::Started { request_id, job_id } => {
                if let Some(page) = self.pending.borrow_mut().remove(&request_id) {
                    page.attach(&job_id);
                    self.running.borrow_mut().insert(job_id, page);
                }
            }
            Event::Failed {
                request_id,
                message,
            } => {
                if let Some(page) = self.pending.borrow_mut().remove(&request_id) {
                    if dbus_client::is_declined(&message) {
                        page.declined();
                    } else {
                        page.failed_to_start(&message);
                    }
                }
            }
            Event::Output {
                job_id,
                stream,
                line,
            } => {
                if let Some(page) = self.running.borrow().get(&job_id) {
                    page.append(stream, &line);
                }
            }
            Event::Finished { job_id, exit_code } => {
                if let Some(page) = self.running.borrow_mut().remove(&job_id) {
                    page.finished(exit_code);
                    // A recipe that changed the variant, the generation or the feature
                    // set has just invalidated the dashboard.
                    self.refresh_state();
                    if let Some(window) = self.window.borrow().as_ref() {
                        window.state_changed();
                    }
                }
            }
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
        pending: RefCell::new(HashMap::new()),
        running: RefCell::new(HashMap::new()),
        next_request_id: RefCell::new(0),
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
