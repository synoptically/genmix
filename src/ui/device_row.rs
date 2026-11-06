//! One sink or source. Sources also get the live preview controls.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;

use super::picker::{Item, Picker};
use super::volume_slider::{Throttle, VolumeControl};
use super::{
    column, detail_lines, fitted_label, icon_toggle, line, meter, row_with, set_accessible_name, wrapped_label,
};
use crate::backend::{Backend, DeviceInfo, Target};

/// What the window must do for a row's preview controls.
pub struct PreviewHandlers {
    /// `true` asks to start, `false` to stop.
    pub on_toggle: Box<dyn Fn(u32, bool)>,
    pub on_target_changed: Box<dyn Fn(u32)>,
    pub on_volume: Box<dyn Fn(u32, f64)>,
}

pub struct PreviewControls {
    toggle: gtk::ToggleButton,
    /// `None` is "whatever the default output is".
    pub target: Rc<Picker<Option<u32>>>,
    volume: gtk::Scale,
    latency: gtk::Label,
    updating: Rc<Cell<bool>>,
}

impl PreviewControls {
    fn new(index: u32, name: &str, handlers: PreviewHandlers) -> (Rc<Self>, gtk::Box) {
        let PreviewHandlers { on_toggle, on_target_changed, on_volume } = handlers;
        let toggle = icon_toggle("audio-headphones-symbolic", "Live preview: hear this input");
        let target = Picker::new(&format!("{name} preview output"), move |_| on_target_changed(index));
        let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
        volume.set_value(100.0);
        volume.set_draw_value(false);
        volume.set_hexpand(true);
        volume.set_tooltip_text(Some("Preview volume"));
        set_accessible_name(&volume, &format!("{name} preview volume"));
        let latency = gtk::Label::builder().width_chars(7).xalign(1.0).build();
        latency.set_tooltip_text(Some("Measured preview latency"));
        latency.add_css_class("dim-label");

        let updating = Rc::new(Cell::new(false));
        let guard = Rc::clone(&updating);
        toggle.connect_toggled(move |t| {
            if !guard.get() {
                on_toggle(index, t.is_active());
            }
        });
        let throttle = Throttle::new(move |percent| on_volume(index, percent), || {});
        volume.connect_value_changed(move |v| throttle.push(v.value()));

        let root = line();
        root.append(&toggle);
        root.append(&target.dropdown);
        root.append(&volume);
        root.append(&latency);
        (Rc::new(Self { toggle, target, volume, latency, updating }), root)
    }

    pub fn volume(&self) -> f64 {
        self.volume.value()
    }

    #[cfg(test)]
    pub fn set_volume(&self, percent: f64) {
        self.volume.set_value(percent);
    }

    #[cfg(test)]
    pub fn latency_text(&self) -> String {
        self.latency.text().to_string()
    }

    pub fn is_on(&self) -> bool {
        self.toggle.is_active()
    }

    pub fn set_on(&self, on: bool) {
        self.updating.set(true);
        self.toggle.set_active(on);
        self.updating.set(false);
        if !on {
            self.latency.set_text("");
        }
    }

    pub fn set_latency(&self, usec: Option<u64>) {
        self.latency.set_text(&usec.map_or("-".to_string(), |u| format!("{} ms", u / 1000)));
    }
}

pub struct DeviceRow {
    pub row: gtk::ListBoxRow,
    pub info: RefCell<DeviceInfo>,
    pub control: Rc<VolumeControl>,
    pub meter: gtk::LevelBar,
    pub preview: Option<Rc<PreviewControls>>,
    target: Target,
    is_input: bool,
    icon: gtk::Image,
    title: gtk::Label,
    default_mark: gtk::Label,
    default_button: gtk::Button,
    port_line: gtk::Box,
    ports: Rc<Picker<String>>,
    details: gtk::Expander,
    details_text: gtk::Label,
}

impl DeviceRow {
    pub fn new(
        backend: &Backend,
        info: DeviceInfo,
        is_input: bool,
        limit: f64,
        preview: Option<PreviewHandlers>,
    ) -> Rc<Self> {
        let target = if is_input { Target::Source(info.index) } else { Target::Sink(info.index) };
        let (b1, b2, b3, b4) = (backend.clone(), backend.clone(), backend.clone(), backend.clone());
        let control = VolumeControl::new(
            &info.description,
            limit,
            move |volume| b1.set_volume(target, volume),
            move |mute| b2.set_mute(target, mute),
        );
        let ports = Picker::new(&format!("{} port", info.description), move |port: String| {
            b3.set_port(target, &port);
        });

        let root = column();
        let icon = gtk::Image::new();
        let title = fitted_label(Some("heading"));
        let default_mark = gtk::Label::new(None);
        default_mark.set_markup("<b>Default</b>");
        let default_button = gtk::Button::with_label("Set as default");
        let header = line();
        header.append(&icon);
        header.append(&title);
        header.append(&default_mark);
        header.append(&default_button);
        root.append(&header);
        root.append(&control.root);

        let bar = meter();
        root.append(&bar);

        let port_line = line();
        port_line.append(&gtk::Label::new(Some("Port")));
        port_line.append(&ports.dropdown);
        root.append(&port_line);

        let preview = preview.map(|handlers| {
            let (controls, widget) = PreviewControls::new(info.index, &info.description, handlers);
            root.append(&widget);
            controls
        });

        let details_text = wrapped_label("");
        details_text.set_selectable(true);
        let details = gtk::Expander::builder().label("Details").child(&details_text).build();
        root.append(&details);

        let row = Rc::new(Self {
            row: row_with(&root),
            info: RefCell::new(info.clone()),
            control,
            meter: bar,
            preview,
            target,
            is_input,
            icon,
            title,
            default_mark,
            default_button,
            port_line,
            ports,
            details,
            details_text,
        });
        let weak = Rc::downgrade(&row);
        row.default_button.connect_clicked(move |_| {
            if let Some(row) = weak.upgrade() {
                b4.set_default(row.target, &row.info.borrow().name);
            }
        });
        row.update(info);
        row
    }

    pub fn update(&self, info: DeviceInfo) {
        self.icon.set_icon_name(Some(info.icon(self.is_input)));
        self.title.set_text(&info.description);
        self.title.set_tooltip_text(Some(&info.description));
        self.control.update(info.volume, info.map, info.mute, true);

        let items = info
            .ports
            .iter()
            .map(|p| {
                let label = if p.unplugged {
                    format!("{} (unplugged)", p.description)
                } else {
                    p.description.clone()
                };
                Item { id: p.name.clone(), label, dimmed: p.unplugged }
            })
            .collect();
        self.ports.set_items(items, info.active_port.as_ref());
        self.port_line.set_visible(info.ports.len() > 1);

        self.details_text.set_text(&details_text(&info));
        if !info.running {
            self.meter.set_value(0.0);
        }
        *self.info.borrow_mut() = info;
    }

    pub fn set_default(&self, is_default: bool) {
        self.default_mark.set_visible(is_default);
        self.default_button.set_visible(!is_default);
    }

    pub fn set_details_visible(&self, visible: bool) {
        self.details.set_visible(visible);
    }
}

fn details_text(d: &DeviceInfo) -> String {
    let mut lines = detail_lines(&d.name, &d.driver, d.bus.as_deref(), d.codec.as_deref());
    lines.push(format!("Sample spec: {}", d.spec.print()));
    lines.push(format!("Latency: {:.1} ms", d.latency_usec as f64 / 1000.0));
    lines.join("\n")
}
