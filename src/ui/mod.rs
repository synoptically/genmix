//! GTK interface. Only `window` talks to the backend's event stream.

mod card_row;
mod device_row;
mod picker;
mod settings;
mod stream_row;
mod volume_slider;
pub mod window;

use gtk::prelude::*;

const SPACING: i32 = 6;

fn row_with(child: &impl IsA<gtk::Widget>) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::builder().activatable(false).selectable(false).child(child).build();
    row.set_focusable(false);
    row
}

fn column() -> gtk::Box {
    let column = gtk::Box::new(gtk::Orientation::Vertical, SPACING);
    column.set_margin_top(10);
    column.set_margin_bottom(10);
    column.set_margin_start(12);
    column.set_margin_end(12);
    column
}

fn line() -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Horizontal, SPACING)
}

/// A label that shortens instead of forcing the window wider.
fn fitted_label(css: Option<&str>) -> gtk::Label {
    let label = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    if let Some(css) = css {
        label.add_css_class(css);
    }
    label
}

/// Text block that wraps, for details and profile names.
fn wrapped_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .build()
}

/// The lines every details panel starts with.
fn detail_lines(name: &str, driver: &str, bus: Option<&str>, codec: Option<&str>) -> Vec<String> {
    let mut lines = vec![format!("Name: {name}"), format!("Driver: {driver}")];
    lines.extend(bus.map(|bus| format!("Bus: {bus}")));
    lines.extend(codec.map(|codec| format!("Codec: {}", codec.to_uppercase())));
    lines
}

fn meter() -> gtk::LevelBar {
    let bar = gtk::LevelBar::new();
    // The default offsets recolor the bar by level, which reads as a warning.
    for offset in ["low", "high", "full"] {
        bar.remove_offset_value(Some(offset));
    }
    bar.set_tooltip_text(Some("Level"));
    bar
}

fn set_accessible_name(widget: &impl IsA<gtk::Accessible>, name: &str) {
    widget.update_property(&[gtk::accessible::Property::Label(name)]);
}

fn icon_toggle(icon: &str, tooltip: &str) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::builder().icon_name(icon).tooltip_text(tooltip).build();
    set_accessible_name(&button, tooltip);
    button
}
