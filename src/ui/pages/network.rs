//! The PIA VPN as live state with controls, plus Tailscale.

use crate::app::App;
use crate::system::vpn::{self, Probe, Region, VpnStatus};
use crate::ui::{self, window::Window};

use adw::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

/// Shown as controls on the status card rather than as rows.
const CONTROLS: [&str; 7] = [
    "vpn-status",
    "vpn-up",
    "vpn-down",
    "vpn-region",
    "vpn-protocol",
    "kill-switch-on",
    "kill-switch-off",
];

const PROTOCOLS: [(&str, &str); 2] = [("wireguard", "WireGuard"), ("openvpn", "OpenVPN")];

/// How often the card re-reads `vexos-vpn status` while it is on screen.
const POLL_SECONDS: u32 = 5;

pub fn build(app: &Rc<App>, window: &Window) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();

    if app.visible("vpn-up").is_some() {
        if vpn::is_installed() {
            page.add(&Card::build(app, window));
        } else {
            page.add(&not_installed(app, window));
        }
    }
    for group in ui::action_groups(app, window, "network", &CONTROLS) {
        page.add(&group);
    }

    ui::page(app, window, "VPN & Network", &page)
}

fn not_installed(app: &Rc<App>, window: &Window) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("VPN").build();
    let row = adw::ActionRow::builder()
        .title("The VPN is not installed")
        .subtitle("Turn on the PIA VPN feature and rebuild to use it.")
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("network-vpn-disabled-symbolic"));
    if app.visible_pages().iter().any(|p| p.id == "features") {
        let button = gtk::Button::with_label("Features");
        button.set_valign(gtk::Align::Center);
        button.connect_clicked({
            let window = window.clone();
            move |_| window.show("features")
        });
        row.add_suffix(&button);
    }
    group.add(&row);
    group
}

struct Card {
    app: Rc<App>,
    window: Window,
    status_row: adw::ActionRow,
    status_icon: gtk::Image,
    connect: gtk::Button,
    killswitch: adw::SwitchRow,
    region: adw::ComboRow,
    protocol: adw::ComboRow,
    /// Region ids in the combo's order; index 0 is `auto`.
    region_ids: RefCell<Vec<String>>,
    /// Set while the card writes its controls, so those writes are not requests.
    updating: Cell<bool>,
    /// What the card last showed, so a control can be put back after a failure.
    last: RefCell<VpnStatus>,
}

impl Card {
    fn build(app: &Rc<App>, window: &Window) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title("VPN")
            .description("Private Internet Access")
            .build();

        let status_icon = gtk::Image::from_icon_name("network-vpn-acquiring-symbolic");
        status_icon.add_css_class("vex-status-icon");
        let status_row = adw::ActionRow::builder().title("Checking…").build();
        status_row.add_css_class("vex-status-row");
        status_row.add_prefix(&status_icon);
        let connect = gtk::Button::with_label("Connect");
        connect.set_valign(gtk::Align::Center);
        connect.add_css_class("pill");
        connect.set_sensitive(false);
        status_row.add_suffix(&connect);

        let killswitch = adw::SwitchRow::builder()
            .title("Kill Switch")
            .subtitle("Block all internet traffic outside the VPN tunnel")
            .sensitive(false)
            .build();
        let region = adw::ComboRow::builder()
            .title("Region")
            .model(&gtk::StringList::new(&["Fastest (auto)"]))
            .sensitive(false)
            .build();
        let protocol_labels: Vec<&str> = PROTOCOLS.iter().map(|(_, l)| *l).collect();
        let protocol = adw::ComboRow::builder()
            .title("Protocol")
            .model(&gtk::StringList::new(&protocol_labels))
            .sensitive(false)
            .build();

        group.add(&status_row);
        group.add(&killswitch);
        group.add(&region);
        group.add(&protocol);

        let card = Rc::new(Card {
            app: app.clone(),
            window: window.clone(),
            status_row,
            status_icon,
            connect,
            killswitch,
            region,
            protocol,
            region_ids: RefCell::new(vec!["auto".to_string()]),
            updating: Cell::new(false),
            last: RefCell::new(VpnStatus::default()),
        });
        card.connect();
        Card::load(Rc::downgrade(&card), true);
        Card::poll(Rc::downgrade(&card));
        // The group owns the card: the controls' handlers only hold it weakly, and
        // it goes when the page does.
        let owner = RefCell::new(Some(card));
        group.connect_destroy(move |_| {
            owner.borrow_mut().take();
        });
        group
    }

    fn connect(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.connect.connect_clicked({
            let weak = weak.clone();
            move |_| {
                let Some(card) = weak.upgrade() else { return };
                let up = !card.last.borrow().is_connected();
                card.connect.set_sensitive(false);
                card.request(
                    if up { "vpn-up" } else { "vpn-down" },
                    HashMap::new(),
                    if up {
                        "VPN connected"
                    } else {
                        "VPN disconnected"
                    },
                );
            }
        });
        self.killswitch.connect_active_notify({
            let weak = weak.clone();
            move |row| {
                let Some(card) = weak.upgrade() else { return };
                if card.updating.get() {
                    return;
                }
                let on = row.is_active();
                card.request(
                    if on {
                        "kill-switch-on"
                    } else {
                        "kill-switch-off"
                    },
                    HashMap::new(),
                    if on {
                        "Kill switch on"
                    } else {
                        "Kill switch off"
                    },
                );
            }
        });
        self.region.connect_selected_notify({
            let weak = weak.clone();
            move |row| {
                let Some(card) = weak.upgrade() else { return };
                if card.updating.get() {
                    return;
                }
                let Some(id) = card
                    .region_ids
                    .borrow()
                    .get(row.selected() as usize)
                    .cloned()
                else {
                    return;
                };
                let mut args = HashMap::new();
                args.insert("region".to_string(), id);
                card.request("vpn-region", args, "VPN region changed");
            }
        });
        self.protocol.connect_selected_notify({
            let weak = weak.clone();
            move |row| {
                let Some(card) = weak.upgrade() else { return };
                if card.updating.get() {
                    return;
                }
                let (value, label) = PROTOCOLS[row.selected() as usize % PROTOCOLS.len()];
                let mut args = HashMap::new();
                args.insert("protocol".to_string(), value.to_string());
                card.request("vpn-protocol", args, &format!("VPN now uses {label}"));
            }
        });
    }

    /// Run one change through the daemon, then re-read the real state — which also
    /// puts the control back if the change failed or was cancelled.
    fn request(self: &Rc<Self>, id: &str, args: HashMap<String, String>, success: &str) {
        let weak = Rc::downgrade(self);
        ui::actions::run_quietly(&self.app, &self.window, id, args, success, move |_| {
            Card::load(weak.clone(), false);
        });
    }

    fn load(card: Weak<Card>, with_regions: bool) {
        glib::spawn_future_local(async move {
            let status = vpn::status().await;
            let regions = if with_regions {
                Some(vpn::regions().await)
            } else {
                None
            };
            let Some(card) = card.upgrade() else { return };
            if let Some(Probe::Ready(regions)) = regions {
                card.set_regions(&regions);
            }
            card.show(&status);
        });
    }

    /// Re-read while the card is on screen; stop once its page has been replaced.
    fn poll(card: Weak<Card>) {
        glib::timeout_add_seconds_local(POLL_SECONDS, move || {
            let Some(strong) = card.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if strong.status_row.root().is_none() {
                return glib::ControlFlow::Break;
            }
            if strong.status_row.is_mapped() {
                Card::load(card.clone(), false);
            }
            glib::ControlFlow::Continue
        });
    }

    fn set_regions(&self, regions: &[Region]) {
        let mut labels = vec!["Fastest (auto)".to_string()];
        let mut ids = vec!["auto".to_string()];
        for region in regions {
            labels.push(match region.latency_s {
                Some(latency) => format!("{} — {} ms", region.name, (latency * 1000.0) as u32),
                None => region.name.clone(),
            });
            ids.push(region.id.clone());
        }
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        self.updating.set(true);
        self.region.set_model(Some(&gtk::StringList::new(&labels)));
        *self.region_ids.borrow_mut() = ids;
        self.updating.set(false);
    }

    fn show(&self, probe: &Probe<VpnStatus>) {
        let status = match probe {
            Probe::Ready(status) => status,
            Probe::NotInstalled | Probe::Unavailable(_) => {
                let message = match probe {
                    Probe::Unavailable(message) => message.clone(),
                    _ => "vexos-vpn is not installed".to_string(),
                };
                self.status_row.set_title("VPN status unavailable");
                self.status_row.set_subtitle(&message);
                self.status_icon
                    .set_icon_name(Some("network-vpn-disabled-symbolic"));
                for widget in [
                    self.connect.upcast_ref::<gtk::Widget>(),
                    self.killswitch.upcast_ref(),
                    self.region.upcast_ref(),
                    self.protocol.upcast_ref(),
                ] {
                    widget.set_sensitive(false);
                }
                return;
            }
        };

        self.updating.set(true);
        self.status_row.set_title(&status.headline());
        let mut details: Vec<String> = Vec::new();
        if status.is_connected() {
            if !status.protocol.is_empty() {
                details.push(protocol_label(&status.protocol).to_string());
            }
            if !status.server_cn.is_empty() {
                details.push(status.server_cn.clone());
            }
        }
        if !status.logged_in {
            details.push("Not signed in — use Sign In to PIA below".to_string());
        }
        if !status.last_error.is_empty() {
            details.push(status.last_error.clone());
        }
        self.status_row.set_subtitle(&details.join(" · "));
        self.status_icon
            .set_icon_name(Some(if status.is_connected() {
                "network-vpn-symbolic"
            } else if status.state == "connecting" {
                "network-vpn-acquiring-symbolic"
            } else {
                "network-vpn-disconnected-symbolic"
            }));
        self.status_row.set_css_classes(&[
            "vex-status-row",
            if status.is_connected() {
                "connected"
            } else {
                "disconnected"
            },
        ]);

        self.connect.set_label(if status.is_connected() {
            "Disconnect"
        } else {
            "Connect"
        });
        if status.is_connected() {
            self.connect.remove_css_class("suggested-action");
        } else {
            self.connect.add_css_class("suggested-action");
        }
        self.connect.set_sensitive(status.state != "connecting");

        self.killswitch.set_active(status.killswitch);
        self.killswitch.set_sensitive(true);

        let region = if status.region_setting.is_empty() {
            "auto"
        } else {
            &status.region_setting
        };
        if let Some(index) = self.region_ids.borrow().iter().position(|id| id == region) {
            self.region.set_selected(index as u32);
        }
        self.region.set_sensitive(true);

        let protocol = if status.protocol_setting.is_empty() {
            &status.protocol
        } else {
            &status.protocol_setting
        };
        if let Some(index) = PROTOCOLS.iter().position(|(value, _)| value == protocol) {
            self.protocol.set_selected(index as u32);
        }
        self.protocol.set_sensitive(true);
        self.updating.set(false);

        *self.last.borrow_mut() = status.clone();
    }
}

fn protocol_label(value: &str) -> &str {
    PROTOCOLS
        .iter()
        .find(|(v, _)| *v == value)
        .map_or(value, |(_, label)| label)
}
