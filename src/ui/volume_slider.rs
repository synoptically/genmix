//! Volume control shared by every row: slider, percent, mute, balance, channels.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::glib::{self, ControlFlow};
use gtk::prelude::*;
use libpulse_binding::channelmap::{Map, Position};
use libpulse_binding::volume::{ChannelVolumes, Volume};

use super::{fitted_label, icon_toggle, line, set_accessible_name, SPACING};
use crate::backend::{from_percent, to_percent};

const SEND_INTERVAL: Duration = Duration::from_millis(30);
/// Ticks with no new value before the user counts as done. About 240 ms.
const QUIET_TICKS: u8 = 8;
const NORMAL_PERCENT: f64 = 100.0;

/// Sends only the latest value, at most once per `SEND_INTERVAL`.
pub struct Throttle<T> {
    pending: RefCell<Option<T>>,
    quiet: Cell<u8>,
    running: Cell<bool>,
    send: Box<dyn Fn(T)>,
    on_idle: Box<dyn Fn()>,
}

impl<T: 'static> Throttle<T> {
    pub fn new(send: impl Fn(T) + 'static, on_idle: impl Fn() + 'static) -> Rc<Self> {
        Rc::new(Self {
            pending: RefCell::new(None),
            quiet: Cell::new(0),
            running: Cell::new(false),
            send: Box::new(send),
            on_idle: Box::new(on_idle),
        })
    }

    pub fn push(self: &Rc<Self>, value: T) {
        *self.pending.borrow_mut() = Some(value);
        self.quiet.set(0);
        if !self.running.replace(true) {
            let weak = Rc::downgrade(self);
            glib::timeout_add_local(SEND_INTERVAL, move || {
                weak.upgrade().map_or(ControlFlow::Break, |t| t.tick())
            });
        }
    }

    pub fn busy(&self) -> bool {
        self.running.get()
    }

    fn tick(&self) -> ControlFlow {
        let value = self.pending.borrow_mut().take();
        if let Some(value) = value {
            (self.send)(value);
            return ControlFlow::Continue;
        }
        self.quiet.set(self.quiet.get() + 1);
        if self.quiet.get() < QUIET_TICKS {
            return ControlFlow::Continue;
        }
        self.running.set(false);
        (self.on_idle)();
        ControlFlow::Break
    }
}

/// Top of the slider. Never below the current value, so that a device already above
/// the limit shows its real volume and is not lowered.
pub fn slider_upper(limit: f64, current: f64) -> f64 {
    limit.max(current.ceil())
}

#[derive(Clone, Copy)]
struct ServerState {
    volume: ChannelVolumes,
    map: Map,
    mute: bool,
    writable: bool,
}

pub struct VolumeControl {
    pub root: gtk::Box,
    name: String,
    mute: gtk::ToggleButton,
    main: gtk::Scale,
    percent: gtk::Label,
    balance_line: gtk::Box,
    balance: gtk::Scale,
    unlock: gtk::ToggleButton,
    channel_box: gtk::Box,
    channels: RefCell<Vec<(gtk::Scale, gtk::Label)>>,
    volume: Cell<ChannelVolumes>,
    /// Channel ratio the main slider scales. Scaling the live value would lose the
    /// balance at 0, where all channels are equal, and drift at low volumes.
    shape: Cell<ChannelVolumes>,
    map: Cell<Map>,
    limit: Cell<f64>,
    upper: Cell<f64>,
    /// True while we move widgets ourselves, so their signals are not user input.
    updating: Cell<bool>,
    /// Latest server state received during a drag. Applying it then would pull the
    /// slider back to an older value.
    held: Cell<Option<ServerState>>,
    throttle: Rc<Throttle<ChannelVolumes>>,
}

impl VolumeControl {
    pub fn new(
        name: &str,
        limit: f64,
        on_volume: impl Fn(&ChannelVolumes) + 'static,
        on_mute: impl Fn(bool) + 'static,
    ) -> Rc<Self> {
        let control = Rc::new_cyclic(|weak: &Weak<Self>| {
            let weak = weak.clone();
            let throttle = Throttle::new(
                move |volume: ChannelVolumes| on_volume(&volume),
                move || {
                    if let Some(control) = weak.upgrade() {
                        match control.held.take() {
                            Some(state) => control.apply(state),
                            None => control.quietly(|| control.apply_range()),
                        }
                    }
                },
            );
            Self {
                root: gtk::Box::new(gtk::Orientation::Vertical, SPACING),
                name: name.to_string(),
                mute: icon_toggle("audio-volume-muted-symbolic", "Mute"),
                main: level_scale(&format!("{name} volume")),
                percent: gtk::Label::builder().width_chars(5).xalign(1.0).build(),
                balance_line: line(),
                balance: gtk::Scale::with_range(gtk::Orientation::Horizontal, -1.0, 1.0, 0.05),
                unlock: icon_toggle("changes-allow-symbolic", "Edit channels separately"),
                channel_box: gtk::Box::new(gtk::Orientation::Vertical, SPACING),
                channels: RefCell::default(),
                volume: Cell::new(ChannelVolumes::default()),
                shape: Cell::new(ChannelVolumes::default()),
                map: Cell::new(Map::default()),
                limit: Cell::new(limit),
                upper: Cell::new(0.0),
                updating: Cell::new(false),
                held: Cell::new(None),
                throttle,
            }
        });
        control.layout();
        control.connect(on_mute);
        control
    }

    /// Acts as the user dragging the main slider.
    #[cfg(test)]
    pub fn drag_to(&self, percent: f64) {
        self.main.set_value(percent);
    }

    #[cfg(test)]
    pub fn channel_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut row = self.channel_box.first_child();
        while let Some(line) = row {
            if let Some(label) = line.first_child().and_downcast::<gtk::Label>() {
                names.push(label.text().to_string());
            }
            row = line.next_sibling();
        }
        names
    }

    #[cfg(test)]
    pub fn shown(&self) -> (f64, f64) {
        (self.main.value(), self.main.adjustment().upper())
    }

    fn layout(&self) {
        let top = line();
        top.append(&self.mute);
        top.append(&self.main);
        top.append(&self.percent);
        top.append(&self.unlock);
        self.root.append(&top);

        self.balance.set_draw_value(false);
        self.balance.set_hexpand(true);
        self.balance.add_mark(0.0, gtk::PositionType::Bottom, None);
        set_accessible_name(&self.balance, &format!("{} balance", self.name));
        self.balance_line.append(&gtk::Label::new(Some("L")));
        self.balance_line.append(&self.balance);
        self.balance_line.append(&gtk::Label::new(Some("R")));
        self.root.append(&self.balance_line);

        self.channel_box.set_visible(false);
        self.root.append(&self.channel_box);
    }

    fn connect(self: &Rc<Self>, on_mute: impl Fn(bool) + 'static) {
        let weak = Rc::downgrade(self);
        self.main.connect_value_changed(move |scale| {
            with_user(&weak, |c| {
                let mut volume = c.shape.get();
                volume.scale(from_percent(scale.value()));
                c.show_and_send(volume);
            });
        });
        let weak = Rc::downgrade(self);
        self.balance.connect_value_changed(move |scale| {
            with_user(&weak, |c| {
                let mut volume = c.volume.get();
                volume.set_balance(&c.map.get(), scale.value() as f32);
                c.user_changed(volume);
            });
        });
        let weak = Rc::downgrade(self);
        self.mute.connect_toggled(move |button| {
            with_user(&weak, |_| on_mute(button.is_active()));
        });
        let weak = Rc::downgrade(self);
        self.unlock.connect_toggled(move |_| {
            if let Some(c) = weak.upgrade() {
                c.show_channel_mode();
            }
        });
    }

    /// Takes a new state from the server.
    pub fn update(self: &Rc<Self>, volume: ChannelVolumes, map: Map, mute: bool, writable: bool) {
        let state = ServerState { volume, map, mute, writable };
        if self.throttle.busy() {
            self.held.set(Some(state));
        } else {
            self.apply(state);
        }
    }

    pub fn set_limit(&self, limit: f64) {
        self.limit.set(limit);
        if !self.throttle.busy() {
            self.quietly(|| self.apply_range());
        }
    }

    fn apply(self: &Rc<Self>, state: ServerState) {
        self.volume.set(state.volume);
        self.remember_shape(state.volume);
        let remapped = self.map.replace(state.map) != state.map;
        self.quietly(|| {
            self.mute.set_active(state.mute);
            self.ensure_channels(state.volume.len() as usize, remapped);
            self.apply_range();
            self.show_values();
            self.main.set_sensitive(state.writable);
            self.balance.set_sensitive(state.writable);
            self.channel_box.set_sensitive(state.writable);
        });
        self.show_channel_mode();
    }

    fn quietly(&self, f: impl FnOnce()) {
        self.updating.set(true);
        f();
        self.updating.set(false);
    }

    fn remember_shape(&self, volume: ChannelVolumes) {
        if volume.max() > Volume::MUTED || volume.len() != self.shape.get().len() {
            self.shape.set(volume);
        }
    }

    /// For a change that sets a new channel ratio.
    fn user_changed(&self, volume: ChannelVolumes) {
        self.remember_shape(volume);
        self.show_and_send(volume);
    }

    fn show_and_send(&self, volume: ChannelVolumes) {
        self.volume.set(volume);
        self.quietly(|| self.show_values());
        self.throttle.push(volume);
    }

    fn ensure_channels(self: &Rc<Self>, count: usize, remapped: bool) {
        let map = self.map.get();
        let mut channels = self.channels.borrow_mut();
        if channels.len() == count && !remapped {
            return;
        }
        while let Some(child) = self.channel_box.first_child() {
            self.channel_box.remove(&child);
        }
        channels.clear();
        for (i, position) in map.get().iter().take(count).enumerate() {
            let label_text = Position::to_pretty_string(*position).unwrap_or_else(|| format!("Channel {}", i + 1));
            let scale = level_scale(&format!("{} {label_text}", self.name));
            let name = fitted_label(None);
            name.set_text(&label_text);
            name.set_hexpand(false);
            name.set_width_chars(10);
            let percent = gtk::Label::builder().width_chars(5).xalign(1.0).build();
            let row = line();
            row.append(&name);
            row.append(&scale);
            row.append(&percent);
            self.channel_box.append(&row);

            let weak = Rc::downgrade(self);
            scale.connect_value_changed(move |scale| {
                with_user(&weak, |c| {
                    let mut volume = c.volume.get();
                    if let Some(slot) = volume.get_mut().get_mut(i) {
                        *slot = from_percent(scale.value());
                    }
                    c.user_changed(volume);
                });
            });
            channels.push((scale, percent));
        }
        self.upper.set(0.0);
    }

    fn apply_range(&self) {
        let upper = slider_upper(self.limit.get(), to_percent(self.volume.get().max()));
        if upper == self.upper.replace(upper) {
            return;
        }
        let channels = self.channels.borrow();
        for scale in std::iter::once(&self.main).chain(channels.iter().map(|(s, _)| s)) {
            scale.set_range(0.0, upper);
            scale.clear_marks();
            if upper > NORMAL_PERCENT {
                scale.add_mark(NORMAL_PERCENT, gtk::PositionType::Bottom, Some("100%"));
            }
        }
    }

    fn show_values(&self) {
        let volume = self.volume.get();
        show_level(&self.main, &self.percent, to_percent(volume.max()));
        self.balance.set_value(f64::from(volume.get_balance(&self.map.get())));
        for ((scale, percent), v) in self.channels.borrow().iter().zip(volume.get()) {
            show_level(scale, percent, to_percent(*v));
        }
    }

    fn show_channel_mode(&self) {
        let map = self.map.get();
        let unlocked = self.unlock.is_active();
        self.unlock.set_visible(map.len() > 1);
        self.balance_line.set_visible(map.can_balance() && !unlocked);
        self.channel_box.set_visible(map.len() > 1 && unlocked);
    }
}

fn with_user(weak: &Weak<VolumeControl>, f: impl FnOnce(&Rc<VolumeControl>)) {
    if let Some(control) = weak.upgrade() {
        if !control.updating.get() {
            f(&control);
        }
    }
}

fn level_scale(name: &str) -> gtk::Scale {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, NORMAL_PERCENT, 1.0);
    scale.set_draw_value(false);
    scale.set_hexpand(true);
    set_accessible_name(&scale, name);
    scale
}

fn show_level(scale: &gtk::Scale, label: &gtk::Label, percent: f64) {
    scale.set_value(percent);
    label.set_text(&format!("{percent:.0}%"));
    if percent > NORMAL_PERCENT + 0.5 {
        scale.add_css_class("overamp");
    } else {
        scale.remove_css_class("overamp");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_never_hides_a_louder_device() {
        assert_eq!(slider_upper(100.0, 50.0), 100.0);
        assert_eq!(slider_upper(100.0, 130.0), 130.0);
        assert_eq!(slider_upper(150.0, 130.0), 150.0);
        assert_eq!(slider_upper(100.0, 130.4), 131.0);
    }

    // One test, because it needs the thread's default main context to itself.
    #[test]
    fn throttle_sends_latest_value_then_goes_idle() {
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let sent = Rc::new(RefCell::new(Vec::new()));
        let idle = Rc::new(Cell::new(false));
        let (s, i) = (Rc::clone(&sent), Rc::clone(&idle));
        let throttle = Throttle::new(move |v: u32| s.borrow_mut().push(v), move || i.set(true));

        for value in 0..100 {
            throttle.push(value);
        }
        assert!(throttle.busy());
        assert!(sent.borrow().is_empty());
        while !idle.get() {
            context.iteration(true);
        }
        assert_eq!(*sent.borrow(), [99]);
        assert!(!throttle.busy());
    }
}
