//! The Tailscale card on the VPN & Network page.
//!
//! Setting Tailscale up takes two accounts: the daemon makes this account the operator
//! (the root half, `tailscale-operator`), then `tailscale up` runs here as the user and
//! the sign-in link is shown on the card, so no terminal is involved.
//!
//! When the operator job finishes, the app rebuilds the pages, so the card that started
//! the setup may be gone by the time the sign-in begins. What a setup in progress needs
//! (the running `tailscale up`, the link, the busy flag) therefore lives in [`Shared`],
//! which outlives any one card; each new card picks it up and the setup reports to
//! whichever card is on screen.

use crate::app::App;
use crate::system::tailscale::{self, State};
use crate::ui::{actions, window::Window};

use adw::prelude::*;
use gtk::gdk;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::time::Duration;
use vexportal_ops::tailscale::Backend;

/// How often a sign-in in progress is checked, and how long to wait for the person.
const POLL: Duration = Duration::from_millis(1500);
const GIVE_UP_AFTER_POLLS: u32 = 400;

thread_local! {
    static SHARED: Rc<Shared> = Rc::new(Shared::default());
}

/// A setup in progress, which no card owns.
#[derive(Default)]
struct Shared {
    /// A setup is under way, so Set Up stays off.
    busy: Cell<bool>,
    /// The running `tailscale up`.
    up: RefCell<Option<gio::Subprocess>>,
    /// The sign-in link on show.
    url: RefCell<Option<String>>,
    /// The card on screen now, if any.
    card: RefCell<Weak<Card>>,
}

impl Shared {
    fn card(&self) -> Option<Rc<Card>> {
        self.card.borrow().upgrade()
    }

    /// Bring the card on screen in line with the setup's state.
    fn paint(&self) {
        if let Some(card) = self.card() {
            card.paint(self);
        }
    }

    fn refresh(&self) {
        if let Some(card) = self.card() {
            Card::refresh(&card);
        }
    }

    fn toast(&self, message: &str) {
        if let Some(card) = self.card() {
            card.window.toast(message);
        }
    }

    fn alert(&self, message: &str) {
        if let Some(card) = self.card() {
            card.window.alert("Tailscale", message);
        }
    }

    fn finish(&self) {
        self.busy.set(false);
        *self.url.borrow_mut() = None;
        self.paint();
        self.refresh();
    }

    /// Step one: the daemon lets this account run `tailscale`. Step two follows if it did.
    fn begin(shared: &Rc<Shared>, app: &Rc<App>, window: &Window) {
        if shared.busy.replace(true) {
            return;
        }
        shared.paint();
        let next = shared.clone();
        actions::run_quietly(
            app,
            window,
            "tailscale-operator",
            HashMap::new(),
            "You can now control Tailscale",
            move |ok| {
                if ok {
                    Shared::sign_in(&next);
                } else {
                    next.finish();
                }
            },
        );
    }

    /// Step two: `tailscale up` as this account, with the link shown while it waits.
    fn sign_in(shared: &Rc<Shared>) {
        let process = match tailscale::start_up() {
            Ok(process) => process,
            Err(e) => {
                shared.alert(&format!("Could not start Tailscale: {e}"));
                shared.finish();
                return;
            }
        };
        *shared.up.borrow_mut() = Some(process.clone());

        // How `tailscale up` ended, once it has: `Some(true)` for a clean exit. A signal
        // counts as a failure, which `has_exited` would not report.
        let ended: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
        glib::spawn_future_local({
            let process = process.clone();
            let ended = ended.clone();
            async move {
                ended.set(Some(process.wait_check_future().await.is_ok()));
            }
        });

        let shared = shared.clone();
        glib::spawn_future_local(async move {
            let mut outcome = Outcome::GaveUp;
            for _ in 0..GIVE_UP_AFTER_POLLS {
                if shared.up.borrow().is_none() {
                    outcome = Outcome::Cancelled;
                    break;
                }
                if let Some(clean) = ended.get() {
                    outcome = if clean {
                        Outcome::Connected
                    } else {
                        Outcome::Failed
                    };
                    break;
                }
                let state = tailscale::status().await;
                // A cancel or the application closing can land while status was running.
                if shared.up.borrow().is_none() {
                    outcome = Outcome::Cancelled;
                    break;
                }
                if let State::Ready(status) = state {
                    if status.backend == Backend::Running {
                        outcome = Outcome::Connected;
                        break;
                    }
                    if let Some(url) = status.auth_url {
                        if shared.url.borrow().as_deref() != Some(url.as_str()) {
                            *shared.url.borrow_mut() = Some(url);
                            shared.paint();
                        }
                    }
                }
                glib::timeout_future(POLL).await;
            }

            // Cancelled: the button took it. Otherwise stop what is left of it.
            let leftover = shared.up.borrow_mut().take();
            if let Some(process) = leftover {
                if ended.get().is_none() {
                    process.force_exit();
                }
            }
            match outcome {
                Outcome::Connected => shared.toast("Tailscale connected"),
                Outcome::Failed => shared.alert("Tailscale could not connect. Try again."),
                Outcome::GaveUp => shared.toast("Stopped waiting for the Tailscale sign-in"),
                Outcome::Cancelled => {}
            }
            shared.finish();
        });
    }
}

/// Stop a sign-in that is still waiting. Called when the application shuts down, because
/// `tailscale up` has no timeout of its own and would be left running.
pub fn stop_sign_in() {
    SHARED.with(|shared| {
        let process = shared.up.borrow_mut().take();
        if let Some(process) = process {
            process.force_exit();
        }
    });
}

enum Outcome {
    Connected,
    Failed,
    Cancelled,
    GaveUp,
}

pub fn build(app: &Rc<App>, window: &Window) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Tailscale").build();

    let status_row = adw::ActionRow::builder().title("Checking…").build();
    status_row.add_prefix(&gtk::Image::from_icon_name("network-workgroup-symbolic"));
    let button = gtk::Button::with_label("Set Up");
    button.set_valign(gtk::Align::Center);
    button.add_css_class("pill");
    button.set_visible(false);
    status_row.add_suffix(&button);

    let sign_in_row = adw::ActionRow::builder()
        .title("Finish signing in")
        .subtitle_selectable(true)
        .visible(false)
        .build();
    sign_in_row.set_subtitle_lines(0);
    let open = gtk::Button::with_label("Open");
    open.set_valign(gtk::Align::Center);
    open.add_css_class("suggested-action");
    let copy = gtk::Button::with_label("Copy Link");
    copy.set_valign(gtk::Align::Center);
    let cancel = gtk::Button::with_label("Cancel");
    cancel.set_valign(gtk::Align::Center);
    for b in [&cancel, &copy, &open] {
        sign_in_row.add_suffix(b);
    }

    group.add(&status_row);
    group.add(&sign_in_row);

    let shared = SHARED.with(Rc::clone);
    let card = Rc::new(Card {
        app: app.clone(),
        window: window.clone(),
        shared: shared.clone(),
        status_row,
        button,
        sign_in_row,
    });
    // This card is the one on screen now; a setup already under way reports to it.
    *shared.card.borrow_mut() = Rc::downgrade(&card);

    let weak = Rc::downgrade(&card);
    card.button.connect_clicked({
        let weak = weak.clone();
        move |_| {
            if let Some(card) = weak.upgrade() {
                Shared::begin(&card.shared, &card.app, &card.window);
            }
        }
    });
    open.connect_clicked({
        let weak = weak.clone();
        move |_| {
            let Some(card) = weak.upgrade() else { return };
            let url = card.shared.url.borrow().clone();
            if let Some(url) = url {
                let _ = gio::AppInfo::launch_default_for_uri(&url, gio::AppLaunchContext::NONE);
            }
        }
    });
    copy.connect_clicked({
        let weak = weak.clone();
        move |_| {
            let Some(card) = weak.upgrade() else { return };
            let url = card.shared.url.borrow().clone();
            if let (Some(url), Some(display)) = (url, gdk::Display::default()) {
                display.clipboard().set_text(&url);
                card.window.toast("Link copied");
            }
        }
    });
    cancel.connect_clicked({
        let weak = weak.clone();
        move |_| {
            if let Some(card) = weak.upgrade() {
                // The sign-in loop sees the process gone and stops.
                let process = card.shared.up.borrow_mut().take();
                if let Some(process) = process {
                    process.force_exit();
                }
            }
        }
    });

    card.paint(&shared);
    Card::refresh(&card);
    // The group owns the card; handlers hold it weakly, and it goes when the page does.
    let owner = RefCell::new(Some(card));
    group.connect_destroy(move |_| {
        owner.borrow_mut().take();
    });
    group
}

struct Card {
    app: Rc<App>,
    window: Window,
    shared: Rc<Shared>,
    status_row: adw::ActionRow,
    button: gtk::Button,
    sign_in_row: adw::ActionRow,
}

impl Card {
    fn refresh(card: &Rc<Card>) {
        let card = card.clone();
        glib::spawn_future_local(async move {
            let state = tailscale::status().await;
            card.show(&state);
        });
    }

    fn show(&self, state: &State) {
        let (title, subtitle, can_set_up) = describe(state);
        self.status_row.set_title(&title);
        self.status_row.set_subtitle(&subtitle);
        self.button.set_visible(can_set_up);
        self.paint(&self.shared);
    }

    /// The parts that follow the setup in progress: the button, and the sign-in link.
    fn paint(&self, shared: &Shared) {
        self.button.set_sensitive(!shared.busy.get());
        let url = shared.url.borrow();
        match url.as_deref() {
            Some(url) => {
                self.sign_in_row.set_subtitle(url);
                self.sign_in_row.set_visible(true);
            }
            None => self.sign_in_row.set_visible(false),
        }
    }
}

/// The row's title and subtitle, and whether the Set Up button applies.
fn describe(state: &State) -> (String, String, bool) {
    match state {
        State::NotInstalled => (
            "Tailscale is not installed".into(),
            "Turn on services.tailscale in your NixOS configuration and rebuild.".into(),
            false,
        ),
        State::Unavailable(message) => ("Tailscale is not running".into(), message.clone(), false),
        State::Ready(status) => match &status.backend {
            Backend::Running => {
                let mut details = Vec::new();
                if let Some(host) = &status.host {
                    details.push(host.clone());
                }
                if let Some(ip) = status.ips.first() {
                    details.push(ip.clone());
                }
                if let Some(tailnet) = &status.tailnet {
                    details.push(tailnet.clone());
                }
                ("Connected".into(), details.join(" · "), false)
            }
            Backend::NeedsLogin => (
                "Not signed in".into(),
                "Set Up lets your account control Tailscale, then signs you in.".into(),
                true,
            ),
            Backend::NeedsMachineAuth => (
                "Waiting for approval".into(),
                "An admin of your tailnet has to approve this machine.".into(),
                false,
            ),
            Backend::Stopped => (
                "Tailscale is turned off".into(),
                "Set Up connects it again.".into(),
                true,
            ),
            Backend::Starting => ("Starting…".into(), String::new(), false),
            Backend::Other(state) => ("Tailscale".into(), state.clone(), true),
        },
    }
}
