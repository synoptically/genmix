//! GenMix: control every device and stream on PipeWire or PulseAudio.

mod backend;
mod cli;
mod settings;
mod ui;

use gtk::prelude::*;

pub const APP_ID: &str = "org.genmix.GenMix";
pub const ICON_NAME: &str = "multimedia-volume-control";

const CSS: &str = "
scale.overamp trough highlight { background: @warning_color; border-color: @warning_color; }
.banner { background-color: @warning_color; color: black; padding: 6px 12px; }
.heading { font-weight: bold; }
";

fn main() -> gtk::glib::ExitCode {
    // Lossy, because `args()` panics on a non-UTF-8 argument.
    let args: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    if !args.is_empty() {
        return cli::run(&args);
    }
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        let css = gtk::CssProvider::new();
        css.load_from_string(CSS);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
    app.connect_activate(|app| match app.active_window() {
        Some(window) => window.present(),
        None => drop(ui::window::Window::present(app)),
    });
    app.run()
}
