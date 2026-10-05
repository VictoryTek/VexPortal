//! The main window: a page sidebar beside a navigation stack.

use crate::app::App;
use crate::ui::pages;

use adw::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Clone)]
pub struct Window {
    window: adw::ApplicationWindow,
    navigation: adw::NavigationView,
    toasts: adw::ToastOverlay,
    sidebar: gtk::ListBox,
    /// Sidebar row order, as page ids.
    ids: Rc<Vec<String>>,
    activity_spinner: adw::Spinner,
    app: Rc<App>,
    /// The page currently shown, so a state change can rebuild it in place.
    current: Rc<RefCell<String>>,
    /// The system changed while a subpage was open; rebuild the root on the way back.
    stale: Rc<Cell<bool>>,
}

impl Window {
    pub fn new(application: &adw::Application, app: &Rc<App>) -> Self {
        let window = adw::ApplicationWindow::builder()
            .application(application)
            .title("VexPortal")
            .default_width(1040)
            .default_height(720)
            .width_request(360)
            .height_request(480)
            .build();

        let navigation = adw::NavigationView::new();
        let toasts = adw::ToastOverlay::new();
        let sidebar = gtk::ListBox::new();
        let activity_spinner = adw::Spinner::new();
        activity_spinner.set_visible(false);

        // Overview first, then only the pages that have something to show for this
        // role — a desktop has no reason to display an empty Services page, and
        // hiding it is clearer than showing it disabled — then Activity.
        let mut entries: Vec<(String, String, String)> = vec![(
            pages::OVERVIEW.to_string(),
            "Overview".to_string(),
            "go-home-symbolic".to_string(),
        )];
        for page in app.visible_pages() {
            entries.push((page.id.clone(), page.title.clone(), page.icon.clone()));
        }
        entries.push((
            pages::ACTIVITY.to_string(),
            "Activity".to_string(),
            "view-list-bullet-symbolic".to_string(),
        ));

        let this = Window {
            window: window.clone(),
            navigation: navigation.clone(),
            toasts: toasts.clone(),
            sidebar: sidebar.clone(),
            ids: Rc::new(entries.iter().map(|(id, _, _)| id.clone()).collect()),
            activity_spinner: activity_spinner.clone(),
            app: app.clone(),
            current: Rc::new(RefCell::new(pages::OVERVIEW.to_string())),
            stale: Rc::new(Cell::new(false)),
        };

        let split = adw::NavigationSplitView::builder()
            .sidebar(&this.build_sidebar(&entries))
            .content(
                &adw::NavigationPage::builder()
                    .title("VexPortal")
                    .child(&navigation)
                    .build(),
            )
            .min_sidebar_width(220.0)
            .max_sidebar_width(260.0)
            .build();

        // Back at the root after the system changed under a subpage: catch up now.
        navigation.connect_popped({
            let this = this.clone();
            move |navigation, _| {
                if this.stale.get() && navigation.navigation_stack().n_items() == 1 {
                    this.stale.set(false);
                    this.rebuild_current();
                }
            }
        });

        toasts.set_child(Some(&split));
        window.set_content(Some(&toasts));

        this.show(pages::OVERVIEW);
        this
    }

    fn build_sidebar(&self, entries: &[(String, String, String)]) -> adw::NavigationPage {
        let list = &self.sidebar;
        list.add_css_class("navigation-sidebar");
        list.set_selection_mode(gtk::SelectionMode::Single);

        for (id, title, icon) in entries {
            let row = sidebar_row(title, icon);
            if id == pages::ACTIVITY {
                if let Some(box_) = row.child().and_downcast::<gtk::Box>() {
                    box_.append(&self.activity_spinner);
                }
            }
            list.append(&row);
        }

        list.connect_row_selected({
            let this = self.clone();
            move |_, row| {
                let Some(row) = row else { return };
                if let Some(id) = this.ids.get(row.index() as usize) {
                    if *this.current.borrow() != *id
                        || this.navigation.navigation_stack().n_items() > 1
                    {
                        this.show(id);
                    }
                }
            }
        });
        if let Some(row) = list.row_at_index(0) {
            list.select_row(Some(&row));
        }

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(list)
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

    /// Show a page by id, and select it in the sidebar.
    pub fn show(&self, id: &str) {
        *self.current.borrow_mut() = id.to_string();
        if let Some(index) = self.ids.iter().position(|i| i == id) {
            let selected = self.sidebar.selected_row().map(|r| r.index());
            if selected != Some(index as i32) {
                if let Some(row) = self.sidebar.row_at_index(index as i32) {
                    // Re-enters through `connect_row_selected`, which sees the page is
                    // already current and does nothing.
                    self.sidebar.select_row(Some(&row));
                }
            }
        }
        self.rebuild_current();
    }

    fn rebuild_current(&self) {
        let id = self.current.borrow().clone();
        let page = pages::build(&self.app, self, &id);
        // `replace` rather than `push`: choosing a page in the sidebar is a sideways
        // move, and leaving a back button pointing at the previous page would be a
        // second, competing navigation model.
        self.navigation.replace(&[page]);
    }

    /// Push a subpage (a service's details) with a working back button.
    pub fn push(&self, page: &adw::NavigationPage) {
        self.navigation.push(page);
    }

    /// Rebuild the visible page after something changed what it displays. A subpage
    /// is left alone — rebuilding it under someone mid-task would be worse than
    /// useless — and the root catches up when they go back.
    pub fn state_changed(&self) {
        if self.navigation.navigation_stack().n_items() > 1 {
            self.stale.set(true);
        } else {
            self.rebuild_current();
        }
    }

    /// A job started or changed state.
    pub fn jobs_changed(&self, finished: bool) {
        let active = self.app.jobs().iter().any(|j| j.state().is_active());
        self.activity_spinner.set_visible(active);
        if finished || *self.current.borrow() == pages::ACTIVITY {
            self.state_changed();
        }
    }

    pub fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    pub fn add_toast(&self, toast: adw::Toast) {
        self.toasts.add_toast(toast);
    }

    pub fn alert(&self, title: &str, body: &str) {
        let dialog = adw::AlertDialog::new(Some(title), Some(body));
        dialog.add_response("ok", "OK");
        dialog.present(Some(&self.window));
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn root(&self) -> &adw::ApplicationWindow {
        &self.window
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
    label.set_hexpand(true);
    box_.append(&label);

    gtk::ListBoxRow::builder().child(&box_).build()
}
