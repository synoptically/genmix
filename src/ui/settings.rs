//! Settings window.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use libpulse_binding::volume::Volume;

use super::{column, fitted_label, line, wrapped_label};
use crate::backend::to_percent;
use crate::settings::{Settings, MAX_PERCENT, MIN_PERCENT};

type Flag = fn(&mut Settings) -> &mut bool;

/// `changed` runs after each edit has been stored in `settings`.
pub fn show(parent: &gtk::ApplicationWindow, settings: &Rc<RefCell<Settings>>, changed: Rc<dyn Fn()>) {
    let window = gtk::Window::builder()
        .title("Settings")
        .transient_for(parent)
        .modal(true)
        .default_width(420)
        .build();
    let root = column();

    let flag_row = |text: &str, flag: Flag| {
        let switch = gtk::Switch::builder().active(*flag(&mut settings.borrow_mut())).build();
        super::set_accessible_name(&switch, text);
        let (settings, changed) = (Rc::clone(settings), Rc::clone(&changed));
        switch.connect_active_notify(move |switch| {
            *flag(&mut settings.borrow_mut()) = switch.is_active();
            changed();
        });
        let row = line();
        let label = fitted_label(None);
        label.set_text(text);
        row.append(&label);
        row.append(&switch);
        root.append(&row);
        switch
    };

    let overamp = flag_row("Allow volume above 100%", |s| &mut s.allow_overamp);

    let max = gtk::SpinButton::with_range(f64::from(MIN_PERCENT), f64::from(MAX_PERCENT), 5.0);
    max.set_value(f64::from(settings.borrow().max_percent));
    max.set_sensitive(overamp.is_active());
    super::set_accessible_name(&max, "Maximum volume percent");
    let (s, c) = (Rc::clone(settings), Rc::clone(&changed));
    max.connect_value_changed(move |max| {
        s.borrow_mut().max_percent = max.value() as u32;
        c();
    });
    overamp.bind_property("active", &max, "sensitive").build();
    let row = line();
    let label = fitted_label(None);
    label.set_text("Maximum volume (%)");
    row.append(&label);
    row.append(&max);
    root.append(&row);
    let hint = wrapped_label(&format!(
        "Above about {:.0}% sound usually starts to distort.",
        to_percent(Volume::ui_max())
    ));
    hint.add_css_class("dim-label");
    root.append(&hint);

    flag_row("Show monitor sources", |s| &mut s.show_monitors);
    flag_row("Show event sound streams", |s| &mut s.show_event_streams);
    flag_row("Show level meters", |s| &mut s.show_meters);
    flag_row("Show details panels", |s| &mut s.show_details);
    flag_row("Skip speaker feedback warning", |s| &mut s.skip_speaker_warning);

    window.set_child(Some(&root));
    window.present();
}
