//! Live mic preview: a record stream copied into a playback stream, in this process.
//! Nothing is loaded on the server, so a crash leaves nothing behind.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;
use libpulse_binding::channelmap::Map;
use libpulse_binding::context::Context;
use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::Spec;
use libpulse_binding::stream::{FlagSet, Latency, PeekResult, SeekMode, State, Stream};
use libpulse_binding::time::MicroSeconds;
use libpulse_binding::volume::{ChannelVolumes, Volume};

use super::meter::{close_stream, SharedStream};
use super::Event;

const RECORD_FRAGMENT: MicroSeconds = MicroSeconds(10_000);
const PLAYBACK_TARGET: MicroSeconds = MicroSeconds(30_000);

pub struct Preview {
    pub source: u32,
    pub sink: u32,
    pub sink_name: String,
    record: SharedStream,
    playback: SharedStream,
    channels: u8,
    timer: Option<glib::SourceId>,
}

impl Drop for Preview {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
        close_stream(&self.record);
        close_stream(&self.playback);
    }
}

impl Preview {
    /// Returns the sink input index once the server has assigned one.
    pub fn playback_index(&self) -> Option<u32> {
        self.playback.borrow().get_index()
    }

    pub fn volumes(&self, volume: Volume) -> ChannelVolumes {
        let mut cv = ChannelVolumes::default();
        cv.set(self.channels, volume);
        cv
    }
}

/// Both streams share the source's format, so the copy needs no conversion.
pub struct Params<'a> {
    pub source: u32,
    pub source_name: &'a str,
    pub sink: u32,
    pub sink_name: &'a str,
    pub spec: Spec,
    pub map: Map,
    pub volume: Volume,
}

/// `on_end` runs if a stream dies, and `on_ready` once audio can flow. Both run
/// inside stream callbacks, so they must defer any teardown.
pub fn start(
    ctx: &mut Context,
    p: &Params,
    emit: Rc<dyn Fn(Event)>,
    on_ready: Rc<dyn Fn()>,
    on_end: Rc<dyn Fn()>,
) -> Result<Preview, String> {
    let failed = || "Could not create preview stream".to_string();
    let mut record = Stream::new(ctx, "Live preview input", &p.spec, Some(&p.map)).ok_or_else(failed)?;
    let mut playback = Stream::new(ctx, "Live preview output", &p.spec, Some(&p.map)).ok_or_else(failed)?;

    let frag = p.spec.usec_to_bytes(RECORD_FRAGMENT) as u32;
    let target = p.spec.usec_to_bytes(PLAYBACK_TARGET) as u32;
    let record_attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: frag,
    };
    let playback_attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: target,
        prebuf: target / 2,
        minreq: frag,
        fragsize: u32::MAX,
    };
    // DONT_MOVE: if the device goes away the server ends the stream, which is what
    // stops the preview, instead of moving it to some other device.
    let flags = FlagSet::ADJUST_LATENCY
        | FlagSet::AUTO_TIMING_UPDATE
        | FlagSet::INTERPOLATE_TIMING
        | FlagSet::DONT_MOVE;

    let mut volume = ChannelVolumes::default();
    volume.set(p.spec.channels, p.volume);
    playback
        .connect_playback(Some(p.sink_name), Some(&playback_attr), flags, Some(&volume), None)
        .map_err(|e| format!("Preview output failed: {e}"))?;
    let playback = Rc::new(RefCell::new(playback));
    if let Err(e) = record.connect_record(Some(p.source_name), Some(&record_attr), flags) {
        close_stream(&playback);
        return Err(format!("Preview input failed: {e}"));
    }

    let record = Rc::new(RefCell::new(record));
    let frame = p.spec.frame_size();

    let (from, to) = (Rc::downgrade(&record), Rc::downgrade(&playback));
    record.borrow_mut().set_read_callback(Some(Box::new(move |_| {
        if let (Some(from), Some(to)) = (from.upgrade(), to.upgrade()) {
            copy_available(&mut from.borrow_mut(), &mut to.borrow_mut(), frame);
        }
    })));
    watch_state(&record, Rc::clone(&on_end), None);
    watch_state(&playback, on_end, Some(on_ready));

    let (r, w) = (Rc::downgrade(&record), Rc::downgrade(&playback));
    let timer = glib::timeout_add_local(Duration::from_millis(500), move || {
        let (Some(r), Some(w)) = (r.upgrade(), w.upgrade()) else {
            return glib::ControlFlow::Break;
        };
        let total = delay(&r.borrow()).zip(delay(&w.borrow())).map(|(a, b)| a + b);
        emit(Event::PreviewLatency(total));
        glib::ControlFlow::Continue
    });

    Ok(Preview {
        source: p.source,
        sink: p.sink,
        sink_name: p.sink_name.to_string(),
        record,
        playback,
        channels: p.spec.channels,
        timer: Some(timer),
    })
}

fn watch_state(stream: &SharedStream, on_end: Rc<dyn Fn()>, on_ready: Option<Rc<dyn Fn()>>) {
    let weak = Rc::downgrade(stream);
    stream.borrow_mut().set_state_callback(Some(Box::new(move || {
        let Some(stream) = weak.upgrade() else { return };
        let state = stream.borrow().get_state();
        match state {
            State::Failed | State::Terminated => on_end(),
            State::Ready => {
                if let Some(on_ready) = &on_ready {
                    on_ready();
                }
            }
            _ => {}
        }
    })));
}

fn delay(stream: &Stream) -> Option<u64> {
    match stream.get_latency() {
        Ok(Latency::Positive(usec)) => Some(usec.0),
        Ok(Latency::Negative(_)) => Some(0),
        _ => None,
    }
}

/// Moves all pending input to the output. Input the output has no room for is
/// dropped, because the two device clocks drift and a queue would grow without limit.
fn copy_available(from: &mut Stream, to: &mut Stream, frame: usize) {
    loop {
        match from.peek() {
            Ok(PeekResult::Data(data)) => {
                let room = if to.get_state() == State::Ready {
                    to.writable_size().unwrap_or(0)
                } else {
                    0
                };
                let n = writable_frames(data.len(), room, frame);
                if n > 0 {
                    let _ = to.write_copy(&data[..n], 0, SeekMode::Relative);
                }
                let _ = from.discard();
            }
            Ok(PeekResult::Hole(_)) => {
                let _ = from.discard();
            }
            _ => return,
        }
    }
}

fn writable_frames(available: usize, room: usize, frame: usize) -> usize {
    available.min(room) / frame * frame
}

#[cfg(test)]
mod tests {
    use super::writable_frames;

    #[test]
    fn writes_only_whole_frames_that_fit() {
        assert_eq!(writable_frames(100, 1000, 8), 96);
        assert_eq!(writable_frames(1000, 100, 8), 96);
        assert_eq!(writable_frames(7, 1000, 8), 0);
        assert_eq!(writable_frames(1000, 0, 8), 0);
    }
}
