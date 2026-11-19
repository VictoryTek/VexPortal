//! The form that replaces `just remote-storage attach`'s questions.
//!
//! It can look the servers on the network up, and ask a server what it shares, so most of
//! the answers are picked rather than typed. The lookups run here, as you, the way the
//! tools do in a terminal; what is chosen is then sent to the daemon in one request, which
//! tests the share and writes the configuration.

use crate::app::App;
use crate::system::remote as network;
use crate::ui::{actions, window::Window};

use adw::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_catalog::Widget;
use vexportal_ops::remote::Host;

const ACTION: &str = "remote-storage-attach";

/// Replace a combo row's choices. The first entry is a prompt, so showing the list picks
/// nothing; the handlers below ignore it.
fn fill(row: &adw::ComboRow, items: &[String]) {
    let mut refs: Vec<&str> = vec!["Choose…"];
    refs.extend(items.iter().map(String::as_str));
    row.set_model(Some(&gtk::StringList::new(&refs)));
}

fn is_smb(protocol: &adw::ComboRow) -> bool {
    protocol.selected() == 1
}

pub fn present(app: &Rc<App>, window: &Window) {
    if app.catalog.action(ACTION).is_none() {
        log::error!("no catalog action `{ACTION}`");
        return;
    }

    let protocol = adw::ComboRow::builder()
        .title("Protocol")
        .subtitle("NFS is for Linux and Unix machines; SMB for Windows, Samba and most NAS units.")
        .model(&gtk::StringList::new(&["NFS", "SMB"]))
        .build();

    // ── Server ────────────────────────────────────────────────────────────────
    let server = adw::EntryRow::builder()
        .title("Server (name or IP address)")
        .build();
    let find = gtk::Button::with_label("Find on Network");
    find.set_valign(gtk::Align::Center);
    server.add_suffix(&find);
    let found = adw::ComboRow::builder()
        .title("Found on the network")
        .visible(false)
        .build();
    let hosts: Rc<RefCell<Vec<Host>>> = Rc::default();

    let username = adw::EntryRow::builder().title("SMB username").build();
    let password = adw::PasswordEntryRow::builder()
        .title("SMB password")
        .build();

    let servers = adw::PreferencesGroup::builder().title("Server").build();
    servers.add(&protocol);
    servers.add(&server);
    servers.add(&found);
    servers.add(&username);
    servers.add(&password);

    // ── Share ─────────────────────────────────────────────────────────────────
    let share = adw::EntryRow::builder()
        .title("Share (SMB name, or NFS export path)")
        .build();
    let list = gtk::Button::with_label("List Shares");
    list.set_valign(gtk::Align::Center);
    share.add_suffix(&list);
    let shares = adw::ComboRow::builder()
        .title("Offered by the server")
        .visible(false)
        .build();
    let offered: Rc<RefCell<Vec<String>>> = Rc::default();

    let shares_group = adw::PreferencesGroup::builder().title("Share").build();
    shares_group.add(&share);
    shares_group.add(&shares);

    // ── Options ───────────────────────────────────────────────────────────────
    let test = adw::SwitchRow::builder()
        .title("Try the share first")
        .subtitle("Mount it once to check the address and login. Nothing is saved if it fails.")
        .active(true)
        .build();
    let owner = adw::SwitchRow::builder()
        .title("Let my account write")
        .subtitle("SMB only. Your account owns the mount; the login still decides what the server allows.")
        .active(true)
        .build();
    let media = adw::SwitchRow::builder()
        .title("Let media services write")
        .subtitle("Sonarr, Radarr, Plex and the others join a shared `media` group.")
        .active(true)
        .build();
    let gid = adw::EntryRow::builder()
        .title("Group number (NFS, optional)")
        .build();
    let mount = adw::EntryRow::builder()
        .title("Mount point (optional)")
        .build();
    mount.set_tooltip_text(Some(
        "A folder under /mnt. Left empty it is /mnt/<server>/<share>.",
    ));
    let options = adw::PreferencesGroup::builder().title("Options").build();
    options.add(&test);
    options.add(&owner);
    options.add(&media);
    options.add(&gid);
    options.add(&mount);

    // What only one protocol has.
    let sync_protocol = {
        let username = username.clone();
        let password = password.clone();
        let owner = owner.clone();
        let gid = gid.clone();
        let protocol = protocol.clone();
        move || {
            let smb = is_smb(&protocol);
            username.set_visible(smb);
            password.set_visible(smb);
            owner.set_visible(smb);
            gid.set_visible(!smb);
        }
    };
    sync_protocol();
    protocol.connect_selected_notify({
        let found = found.clone();
        let shares = shares.clone();
        move |_| {
            sync_protocol();
            found.set_visible(false);
            shares.set_visible(false);
        }
    });

    // Find servers.
    find.connect_clicked({
        let protocol = protocol.clone();
        let found = found.clone();
        let hosts = hosts.clone();
        let find = find.clone();
        move |button| {
            button.set_sensitive(false);
            button.set_label("Searching…");
            let proto = if is_smb(&protocol) { "cifs" } else { "nfs" };
            let (found, hosts, find) = (found.clone(), hosts.clone(), find.clone());
            glib::spawn_future_local(async move {
                let result = network::find_hosts(proto).await;
                find.set_sensitive(true);
                find.set_label("Find on Network");
                // The data first: replacing the model notifies the handler, which reads it.
                let labels: Vec<String> = result.iter().map(|h| h.label.clone()).collect();
                found.set_title(if result.is_empty() {
                    "No servers found: type the address instead"
                } else {
                    "Found on the network"
                });
                *hosts.borrow_mut() = result;
                fill(&found, &labels);
                found.set_visible(true);
            });
        }
    });
    found.connect_selected_notify({
        let server = server.clone();
        let hosts = hosts.clone();
        move |row| {
            if let Some(host) = (row.selected() as usize)
                .checked_sub(1)
                .and_then(|i| hosts.borrow().get(i).cloned())
            {
                server.set_text(&host.address);
            }
        }
    });

    // List what the server shares.
    list.connect_clicked({
        let protocol = protocol.clone();
        let server = server.clone();
        let username = username.clone();
        let password = password.clone();
        let shares = shares.clone();
        let offered = offered.clone();
        let list = list.clone();
        let window = window.clone();
        move |button| {
            let host = server.text().trim().to_string();
            if host.is_empty() {
                window.alert("Server needed", "Enter or pick the server first.");
                return;
            }
            let smb = is_smb(&protocol);
            let (user, pass) = (
                username.text().trim().to_string(),
                password.text().to_string(),
            );
            if smb && user.is_empty() {
                window.alert(
                    "Login needed",
                    "Most NAS units only list their shares to a login. Enter the SMB username and password.",
                );
                return;
            }
            button.set_sensitive(false);
            button.set_label("Asking…");
            let (shares, offered, list) = (shares.clone(), offered.clone(), list.clone());
            glib::spawn_future_local(async move {
                let result = if smb {
                    network::smb_shares(&host, &user, &pass).await
                } else {
                    network::nfs_exports(&host).await
                };
                list.set_sensitive(true);
                list.set_label("List Shares");
                if result.is_empty() {
                    shares.set_title("Nothing listed: type the name instead");
                } else {
                    shares.set_title("Offered by the server");
                }
                let names = result.clone();
                *offered.borrow_mut() = result;
                fill(&shares, &names);
                shares.set_visible(true);
            });
        }
    });
    shares.connect_selected_notify({
        let share = share.clone();
        let offered = offered.clone();
        move |row| {
            if let Some(name) = (row.selected() as usize)
                .checked_sub(1)
                .and_then(|i| offered.borrow().get(i).cloned())
            {
                share.set_text(&name);
            }
        }
    });

    // ── The dialog ────────────────────────────────────────────────────────────
    let page = adw::PreferencesPage::new();
    page.add(&servers);
    page.add(&shares_group);
    page.add(&options);

    let dialog = adw::Dialog::builder()
        .title("Attach Remote Share")
        .content_width(560)
        .build();
    let header = adw::HeaderBar::new();
    let cancel = gtk::Button::with_label("Cancel");
    let attach = gtk::Button::with_label("Attach");
    attach.add_css_class("suggested-action");
    header.pack_start(&cancel);
    header.pack_end(&attach);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&page));
    dialog.set_child(Some(&toolbar));
    cancel.connect_clicked({
        let dialog = dialog.clone();
        move |_| {
            dialog.close();
        }
    });

    attach.connect_clicked({
        let app = app.clone();
        let window = window.clone();
        let dialog = dialog.clone();
        move |_| {
            let smb = is_smb(&protocol);
            let yes_no = |on: bool| if on { "yes" } else { "no" }.to_string();
            let mut args: HashMap<String, String> = HashMap::new();
            args.insert("protocol".into(), if smb { "cifs" } else { "nfs" }.into());
            args.insert("server".into(), server.text().trim().to_string());
            args.insert("share".into(), share.text().trim().to_string());
            args.insert("test_mount".into(), yes_no(test.is_active()));
            args.insert("media_write".into(), yes_no(media.is_active()));
            args.insert("mountpoint".into(), mount.text().trim().to_string());
            if smb {
                args.insert("owner_write".into(), yes_no(owner.is_active()));
                args.insert("username".into(), username.text().trim().to_string());
                args.insert("password".into(), password.text().to_string());
            } else {
                args.insert("gid".into(), gid.text().trim().to_string());
            }
            args.retain(|_, v| !v.is_empty());

            let Some(action) = app.catalog.action(ACTION) else {
                return;
            };
            for (name, label) in [("server", "Server"), ("share", "Share")] {
                if !args.contains_key(name) {
                    window.alert("Missing information", &format!("`{label}` is required."));
                    return;
                }
            }
            if smb && (!args.contains_key("username") || !args.contains_key("password")) {
                window.alert(
                    "Missing information",
                    "An SMB share needs a username and a password.",
                );
                return;
            }
            for (id, value) in &args {
                let Some(param) = action.params.iter().find(|p| p.name == *id) else {
                    continue;
                };
                if let Widget::Text { format } = &param.widget {
                    if let Err(e) = format.validate(value) {
                        window.alert("Check the answer", &format!("{}: {e}", param.label));
                        return;
                    }
                }
            }
            dialog.close();
            actions::proceed(&app, &window, action, args);
        }
    });

    dialog.present(Some(window.root()));
}
