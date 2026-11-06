//! Tests against the real audio server and a display. Run them with:
//!
//!     cargo test -- --ignored --nocapture
//!
//! They create null sinks and remove them again. They may call `pactl`; the app never
//! does. GTK allows one thread, so the steps share a single `#[test]`.
//!
//! `GENMIX_TEST_BLUETOOTH=1` also switches the first Bluetooth card through its
//! profiles and back. That interrupts any audio playing on the headset.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use gtk::gio;
use libpulse_binding::volume::ChannelVolumes;

use super::*;
use crate::backend::{from_percent, profile_group, ProfileGroup};
use crate::ui::volume_slider::VolumeControl;

const RATE: usize = 48_000;
const TONE_HZ: f64 = 440.0;

fn pactl(args: &[&str]) -> String {
    let out = Command::new("pactl").args(args).output().expect("run pactl");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn pump(duration: Duration) {
    let context = glib::MainContext::default();
    let end = Instant::now() + duration;
    while Instant::now() < end {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(8);
    while !ready() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        pump(Duration::from_millis(10));
    }
}

/// Removes the sink even when a step fails.
struct NullSink {
    name: &'static str,
    module: String,
}

impl NullSink {
    fn load(name: &'static str, properties: &str) -> Self {
        let props = format!("sink_properties=device.description={name}{properties}");
        let sink_name = format!("sink_name={name}");
        let module = pactl(&["load-module", "module-null-sink", &sink_name, &props]);
        Self { name, module: module.trim().to_string() }
    }

    fn unload(&mut self) {
        if !self.module.is_empty() {
            pactl(&["unload-module", &self.module]);
            self.module.clear();
        }
    }

    fn volume(&self) -> String {
        pactl(&["get-sink-volume", self.name])
    }
}

impl Drop for NullSink {
    fn drop(&mut self) {
        self.unload();
    }
}

/// Removes the directory even when a step fails.
struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Puts the default sink back even when a step fails.
struct RestoreDefault(String);

impl Drop for RestoreDefault {
    fn drop(&mut self) {
        pactl(&["set-default-sink", &self.0]);
    }
}

/// Puts the headset back even when a step fails.
struct RestoreProfile {
    card: String,
    profile: String,
}

impl Drop for RestoreProfile {
    fn drop(&mut self) {
        pactl(&["set-card-profile", &self.card, &self.profile]);
    }
}

fn sink_row(win: &Window, name: &str) -> Option<Rc<DeviceRow>> {
    win.sinks.borrow().values().find(|r| r.info.borrow().name == name).cloned()
}

fn source_row(win: &Window, name: &str) -> Option<Rc<DeviceRow>> {
    win.sources.borrow().values().find(|r| r.info.borrow().name == name).cloned()
}

fn select_preview_target(row: &DeviceRow, sink: &DeviceRow) {
    let preview = row.preview.as_ref().expect("source row has preview controls");
    let index = sink.info.borrow().index;
    preview.target.select(&Some(index));
    assert_eq!(preview.target.selected(), Some(Some(index)), "target is in the list");
}

fn close_dialog() {
    let windows = gtk::Window::toplevels();
    let dialog = (0..windows.n_items())
        .filter_map(|i| windows.item(i).and_downcast::<gtk::Window>())
        .find(|w| w.transient_for().is_some())
        .expect("dialog window");
    dialog.close();
    pump(Duration::from_millis(100));
}

/// Writes a tone to one sink for `seconds`, and returns what arrives at another's monitor.
fn play_and_capture(into: &str, capture: &str, seconds: f64) -> Vec<i16> {
    let frames = (RATE as f64 * (seconds + 2.0)) as usize;
    let mut tone = Vec::with_capacity(frames * 2);
    for n in 0..frames {
        let phase = n as f64 * TONE_HZ * std::f64::consts::TAU / RATE as f64;
        tone.extend_from_slice(&((phase.sin() * 16_000.0) as i16).to_ne_bytes());
    }
    let format = ["--format=s16ne", "--rate=48000", "--channels=1", "--raw", "--latency-msec=20"];
    let mut play = Command::new("pacat")
        .args(["--playback", &format!("--device={into}")])
        .args(format)
        .stdin(Stdio::piped())
        .spawn()
        .expect("run pacat");
    let mut stdin = play.stdin.take().expect("pacat stdin");
    std::thread::spawn(move || {
        let _ = stdin.write_all(&tone);
    });
    let mut record = Command::new("pacat")
        .args(["--record", &format!("--device={capture}.monitor")])
        .args(format)
        .stdout(Stdio::piped())
        .spawn()
        .expect("run pacat");
    let mut stdout = record.stdout.take().expect("pacat stdout");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    pump(Duration::from_secs_f64(seconds));
    let _ = record.kill();
    let _ = record.wait();
    let _ = play.kill();
    let _ = play.wait();
    let bytes = reader.join().expect("reader thread");
    bytes.as_chunks().0.iter().map(|b| i16::from_ne_bytes(*b)).collect()
}

/// A clean sine never jumps further per sample than its slope allows, so any larger
/// jump is a gap or a dropped block.
fn count_glitches(samples: &[i16]) -> usize {
    let slope = 16_000.0 * TONE_HZ * std::f64::consts::TAU / RATE as f64;
    let limit = (slope * 1.5) as i32;
    samples.windows(2).filter(|w| (i32::from(w[1]) - i32::from(w[0])).abs() > limit).count()
}

#[test]
fn glitch_counter_sees_a_dropped_block() {
    let tone: Vec<i16> = (0..RATE)
        .map(|n| ((n as f64 * TONE_HZ * std::f64::consts::TAU / RATE as f64).sin() * 16_000.0) as i16)
        .collect();
    assert_eq!(count_glitches(&tone), 0);
    let dropped = [&tone[..1000], &tone[1040..]].concat();
    assert!(count_glitches(&dropped) > 0);
}

fn throttle_and_feedback_guard() {
    let sent: Rc<RefCell<Vec<(Instant, ChannelVolumes)>>> = Rc::default();
    let log = Rc::clone(&sent);
    let control = VolumeControl::new(
        "test",
        100.0,
        move |volume| log.borrow_mut().push((Instant::now(), *volume)),
        |_| {},
    );
    let mut map = libpulse_binding::channelmap::Map::default();
    map.init_stereo();
    let mut volume = ChannelVolumes::default();
    volume.set(2, from_percent(50.0));

    control.update(volume, map, false, true);
    pump(Duration::from_millis(100));
    assert!(sent.borrow().is_empty(), "a server update must not be sent back");

    for step in 0..50 {
        control.drag_to(50.0 + f64::from(step));
        pump(Duration::from_millis(3));
    }
    // An echo of an old value during the drag must not move the slider.
    control.update(volume, map, false, true);
    pump(Duration::from_millis(10));
    assert_eq!(control.shown().0.round(), 99.0, "stale echo moved the slider");
    pump(Duration::from_millis(400));

    let sent = sent.borrow();
    assert!(sent.len() < 10, "50 moves in 150 ms must coalesce, got {}", sent.len());
    assert!(sent.windows(2).all(|w| w[1].0 - w[0].0 >= Duration::from_millis(25)));
    assert_eq!(sent.last().expect("something was sent").1.max(), from_percent(99.0));
    // Once the drag ends, the newest server state wins.
    assert_eq!(control.shown().0.round(), 50.0);
}

fn balance_survives_zero() {
    let sent: Rc<RefCell<Vec<ChannelVolumes>>> = Rc::default();
    let log = Rc::clone(&sent);
    let control = VolumeControl::new("test", 100.0, move |v| log.borrow_mut().push(*v), |_| {});
    let mut map = libpulse_binding::channelmap::Map::default();
    map.init_stereo();
    let mut volume = ChannelVolumes::default();
    volume.set(2, from_percent(50.0));
    volume.get_mut()[1] = from_percent(25.0);
    control.update(volume, map, false, true);

    control.drag_to(0.0);
    pump(Duration::from_millis(350));
    // The server's echo of silence must not erase the ratio.
    volume.set(2, from_percent(0.0));
    control.update(volume, map, false, true);
    control.drag_to(60.0);
    pump(Duration::from_millis(100));

    let last = *sent.borrow().last().expect("something was sent");
    assert_eq!(last.get()[0], from_percent(60.0));
    assert_eq!(last.get()[1], from_percent(30.0));
}

fn channel_labels_follow_the_map() {
    use libpulse_binding::channelmap::Position;
    let control = VolumeControl::new("test", 100.0, |_| {}, |_| {});
    let mut map = libpulse_binding::channelmap::Map::default();
    map.init_stereo();
    let mut volume = ChannelVolumes::default();
    volume.set(2, from_percent(50.0));
    control.update(volume, map, false, true);
    let front = control.channel_names();
    map.get_mut()[0] = Position::RearLeft;
    map.get_mut()[1] = Position::RearRight;
    control.update(volume, map, false, true);
    assert_ne!(control.channel_names(), front);
    assert!(control.channel_names()[0].contains("Rear"));
}

fn card_keeps_the_users_expander_state(win: &Window) {
    let info = win.cards.borrow().values().next().expect("a card").info.borrow().clone();
    let card = CardRow::new(&win.backend, info.clone());
    let wanted = !card.profiles_expander().is_expanded();
    card.profiles_expander().set_expanded(wanted);
    let mut changed = info;
    changed.profiles[0].available = !changed.profiles[0].available;
    card.update(changed);
    assert_eq!(card.profiles_expander().is_expanded(), wanted);
}

fn hiding_monitors_stops_their_preview(win: &Window, feed: &NullSink, out: &NullSink) {
    let source = source_row(win, &format!("{}.monitor", feed.name)).expect("source row");
    let preview = source.preview.clone().expect("preview controls");
    select_preview_target(&source, &sink_row(win, out.name).expect("target row"));
    win.settings.borrow_mut().show_monitors = true;
    win.settings_changed();
    let index = source.info.borrow().index;
    win.request_preview(index);
    wait_for("preview to start", || preview.is_on());
    win.settings.borrow_mut().show_monitors = false;
    win.settings_changed();
    wait_for("hidden row's preview to stop", || !preview.is_on());
    wait_for("streams to close", || !pactl(&["list", "sink-inputs"]).contains("Live preview"));
}

fn volume_round_trip(win: &Window, sink: &NullSink) {
    let row = sink_row(win, sink.name).expect("sink row");
    pactl(&["set-sink-volume", sink.name, "33%"]);
    wait_for("external volume change", || row.control.shown().0.round() == 33.0);
    pump(Duration::from_millis(200));
    assert!(sink.volume().contains("33%"), "app must not write back");

    row.control.drag_to(55.0);
    wait_for("slider to reach the server", || sink.volume().contains("55%"));

    pactl(&["set-sink-volume", sink.name, "130%"]);
    pump(Duration::from_millis(400));
    wait_for("over-limit volume", || row.control.shown().1 == 130.0);
    assert!(sink.volume().contains("130%"), "over-limit volume was lowered");
    pactl(&["set-sink-volume", sink.name, "40%"]);
    wait_for("range to shrink back", || row.control.shown().1 == 100.0);
}

/// Passive meters on suspended devices cannot close. They must not pile up either.
fn meters_do_not_pile_up(win: &Window) {
    let meters = || pactl(&["list", "source-outputs"]).matches("Peak detect").count();
    let visit_all_tabs = || {
        for page in [PLAYBACK, RECORDING, OUTPUTS, INPUTS, CARDS] {
            win.notebook.set_current_page(Some(page));
            pump(Duration::from_millis(250));
        }
    };
    visit_all_tabs();
    let after_one_round = meters();
    for _ in 0..3 {
        visit_all_tabs();
    }
    pump(Duration::from_millis(300));
    assert!(meters() <= after_one_round, "{} meters, was {after_one_round}", meters());
    win.notebook.set_current_page(Some(PLAYBACK));
}

fn own_stream_ids(kind: &str, marker: &str) -> Vec<u32> {
    let listing = pactl(&["list", kind]);
    let blocks = listing.split("\n\n").filter(|b| b.contains(marker));
    blocks.filter_map(|b| b.lines().next()?.rsplit('#').next()?.parse().ok()).collect()
}

fn dead_meter_reopens(win: &Window) {
    win.notebook.set_current_page(Some(INPUTS));
    wait_for("input meters", || !own_stream_ids("source-outputs", "Peak detect").is_empty());
    pump(Duration::from_millis(500));
    let before = own_stream_ids("source-outputs", "Peak detect");
    // The newest one belongs to this tab. Older ones may be pending from other tabs.
    let victim = *before.iter().max().expect("a meter");
    win.backend.kill(Target::SourceOutput(victim));
    wait_for("meter to die", || !own_stream_ids("source-outputs", "Peak detect").contains(&victim));
    wait_for("meter to reopen", || {
        own_stream_ids("source-outputs", "Peak detect").len() == before.len()
    });
    win.notebook.set_current_page(Some(PLAYBACK));
}

fn command_errors(win: &Window) {
    let mut volume = ChannelVolumes::default();
    volume.set(2, from_percent(10.0));
    win.error.set_reveal_child(false);
    win.backend.set_volume(Target::SinkInput(4_000_000), &volume);
    pump(Duration::from_millis(300));
    assert!(!win.error.reveals_child(), "a vanished stream is not an error");
    let card = *win.cards.borrow().keys().next().expect("a card");
    win.backend.set_profile(card, "genmix-no-such-profile");
    wait_for("error banner", || win.error.reveals_child());
    win.error.set_reveal_child(false);
}

fn monitor_into_own_sink_is_refused(win: &Window, sink: &str) {
    let source = source_row(win, &format!("{sink}.monitor")).expect("monitor row");
    let target = sink_row(win, sink).expect("sink row");
    select_preview_target(&source, &target);
    let index = source.info.borrow().index;
    win.request_preview(index);
    pump(Duration::from_millis(100));
    assert!(win.error.reveals_child(), "banner explains the refusal");
    assert!(!source.preview.as_ref().expect("preview").is_on());
}

fn speaker_target_asks_first(win: &Window, source: &str, sink: &str) {
    let source = source_row(win, source).expect("source row");
    let target = sink_row(win, sink).expect("sink row");
    select_preview_target(&source, &target);
    let before = gtk::Window::toplevels().n_items();
    let index = source.info.borrow().index;
    win.request_preview(index);
    pump(Duration::from_millis(100));
    assert_eq!(gtk::Window::toplevels().n_items(), before + 1, "warning dialog opens");
    assert!(!pactl(&["list", "sink-inputs"]).contains("Live preview"), "no audio before an answer");
    close_dialog();
}

/// The volume line of the preview's playback stream.
fn preview_volume() -> String {
    let all = pactl(&["list", "sink-inputs"]);
    let stream = all.split("Sink Input #").find(|s| s.contains("Live preview output")).unwrap_or_default();
    stream.lines().find(|l| l.trim_start().starts_with("Volume:")).unwrap_or_default().to_string()
}

/// A second source's controls must not reach the preview that is running.
fn other_source_leaves_the_preview_alone(win: &Window, running: &DeviceRow, other: &str, speaker: &str) {
    let other = source_row(win, other).expect("other source row");
    let controls = other.preview.clone().expect("preview controls");
    let volume = preview_volume();
    controls.set_volume(10.0);
    pump(Duration::from_millis(300));
    assert_eq!(preview_volume(), volume, "another row's slider changed the volume");

    select_preview_target(&other, &sink_row(win, speaker).expect("speaker row"));
    win.request_preview(other.info.borrow().index);
    pump(Duration::from_millis(100));
    close_dialog();
    assert!(running.preview.as_ref().expect("preview").is_on(), "cancel stopped another preview");
    assert!(pactl(&["list", "sink-inputs"]).contains("Live preview output"));
}

/// A stream can die before the sink's removal arrives, or with no removal at all.
fn preview_survives_one_kill_but_not_two(win: &Window, feed: &NullSink, out: &NullSink) {
    let source = source_row(win, &format!("{}.monitor", feed.name)).expect("source row");
    let preview = source.preview.clone().expect("preview controls");
    select_preview_target(&source, &sink_row(win, out.name).expect("target row"));
    let index = source.info.borrow().index;
    let streams = || own_stream_ids("sink-inputs", "Live preview output");
    win.request_preview(index);
    wait_for("preview to start", || preview.is_on() && !streams().is_empty());

    win.backend.kill(Target::SinkInput(streams()[0]));
    pump(Duration::from_millis(200));
    assert!(streams().is_empty(), "the kill must really happen");
    assert!(preview.is_on(), "toggle waits for the resume");
    wait_for("preview to resume", || !streams().is_empty());

    pump(Duration::from_millis(300));
    win.backend.kill(Target::SinkInput(streams()[0]));
    wait_for("second loss to stop it", || !preview.is_on());
    assert!(streams().is_empty());
    // The next step needs a clean slate for its own resume.
    win.last_preview_resume.set(None);
    win.error.set_reveal_child(false);
}

/// Changes the real default sink for a moment, so it runs only in the disruptive mode.
fn default_change_stops_preview(win: &Window, feed: &NullSink, out: &NullSink) {
    let original = pactl(&["get-default-sink"]).trim().to_string();
    let _restore = RestoreDefault(original);
    pactl(&["set-default-sink", out.name]);
    wait_for("default to change", || *win.default_sink.borrow() == out.name);

    let source = source_row(win, &format!("{}.monitor", feed.name)).expect("source row");
    let preview = source.preview.clone().expect("preview controls");
    preview.target.select(&None);
    let index = source.info.borrow().index;
    win.request_preview(index);
    wait_for("preview on default", || preview.is_on());

    // A short flip, as when a Bluetooth sink is rebuilt, must not stop it.
    pactl(&["set-default-sink", feed.name]);
    pump(Duration::from_millis(100));
    pactl(&["set-default-sink", out.name]);
    pump(Duration::from_millis(1500));
    assert!(preview.is_on(), "a brief flip stopped the preview");

    pactl(&["set-default-sink", feed.name]);
    wait_for("preview to stop", || !preview.is_on());
    assert!(win.error_text.text().contains("default output"));
    win.error.set_reveal_child(false);
}

/// Moving a running preview to a speaker asks first. Cancel must silence it.
fn cancelling_a_retarget_stops_the_audio(win: &Window, source: &DeviceRow, speaker: &str) {
    let preview = source.preview.clone().expect("preview controls");
    let index = source.info.borrow().index;
    select_preview_target(source, &sink_row(win, speaker).expect("speaker row"));
    win.retarget_preview(index);
    pump(Duration::from_millis(100));
    assert!(preview.is_on(), "still playing while the question is open");
    close_dialog();
    assert!(!preview.is_on());
    wait_for("streams to close", || !pactl(&["list", "sink-inputs"]).contains("Live preview"));
}

fn preview_copies_audio_and_stops_on_removal(win: &Window, feed: &NullSink, out: &mut NullSink) {
    let source = source_row(win, &format!("{}.monitor", feed.name)).expect("source row");
    let target = sink_row(win, out.name).expect("target row");
    let preview = source.preview.clone().expect("preview controls");
    select_preview_target(&source, &target);
    let index = source.info.borrow().index;
    win.request_preview(index);
    wait_for("preview to start", || preview.is_on());

    let inputs = pactl(&["list", "short", "sink-inputs"]).lines().count();
    assert!(inputs >= 1);
    assert!(pactl(&["list", "sink-inputs"]).contains("Live preview output"));
    pump(Duration::from_millis(100));
    assert_eq!(win.playback.borrow().len(), win.playback.borrow().values().filter(|r| {
        r.info.borrow().media_name != "Live preview output"
    }).count(), "own stream is hidden");

    // Earlier steps leave the feed quiet, and the glitch check needs the full tone.
    pactl(&["set-sink-volume", feed.name, "100%"]);
    let samples = play_and_capture(feed.name, out.name, 4.0);
    // Skip the start, where the tone has not yet arrived.
    let steady = samples.get(RATE..).unwrap_or_default();
    let glitches = count_glitches(steady);
    let peak = steady.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    println!(
        "preview: {} samples, peak {peak}, {glitches} glitches, latency {}",
        steady.len(),
        preview.latency_text()
    );
    assert!(peak > 8_000, "tone did not get through the preview");
    // Heavy CPU load breaks this check: audio here glitches then even with no preview.
    assert!(glitches < 5, "preview damaged the audio: {glitches} glitches");

    other_source_leaves_the_preview_alone(win, &source, &format!("{}.monitor", out.name), feed.name);
    cancelling_a_retarget_stops_the_audio(win, &source, feed.name);
    select_preview_target(&source, &target);
    win.request_preview(index);
    wait_for("preview to restart", || preview.is_on());

    out.unload();
    wait_for("preview to stop on removal", || !preview.is_on());
    assert!(win.error.reveals_child(), "banner says why it stopped");
    assert!(!pactl(&["list", "source-outputs"]).contains("Live preview input"));
}

fn bluetooth_profiles(win: &Window) {
    let card = win.cards.borrow().values().find(|c| c.info.borrow().is_bluetooth()).cloned();
    let card = card.expect("a Bluetooth card");
    let (index, name) = (card.info.borrow().index, card.info.borrow().name.clone());
    let original = card.info.borrow().active_profile.clone().expect("active profile");
    let _restore = RestoreProfile { card: name.clone(), profile: original.clone() };
    let active = || card.info.borrow().active_profile.clone().unwrap_or_default();
    let server_profile = || {
        let all = pactl(&["list", "cards"]);
        let tail = all.split_once(name.as_str()).map_or("", |(_, tail)| tail);
        let line = tail.lines().find(|l| l.contains("Active Profile")).unwrap_or_default();
        line.split(": ").nth(1).unwrap_or_default().to_string()
    };

    let names: Vec<String> = card.info.borrow().profiles.iter().map(|p| p.name.clone()).collect();
    for profile in names.iter().filter(|p| *p != "off" && **p != original) {
        card.pick(profile);
        wait_for("profile switch", || active() == *profile && server_profile() == *profile);
        pump(Duration::from_millis(1500));
        println!("bluetooth: {profile} -> {:?}", card.label_of(profile));
    }

    // Back in A2DP, a preview on the loopback source must offer the switch and not connect.
    let a2dp = names.iter().find(|p| profile_group(p) == ProfileGroup::A2dp).expect("A2DP profile");
    card.pick(a2dp);
    wait_for("A2DP", || active() == *a2dp);
    pump(Duration::from_millis(1500));
    let source = win.sources.borrow().values().find(|r| {
        r.info.borrow().card == Some(index) && r.info.borrow().monitor.is_none()
    }).cloned();
    if let Some(source) = source {
        let before = gtk::Window::toplevels().n_items();
        let source_index = source.info.borrow().index;
        win.request_preview(source_index);
        pump(Duration::from_millis(1000));
        assert_eq!(gtk::Window::toplevels().n_items(), before + 1, "switch prompt opens");
        assert_eq!(active(), *a2dp, "the prompt must not switch by itself");
        close_dialog();
    }

    // One-click switch, then the mic preview, then a profile switch stops it.
    win.switch_to_headset(index);
    wait_for("headset profile", || profile_group(&active()) == ProfileGroup::Headset);
    let mic = |win: &Window| {
        win.sources.borrow().values().find(|r| {
            r.info.borrow().card == Some(index) && r.info.borrow().monitor.is_none()
        }).cloned()
    };
    wait_for("Bluetooth mic source", || mic(win).is_some());
    pump(Duration::from_millis(1500));
    let source = mic(win).expect("mic row");
    let preview = source.preview.clone().expect("preview controls");
    preview.set_volume(5.0);
    preview.target.dropdown.set_selected(0);
    let default_sink = win.default_sink_row().expect("default sink");
    println!("bluetooth mic preview to {}", default_sink.info.borrow().description);
    let source_index = source.info.borrow().index;
    win.request_preview(source_index);
    wait_for("Bluetooth mic preview", || preview.is_on());
    // The sink may be rebuilt as the mic transport starts. The preview must survive that.
    pump(Duration::from_millis(4000));
    assert!(preview.is_on(), "preview died: {}", win.error_text.text());
    assert!(pactl(&["list", "sink-inputs"]).contains("Live preview output"));
    println!("bluetooth mic latency {}", preview.latency_text());
    assert!(preview.latency_text().ends_with(" ms"), "no latency shown");

    card.pick(&original);
    wait_for("preview to stop on profile switch", || !preview.is_on());
    assert!(win.error_text.text().contains("removed"), "banner says why it stopped");
    wait_for("original profile", || active() == original && server_profile() == original);
}

#[test]
#[ignore = "needs a running audio server and a display"]
fn live_window() {
    // Keeps the test away from the user's real settings file.
    let config = TempDir(std::env::temp_dir().join(format!("genmix-live-test-{}", std::process::id())));
    std::fs::create_dir(&config.0).expect("new config dir");
    std::env::set_var("XDG_CONFIG_HOME", &config.0);
    gtk::init().expect("display");
    let app = gtk::Application::builder().flags(gio::ApplicationFlags::NON_UNIQUE).build();
    app.register(gio::Cancellable::NONE).expect("register application");
    let win = Window::present(&app);
    wait_for("first devices", || !win.sinks.borrow().is_empty());

    throttle_and_feedback_guard();
    balance_survives_zero();
    channel_labels_follow_the_map();
    card_keeps_the_users_expander_state(&win);

    let speaker = NullSink::load("genmix_test_speaker", "");
    let mut phones = NullSink::load("genmix_test_phones", " device.form_factor=headset");
    wait_for("test sinks", || {
        sink_row(&win, speaker.name).is_some() && sink_row(&win, phones.name).is_some()
    });
    wait_for("test monitors", || {
        source_row(&win, "genmix_test_speaker.monitor").is_some()
            && source_row(&win, "genmix_test_phones.monitor").is_some()
    });

    volume_round_trip(&win, &speaker);
    meters_do_not_pile_up(&win);
    dead_meter_reopens(&win);
    command_errors(&win);
    monitor_into_own_sink_is_refused(&win, phones.name);
    speaker_target_asks_first(&win, "genmix_test_phones.monitor", speaker.name);
    hiding_monitors_stops_their_preview(&win, &speaker, &phones);
    preview_survives_one_kill_but_not_two(&win, &speaker, &phones);
    if std::env::var_os("GENMIX_TEST_BLUETOOTH").is_some() {
        default_change_stops_preview(&win, &speaker, &phones);
    }
    preview_copies_audio_and_stops_on_removal(&win, &speaker, &mut phones);

    if std::env::var_os("GENMIX_TEST_BLUETOOTH").is_some() {
        bluetooth_profiles(&win);
    }
}
