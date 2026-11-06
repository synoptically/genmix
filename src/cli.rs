//! Command line access. Any argument selects this instead of the window.
//!
//! It drives the same backend as the window and creates no widgets, so it runs without
//! a display. Each run connects, reads the server's objects, sends one command, waits
//! for the reply and exits.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Write;
use std::rc::Rc;

use gtk::glib;
use libpulse_binding::volume::ChannelVolumes;

use crate::backend::{
    from_percent, profile_label, to_percent, Backend, CardInfo, DeviceInfo, Event, StreamInfo, Target,
};
use crate::settings::Settings;
use crate::ui::slider_upper;

const USAGE: &str = "\
Usage:
  genmix                                  open the window
  genmix list [TYPE]                      print objects as tab-separated rows
  genmix volume KIND TARGET PERCENT       set volume: 50, +5 or -5
  genmix mute KIND TARGET on|off|toggle
  genmix default KIND TARGET              set the default output or input
  genmix port KIND TARGET PORT            set the port of an output or input
  genmix move KIND TARGET DEVICE          move a playback or recording stream
  genmix profile CARD PROFILE             set the profile of a card
  genmix help

KIND is output, input, playback or recording.
TYPE is a KIND, or card, port or profile.
TARGET, DEVICE and CARD are an index or a name from `genmix list`.
Outputs and inputs also accept `default`.
";

const LIST_TYPES: [&str; 7] = ["output", "input", "playback", "recording", "card", "port", "profile"];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Output,
    Input,
    Playback,
    Recording,
}

impl Kind {
    fn parse(word: &str) -> Result<Self, String> {
        match word {
            "output" => Ok(Self::Output),
            "input" => Ok(Self::Input),
            "playback" => Ok(Self::Playback),
            "recording" => Ok(Self::Recording),
            _ => Err(format!("Unknown kind: {word}")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Level {
    percent: f64,
    relative: bool,
}

impl Level {
    fn parse(word: &str) -> Result<Self, String> {
        let number = word.strip_suffix('%').unwrap_or(word);
        match number.parse::<f64>() {
            Ok(percent) if percent.is_finite() => {
                Ok(Self { percent, relative: number.starts_with(['+', '-']) })
            }
            _ => Err(format!("Bad volume: {word}")),
        }
    }

    fn from(self, current: f64) -> f64 {
        if self.relative { current + self.percent } else { self.percent }
    }
}

#[derive(Debug, PartialEq)]
enum Command<'a> {
    Help,
    List(Option<&'a str>),
    Volume(Kind, &'a str, Level),
    /// `None` toggles.
    Mute(Kind, &'a str, Option<bool>),
    Default(Kind, &'a str),
    Port(Kind, &'a str, &'a str),
    Move(Kind, &'a str, &'a str),
    Profile(&'a str, &'a str),
}

fn parse(args: &[String]) -> Result<Command<'_>, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(match *words.as_slice() {
        ["help" | "--help" | "-h"] => Command::Help,
        ["list"] => Command::List(None),
        ["list", kind] if LIST_TYPES.contains(&kind) => Command::List(Some(kind)),
        ["volume", kind, target, level] => Command::Volume(Kind::parse(kind)?, target, Level::parse(level)?),
        ["mute", kind, target, "on"] => Command::Mute(Kind::parse(kind)?, target, Some(true)),
        ["mute", kind, target, "off"] => Command::Mute(Kind::parse(kind)?, target, Some(false)),
        ["mute", kind, target, "toggle"] => Command::Mute(Kind::parse(kind)?, target, None),
        ["default", kind, target] => Command::Default(Kind::parse(kind)?, target),
        ["port", kind, target, port] => Command::Port(Kind::parse(kind)?, target, port),
        ["move", kind, target, device] => Command::Move(Kind::parse(kind)?, target, device),
        ["profile", card, profile] => Command::Profile(card, profile),
        _ => return Err("Bad arguments. Run `genmix help`.".into()),
    })
}

/// What the server reported, plus how far this run has come.
#[derive(Default)]
struct Mixer {
    default_sink: String,
    default_source: String,
    sinks: BTreeMap<u32, DeviceInfo>,
    sources: BTreeMap<u32, DeviceInfo>,
    playback: BTreeMap<u32, StreamInfo>,
    recording: BTreeMap<u32, StreamInfo>,
    cards: BTreeMap<u32, CardInfo>,
    loaded: bool,
    lost: bool,
    done: Option<bool>,
    error: Option<String>,
}

/// The parts of a device or stream that `volume` and `mute` need.
struct Control {
    target: Target,
    volume: ChannelVolumes,
    mute: bool,
    writable: bool,
}

impl Mixer {
    fn apply(&mut self, event: Event) {
        match event {
            Event::Server { default_sink, default_source } => {
                self.default_sink = default_sink;
                self.default_source = default_source;
            }
            Event::Sink(info) => drop(self.sinks.insert(info.index, info)),
            Event::Source(info) => drop(self.sources.insert(info.index, info)),
            Event::SinkInput(info) => drop(self.playback.insert(info.index, info)),
            Event::SourceOutput(info) => drop(self.recording.insert(info.index, info)),
            Event::Card(info) => drop(self.cards.insert(info.index, info)),
            Event::Loaded => self.loaded = true,
            // The backend retries forever. A command should fail instead.
            Event::Disconnected => self.lost = true,
            Event::Done { ok } => self.done = Some(ok),
            Event::Error(message) => self.error = Some(message),
            _ => {}
        }
    }

    fn device(&self, kind: Kind, query: &str) -> Result<(Target, &DeviceInfo), String> {
        let (rows, default, target): (_, _, fn(u32) -> Target) = match kind {
            Kind::Output => (&self.sinks, &self.default_sink, Target::Sink),
            Kind::Input => (&self.sources, &self.default_source, Target::Source),
            _ => return Err("Expected output or input".into()),
        };
        let query = if query == "default" { default } else { query };
        let info = find(rows, query, |d| &d.name)?;
        Ok((target(info.index), info))
    }

    fn stream(&self, kind: Kind, query: &str) -> Result<(Target, &StreamInfo), String> {
        let (rows, target): (_, fn(u32) -> Target) = match kind {
            Kind::Playback => (&self.playback, Target::SinkInput),
            Kind::Recording => (&self.recording, Target::SourceOutput),
            _ => return Err("Expected playback or recording".into()),
        };
        let info = find(rows, query, |s| &s.app_name)?;
        Ok((target(info.index), info))
    }

    fn control(&self, kind: Kind, query: &str) -> Result<Control, String> {
        if matches!(kind, Kind::Output | Kind::Input) {
            let (target, d) = self.device(kind, query)?;
            return Ok(Control { target, volume: d.volume, mute: d.mute, writable: true });
        }
        let (target, s) = self.stream(kind, query)?;
        Ok(Control { target, volume: s.volume, mute: s.mute, writable: s.has_volume && s.volume_writable })
    }

    fn rows(&self) -> Vec<String> {
        let mut rows = Vec::new();
        let devices = [("output", &self.sinks, &self.default_sink), ("input", &self.sources, &self.default_source)];
        for (kind, devices, default) in devices {
            for d in devices.values() {
                let index = d.index.to_string();
                let is_default = flag(d.name == *default, "default");
                rows.push(row(&[kind, &index, &d.name, &percent(&d.volume), flag(d.mute, "muted"), is_default, &d.description]));
                for p in &d.ports {
                    let active = flag(d.active_port.as_ref() == Some(&p.name), "active");
                    rows.push(row(&["port", kind, &index, &p.name, active, flag(p.unplugged, "unplugged"), &p.description]));
                }
            }
        }
        for (kind, streams) in [("playback", &self.playback), ("recording", &self.recording)] {
            for s in streams.values() {
                let (index, device) = (s.index.to_string(), s.device.to_string());
                rows.push(row(&[kind, &index, &s.app_name, &percent(&s.volume), flag(s.mute, "muted"), &device, &s.media_name]));
            }
        }
        for c in self.cards.values() {
            let index = c.index.to_string();
            rows.push(row(&["card", &index, &c.name, c.active_profile.as_deref().unwrap_or("-"), &c.description]));
            for p in &c.profiles {
                let active = flag(c.active_profile.as_ref() == Some(&p.name), "active");
                let unavailable = flag(!p.available, "unavailable");
                rows.push(row(&["profile", &index, &p.name, active, unavailable, &profile_label(p, None)]));
            }
        }
        rows
    }
}

/// An all-digit query is an index. Anything else must match exactly one name.
fn find<'a, T>(rows: &'a BTreeMap<u32, T>, query: &str, name: impl Fn(&T) -> &str) -> Result<&'a T, String> {
    if let Ok(index) = query.parse() {
        return rows.get(&index).ok_or_else(|| format!("Nothing has index {query}"));
    }
    let mut hits = rows.values().filter(|row| name(row) == query);
    match (hits.next(), hits.next()) {
        (Some(hit), None) => Ok(hit),
        (Some(_), Some(_)) => Err(format!("Several objects are named {query}. Use an index.")),
        _ => Err(format!("Nothing is named {query}")),
    }
}

fn percent(volume: &ChannelVolumes) -> String {
    format!("{:.0}%", to_percent(volume.max()))
}

fn flag(on: bool, word: &str) -> &str {
    if on { word } else { "-" }
}

/// Names and titles come from other programs. Control characters in them would break
/// the columns, or reach the terminal as escape codes.
fn row(fields: &[&str]) -> String {
    let clean: Vec<String> = fields.iter().map(|f| f.replace(char::is_control, " ")).collect();
    clean.join("\t")
}

fn list(mixer: &Mixer, wanted: Option<&str>) -> String {
    let matches = |row: &String| wanted.is_none_or(|kind| row.split('\t').next() == Some(kind));
    mixer.rows().into_iter().filter(matches).map(|row| row + "\n").collect()
}

/// A closed pipe, as in `genmix list | head`, is not an error.
fn print(text: &str) {
    let _ = std::io::stdout().write_all(text.as_bytes());
}

/// Each path that returns `Ok` sends one command, so one `Event::Done` will follow.
fn send(backend: &Backend, mixer: &Mixer, command: &Command) -> Result<(), String> {
    match *command {
        Command::Help | Command::List(_) => unreachable!("handled in `execute`"),
        Command::Volume(kind, target, level) => {
            let control = mixer.control(kind, target)?;
            if !control.writable {
                return Err(format!("The volume of {target} cannot be changed"));
            }
            let current = to_percent(control.volume.max());
            let upper = slider_upper(Settings::load().limit(), current);
            let wanted = level.from(current);
            if wanted > upper {
                eprintln!("genmix: limited to {upper:.0}%. The limit is in the window's settings.");
            }
            let mut volume = control.volume;
            volume.scale(from_percent(wanted.clamp(0.0, upper)));
            backend.set_volume(control.target, &volume);
        }
        Command::Mute(kind, target, mute) => {
            let control = mixer.control(kind, target)?;
            backend.set_mute(control.target, mute.unwrap_or(!control.mute));
        }
        Command::Default(kind, target) => {
            let (target, info) = mixer.device(kind, target)?;
            backend.set_default(target, &info.name);
        }
        Command::Port(kind, target, port) => {
            let (target, info) = mixer.device(kind, target)?;
            if !info.ports.iter().any(|p| p.name == port) {
                return Err(format!("{} has no port {port}", info.name));
            }
            backend.set_port(target, port);
        }
        Command::Move(kind, target, device) => {
            let (target, _) = mixer.stream(kind, target)?;
            let device_kind = if kind == Kind::Playback { Kind::Output } else { Kind::Input };
            backend.move_stream(target, mixer.device(device_kind, device)?.1.index);
        }
        Command::Profile(card, profile) => {
            backend.set_profile(find(&mixer.cards, card, |c| &c.name)?.index, profile);
        }
    }
    Ok(())
}

fn wait(mixer: &RefCell<Mixer>, until: impl Fn(&Mixer) -> bool) {
    let context = glib::MainContext::default();
    while !until(&mixer.borrow()) {
        context.iteration(true);
    }
}

fn execute(command: &Command) -> Result<(), String> {
    if *command == Command::Help {
        print(USAGE);
        return Ok(());
    }
    let state = Rc::new(RefCell::new(Mixer::default()));
    let events = Rc::clone(&state);
    let backend = Backend::new(move |event| events.borrow_mut().apply(event));
    backend.start();
    wait(&state, |m| m.loaded || m.lost);
    // Taken, so that no borrow is held while the backend runs.
    let mixer = state.take();
    if mixer.lost {
        return Err("Cannot reach the audio server".into());
    }
    if let Command::List(wanted) = *command {
        print(&list(&mixer, wanted));
        return Ok(());
    }
    send(&backend, &mixer, command)?;
    wait(&state, |m| m.done.is_some() || m.lost);
    let result = state.take();
    match result.done {
        Some(true) => Ok(()),
        Some(false) => Err(result.error.unwrap_or_else(|| "The target is gone".into())),
        None => Err("Lost the audio server".into()),
    }
}

pub fn run(args: &[String]) -> glib::ExitCode {
    match parse(args).and_then(|command| execute(&command)) {
        Ok(()) => glib::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("genmix: {message}");
            glib::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use libpulse_binding::channelmap::Map;
    use libpulse_binding::def::DevicePortType;
    use libpulse_binding::sample::{Format, Spec};

    use super::*;
    use crate::backend::{PortInfo, ProfileInfo};

    fn words(line: &str) -> Vec<String> {
        line.split(' ').map(String::from).collect()
    }

    fn device(index: u32, name: &str) -> DeviceInfo {
        let mut volume = ChannelVolumes::default();
        volume.set(2, from_percent(40.0));
        DeviceInfo {
            index,
            name: name.into(),
            description: "Desk\tspeakers".into(),
            volume,
            map: Map::default(),
            mute: true,
            monitor: None,
            running: false,
            ports: vec![PortInfo {
                name: "line-out".into(),
                description: "Line Out".into(),
                unplugged: true,
                kind: DevicePortType::Line,
            }],
            active_port: Some("line-out".into()),
            card: None,
            form_factor: None,
            bus: None,
            driver: String::new(),
            codec: None,
            spec: Spec { format: Format::S16NE, channels: 2, rate: 48_000 },
            latency_usec: 0,
        }
    }

    fn stream(index: u32, app: &str, writable: bool) -> StreamInfo {
        StreamInfo {
            index,
            app_name: app.into(),
            media_name: "title\x1b[2J".into(),
            icon_name: None,
            binary: None,
            device: 7,
            volume: ChannelVolumes::default(),
            map: Map::default(),
            mute: false,
            has_volume: true,
            volume_writable: writable,
            corked: false,
            is_event: false,
        }
    }

    fn mixer() -> Mixer {
        let card = CardInfo {
            index: 3,
            name: "card_a".into(),
            description: "Card A".into(),
            bus: None,
            driver: String::new(),
            form_factor: None,
            profiles: vec![ProfileInfo {
                name: "off".into(),
                description: "Off".into(),
                priority: 0,
                available: false,
                n_sinks: 0,
                n_sources: 0,
            }],
            active_profile: None,
            codec: None,
        };
        Mixer {
            default_sink: "sink_b".into(),
            sinks: BTreeMap::from([(7, device(7, "sink_a")), (8, device(8, "sink_b"))]),
            playback: BTreeMap::from([
                (1, stream(1, "player", true)),
                (2, stream(2, "browser", true)),
                (3, stream(3, "browser", false)),
            ]),
            cards: BTreeMap::from([(3, card)]),
            ..Mixer::default()
        }
    }

    #[test]
    fn parses_commands() {
        let parsed = |line| parse(&words(line)).map(|c| format!("{c:?}"));
        let absolute = Command::Volume(Kind::Output, "x", Level { percent: 50.0, relative: false });
        assert_eq!(parsed("volume output x 50"), Ok(format!("{absolute:?}")));
        assert_eq!(parsed("volume output x 50%"), Ok(format!("{absolute:?}")));
        let down = Command::Volume(Kind::Input, "default", Level { percent: -5.0, relative: true });
        assert_eq!(parsed("volume input default -5"), Ok(format!("{down:?}")));
        let up = Command::Volume(Kind::Playback, "1", Level { percent: 5.0, relative: true });
        assert_eq!(parsed("volume playback 1 +5%"), Ok(format!("{up:?}")));
        let toggle = Command::Mute(Kind::Recording, "1", None);
        assert_eq!(parsed("mute recording 1 toggle"), Ok(format!("{toggle:?}")));
        assert_eq!(parsed("list port"), Ok(format!("{:?}", Command::List(Some("port")))));
        assert_eq!(parsed("--help"), Ok(format!("{:?}", Command::Help)));
    }

    #[test]
    fn rejects_bad_commands() {
        for line in [
            "bogus",
            "list bogus",
            "volume output x",
            "volume output x loud",
            "volume output x NaN",
            "volume output x inf",
            "volume card x 50",
            "mute output x maybe",
            "profile a b c",
        ] {
            assert!(parse(&words(line)).is_err(), "{line}");
        }
    }

    #[test]
    fn relative_level_starts_from_current() {
        assert_eq!(Level::parse("+5").unwrap().from(40.0), 45.0);
        assert_eq!(Level::parse("-5").unwrap().from(40.0), 35.0);
        assert_eq!(Level::parse("5").unwrap().from(40.0), 5.0);
    }

    #[test]
    fn targets_resolve_by_index_name_and_default() {
        let m = mixer();
        assert_eq!(m.device(Kind::Output, "7").unwrap().1.name, "sink_a");
        assert_eq!(m.device(Kind::Output, "sink_a").unwrap().0, Target::Sink(7));
        assert_eq!(m.device(Kind::Output, "default").unwrap().1.name, "sink_b");
        assert_eq!(m.stream(Kind::Playback, "player").unwrap().0, Target::SinkInput(1));
        assert_eq!(m.stream(Kind::Playback, "3").unwrap().0, Target::SinkInput(3));
    }

    #[test]
    fn bad_targets_are_refused() {
        let m = mixer();
        assert!(m.device(Kind::Output, "99").is_err());
        assert!(m.device(Kind::Output, "no_such").is_err());
        assert!(m.device(Kind::Input, "default").is_err(), "no default input is known");
        assert!(m.device(Kind::Playback, "1").is_err());
        assert!(m.stream(Kind::Output, "7").is_err());
        assert!(m.stream(Kind::Playback, "browser").unwrap_err().contains("Several"));
    }

    #[test]
    fn read_only_stream_is_reported() {
        let m = mixer();
        assert!(m.control(Kind::Playback, "1").unwrap().writable);
        assert!(!m.control(Kind::Playback, "3").unwrap().writable);
        assert!(m.control(Kind::Output, "7").unwrap().writable);
    }

    #[test]
    fn list_prints_clean_rows_and_filters_by_type() {
        let m = mixer();
        assert_eq!(
            list(&m, Some("output")),
            "output\t7\tsink_a\t40%\tmuted\t-\tDesk speakers\n\
             output\t8\tsink_b\t40%\tmuted\tdefault\tDesk speakers\n"
        );
        assert_eq!(list(&m, Some("port")).lines().next(), Some("port\toutput\t7\tline-out\tactive\tunplugged\tLine Out"));
        assert_eq!(list(&m, Some("playback")).lines().next(), Some("playback\t1\tplayer\t0%\t-\t7\ttitle [2J"));
        assert_eq!(list(&m, Some("card")), "card\t3\tcard_a\t-\tCard A\n");
        assert_eq!(list(&m, Some("profile")), "profile\t3\toff\t-\tunavailable\tOff\n");
        assert_eq!(list(&m, Some("input")), "");
        assert_eq!(list(&m, None).lines().count(), 9);
    }
}
