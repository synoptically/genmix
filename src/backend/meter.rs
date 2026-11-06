//! Peak level streams, and the one safe way to close any stream.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;
use libpulse_binding::context::Context;
use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::{FlagSet, PeekResult, State, Stream};

use super::{Event, Target};

pub type SharedStream = Rc<RefCell<Stream>>;

const REOPEN_DELAY: Duration = Duration::from_secs(1);

/// What a meter listens to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeterSource {
    pub source: u32,
    /// Limits the meter to one sink input on that source's sink.
    pub sink_input: Option<u32>,
    /// Keeps the device awake. A suspended mic gives no levels, so the Input tab
    /// sets this. An output has nothing to show while suspended, so it never needs it.
    pub wake: bool,
}

pub struct Meter {
    pub from: MeterSource,
    stream: SharedStream,
}

impl Meter {
    pub fn is_pending(&self) -> bool {
        self.stream.borrow().get_state() == State::Creating
    }

    pub fn is_dead(&self) -> bool {
        is_dead(&self.stream)
    }
}

fn is_dead(stream: &SharedStream) -> bool {
    matches!(stream.borrow().get_state(), State::Failed | State::Terminated)
}

/// Whether a running meter stays. A pending one nobody wants stays because it cannot
/// close yet, but one with the wrong `from` must go, or the wanted one never opens.
pub fn keep(wanted: Option<&MeterSource>, from: &MeterSource, pending: bool) -> bool {
    wanted.map_or(pending, |w| w == from)
}

impl Drop for Meter {
    fn drop(&mut self) {
        close_stream(&self.stream);
    }
}

/// Closes a stream without leaving C pointers to freed closures.
///
/// `Stream::drop` frees the closures but does not unset them, and the server's reply
/// to the disconnect can still fire one. It also cannot disconnect a stream that is
/// still `Creating`, which would leave that stream open on the server. So wait for
/// the server to answer first. Do not call this from inside the stream's own
/// callback, because replacing a callback frees the closure that is running.
pub fn close_stream(stream: &SharedStream) {
    let mut s = stream.borrow_mut();
    s.set_read_callback(None);
    if s.get_state() == State::Creating {
        let keep = Rc::clone(stream);
        s.set_state_callback(Some(Box::new(move || {
            let keep = Rc::clone(&keep);
            if keep.borrow().get_state() != State::Creating {
                glib::idle_add_local_once(move || close_stream(&keep));
            }
        })));
    } else {
        s.set_state_callback(None);
    }
}

pub fn open(
    ctx: &mut Context,
    target: Target,
    from: MeterSource,
    emit: Rc<dyn Fn(Event)>,
) -> Option<Meter> {
    // The server sends one peak value per fragment: 25 floats a second.
    let spec = Spec { format: Format::FLOAT32NE, channels: 1, rate: 25 };
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: size_of::<f32>() as u32,
    };
    let mut stream = Stream::new(ctx, "Peak detect", &spec, None)?;
    if let Some(index) = from.sink_input {
        stream.set_monitor_stream(index).ok()?;
    }
    let mut flags = FlagSet::PEAK_DETECT | FlagSet::ADJUST_LATENCY | FlagSet::DONT_MOVE;
    if !from.wake {
        flags |= FlagSet::DONT_INHIBIT_AUTO_SUSPEND;
    }
    stream.connect_record(Some(&from.source.to_string()), Some(&attr), flags).ok()?;

    let stream = Rc::new(RefCell::new(stream));
    let (weak, lost) = (Rc::downgrade(&stream), Rc::clone(&emit));
    stream.borrow_mut().set_state_callback(Some(Box::new(move || {
        if weak.upgrade().is_some_and(|stream| is_dead(&stream)) {
            let lost = Rc::clone(&lost);
            glib::timeout_add_local_once(REOPEN_DELAY, move || lost(Event::MeterLost));
        }
    })));
    let weak = Rc::downgrade(&stream);
    stream.borrow_mut().set_read_callback(Some(Box::new(move |_| {
        let Some(stream) = weak.upgrade() else { return };
        let level = read_last_peak(&mut stream.borrow_mut());
        if let Some(level) = level {
            emit(Event::Peak { meter: target, level });
        }
    })));
    Some(Meter { from, stream })
}

fn read_last_peak(stream: &mut Stream) -> Option<f32> {
    let mut level = None;
    loop {
        match stream.peek() {
            Ok(PeekResult::Data(data)) => {
                level = data.last_chunk().map(|b| f32::from_ne_bytes(*b)).or(level);
                let _ = stream.discard();
            }
            Ok(PeekResult::Hole(_)) => {
                let _ = stream.discard();
            }
            _ => return level,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_meter_stays_only_while_it_blocks_nothing() {
        let passive = MeterSource { source: 1, sink_input: None, wake: false };
        let waking = MeterSource { wake: true, ..passive };
        assert!(keep(Some(&passive), &passive, false));
        assert!(keep(None, &passive, true));
        assert!(!keep(None, &passive, false));
        assert!(!keep(Some(&waking), &passive, true));
    }
}
