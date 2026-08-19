//! VexPortal — a graphical front end for the vexos-nix justfile.

mod app;
mod dbus_client;
mod just;
mod system;
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

    let application = adw::Application::builder()
        .application_id(APP_ID)
        .build();

    application.connect_startup(|_| {
        adw::init().expect("libadwaita should initialise");
        ui::load_stylesheet();
    });

    application.connect_activate(app::build);
    application.run()
}
