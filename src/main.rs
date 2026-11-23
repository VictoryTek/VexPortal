//! VexPortal — a graphical front end for the vexos-nix justfile.

mod app;
mod dbus_client;
mod job;
mod just;
mod system;
mod terminal;
mod ui;

use adw::prelude::*;

/// Also the gresource prefix and the polkit/D-Bus vendor id.
pub const APP_ID: &str = "io.github.vexportal";

fn main() -> glib::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    // The resource bundle carries the stylesheet and the application icon, so it has
    // to be registered before any window is built.
    gio::resources_register_include!("compiled.gresource")
        .expect("the compiled gresource bundle should be embedded in the binary");

    // HANDLES_COMMAND_LINE makes a second `vexportal --page ai` reach the running
    // instance's `command_line` handler instead of starting another process.
    let application = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    application.add_main_option(
        "page",
        glib::Char::from(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::String,
        "Open on this page, e.g. ai",
        Some("ID"),
    );

    // Also reachable as `org.freedesktop.Application.ActivateAction("show-page", ["ai"])`.
    let show_page = gio::SimpleAction::new("show-page", Some(glib::VariantTy::STRING));
    show_page.connect_activate({
        let application = application.clone();
        move |_, parameter| {
            // Builds the window if none exists yet, and just raises it otherwise.
            application.activate();
            app::show_page(parameter.and_then(|p| p.str()));
        }
    });
    application.add_action(&show_page);

    application.connect_command_line(|application, command_line| {
        let page = command_line
            .options_dict()
            .lookup::<String>("page")
            .ok()
            .flatten();
        application.activate();
        app::show_page(page.as_deref());
        0
    });

    application.connect_startup(|_| {
        adw::init().expect("libadwaita should initialise");
        ui::load_stylesheet();
    });

    application.connect_activate(app::build);
    application.run()
}
