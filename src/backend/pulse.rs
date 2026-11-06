//! Connection to the server, live object updates, and all commands.
//!
//! Everything runs on the GLib main context. Rules that keep it safe:
//! - No `RefCell` borrow is held across a call that can fire a callback at once.
//! - Streams and contexts are never closed from inside their own callbacks.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;
use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::introspect as pa;
use libpulse_binding::context::subscribe::{Facility, InterestMaskSet, Operation};
use libpulse_binding::context::{Context, FlagSet, State};
use libpulse_binding::def::{PortAvailable, SinkState, SourceState};
use libpulse_binding::error::Code;
use libpulse_binding::proplist::{properties, Proplist};
use libpulse_binding::volume::{ChannelVolumes, Volume};
use libpulse_glib_binding::Mainloop;

use super::meter::{self, Meter, MeterSource};
use super::preview::{self, Preview};
use super::types::{CardInfo, DeviceInfo, PortInfo, ProfileInfo, SinkInfo, SourceInfo, StreamInfo};
use super::{Event, Target};

const RETRY_DELAY: Duration = Duration::from_secs(1);

/// Handle to the audio server. Cheap to clone.
#[derive(Clone)]
pub struct Backend(Rc<Inner>);

// Callbacks hold strong references to Inner. That cycle is fine: the backend lives
// until the process exits.
struct Inner {
    mainloop: Mainloop,
    context: RefCell<Option<Context>>,
    emit: Rc<dyn Fn(Event)>,
    meters: RefCell<HashMap<Target, Meter>>,
    preview: RefCell<Option<Preview>>,
    /// Tells a late "stream ended" notice from an old preview apart from the current one.
    preview_generation: Cell<u64>,
    preview_volume: Cell<Volume>,
    own_client: Cell<Option<u32>>,
}

impl Backend {
    /// Does not connect yet; call `start`.
    pub fn new(emit: impl Fn(Event) + 'static) -> Self {
        Self(Rc::new(Inner {
            // Fails only when out of memory. No server is involved yet.
            mainloop: Mainloop::new(None).expect("create libpulse mainloop"),
            context: RefCell::new(None),
            emit: Rc::new(emit),
            meters: RefCell::default(),
            preview: RefCell::new(None),
            preview_generation: Cell::new(0),
            preview_volume: Cell::new(Volume::NORMAL),
            own_client: Cell::new(None),
        }))
    }

    pub fn start(&self) {
        self.0.connect();
    }

    pub fn set_volume(&self, target: Target, volume: &ChannelVolumes) {
        let done = self.0.on_done("Could not set volume", target);
        self.0.with_context(|ctx| {
            let mut intro = ctx.introspect();
            match target {
                Target::Sink(i) => drop(intro.set_sink_volume_by_index(i, volume, done)),
                Target::Source(i) => drop(intro.set_source_volume_by_index(i, volume, done)),
                Target::SinkInput(i) => drop(intro.set_sink_input_volume(i, volume, done)),
                Target::SourceOutput(i) => drop(intro.set_source_output_volume(i, volume, done)),
                Target::Card(_) => {}
            }
        });
    }

    pub fn set_mute(&self, target: Target, mute: bool) {
        let done = self.0.on_done("Could not change mute", target);
        self.0.with_context(|ctx| {
            let mut intro = ctx.introspect();
            match target {
                Target::Sink(i) => drop(intro.set_sink_mute_by_index(i, mute, done)),
                Target::Source(i) => drop(intro.set_source_mute_by_index(i, mute, done)),
                Target::SinkInput(i) => drop(intro.set_sink_input_mute(i, mute, done)),
                Target::SourceOutput(i) => drop(intro.set_source_output_mute(i, mute, done)),
                Target::Card(_) => {}
            }
        });
    }

    pub fn set_port(&self, target: Target, port: &str) {
        let done = self.0.on_done("Could not change port", target);
        self.0.with_context(|ctx| {
            let mut intro = ctx.introspect();
            match target {
                Target::Sink(i) => drop(intro.set_sink_port_by_index(i, port, done)),
                Target::Source(i) => drop(intro.set_source_port_by_index(i, port, done)),
                _ => {}
            }
        });
    }

    pub fn set_default(&self, target: Target, name: &str) {
        let mut done = self.0.on_done("Could not set default device", target);
        let finish = move |ok| {
            if let Some(done) = done.as_mut() {
                done(ok);
            }
        };
        self.0.with_context(|ctx| match target {
            Target::Sink(_) => drop(ctx.set_default_sink(name, finish)),
            Target::Source(_) => drop(ctx.set_default_source(name, finish)),
            _ => {}
        });
    }

    pub fn move_stream(&self, target: Target, device: u32) {
        let done = self.0.on_done("Could not move stream", target);
        self.0.with_context(|ctx| {
            let mut intro = ctx.introspect();
            match target {
                Target::SinkInput(i) => drop(intro.move_sink_input_by_index(i, device, done)),
                Target::SourceOutput(i) => drop(intro.move_source_output_by_index(i, device, done)),
                _ => {}
            }
        });
    }

    pub fn set_profile(&self, card: u32, profile: &str) {
        let done = self.0.on_done("Could not change profile", Target::Card(card));
        self.0.with_context(|ctx| {
            drop(ctx.introspect().set_card_profile_by_index(card, profile, done));
        });
    }

    /// Acts as another tool killing a stream. `pactl` has no command for this.
    #[cfg(test)]
    pub fn kill(&self, target: Target) {
        self.0.with_context(|ctx| {
            let mut intro = ctx.introspect();
            match target {
                Target::SinkInput(i) => drop(intro.kill_sink_input(i, |_| {})),
                Target::SourceOutput(i) => drop(intro.kill_source_output(i, |_| {})),
                _ => {}
            }
        });
    }

    /// Makes the running meters match `wanted`.
    pub fn set_meters(&self, wanted: HashMap<Target, MeterSource>) {
        let inner = &self.0;
        // A passive stream on a suspended device stays `Creating` until the device
        // wakes, and cannot be closed before that. Keeping it in the map means a return
        // to this tab reuses it, instead of queueing up one more each visit.
        inner
            .meters
            .borrow_mut()
            .retain(|key, m| !m.is_dead() && meter::keep(wanted.get(key), &m.from, m.is_pending()));
        inner.with_context(|ctx| {
            let mut meters = inner.meters.borrow_mut();
            for (key, from) in wanted {
                if let Entry::Vacant(slot) = meters.entry(key) {
                    if let Some(m) = meter::open(ctx, key, from, Rc::clone(&inner.emit)) {
                        slot.insert(m);
                    }
                }
            }
        });
    }

    /// Replaces any running preview: only one can run at a time.
    pub fn start_preview(&self, source: &SourceInfo, sink: &SinkInfo, percent: f64) {
        let inner = &self.0;
        // A retarget keeps the source's toggle on, so it gets no stop notice.
        if inner.previews(source.index) {
            drop(inner.take_preview());
        }
        inner.stop_preview(None);
        if source.monitor == Some(sink.index) {
            (inner.emit)(Event::PreviewStopped {
                source: source.index,
                reason: Some("Cannot preview a monitor into its own output".into()),
            });
            return;
        }
        inner.preview_volume.set(super::from_percent(percent));
        let generation = inner.preview_generation.get() + 1;
        inner.preview_generation.set(generation);

        let i = Rc::clone(inner);
        let (source_index, sink_name) = (source.index, sink.name.clone());
        let on_ready = Rc::new(move || {
            i.apply_preview_volume();
            (i.emit)(Event::PreviewStarted { source: source_index, sink: sink_name.clone() });
        });
        let i = Rc::clone(inner);
        let on_end = Rc::new(move || {
            let i = Rc::clone(&i);
            glib::idle_add_local_once(move || {
                if i.preview_generation.get() == generation {
                    i.preview_output_lost();
                }
            });
        });
        let params = preview::Params {
            source: source.index,
            source_name: &source.name,
            sink: sink.index,
            sink_name: &sink.name,
            spec: source.spec,
            map: source.map,
            volume: inner.preview_volume.get(),
        };
        let started = inner.with_context(|ctx| {
            preview::start(ctx, &params, Rc::clone(&inner.emit), on_ready, on_end)
        });
        match started {
            Some(Ok(p)) => *inner.preview.borrow_mut() = Some(p),
            Some(Err(reason)) => (inner.emit)(Event::PreviewStopped {
                source: source.index,
                reason: Some(reason),
            }),
            None => (inner.emit)(Event::PreviewStopped { source: source.index, reason: None }),
        }
    }

    pub fn stop_preview(&self) {
        self.0.stop_preview(None);
    }

    /// Stops the preview only if it is `source`'s.
    pub fn stop_preview_of(&self, source: u32) {
        if self.0.previews(source) {
            self.0.stop_preview(None);
        }
    }

    /// Every source row has its own slider, so only `source`'s preview follows it.
    pub fn set_preview_volume(&self, source: u32, percent: f64) {
        if self.0.previews(source) {
            self.0.preview_volume.set(super::from_percent(percent));
            self.0.apply_preview_volume();
        }
    }
}

impl Inner {
    /// Runs `f` if connected. Keep `f` free of calls that re-enter this context.
    fn with_context<R>(&self, f: impl FnOnce(&mut Context) -> R) -> Option<R> {
        let mut context = self.context.borrow_mut();
        context.as_mut().filter(|c| c.get_state() == State::Ready).map(f)
    }

    /// Result handler for a command. On failure the UI shows `what` and gets the
    /// object's real state back, since its control already moved.
    fn on_done(self: &Rc<Self>, what: &'static str, target: Target) -> Option<Box<dyn FnMut(bool)>> {
        let inner = Rc::clone(self);
        Some(Box::new(move |ok| {
            if !ok {
                // A stream that ended during a drag is not an error. Its row is leaving.
                // A card still exists, so there this means a bad profile name.
                let gone = !matches!(target, Target::Card(_)) && inner.last_error_is_no_entity();
                if !gone {
                    (inner.emit)(Event::Error(what.to_string()));
                }
                inner.fetch(target);
            }
            (inner.emit)(Event::Done { ok });
        }))
    }

    fn last_error_is_no_entity(&self) -> bool {
        // `errno` passes on C's positive code, while `PAErr` constants are negative.
        self.context.borrow().as_ref().is_some_and(|c| c.errno().0.abs() == Code::NoEntity as i32)
    }

    fn connect(self: &Rc<Self>) {
        let mut props = Proplist::new();
        if let Some(props) = props.as_mut() {
            let _ = props.set_str(properties::APPLICATION_NAME, "GenMix");
            let _ = props.set_str(properties::APPLICATION_ID, crate::APP_ID);
            let _ = props.set_str(properties::APPLICATION_ICON_NAME, crate::ICON_NAME);
        }
        let context = props.and_then(|p| Context::new_with_proplist(&self.mainloop, "GenMix", &p));
        let Some(mut context) = context else {
            return self.retry_later();
        };
        let inner = Rc::clone(self);
        context.set_state_callback(Some(Box::new(move || inner.on_state())));
        // `connect` fires the state callback right away. `self.context` is still
        // empty then, so the callback just returns.
        if context.connect(None, FlagSet::NOAUTOSPAWN, None).is_err() {
            context.set_state_callback(None);
            return self.retry_later();
        }
        *self.context.borrow_mut() = Some(context);
    }

    fn retry_later(self: &Rc<Self>) {
        (self.emit)(Event::Disconnected);
        let inner = Rc::clone(self);
        glib::timeout_add_local_once(RETRY_DELAY, move || inner.connect());
    }

    fn on_state(self: &Rc<Self>) {
        let state = self.context.borrow().as_ref().map(Context::get_state);
        match state {
            Some(State::Ready) => self.on_connected(),
            Some(State::Failed | State::Terminated) => {
                let inner = Rc::clone(self);
                glib::idle_add_local_once(move || inner.tear_down());
            }
            _ => {}
        }
    }

    fn tear_down(self: &Rc<Self>) {
        self.meters.borrow_mut().clear();
        self.stop_preview(None);
        let context = self.context.borrow_mut().take();
        if let Some(mut context) = context {
            context.set_state_callback(None);
            context.set_subscribe_callback(None);
            context.disconnect();
        }
        self.retry_later();
    }

    fn on_connected(self: &Rc<Self>) {
        let inner = Rc::clone(self);
        self.with_context(|ctx| {
            self.own_client.set(ctx.get_index());
            ctx.set_subscribe_callback(Some(Box::new(move |facility, op, index| {
                inner.on_server_event(facility, op, index);
            })));
            let mask = InterestMaskSet::SINK
                | InterestMaskSet::SOURCE
                | InterestMaskSet::SINK_INPUT
                | InterestMaskSet::SOURCE_OUTPUT
                | InterestMaskSet::CARD
                | InterestMaskSet::SERVER;
            let inner = Rc::clone(self);
            drop(ctx.subscribe(mask, move |ok| {
                if !ok {
                    (inner.emit)(Event::Error("Live updates are not available".into()));
                }
            }));
        });
        (self.emit)(Event::Connected);
        // Subscribed first, so nothing is missed. Any overlap is a harmless repeat.
        self.with_context(|ctx| {
            let intro = ctx.introspect();
            drop(intro.get_server_info(self.server_handler()));
            drop(intro.get_card_info_list(self.card_handler()));
            drop(intro.get_sink_info_list(self.sink_handler()));
            drop(intro.get_source_info_list(self.source_handler()));
            drop(intro.get_sink_input_info_list(self.sink_input_handler()));
            drop(intro.get_source_output_info_list(self.source_output_handler()));
            // The server answers these in order, so this reply follows all the lists.
            let inner = Rc::clone(self);
            drop(intro.get_server_info(move |_| (inner.emit)(Event::Loaded)));
        });
    }

    fn on_server_event(self: &Rc<Self>, facility: Option<Facility>, op: Option<Operation>, index: u32) {
        let target = match facility {
            Some(Facility::Sink) => Target::Sink(index),
            Some(Facility::Source) => Target::Source(index),
            Some(Facility::SinkInput) => Target::SinkInput(index),
            Some(Facility::SourceOutput) => Target::SourceOutput(index),
            Some(Facility::Card) => Target::Card(index),
            Some(Facility::Server) => {
                self.with_context(|ctx| drop(ctx.introspect().get_server_info(self.server_handler())));
                return;
            }
            _ => return,
        };
        if op == Some(Operation::Removed) {
            self.stop_preview_if_uses(target);
            (self.emit)(Event::Removed(target));
        } else {
            self.fetch(target);
        }
    }

    fn fetch(self: &Rc<Self>, target: Target) {
        self.with_context(|ctx| {
            let intro = ctx.introspect();
            match target {
                Target::Sink(i) => drop(intro.get_sink_info_by_index(i, self.sink_handler())),
                Target::Source(i) => drop(intro.get_source_info_by_index(i, self.source_handler())),
                Target::SinkInput(i) => drop(intro.get_sink_input_info(i, self.sink_input_handler())),
                Target::SourceOutput(i) => {
                    drop(intro.get_source_output_info(i, self.source_output_handler()));
                }
                Target::Card(i) => drop(intro.get_card_info_by_index(i, self.card_handler())),
            }
        });
    }

    // In the handlers below, `ListResult::Error` from a by-index fetch means the object
    // went away before the reply. Its removal event covers that, so errors are ignored.

    fn server_handler(self: &Rc<Self>) -> impl FnMut(&pa::ServerInfo) + 'static {
        let inner = Rc::clone(self);
        move |s| {
            (inner.emit)(Event::Server {
                default_sink: text(&s.default_sink_name),
                default_source: text(&s.default_source_name),
            });
        }
    }

    fn sink_handler(self: &Rc<Self>) -> impl FnMut(ListResult<&pa::SinkInfo>) + 'static {
        let inner = Rc::clone(self);
        move |r| {
            if let ListResult::Item(s) = r {
                let running = s.state == SinkState::Running;
                (inner.emit)(Event::Sink(device_info!(s, Some(s.monitor_source), running)));
            }
        }
    }

    fn source_handler(self: &Rc<Self>) -> impl FnMut(ListResult<&pa::SourceInfo>) + 'static {
        let inner = Rc::clone(self);
        move |r| {
            if let ListResult::Item(s) = r {
                let running = s.state == SourceState::Running;
                (inner.emit)(Event::Source(device_info!(s, s.monitor_of_sink, running)));
            }
        }
    }

    fn sink_input_handler(self: &Rc<Self>) -> impl FnMut(ListResult<&pa::SinkInputInfo>) + 'static {
        let inner = Rc::clone(self);
        move |r| {
            if let ListResult::Item(s) = r {
                if s.client.is_none() || s.client != inner.own_client.get() {
                    (inner.emit)(Event::SinkInput(stream_info!(s, s.sink)));
                }
            }
        }
    }

    fn source_output_handler(self: &Rc<Self>) -> impl FnMut(ListResult<&pa::SourceOutputInfo>) + 'static {
        let inner = Rc::clone(self);
        move |r| {
            if let ListResult::Item(s) = r {
                if s.client.is_none() || s.client != inner.own_client.get() {
                    (inner.emit)(Event::SourceOutput(stream_info!(s, s.source)));
                }
            }
        }
    }

    fn card_handler(self: &Rc<Self>) -> impl FnMut(ListResult<&pa::CardInfo>) + 'static {
        let inner = Rc::clone(self);
        move |r| {
            if let ListResult::Item(c) = r {
                (inner.emit)(Event::Card(card_info(c)));
            }
        }
    }

    fn previews(&self, source: u32) -> bool {
        self.preview.borrow().as_ref().is_some_and(|p| p.source == source)
    }

    /// Takes the running preview. Its late "stream ended" notice is ignored from now on.
    fn take_preview(&self) -> Option<Preview> {
        let preview = self.preview.borrow_mut().take();
        if preview.is_some() {
            self.preview_generation.set(self.preview_generation.get() + 1);
        }
        preview
    }

    fn stop_preview(&self, reason: Option<String>) {
        if let Some(preview) = self.take_preview() {
            let source = preview.source;
            drop(preview);
            (self.emit)(Event::PreviewStopped { source, reason });
        }
    }

    fn stop_preview_if_uses(&self, target: Target) {
        let Some((source, sink)) = self.preview.borrow().as_ref().map(|p| (p.source, p.sink)) else {
            return;
        };
        if target == Target::Source(source) {
            self.stop_preview(Some("Preview stopped: device was removed".into()));
        } else if target == Target::Sink(sink) {
            self.preview_output_lost();
        }
    }

    /// Closes the streams and leaves the decision to the UI, which knows the cards. A
    /// stream's death and the sink's removal arrive in either order, and both end here.
    fn preview_output_lost(&self) {
        if let Some(preview) = self.take_preview() {
            let (source, sink) = (preview.source, preview.sink_name.clone());
            drop(preview);
            (self.emit)(Event::PreviewTargetLost { source, sink });
        }
    }

    fn apply_preview_volume(self: &Rc<Self>) {
        let target = self.preview.borrow().as_ref().and_then(|p| {
            Some((p.playback_index()?, p.volumes(self.preview_volume.get())))
        });
        if let Some((index, volume)) = target {
            let done = self.on_done("Could not set preview volume", Target::SinkInput(index));
            self.with_context(|ctx| {
                drop(ctx.introspect().set_sink_input_volume(index, &volume, done));
            });
        }
    }
}

fn text(value: &Option<Cow<str>>) -> String {
    value.as_deref().unwrap_or_default().to_string()
}

fn codec(props: &Proplist) -> Option<String> {
    props.get_str("api.bluez5.codec").or_else(|| props.get_str("bluez5.codec"))
}

fn card_info(c: &pa::CardInfo) -> CardInfo {
    let profile = |p: &pa::CardProfileInfo| ProfileInfo {
        name: text(&p.name),
        description: text(&p.description),
        priority: p.priority,
        available: p.available,
        n_sinks: p.n_sinks,
        n_sources: p.n_sources,
    };
    CardInfo {
        index: c.index,
        name: text(&c.name),
        description: c
            .proplist
            .get_str(properties::DEVICE_DESCRIPTION)
            .unwrap_or_else(|| text(&c.name)),
        bus: c.proplist.get_str(properties::DEVICE_BUS),
        driver: text(&c.driver),
        form_factor: c.proplist.get_str(properties::DEVICE_FORM_FACTOR),
        profiles: c.profiles.iter().map(profile).collect(),
        active_profile: c.active_profile.as_ref().map(|p| text(&p.name)),
        codec: codec(&c.proplist),
    }
}

/// Sink and source info are separate libpulse types with the same fields.
macro_rules! device_info {
    ($s:expr, $monitor:expr, $running:expr) => {{
        let s = $s;
        DeviceInfo {
            index: s.index,
            name: text(&s.name),
            description: text(&s.description),
            volume: s.volume,
            map: s.channel_map,
            mute: s.mute,
            monitor: $monitor,
            running: $running,
            ports: s
                .ports
                .iter()
                .map(|p| PortInfo {
                    name: text(&p.name),
                    description: text(&p.description),
                    unplugged: p.available == PortAvailable::No,
                    kind: p.r#type,
                })
                .collect(),
            active_port: s.active_port.as_ref().map(|p| text(&p.name)),
            card: s.card,
            form_factor: s.proplist.get_str(properties::DEVICE_FORM_FACTOR),
            bus: s.proplist.get_str(properties::DEVICE_BUS),
            driver: text(&s.driver),
            codec: codec(&s.proplist),
            spec: s.sample_spec,
            latency_usec: s.latency.0,
        }
    }};
}
use device_info;

/// Sink input and source output info: same story.
macro_rules! stream_info {
    ($s:expr, $device:expr) => {{
        let s = $s;
        let props = &s.proplist;
        StreamInfo {
            index: s.index,
            app_name: props.get_str(properties::APPLICATION_NAME).unwrap_or_else(|| text(&s.name)),
            media_name: props.get_str(properties::MEDIA_NAME).unwrap_or_else(|| text(&s.name)),
            icon_name: props.get_str(properties::APPLICATION_ICON_NAME),
            binary: props.get_str(properties::APPLICATION_PROCESS_BINARY),
            device: $device,
            volume: s.volume,
            map: s.channel_map,
            mute: s.mute,
            has_volume: s.has_volume,
            volume_writable: s.volume_writable,
            corked: s.corked,
            is_event: props.get_str(properties::MEDIA_ROLE).as_deref() == Some("event"),
        }
    }};
}
use stream_info;
