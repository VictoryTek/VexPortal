//! Choosing a role, GPU and desktop to rebuild as.
//!
//! `just switch` prompts for anything left empty, and the daemon has no terminal to
//! answer with, so this dialog always fills role and GPU and fills the desktop
//! environment and VM platform whenever they apply. Fields that do not apply to the
//! chosen combination are hidden rather than offered and ignored.

use crate::app::App;
use crate::ui::{actions, window::Window};

use adw::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;
use vexportal_catalog::Role;

const ROLES: [(&str, &str); 6] = [
    ("desktop", "Desktop"),
    ("stateless", "Stateless"),
    ("htpc", "HTPC"),
    ("server", "Server"),
    ("headless-server", "Headless Server"),
    ("vanilla", "Vanilla"),
];

const GPUS: [(&str, &str); 5] = [
    ("amd", "AMD"),
    ("nvidia", "NVIDIA"),
    ("nvidia-legacy580", "NVIDIA legacy (580 driver)"),
    ("intel", "Intel"),
    ("vm", "Virtual machine"),
];

const DESKTOPS: [(&str, &str); 3] = [
    ("gnome", "GNOME"),
    ("cosmic", "COSMIC"),
    ("hyprland", "Hyprland"),
];

const VM_PLATFORMS: [(&str, &str); 2] = [
    ("qemu", "QEMU / KVM / Proxmox"),
    ("virtualbox", "VirtualBox"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `just switch`: rebuild and activate.
    Switch,
    /// `just build`: a dry-run build that changes nothing.
    TestBuild,
}

pub fn present(app: &Rc<App>, window: &Window, mode: Mode) {
    let action_id = match mode {
        Mode::Switch => "switch",
        Mode::TestBuild => "build",
    };
    let Some(action) = app.visible(action_id) else {
        return;
    };

    let state = app.state.borrow();
    let current_role = app.variant.as_ref().map_or(Role::Desktop, |v| v.role);
    let current_gpu = app
        .variant
        .as_ref()
        .map(|v| v.gpu.clone())
        .unwrap_or_default();

    let role = combo("Role", &ROLES, current_role.as_str());
    let gpu = combo("GPU", &GPUS, &current_gpu);
    gpu.set_subtitle("The legacy driver covers Maxwell, Pascal and Volta cards.");
    let desktop = combo(
        "Desktop environment",
        &DESKTOPS,
        state.settings.desktop_environment().unwrap_or("gnome"),
    );
    let vm_platform = combo(
        "VM platform",
        &VM_PLATFORMS,
        state.settings.vm_platform().unwrap_or("qemu"),
    );
    vm_platform.set_subtitle("VirtualBox pins the 6.18 LTS kernel for its guest additions.");
    drop(state);

    let flake = adw::EntryRow::builder().title("Flake override").build();
    let advanced = adw::ExpanderRow::builder()
        .title("Advanced")
        .subtitle("Build from a different flake than this machine's own")
        .build();
    advanced.add_row(&flake);

    let group = adw::PreferencesGroup::new();
    group.set_description(Some(&action.blurb));
    group.add(&role);
    group.add(&gpu);
    group.add(&desktop);
    group.add(&vm_platform);
    group.add(&advanced);

    let summary = gtk::Label::new(None);
    summary.set_wrap(true);
    summary.set_xalign(0.0);
    summary.add_css_class("dim-label");
    summary.set_margin_top(12);
    group.add(&summary);

    let page = adw::PreferencesPage::new();
    page.add(&group);

    let update = {
        let (role, gpu, desktop, vm_platform, summary) = (
            role.clone(),
            gpu.clone(),
            desktop.clone(),
            vm_platform.clone(),
            summary.clone(),
        );
        move || {
            let role_value = value(&role, &ROLES);
            let gpu_value = value(&gpu, &GPUS);
            // The desktop environment is a desktop-role setting and the VM platform a
            // vm-variant one; a test build takes neither.
            desktop.set_visible(mode == Mode::Switch && role_value == "desktop");
            vm_platform.set_visible(mode == Mode::Switch && gpu_value == "vm");
            let mut text = format!(
                "{} as vexos-{role_value}-{gpu_value}",
                if mode == Mode::Switch {
                    "Rebuilds"
                } else {
                    "Test-builds"
                }
            );
            if desktop.is_visible() {
                text.push_str(&format!(" with {}", label(&desktop, &DESKTOPS)));
            }
            if vm_platform.is_visible() {
                text.push_str(&format!(" on {}", label(&vm_platform, &VM_PLATFORMS)));
            }
            text.push('.');
            summary.set_text(&text);
        }
    };
    update();
    for row in [&role, &gpu, &desktop, &vm_platform] {
        let update = update.clone();
        row.connect_selected_notify(move |_| update());
    }

    let dialog = adw::Dialog::builder()
        .title(&action.title)
        .content_width(520)
        .build();

    let header = adw::HeaderBar::new();
    header.set_show_end_title_buttons(false);
    let cancel = gtk::Button::with_label("Cancel");
    let go = gtk::Button::with_label(match mode {
        Mode::Switch => "Switch",
        Mode::TestBuild => "Build",
    });
    go.add_css_class("suggested-action");
    header.pack_start(&cancel);
    header.pack_end(&go);

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
    go.connect_clicked({
        let app = app.clone();
        let window = window.clone();
        let dialog = dialog.clone();
        move |_| {
            let mut args = HashMap::new();
            args.insert("role".to_string(), value(&role, &ROLES).to_string());
            args.insert("variant".to_string(), value(&gpu, &GPUS).to_string());
            let flake = flake.text().trim().to_string();
            if !flake.is_empty() {
                args.insert("flake".to_string(), flake);
            }
            if desktop.is_visible() {
                args.insert("de".to_string(), value(&desktop, &DESKTOPS).to_string());
            }
            if vm_platform.is_visible() {
                args.insert(
                    "vmp".to_string(),
                    value(&vm_platform, &VM_PLATFORMS).to_string(),
                );
            }
            dialog.close();
            // Straight to `proceed`: the optional fields left empty here are meant to
            // stay empty, not to be asked for again.
            if let Some(action) = app.catalog.action(action_id) {
                actions::proceed(&app, &window, action, args);
            }
        }
    });

    dialog.present(Some(window.root()));
}

fn combo(title: &str, options: &[(&str, &str)], selected: &str) -> adw::ComboRow {
    let labels: Vec<&str> = options.iter().map(|(_, label)| *label).collect();
    let row = adw::ComboRow::builder()
        .title(title)
        .model(&gtk::StringList::new(&labels))
        .build();
    if let Some(index) = options.iter().position(|(value, _)| *value == selected) {
        row.set_selected(index as u32);
    }
    row
}

fn value(row: &adw::ComboRow, options: &[(&'static str, &'static str)]) -> &'static str {
    options
        .get(row.selected() as usize)
        .map_or(options[0].0, |(value, _)| value)
}

fn label(row: &adw::ComboRow, options: &[(&'static str, &'static str)]) -> &'static str {
    options
        .get(row.selected() as usize)
        .map_or(options[0].1, |(_, label)| label)
}
