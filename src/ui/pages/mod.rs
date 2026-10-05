//! One module per sidebar page.
//!
//! Features, VPN & Network and Services show the system's state with controls on it;
//! the rest list their actions under the catalog's group headings.

mod activity;
mod features;
mod network;
mod overview;
mod services;
mod system;

use crate::app::App;
use crate::ui::{self, window::Window};

use adw::prelude::*;
use std::rc::Rc;

/// Sidebar ids for the pages that are not catalog pages.
pub const OVERVIEW: &str = "__overview";
pub const ACTIVITY: &str = "__activity";

pub fn build(app: &Rc<App>, window: &Window, id: &str) -> adw::NavigationPage {
    match id {
        OVERVIEW => overview::build(app, window),
        ACTIVITY => activity::build(app, window),
        "system" => system::build(app, window),
        "features" => features::build(app, window),
        "network" => network::build(app, window),
        "services" => services::build(app, window),
        other => generic(app, window, other),
    }
}

/// A page that is nothing but its actions, grouped.
fn generic(app: &Rc<App>, window: &Window, id: &str) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();
    let catalog_page = app.catalog.page(id);
    if let Some(description) = catalog_page.and_then(|p| p.description.as_deref()) {
        page.set_description(description);
    }
    for group in ui::action_groups(app, window, id, &[]) {
        page.add(&group);
    }
    ui::page(
        app,
        window,
        catalog_page.map_or("VexPortal", |p| &p.title),
        &page,
    )
}
