//! Audio backend. Wraps libpulse and speaks plain Rust to the UI.

mod meter;
mod preview;
mod pulse;
mod types;

pub use meter::MeterSource;
pub use pulse::Backend;
pub use types::*;

/// Something the backend can act on. Also names a level meter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Sink(u32),
    Source(u32),
    SinkInput(u32),
    SourceOutput(u32),
    Card(u32),
}

#[derive(Debug)]
pub enum Event {
    Connected,
    /// The first full set of objects after `Connected` has arrived.
    Loaded,
    /// Connection lost or not yet made. A retry is already scheduled.
    Disconnected,
    Server { default_sink: String, default_source: String },
    Sink(SinkInfo),
    Source(SourceInfo),
    SinkInput(StreamInfo),
    SourceOutput(StreamInfo),
    Card(CardInfo),
    Removed(Target),
    Peak { meter: Target, level: f32 },
    /// A meter stream died. Sent after a delay, so that reopening cannot spin.
    MeterLost,
    PreviewStarted { source: u32, sink: String },
    PreviewStopped { source: u32, reason: Option<String> },
    /// A preview stream died or its output went away, so its streams are closed.
    /// `sink` is the output's name.
    PreviewTargetLost { source: u32, sink: String },
    /// Total measured delay, or `None` until the server has reported timing.
    PreviewLatency(Option<u64>),
    Error(String),
    /// A command got its reply. A failure sends `Error` first, unless the target is gone.
    Done { ok: bool },
}
