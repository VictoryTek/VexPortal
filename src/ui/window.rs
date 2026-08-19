//! The main window: a category sidebar beside a navigation stack.

use crate::app::App;
use crate::ui::{category_page, dashboard};

use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

/// A category id the sidebar uses for the dashboard, which is not a catalog category.
const DASHBOARD: &str = "__dashboard";

#[derive(Clone)]
pub struct Window {
    window: adw::ApplicationWindow,
    navigation: adw::NavigationView,
    toasts: adw::ToastOverlay,
    app: Rc<App>,
    /// The category currently shown, so a state change can rebuild it in place.
    current: Rc<RefCell<String>>,
}

impl Window {
    pub fn new(application: &adw::Application, app: &Rc<App>) -> Self {
        let window = adw::ApplicationWindow::builder()
            .application(application)
            .title("VexPortal")
            .default_width(1000)
            .default_height(700)
            .width_request(360)
            .height_request(480)
            .build();

        let navigation = adw::NavigationView::new();
        let toasts = adw::ToastOverlay::new();

        let this = Window {
            window: window.clone(),
            navigation: navigation.clone(),
            toasts: toasts.clone(),
            app: app.clone(),
            current: Rc::new(RefCell::new(DASHBOARD.to_string())),
        };

        let split = adw::NavigationSplitView::builder()
            .sidebar(&this.build_sidebar())
            .content(
                &adw::NavigationPage::builder()
                    .title("VexPortal")
                    .child(&navigation)
                    .build(),
            )
            .min_sidebar_width(220.0)
            .max_sidebar_width(280.0)
            .build();

        toasts.set_child(Some(&split));
        window.set_content(Some(&toasts));

        this.show_category(DASHBOARD);
        this
    }

    fn build_sidebar(&self) -> adw::NavigationPage {
        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.set_selection_mode(gtk::SelectionMode::Single);

        // The dashboard first, then only the categories that have something to show
        // for this role — a desktop has no reason to display an empty Server Services
        // page, and hiding it is clearer than showing it disabled.
        let mut ids = vec![DASHBOARD.to_string()];
        list.append(&sidebar_row("Dashboard", "go-home-symbolic"));

        for category in self.app.visible_categories() {
            ids.push(category.id.clone());
            list.append(&sidebar_row(&category.title, &category.icon));
        }

        let ids = Rc::new(ids);
        list.connect_row_selected({
            let this = self.clone();
            let ids = ids.clone();
            move |_, row| {
                let Some(row) = row else { return };
                if let Some(id) = ids.get(row.index() as usize) {
                    this.show_category(id);
                }
            }
        });
        if let Some(row) = list.row_at_index(0) {
            list.select_row(Some(&row));
        }

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .vexpand(true)
            .build();

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));

        adw::NavigationPage::builder()
            .title("VexPortal")
            .child(&toolbar)
            .build()
    }

    fn show_category(&self, id: &str) {
        *self.current.borrow_mut() = id.to_string();
        let page = if id == DASHBOARD {
            dashboard::build(&self.app, self)
        } else {
            category_page::build(&self.app, self, id)
        };
        // `replace` rather than `push`: choosing a category in the sidebar is a
        // sideways move, and leaving a back button pointing at the previous category
        // would be a second, competing navigation model.
        self.navigation.replace(&[page]);
    }

    /// Push a page (a run view) with a working back button.
    pub fn push(&self, page: &adw::NavigationPage) {
        self.navigation.push(page);
    }

    pub fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn root(&self) -> &adw::ApplicationWindow {
        &self.window
    }

    /// Rebuild the visible page after a recipe changed something it displays.
    pub fn state_changed(&self) {
        let current = self.current.borrow().clone();
        // Only the dashboard reads live system state; category pages are static, and
        // rebuilding one under a user who is mid-scroll would be worse than useless.
        if current == DASHBOARD && self.navigation.visible_page().is_some() {
            let page = dashboard::build(&self.app, self);
            self.navigation.replace(&[page]);
        }
    }
}

fn sidebar_row(title: &str, icon: &str) -> gtk::ListBoxRow {
    let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    box_.set_margin_top(10);
    box_.set_margin_bottom(10);
    box_.set_margin_start(6);
    box_.set_margin_end(6);
    box_.append(&gtk::Image::from_icon_name(icon));
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    box_.append(&label);

    gtk::ListBoxRow::builder().child(&box_).build()
}
