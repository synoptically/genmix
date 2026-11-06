//! Main window. Receives backend events and keeps the rows in step with them.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use gtk::glib;
use gtk::prelude::*;

use super::card_row::CardRow;
use super::device_row::{DeviceRow, PreviewHandlers};
use super::picker::Item;
use super::stream_row::StreamRow;
use super::{column, fitted_label, line, row_with, settings as settings_ui, wrapped_label};
use crate::backend::{Backend, CardInfo, DeviceInfo, Event, MeterSource, StreamInfo, Target};
use crate::settings::Settings;

#[cfg(test)]
mod live_tests;

const ERROR_TIMEOUT: Duration = Duration::from_secs(6);
/// How long a lost preview output gets to come back. See `preview_target_lost`.
const PREVIEW_RESUME_DELAY: Duration = Duration::from_secs(1);
/// A second loss this soon after a resume means retrying will not help.
const PREVIEW_RESUME_WINDOW: Duration = Duration::from_secs(5);

// Notebook page numbers, in the order the tabs are added.
const PLAYBACK: u32 = 0;
const RECORDING: u32 = 1;
const OUTPUTS: u32 = 2;
const INPUTS: u32 = 3;
const CARDS: u32 = 4;

const TABS: [(&str, &str); 5] = [
    ("Playback", "No apps are playing audio"),
    ("Recording", "No apps are recording audio"),
    ("Output Devices", "No output devices"),
    ("Input Devices", "No input devices"),
    ("Configuration", "No sound cards"),
];

type Rows<T> = RefCell<BTreeMap<u32, Rc<T>>>;

pub struct Window {
    window: gtk::ApplicationWindow,
    backend: Backend,
    settings: Rc<RefCell<Settings>>,
    notebook: gtk::Notebook,
    lists: Vec<gtk::ListBox>,
    reconnecting: gtk::Revealer,
    error: gtk::Revealer,
    error_text: gtk::Label,
    error_generation: Cell<u32>,
    sinks: Rows<DeviceRow>,
    sources: Rows<DeviceRow>,
    playback: Rows<StreamRow>,
    recording: Rows<StreamRow>,
    cards: Rows<CardRow>,
    mic_hints: RefCell<BTreeMap<u32, gtk::ListBoxRow>>,
    /// Source whose preview is waiting to see if its output comes back, and which
    /// loss that is. Each loss has a timer, and only the latest one may act.
    preview_waiting: Cell<Option<(u32, u64)>>,
    preview_loss_count: Cell<u64>,
    last_preview_resume: Cell<Option<Instant>>,
    /// Name of the sink the running preview plays to.
    preview_output: RefCell<Option<String>>,
    default_sink: RefCell<String>,
    default_source: RefCell<String>,
    weak: Weak<Self>,
}

impl Window {
    pub fn present(app: &gtk::Application) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("GenMix")
            .icon_name(crate::ICON_NAME)
            .default_width(720)
            .default_height(640)
            .build();
        let notebook = gtk::Notebook::builder().scrollable(true).vexpand(true).build();
        let lists: Vec<_> = TABS
            .iter()
            .map(|(title, empty)| {
                let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).build();
                let placeholder = gtk::Label::new(Some(empty));
                placeholder.add_css_class("dim-label");
                placeholder.set_margin_top(24);
                placeholder.set_margin_bottom(24);
                list.set_placeholder(Some(&placeholder));
                let scroller = gtk::ScrolledWindow::builder()
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .child(&list)
                    .build();
                notebook.append_page(&scroller, Some(&gtk::Label::new(Some(title))));
                list
            })
            .collect();

        let reconnecting_text = fitted_label(None);
        reconnecting_text.set_text("Reconnecting…");
        let reconnecting = banner(&reconnecting_text);
        reconnecting.set_reveal_child(true);
        let error_text = fitted_label(None);
        let error = banner(&error_text);

        let settings_button = gtk::Button::from_icon_name("emblem-system-symbolic");
        settings_button.set_tooltip_text(Some("Settings"));
        super::set_accessible_name(&settings_button, "Settings");
        let header = gtk::HeaderBar::new();
        header.pack_end(&settings_button);
        window.set_titlebar(Some(&header));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&reconnecting);
        root.append(&error);
        root.append(&notebook);
        window.set_child(Some(&root));

        let this = Rc::new_cyclic(|weak: &Weak<Self>| {
            let events = weak.clone();
            let backend = Backend::new(move |event| {
                if let Some(this) = events.upgrade() {
                    this.handle(event);
                }
            });
            Self {
                window,
                backend,
                settings: Rc::new(RefCell::new(Settings::load())),
                notebook,
                lists,
                reconnecting,
                error,
                error_text,
                error_generation: Cell::new(0),
                sinks: RefCell::default(),
                sources: RefCell::default(),
                playback: RefCell::default(),
                recording: RefCell::default(),
                cards: RefCell::default(),
                mic_hints: RefCell::default(),
                preview_waiting: Cell::new(None),
                preview_loss_count: Cell::new(0),
                last_preview_resume: Cell::new(None),
                preview_output: RefCell::new(None),
                default_sink: RefCell::default(),
                default_source: RefCell::default(),
                weak: weak.clone(),
            }
        });

        let weak = Rc::downgrade(&this);
        settings_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.open_settings();
            }
        });
        // The tab change and map state decide which meters run.
        let weak = Rc::downgrade(&this);
        this.notebook.connect_page_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.zero_meters();
                this.sync_meters();
            }
        });
        let weak = Rc::downgrade(&this);
        this.window.connect_map(move |_| sync_meters(&weak));
        let weak = Rc::downgrade(&this);
        this.window.connect_unmap(move |_| sync_meters(&weak));

        // The GTK application holds the window; the window closure below holds us.
        let keep = Rc::clone(&this);
        this.window.connect_close_request(move |_| {
            keep.backend.stop_preview();
            glib::Propagation::Proceed
        });

        if this.settings.borrow().dark_mode {
            super::apply_dark_mode(true);
        }
        this.backend.start();
        this.window.present();
        this
    }

    fn handle(&self, event: Event) {
        match event {
            // Peak events are by far the most common, so they return early.
            Event::Peak { meter, level } => return self.show_peak(meter, level),
            Event::PreviewLatency(usec) => return self.show_latency(usec),
            Event::Loaded | Event::Done { .. } => return,
            Event::Connected => self.reconnecting.set_reveal_child(false),
            Event::Disconnected => {
                self.reconnecting.set_reveal_child(true);
                self.clear_rows();
            }
            Event::Server { default_sink, default_source } => {
                let changed = *self.default_sink.borrow() != default_sink;
                *self.default_sink.borrow_mut() = default_sink;
                *self.default_source.borrow_mut() = default_source;
                self.mark_defaults();
                self.refresh_pickers();
                if changed {
                    self.recheck_default_preview();
                }
            }
            Event::Sink(info) => self.upsert_device(info, false),
            Event::Source(info) => self.upsert_device(info, true),
            Event::SinkInput(info) => self.upsert_stream(info, false),
            Event::SourceOutput(info) => self.upsert_stream(info, true),
            Event::Card(info) => self.upsert_card(info),
            Event::Removed(target) => self.remove(target),
            Event::PreviewStarted { source, sink } => {
                *self.preview_output.borrow_mut() = Some(sink);
                self.set_preview_on(source, true);
            }
            Event::PreviewStopped { source, reason } => {
                self.preview_output.borrow_mut().take();
                self.set_preview_on(source, false);
                if let Some(reason) = reason {
                    self.show_error(&reason);
                }
            }
            Event::PreviewTargetLost { source, sink } => self.preview_target_lost(source, sink),
            Event::Error(message) => self.show_error(&message),
            // Nothing to show. The sync below reopens the meter.
            Event::MeterLost => {}
        }
        self.sync_meters();
    }

    fn limit(&self) -> f64 {
        self.settings.borrow().limit()
    }

    fn upsert_device(&self, info: DeviceInfo, is_input: bool) {
        let rows = if is_input { &self.sources } else { &self.sinks };
        let existing = rows.borrow().get(&info.index).cloned();
        let index = info.index;
        let list_changed = match existing {
            Some(row) => {
                let renamed = row.info.borrow().description != info.description;
                row.update(info);
                renamed
            }
            None => {
                let preview = is_input.then(|| self.preview_handlers());
                let row = DeviceRow::new(&self.backend, info, is_input, self.limit(), preview);
                row.set_details_visible(self.settings.borrow().show_details);
                row.meter.set_visible(self.settings.borrow().show_meters);
                self.lists[(if is_input { INPUTS } else { OUTPUTS }) as usize].append(&row.row);
                rows.borrow_mut().insert(index, row);
                true
            }
        };
        if is_input {
            self.apply_source_visibility();
        }
        self.mark_defaults();
        if list_changed {
            self.refresh_pickers();
        }
        self.refresh_cards();
    }

    fn upsert_stream(&self, info: StreamInfo, is_recording: bool) {
        let rows = if is_recording { &self.recording } else { &self.playback };
        let existing = rows.borrow().get(&info.index).cloned();
        let row = match existing {
            Some(row) => {
                row.update(info);
                row
            }
            None => {
                let index = info.index;
                let target = if is_recording { Target::SourceOutput(index) } else { Target::SinkInput(index) };
                let row = StreamRow::new(&self.backend, info, target, self.limit());
                row.meter.set_visible(self.settings.borrow().show_meters);
                self.lists[(if is_recording { RECORDING } else { PLAYBACK }) as usize].append(&row.row);
                rows.borrow_mut().insert(index, Rc::clone(&row));
                row
            }
        };
        row.set_devices(device_items(if is_recording { &self.sources } else { &self.sinks }));
        let show_events = self.settings.borrow().show_event_streams;
        row.row.set_visible(show_events || !row.info.borrow().is_event);
    }

    fn upsert_card(&self, info: CardInfo) {
        let existing = self.cards.borrow().get(&info.index).cloned();
        match existing {
            Some(row) => row.update(info),
            None => {
                let index = info.index;
                let row = CardRow::new(&self.backend, info);
                row.set_details_visible(self.settings.borrow().show_details);
                self.lists[CARDS as usize].append(&row.row);
                self.cards.borrow_mut().insert(index, row);
            }
        }
        self.refresh_cards();
    }

    fn remove(&self, target: Target) {
        let (list, row) = match target {
            Target::Sink(i) => (OUTPUTS, self.sinks.borrow_mut().remove(&i).map(|r| r.row.clone())),
            Target::Source(i) => (INPUTS, self.sources.borrow_mut().remove(&i).map(|r| r.row.clone())),
            Target::SinkInput(i) => (PLAYBACK, self.playback.borrow_mut().remove(&i).map(|r| r.row.clone())),
            Target::SourceOutput(i) => (RECORDING, self.recording.borrow_mut().remove(&i).map(|r| r.row.clone())),
            Target::Card(i) => (CARDS, self.cards.borrow_mut().remove(&i).map(|r| r.row.clone())),
        };
        if let Some(row) = row {
            self.lists[list as usize].remove(&row);
        }
        if matches!(target, Target::Sink(_) | Target::Source(_)) {
            self.refresh_pickers();
        }
        if !matches!(target, Target::SinkInput(_) | Target::SourceOutput(_)) {
            self.refresh_cards();
        }
    }

    fn clear_rows(&self) {
        for list in &self.lists {
            list.remove_all();
        }
        self.sinks.borrow_mut().clear();
        self.sources.borrow_mut().clear();
        self.playback.borrow_mut().clear();
        self.recording.borrow_mut().clear();
        self.cards.borrow_mut().clear();
        self.mic_hints.borrow_mut().clear();
    }

    fn mark_defaults(&self) {
        let (sink, source) = (self.default_sink.borrow(), self.default_source.borrow());
        for row in self.sinks.borrow().values() {
            row.set_default(row.info.borrow().name == *sink);
        }
        for row in self.sources.borrow().values() {
            row.set_default(row.info.borrow().name == *source);
        }
    }

    /// Runs after the device list changes: everything that offers a device choice.
    fn refresh_pickers(&self) {
        let sinks = device_items(&self.sinks);
        let sources = device_items(&self.sources);
        for row in self.playback.borrow().values() {
            row.set_devices(sinks.clone());
        }
        for row in self.recording.borrow().values() {
            row.set_devices(sources.clone());
        }
        let default_label = self
            .default_sink_row()
            .map_or("Default output".to_string(), |r| format!("Default ({})", r.info.borrow().description));
        let mut targets = vec![Item::new(None, default_label)];
        targets.extend(sinks.into_iter().map(|i| Item::new(Some(i.id), i.label)));
        for row in self.sources.borrow().values() {
            if let Some(preview) = &row.preview {
                let chosen = preview.target.selected().flatten();
                let keep = chosen.filter(|id| self.sinks.borrow().contains_key(id));
                preview.target.set_items(targets.clone(), Some(&keep));
            }
        }
    }

    fn default_sink_row(&self) -> Option<Rc<DeviceRow>> {
        self.sink_named(&self.default_sink.borrow())
    }

    fn sink_named(&self, name: &str) -> Option<Rc<DeviceRow>> {
        self.sinks.borrow().values().find(|r| r.info.borrow().name == name).cloned()
    }

    /// Hands each card what its devices know, and updates the "mic is off" rows.
    fn refresh_cards(&self) {
        let (sinks, sources) = (self.sinks.borrow(), self.sources.borrow());
        for card in self.cards.borrow().values() {
            let info = card.info.borrow();
            let attached = |rows: &BTreeMap<u32, Rc<DeviceRow>>| -> Vec<DeviceInfo> {
                let on_card = rows.values().filter(|r| r.info.borrow().is_on_card(&info));
                on_card.map(|r| r.info.borrow().clone()).collect()
            };
            let card_sinks = attached(&sinks);
            let mut card_sources = attached(&sources);
            card_sources.retain(|d| d.monitor.is_none());
            let codec = card_sinks.iter().find_map(|d| d.codec.clone());
            let lines = card_sinks
                .iter()
                .map(|d| ("Output", d))
                .chain(card_sources.iter().map(|d| ("Input", d)))
                .map(|(kind, d)| {
                    format!("{kind}: {}, latency {:.1} ms", d.spec.print(), d.latency_usec as f64 / 1000.0)
                })
                .collect();
            let needs_hint = card_sources.is_empty() && info.headset_profile_for_mic().is_some();
            let (index, description) = (info.index, info.description.clone());
            drop(info);
            card.set_devices(codec, lines);
            self.sync_mic_hint(index, &description, needs_hint);
        }
        let cards = self.cards.borrow();
        let mut hints = self.mic_hints.borrow_mut();
        let list = &self.lists[INPUTS as usize];
        hints.retain(|index, row| {
            let keep = cards.contains_key(index);
            if !keep {
                list.remove(row);
            }
            keep
        });
    }

    /// Plain PulseAudio has no source at all in A2DP mode, so no row exists to press
    /// the preview toggle on. This row offers the same switch.
    fn sync_mic_hint(&self, card: u32, description: &str, needed: bool) {
        let list = &self.lists[INPUTS as usize];
        let mut hints = self.mic_hints.borrow_mut();
        match (needed, hints.contains_key(&card)) {
            (true, false) => {
                let text = fitted_label(None);
                text.set_text(&format!("{description}: microphone is off in this mode"));
                let button = gtk::Button::with_label("Switch to headset mode");
                let weak = self.weak.clone();
                button.connect_clicked(move |_| {
                    if let Some(this) = weak.upgrade() {
                        this.switch_to_headset(card);
                    }
                });
                let content = line();
                content.append(&text);
                content.append(&button);
                let outer = column();
                outer.append(&content);
                let row = row_with(&outer);
                list.append(&row);
                hints.insert(card, row);
            }
            (false, true) => {
                if let Some(row) = hints.remove(&card) {
                    list.remove(&row);
                }
            }
            _ => {}
        }
    }

    fn switch_to_headset(&self, card: u32) {
        let row = self.cards.borrow().get(&card).cloned();
        if let Some(row) = row {
            let info = row.info.borrow();
            if let Some(profile) = info.headset_profile_for_mic() {
                self.backend.set_profile(card, &profile.name);
            }
        }
    }

    fn preview_handlers(&self) -> PreviewHandlers {
        let (w1, w2) = (self.weak.clone(), self.weak.clone());
        let backend = self.backend.clone();
        PreviewHandlers {
            on_toggle: Box::new(move |source, on| {
                if let Some(this) = w1.upgrade() {
                    if on {
                        this.request_preview(source);
                    } else {
                        this.backend.stop_preview();
                    }
                }
            }),
            on_target_changed: Box::new(move |source| {
                if let Some(this) = w2.upgrade() {
                    this.retarget_preview(source);
                }
            }),
            on_volume: Box::new(move |source, percent| backend.set_preview_volume(source, percent)),
        }
    }

    fn retarget_preview(&self, source: u32) {
        let row = self.sources.borrow().get(&source).cloned();
        if row.and_then(|r| r.preview.clone()).is_some_and(|p| p.is_on()) {
            self.request_preview(source);
        }
    }

    /// The checks the user must see before audio flows, in order.
    fn request_preview(&self, source: u32) {
        let Some(row) = self.sources.borrow().get(&source).cloned() else { return };

        if !self.mic_ready(source) {
            self.cancel_preview(source);
            if let Some(card) = row.info.borrow().card {
                self.ask_headset_switch(card);
            }
            return;
        }

        let Some(sink) = self.preview_sink(&row) else { return self.no_preview_output(source) };
        if sink.is_speaker_type() && !self.settings.borrow().skip_speaker_warning {
            return self.ask_speaker_ok(source, sink.name);
        }
        self.start_preview(source);
    }

    fn preview_sink(&self, row: &DeviceRow) -> Option<DeviceInfo> {
        let chosen = row.preview.as_ref()?.target.selected().flatten();
        let sink = match chosen {
            Some(index) => self.sinks.borrow().get(&index).cloned(),
            None => self.default_sink_row(),
        };
        sink.map(|s| s.info.borrow().clone())
    }

    fn start_preview(&self, source: u32) {
        let Some(row) = self.sources.borrow().get(&source).cloned() else { return };
        match self.preview_sink(&row) {
            Some(sink) => self.start_preview_on(&row, &sink),
            None => self.no_preview_output(source),
        }
    }

    /// A retarget gets here with the old preview still playing, so stopping it is part
    /// of turning the toggle off. Another source's preview is left alone.
    fn cancel_preview(&self, source: u32) {
        self.backend.stop_preview_of(source);
        self.set_preview_on(source, false);
    }

    fn no_preview_output(&self, source: u32) {
        self.cancel_preview(source);
        self.show_error("No output device to preview to");
    }

    fn start_preview_on(&self, row: &DeviceRow, sink: &DeviceInfo) {
        let Some(preview) = &row.preview else { return };
        let info = row.info.borrow().clone();
        if let Some((waiting, _)) = self.preview_waiting.take().filter(|w| w.0 != info.index) {
            self.set_preview_on(waiting, false);
        }
        self.backend.start_preview(&info, sink, preview.volume());
    }

    /// PipeWire can rebuild a Bluetooth sink in place when the headset's mic transport
    /// starts. That is not an unplug, so the toggle stays on for a moment. Decide after
    /// the delay, not when the sink returns: a switch to A2DP also brings back a sink of
    /// that name, and recording then would make WirePlumber switch back to the headset
    /// profile. By then the card shows the new profile, and `mic_ready` is false.
    /// Resume only on that same sink. "Default" may mean a speaker by now, and that
    /// output never passed the feedback warning.
    fn preview_target_lost(&self, source: u32, sink: String) {
        self.preview_output.borrow_mut().take();
        let loss = self.preview_loss_count.get() + 1;
        self.preview_loss_count.set(loss);
        self.preview_waiting.set(Some((source, loss)));
        // The removal that follows this event resets the picker to "Default".
        let row = self.sources.borrow().get(&source).cloned();
        let preview = row.and_then(|r| r.preview.clone());
        let was_chosen = preview.is_some_and(|p| p.target.selected().flatten().is_some());
        let weak = self.weak.clone();
        glib::timeout_add_local_once(PREVIEW_RESUME_DELAY, move || {
            let Some(this) = weak.upgrade() else { return };
            if this.preview_waiting.get() != Some((source, loss)) {
                return;
            }
            this.preview_waiting.set(None);
            let Some(row) = this.sources.borrow().get(&source).cloned() else {
                return this.show_error("Preview stopped: device was removed");
            };
            let Some(preview) = row.preview.clone().filter(|p| p.is_on()) else { return };
            let back = this.sink_named(&sink);
            let retried = this.last_preview_resume.get();
            let may_retry = retried.is_none_or(|t| t.elapsed() > PREVIEW_RESUME_WINDOW);
            match back.filter(|_| may_retry && this.mic_ready(source)) {
                Some(sink) => {
                    this.last_preview_resume.set(Some(Instant::now()));
                    let info = sink.info.borrow().clone();
                    if was_chosen {
                        preview.target.select(&Some(info.index));
                    }
                    this.start_preview_on(&row, &info);
                }
                None => {
                    this.set_preview_on(source, false);
                    this.show_error("Preview stopped: device was removed");
                }
            }
        });
    }

    /// A preview on "Default" does not follow a new default: it could land on a speaker
    /// that never passed the warning. It stops, so the label cannot name the wrong
    /// output. The delay skips the short flip while PipeWire rebuilds a Bluetooth sink.
    fn recheck_default_preview(&self) {
        let weak = self.weak.clone();
        glib::timeout_add_local_once(PREVIEW_RESUME_DELAY, move || {
            let Some(this) = weak.upgrade() else { return };
            let output = this.preview_output.borrow().clone();
            if output.is_none_or(|o| o == *this.default_sink.borrow()) {
                return;
            }
            let sources = this.sources.borrow();
            let on_default = sources.iter().find(|(_, r)| {
                let preview = r.preview.as_ref();
                preview.is_some_and(|p| p.is_on() && p.target.selected() == Some(None))
            });
            if let Some((index, _)) = on_default {
                let index = *index;
                drop(sources);
                this.backend.stop_preview_of(index);
                this.show_error("Preview stopped: the default output changed");
            }
        });
    }

    /// False when the source is a mic that its card's profile has turned off.
    fn mic_ready(&self, source: u32) -> bool {
        let Some(row) = self.sources.borrow().get(&source).cloned() else { return false };
        let info = row.info.borrow();
        let cards = self.cards.borrow();
        !cards.values().any(|c| info.mic_is_off(&c.info.borrow()))
    }

    fn set_preview_on(&self, source: u32, on: bool) {
        let row = self.sources.borrow().get(&source).cloned();
        if let Some(preview) = row.and_then(|r| r.preview.clone()) {
            preview.set_on(on);
        }
    }

    fn show_latency(&self, usec: Option<u64>) {
        for row in self.sources.borrow().values() {
            if let Some(preview) = row.preview.as_ref().filter(|p| p.is_on()) {
                preview.set_latency(usec);
            }
        }
    }

    fn ask_headset_switch(&self, card: u32) {
        let weak = self.weak.clone();
        self.ask(
            "Switch to headset mode to use the microphone",
            "Switch to headset mode",
            None,
            Box::new(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.switch_to_headset(card);
                }
            }),
            None,
        );
    }

    /// Accepting starts on `sink`, the output the question was about, even if "Default"
    /// has come to mean something else while the question was open.
    fn ask_speaker_ok(&self, source: u32, sink: String) {
        let (accept, cancel) = (self.weak.clone(), self.weak.clone());
        self.ask(
            "Using speakers may cause feedback. Use headphones?",
            "Start preview",
            Some("Don't ask again"),
            Box::new(move |dont_ask| {
                if let Some(this) = accept.upgrade() {
                    if dont_ask {
                        this.settings.borrow_mut().skip_speaker_warning = true;
                        this.save_settings();
                    }
                    let row = this.sources.borrow().get(&source).cloned();
                    let target = this.sink_named(&sink).map(|s| s.info.borrow().clone());
                    match (row, target) {
                        (Some(row), Some(target)) => this.start_preview_on(&row, &target),
                        _ => this.no_preview_output(source),
                    }
                }
            }),
            Some(Box::new(move || {
                if let Some(this) = cancel.upgrade() {
                    this.cancel_preview(source);
                }
            })),
        );
    }

    /// Small modal question. `gtk::AlertDialog` has no place for a check box.
    fn ask(
        &self,
        message: &str,
        accept_label: &str,
        check_label: Option<&str>,
        on_accept: Box<dyn Fn(bool)>,
        on_cancel: Option<Box<dyn Fn()>>,
    ) {
        let dialog = gtk::Window::builder()
            .title("GenMix")
            .transient_for(&self.window)
            .modal(true)
            .default_width(360)
            .build();
        let root = column();
        root.append(&wrapped_label(message));
        let check = check_label.map(gtk::CheckButton::with_label);
        if let Some(check) = &check {
            root.append(check);
        }
        let cancel = gtk::Button::with_label("Cancel");
        let accept = gtk::Button::with_label(accept_label);
        let buttons = line();
        buttons.set_halign(gtk::Align::End);
        buttons.append(&cancel);
        buttons.append(&accept);
        root.append(&buttons);
        dialog.set_child(Some(&root));

        let accepted = Rc::new(Cell::new(false));
        let (d, flag) = (dialog.clone(), Rc::clone(&accepted));
        accept.connect_clicked(move |_| {
            flag.set(true);
            on_accept(check.as_ref().is_some_and(|c| c.is_active()));
            d.close();
        });
        let d = dialog.clone();
        cancel.connect_clicked(move |_| d.close());
        // Closing by any route other than Accept counts as Cancel.
        dialog.connect_close_request(move |_| {
            if !accepted.get() {
                if let Some(on_cancel) = &on_cancel {
                    on_cancel();
                }
            }
            glib::Propagation::Proceed
        });
        dialog.present();
    }

    fn show_peak(&self, meter: Target, level: f32) {
        // A peak can pass 1.0 on a float source or above 100% volume. The bar ends at 1.0.
        let value = f64::from(level.max(0.0)).min(1.0);
        match meter {
            Target::Sink(i) => {
                if let Some(row) = self.sinks.borrow().get(&i) {
                    row.meter.set_value(value);
                }
            }
            Target::SinkInput(i) => {
                if let Some(row) = self.playback.borrow().get(&i) {
                    row.meter.set_value(value);
                }
            }
            Target::Source(i) => {
                if let Some(row) = self.sources.borrow().get(&i) {
                    row.meter.set_value(value);
                }
                for row in self.recording.borrow().values() {
                    if row.info.borrow().device == i {
                        row.meter.set_value(value);
                    }
                }
            }
            Target::SourceOutput(_) | Target::Card(_) => {}
        }
    }

    fn zero_meters(&self) {
        for row in self.sinks.borrow().values().chain(self.sources.borrow().values()) {
            row.meter.set_value(0.0);
        }
        for row in self.playback.borrow().values().chain(self.recording.borrow().values()) {
            row.meter.set_value(0.0);
        }
    }

    /// Asks the backend for meters on what is on screen now, and nothing else.
    fn sync_meters(&self) {
        let mut wanted = HashMap::new();
        if self.settings.borrow().show_meters && self.window.is_mapped() {
            let sinks = self.sinks.borrow();
            let page = self.notebook.current_page();
            match page {
                Some(PLAYBACK) => {
                    for (index, row) in self.playback.borrow().iter() {
                        let info = row.info.borrow();
                        let monitor = sinks.get(&info.device).and_then(|s| s.info.borrow().monitor);
                        if let (Some(source), false, true) = (monitor, info.corked, row.row.is_visible()) {
                            let from = MeterSource { source, sink_input: Some(*index), wake: false };
                            wanted.insert(Target::SinkInput(*index), from);
                        }
                    }
                }
                Some(RECORDING) => {
                    for row in self.recording.borrow().values() {
                        let info = row.info.borrow();
                        if !info.corked && row.row.is_visible() {
                            let from = MeterSource { source: info.device, sink_input: None, wake: false };
                            wanted.insert(Target::Source(info.device), from);
                        }
                    }
                }
                Some(OUTPUTS) => {
                    for (index, row) in sinks.iter() {
                        if let Some(source) = row.info.borrow().monitor {
                            let from = MeterSource { source, sink_input: None, wake: false };
                            wanted.insert(Target::Sink(*index), from);
                        }
                    }
                }
                Some(INPUTS) => {
                    for (index, row) in self.sources.borrow().iter() {
                        if row.row.is_visible() {
                            let wake = row.info.borrow().monitor.is_none();
                            let from = MeterSource { source: *index, sink_input: None, wake };
                            wanted.insert(Target::Source(*index), from);
                        }
                    }
                }
                _ => {}
            }
        }
        self.backend.set_meters(wanted);
    }

    fn apply_source_visibility(&self) {
        let show = self.settings.borrow().show_monitors;
        for row in self.sources.borrow().values() {
            row.row.set_visible(show || row.info.borrow().monitor.is_none());
        }
    }

    fn open_settings(&self) {
        let weak = self.weak.clone();
        settings_ui::show(
            &self.window,
            &self.settings,
            Rc::new(move || {
                if let Some(this) = weak.upgrade() {
                    this.settings_changed();
                }
            }),
        );
    }

    fn settings_changed(&self) {
        self.save_settings();
        let s = self.settings.borrow().clone();
        let limit = s.limit();
        self.apply_source_visibility();
        // A hidden row has no toggle left to stop its preview with.
        for (index, _) in self.sources.borrow().iter().filter(|(_, r)| !r.row.is_visible()) {
            self.cancel_preview(*index);
        }
        for row in self.sinks.borrow().values().chain(self.sources.borrow().values()) {
            row.control.set_limit(limit);
            row.meter.set_visible(s.show_meters);
            row.set_details_visible(s.show_details);
        }
        for row in self.playback.borrow().values().chain(self.recording.borrow().values()) {
            row.control.set_limit(limit);
            row.meter.set_visible(s.show_meters);
            row.row.set_visible(s.show_event_streams || !row.info.borrow().is_event);
        }
        for row in self.cards.borrow().values() {
            row.set_details_visible(s.show_details);
        }
        self.sync_meters();
    }

    fn save_settings(&self) {
        let result = self.settings.borrow().save();
        if let Err(error) = result {
            self.show_error(&format!("Could not save settings: {error}"));
        }
    }

    fn show_error(&self, message: &str) {
        self.error_text.set_text(message);
        self.error.set_reveal_child(true);
        let generation = self.error_generation.get() + 1;
        self.error_generation.set(generation);
        let weak = self.weak.clone();
        glib::timeout_add_local_once(ERROR_TIMEOUT, move || {
            if let Some(this) = weak.upgrade() {
                if this.error_generation.get() == generation {
                    this.error.set_reveal_child(false);
                }
            }
        });
    }
}

fn sync_meters(weak: &Weak<Window>) {
    if let Some(this) = weak.upgrade() {
        this.sync_meters();
    }
}

fn device_items(rows: &Rows<DeviceRow>) -> Vec<Item<u32>> {
    rows.borrow()
        .iter()
        .map(|(index, row)| Item::new(*index, row.info.borrow().description.clone()))
        .collect()
}

fn banner(content: &gtk::Label) -> gtk::Revealer {
    let bar = line();
    bar.add_css_class("banner");
    bar.append(content);
    gtk::Revealer::builder().child(&bar).build()
}
