//! One playback or recording stream.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

use super::picker::{Item, Picker};
use super::volume_slider::VolumeControl;
use super::{column, fitted_label, line, meter, row_with};
use crate::backend::{Backend, StreamInfo, Target};

const FALLBACK_ICON: &str = "application-x-executable-symbolic";

pub struct StreamRow {
    pub row: gtk::ListBoxRow,
    pub info: RefCell<StreamInfo>,
    pub control: Rc<VolumeControl>,
    pub meter: gtk::LevelBar,
    pub devices: Rc<Picker<u32>>,
    icon: gtk::Image,
    title: gtk::Label,
    subtitle: gtk::Label,
}

impl StreamRow {
    pub fn new(backend: &Backend, info: StreamInfo, target: Target, limit: f64) -> Rc<Self> {
        let (b1, b2, b3) = (backend.clone(), backend.clone(), backend.clone());
        let control = VolumeControl::new(
            &info.app_name,
            limit,
            move |volume| b1.set_volume(target, volume),
            move |mute| b2.set_mute(target, mute),
        );
        let devices = Picker::new(&format!("{} device", info.app_name), move |device| {
            b3.move_stream(target, device);
        });

        let icon = gtk::Image::new();
        icon.set_pixel_size(32);
        let title = fitted_label(Some("heading"));
        let subtitle = fitted_label(Some("dim-label"));
        let names = gtk::Box::new(gtk::Orientation::Vertical, 0);
        names.set_hexpand(true);
        names.append(&title);
        names.append(&subtitle);
        let header = line();
        header.append(&icon);
        header.append(&names);

        let bar = meter();
        let root = column();
        root.append(&header);
        root.append(&control.root);
        root.append(&bar);
        root.append(&devices.dropdown);

        let row = Rc::new(Self {
            row: row_with(&root),
            info: RefCell::new(info.clone()),
            control,
            meter: bar,
            devices,
            icon,
            title,
            subtitle,
        });
        row.update(info);
        row
    }

    pub fn update(&self, info: StreamInfo) {
        self.icon.set_icon_name(Some(&app_icon(&info)));
        self.title.set_text(&info.app_name);
        self.subtitle.set_text(&info.media_name);
        self.subtitle.set_tooltip_text(Some(&info.media_name));
        self.control.update(info.volume, info.map, info.mute, info.has_volume && info.volume_writable);
        if info.corked {
            self.meter.set_value(0.0);
        }
        *self.info.borrow_mut() = info;
    }

    /// `devices` is the list of sinks or sources this stream can move to.
    pub fn set_devices(&self, devices: Vec<Item<u32>>) {
        let current = self.info.borrow().device;
        self.devices.set_items(devices, Some(&current));
    }
}

fn app_icon(info: &StreamInfo) -> String {
    let display = gtk::gdk::Display::default();
    let theme = display.as_ref().map(gtk::IconTheme::for_display);
    [&info.icon_name, &info.binary]
        .into_iter()
        .flatten()
        .find(|name| theme.as_ref().is_some_and(|t| t.has_icon(name)))
        .cloned()
        .unwrap_or_else(|| FALLBACK_ICON.to_string())
}
