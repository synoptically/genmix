//! One sound card: grouped profile list and details.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;

use super::{column, detail_lines, fitted_label, line, row_with, wrapped_label};
use crate::backend::{parse_codec, profile_group, profile_label, Backend, CardInfo, ProfileGroup};

/// Cards with more profiles than this start collapsed. HDMI cards have dozens.
const MAX_EXPANDED_PROFILES: usize = 12;

struct Radio {
    profile: String,
    button: gtk::CheckButton,
    label: gtk::Label,
}

pub struct CardRow {
    pub row: gtk::ListBoxRow,
    pub info: RefCell<CardInfo>,
    backend: Backend,
    icon: gtk::Image,
    title: gtk::Label,
    active_codec: gtk::Label,
    profiles: gtk::Expander,
    profile_box: gtk::Box,
    radios: RefCell<Vec<Radio>>,
    /// Profiles the radios were built from. Same list, same radios.
    shown: RefCell<Vec<crate::backend::ProfileInfo>>,
    sink_codec: RefCell<Option<String>>,
    device_lines: RefCell<Vec<String>>,
    details: gtk::Expander,
    details_text: gtk::Label,
    updating: Cell<bool>,
}

impl CardRow {
    pub fn new(backend: &Backend, info: CardInfo) -> Rc<Self> {
        let root = column();
        let icon = gtk::Image::new();
        let title = fitted_label(Some("heading"));
        let active_codec = gtk::Label::new(None);
        let header = line();
        header.append(&icon);
        header.append(&title);
        header.append(&active_codec);
        root.append(&header);

        let profile_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let profiles = gtk::Expander::builder().child(&profile_box).build();
        root.append(&profiles);

        let details_text = wrapped_label("");
        details_text.set_selectable(true);
        let details = gtk::Expander::builder().label("Details").child(&details_text).build();
        root.append(&details);

        let row = Rc::new(Self {
            row: row_with(&root),
            info: RefCell::new(info.clone()),
            backend: backend.clone(),
            icon,
            title,
            active_codec,
            profiles,
            profile_box,
            radios: RefCell::default(),
            shown: RefCell::default(),
            sink_codec: RefCell::new(None),
            device_lines: RefCell::default(),
            details,
            details_text,
            updating: Cell::new(false),
        });
        row.update(info);
        row
    }

    pub fn update(self: &Rc<Self>, info: CardInfo) {
        self.title.set_text(&info.description);
        self.title.set_tooltip_text(Some(&info.name));
        self.icon.set_icon_name(Some(card_icon(&info)));
        if *self.shown.borrow() != info.profiles {
            self.rebuild_radios(&info);
        }
        *self.info.borrow_mut() = info;
        self.refresh();
    }

    /// Takes what the card's devices report: the codec and one line per device.
    pub fn set_devices(&self, sink_codec: Option<String>, device_lines: Vec<String>) {
        *self.sink_codec.borrow_mut() = sink_codec;
        *self.device_lines.borrow_mut() = device_lines;
        self.refresh();
    }

    pub fn set_details_visible(&self, visible: bool) {
        self.details.set_visible(visible);
    }

    fn rebuild_radios(self: &Rc<Self>, info: &CardInfo) {
        let first_build = self.radios.borrow().is_empty();
        while let Some(child) = self.profile_box.first_child() {
            self.profile_box.remove(&child);
        }
        let mut radios = Vec::with_capacity(info.profiles.len());
        let mut leader: Option<gtk::CheckButton> = None;
        for group in ProfileGroup::ALL {
            let members = || info.profiles.iter().filter(|p| profile_group(&p.name) == group);
            if members().next().is_none() {
                continue;
            }
            let heading = wrapped_label(group.title());
            heading.add_css_class("heading");
            heading.set_margin_top(6);
            self.profile_box.append(&heading);

            let mut sorted: Vec<_> = members().collect();
            sorted.sort_by_key(|p| std::cmp::Reverse(p.priority));
            for profile in sorted {
                let label = wrapped_label("");
                let button = gtk::CheckButton::builder().child(&label).build();
                button.set_group(leader.as_ref());
                leader.get_or_insert_with(|| button.clone());
                if !profile.available {
                    button.add_css_class("dim-label");
                }
                let (weak, name) = (Rc::downgrade(self), profile.name.clone());
                button.connect_toggled(move |button| {
                    let Some(row) = weak.upgrade() else { return };
                    if button.is_active() && !row.updating.get() {
                        let index = row.info.borrow().index;
                        row.backend.set_profile(index, &name);
                    }
                });
                self.profile_box.append(&button);
                radios.push(Radio { profile: profile.name.clone(), button, label });
            }
        }
        *self.radios.borrow_mut() = radios;
        *self.shown.borrow_mut() = info.profiles.clone();
        self.profiles.set_label(Some(&format!("Profiles ({})", info.profiles.len())));
        // Later rebuilds come from availability changes and must not undo the user's choice.
        if first_build {
            self.profiles.set_expanded(info.profiles.len() <= MAX_EXPANDED_PROFILES);
        }
    }

    #[cfg(test)]
    pub fn profiles_expander(&self) -> gtk::Expander {
        self.profiles.clone()
    }

    /// Acts as the user pressing a profile's radio button.
    #[cfg(test)]
    pub fn pick(&self, profile: &str) {
        if let Some(radio) = self.radios.borrow().iter().find(|r| r.profile == profile) {
            radio.button.set_active(true);
        }
    }

    #[cfg(test)]
    pub fn label_of(&self, profile: &str) -> Option<String> {
        let radios = self.radios.borrow();
        radios.iter().find(|r| r.profile == profile).map(|r| r.label.text().to_string())
    }

    /// Redraws everything that depends on the active profile and its codec.
    fn refresh(&self) {
        let info = self.info.borrow();
        let sink_codec = self.sink_codec.borrow();
        let active = info.active_profile.as_deref();
        let known_codec = info.codec.as_deref().or(sink_codec.as_deref());

        self.updating.set(true);
        for radio in self.radios.borrow().iter() {
            let is_active = active == Some(radio.profile.as_str());
            let Some(profile) = info.profiles.iter().find(|p| p.name == radio.profile) else {
                continue;
            };
            let mut text = profile_label(profile, known_codec.filter(|_| is_active));
            text.push_str(&format!(" (priority {})", profile.priority));
            if !profile.available {
                text.push_str(" (unavailable)");
            }
            radio.label.set_text(&text);
            radio.button.set_active(is_active);
        }
        self.updating.set(false);

        let active_profile = info.profiles.iter().find(|p| Some(p.name.as_str()) == active);
        let codec = active_profile
            .and_then(|p| parse_codec(&p.name, &p.description))
            .or_else(|| known_codec.map(str::to_uppercase));
        self.active_codec.set_text(&codec.as_ref().map_or(String::new(), |c| format!("Codec: {c}")));

        let mut lines = detail_lines(&info.name, &info.driver, info.bus.as_deref(), codec.as_deref());
        lines.extend(self.device_lines.borrow().iter().cloned());
        self.details_text.set_text(&lines.join("\n"));
    }
}

fn card_icon(card: &CardInfo) -> &'static str {
    match card.form_factor.as_deref() {
        Some("headset" | "hands-free" | "handset") => "audio-headset-symbolic",
        Some("headphone") => "audio-headphones-symbolic",
        _ if card.is_bluetooth() => "bluetooth-symbolic",
        _ if card.description.contains("HDMI") => "video-display-symbolic",
        _ => "audio-card-symbolic",
    }
}
