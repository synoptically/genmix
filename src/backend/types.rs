//! Plain data the UI works with, plus the rules that read meaning out of it.

use libpulse_binding::channelmap::Map;
use libpulse_binding::def::DevicePortType;
use libpulse_binding::sample::Spec;
use libpulse_binding::volume::{ChannelVolumes, Volume};

#[derive(Clone, Debug)]
pub struct PortInfo {
    pub name: String,
    pub description: String,
    pub unplugged: bool,
    pub kind: DevicePortType,
}

/// One sink or source. libpulse has two near-identical structs; we need one.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub index: u32,
    pub name: String,
    pub description: String,
    pub volume: ChannelVolumes,
    pub map: Map,
    pub mute: bool,
    /// For a sink: its monitor source. For a source: the sink it monitors, if any.
    pub monitor: Option<u32>,
    /// False means no meter data will arrive, so the last level is stale.
    pub running: bool,
    pub ports: Vec<PortInfo>,
    pub active_port: Option<String>,
    pub card: Option<u32>,
    pub form_factor: Option<String>,
    pub bus: Option<String>,
    pub driver: String,
    pub codec: Option<String>,
    pub spec: Spec,
    pub latency_usec: u64,
}

pub type SinkInfo = DeviceInfo;
pub type SourceInfo = DeviceInfo;

/// One sink input or source output.
#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub index: u32,
    pub app_name: String,
    pub media_name: String,
    pub icon_name: Option<String>,
    pub binary: Option<String>,
    /// The sink or source this stream is attached to.
    pub device: u32,
    pub volume: ChannelVolumes,
    pub map: Map,
    pub mute: bool,
    pub has_volume: bool,
    pub volume_writable: bool,
    pub corked: bool,
    pub is_event: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProfileInfo {
    pub name: String,
    pub description: String,
    pub priority: u32,
    pub available: bool,
    pub n_sinks: u32,
    pub n_sources: u32,
}

#[derive(Clone, Debug)]
pub struct CardInfo {
    pub index: u32,
    pub name: String,
    pub description: String,
    pub bus: Option<String>,
    pub driver: String,
    pub form_factor: Option<String>,
    pub profiles: Vec<ProfileInfo>,
    pub active_profile: Option<String>,
    /// Some backends put the codec on the card. PipeWire puts it on the sink.
    pub codec: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileGroup {
    A2dp,
    Headset,
    Off,
    Other,
}

impl ProfileGroup {
    pub const ALL: [Self; 4] = [Self::A2dp, Self::Headset, Self::Other, Self::Off];

    pub fn title(self) -> &'static str {
        match self {
            Self::A2dp => "High quality playback (A2DP)",
            Self::Headset => "Headset with mic (HSP/HFP)",
            Self::Off => "Off",
            Self::Other => "Other",
        }
    }
}

pub fn profile_group(name: &str) -> ProfileGroup {
    match name {
        "off" => ProfileGroup::Off,
        n if n.starts_with("a2dp") => ProfileGroup::A2dp,
        n if n.starts_with("headset") || n.starts_with("handsfree") => ProfileGroup::Headset,
        _ => ProfileGroup::Other,
    }
}

/// Finds the Bluetooth codec of a profile. PipeWire gives the best codec the bare
/// name (`a2dp-sink` can be AAC), so the description is the reliable place to look.
pub fn parse_codec(name: &str, description: &str) -> Option<String> {
    if let Some((_, rest)) = description.split_once("codec ") {
        let codec = rest.split([')', ',']).next().unwrap_or(rest).trim();
        if !codec.is_empty() {
            return Some(codec.to_string());
        }
    }
    let suffix = ["a2dp-sink-", "a2dp-source-", "headset-head-unit-", "headset-audio-gateway-"]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))?;
    Some(suffix.to_uppercase().replace('_', "-"))
}

/// Builds the profile text. `sink_codec` fills in for a profile that names no codec.
pub fn profile_label(profile: &ProfileInfo, sink_codec: Option<&str>) -> String {
    let base = match profile_group(&profile.name) {
        ProfileGroup::A2dp => "High Fidelity Playback (A2DP)",
        ProfileGroup::Headset => "Headset Head Unit (HSP/HFP)",
        _ => return profile.description.clone(),
    };
    let codec = parse_codec(&profile.name, &profile.description);
    match codec.as_deref().or(sink_codec) {
        Some(codec) => format!("{base}: {}", codec.to_uppercase()),
        None => base.to_string(),
    }
}

/// True when output may reach a microphone through the air.
pub fn is_speaker_type(form_factor: Option<&str>, port: Option<DevicePortType>) -> bool {
    use DevicePortType as P;
    let worn_form = matches!(form_factor, Some("headset" | "headphone" | "hands-free" | "handset"));
    let worn_port = matches!(port, Some(P::Headset | P::Headphones | P::Handset | P::Earpiece));
    !(worn_form || worn_port)
}

impl DeviceInfo {
    pub fn port_kind(&self) -> Option<DevicePortType> {
        let active = self.active_port.as_deref()?;
        self.ports.iter().find(|p| p.name == active).map(|p| p.kind)
    }

    pub fn is_speaker_type(&self) -> bool {
        is_speaker_type(self.form_factor.as_deref(), self.port_kind())
    }

    pub fn is_on_card(&self, card: &CardInfo) -> bool {
        self.card == Some(card.index)
    }

    /// True for a mic on a Bluetooth card whose profile has no mic. A monitor taps
    /// the output, so it works in any profile.
    pub fn mic_is_off(&self, card: &CardInfo) -> bool {
        self.monitor.is_none() && self.is_on_card(card) && card.headset_profile_for_mic().is_some()
    }

    pub fn icon(&self, is_input: bool) -> &'static str {
        use DevicePortType as P;
        match (self.form_factor.as_deref(), self.port_kind()) {
            (Some("headset" | "hands-free" | "handset"), _) | (_, Some(P::Headset)) => {
                "audio-headset-symbolic"
            }
            (Some("headphone"), _) | (_, Some(P::Headphones)) => "audio-headphones-symbolic",
            (_, Some(P::HDMI | P::TV)) => "video-display-symbolic",
            _ if is_input => "audio-input-microphone-symbolic",
            (Some("speaker" | "internal"), _) | (_, Some(P::Speaker)) => "audio-speakers-symbolic",
            _ if self.name.contains("hdmi") => "video-display-symbolic",
            _ => "audio-card-symbolic",
        }
    }
}

impl CardInfo {
    pub fn is_bluetooth(&self) -> bool {
        self.bus.as_deref() == Some("bluetooth")
    }

    /// When a Bluetooth card's active profile has no mic, returns the best profile
    /// that has one.
    pub fn headset_profile_for_mic(&self) -> Option<&ProfileInfo> {
        let active = self.active_profile.as_deref()?;
        let has_mic = self.profiles.iter().any(|p| p.name == active && p.n_sources > 0);
        if !self.is_bluetooth() || has_mic {
            return None;
        }
        self.profiles
            .iter()
            .filter(|p| p.n_sources > 0 && profile_group(&p.name) == ProfileGroup::Headset)
            // Not only available ones: a profile can report unavailable for a moment,
            // and without this the app would connect and WirePlumber would switch alone.
            .max_by_key(|p| (p.available, p.priority))
    }
}

pub fn to_percent(v: Volume) -> f64 {
    f64::from(v.0) * 100.0 / f64::from(Volume::NORMAL.0)
}

pub fn from_percent(percent: f64) -> Volume {
    let raw = percent.max(0.0) * f64::from(Volume::NORMAL.0) / 100.0;
    Volume((raw.round() as u32).min(Volume::MAX.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, description: &str, n_sources: u32, priority: u32) -> ProfileInfo {
        ProfileInfo {
            name: name.into(),
            description: description.into(),
            priority,
            available: true,
            n_sinks: 1,
            n_sources,
        }
    }

    // Names and descriptions below are what PipeWire 1.6 reports for a Sony WF-C710N.
    #[test]
    fn codec_comes_from_description_first() {
        let aac = "High Fidelity Playback (A2DP Sink, codec AAC)";
        assert_eq!(parse_codec("a2dp-sink", aac).as_deref(), Some("AAC"));
        let xq = "High Fidelity Playback (A2DP Sink, codec SBC-XQ)";
        assert_eq!(parse_codec("a2dp-sink-sbc_xq", xq).as_deref(), Some("SBC-XQ"));
        let msbc = "Headset Head Unit (HSP/HFP, codec MSBC)";
        assert_eq!(parse_codec("headset-head-unit", msbc).as_deref(), Some("MSBC"));
    }

    #[test]
    fn codec_falls_back_to_name_suffix_then_nothing() {
        assert_eq!(parse_codec("a2dp-sink-ldac", "A2DP").as_deref(), Some("LDAC"));
        assert_eq!(parse_codec("a2dp-sink-sbc_xq", "").as_deref(), Some("SBC-XQ"));
        assert_eq!(parse_codec("a2dp-sink", "High Fidelity Playback (A2DP Sink)"), None);
        assert_eq!(parse_codec("output:analog-stereo", "Analog Stereo Output"), None);
    }

    #[test]
    fn profile_groups() {
        assert_eq!(profile_group("a2dp-sink-sbc"), ProfileGroup::A2dp);
        assert_eq!(profile_group("headset-head-unit-cvsd"), ProfileGroup::Headset);
        assert_eq!(profile_group("off"), ProfileGroup::Off);
        assert_eq!(profile_group("output:hdmi-stereo"), ProfileGroup::Other);
        assert_eq!(profile_group("pro-audio"), ProfileGroup::Other);
    }

    #[test]
    fn labels_use_sink_codec_only_when_profile_names_none() {
        let generic = profile("a2dp-sink", "High Fidelity Playback (A2DP Sink)", 0, 1);
        assert_eq!(profile_label(&generic, None), "High Fidelity Playback (A2DP)");
        assert_eq!(profile_label(&generic, Some("aac")), "High Fidelity Playback (A2DP): AAC");
        let sbc = profile("a2dp-sink-sbc", "(A2DP Sink, codec SBC)", 0, 1);
        assert_eq!(profile_label(&sbc, Some("aac")), "High Fidelity Playback (A2DP): SBC");
        let alsa = profile("output:analog-stereo", "Analog Stereo Output", 0, 1);
        assert_eq!(profile_label(&alsa, Some("aac")), "Analog Stereo Output");
    }

    #[test]
    fn speaker_type_rules() {
        use DevicePortType as P;
        // USB Arctis: form factor decides, the port is only "Analog".
        assert!(!is_speaker_type(Some("headset"), Some(P::Analog)));
        assert!(!is_speaker_type(None, Some(P::Headphones)));
        // Unknown counts as a speaker: a false warning is cheaper than feedback.
        assert!(is_speaker_type(None, Some(P::SPDIF)));
        assert!(is_speaker_type(None, None));
        assert!(is_speaker_type(Some("internal"), Some(P::Speaker)));
    }

    fn card(active: &str, bus: &str) -> CardInfo {
        CardInfo {
            index: 1,
            name: "bluez_card.x".into(),
            description: "WF-C710N".into(),
            bus: Some(bus.into()),
            driver: String::new(),
            form_factor: None,
            profiles: vec![
                profile("a2dp-sink", "codec AAC", 0, 133),
                profile("headset-head-unit-cvsd", "codec CVSD", 1, 5),
                profile("headset-head-unit", "codec MSBC", 1, 6),
            ],
            active_profile: Some(active.into()),
            codec: None,
        }
    }

    #[test]
    fn mic_switch_offers_best_headset_profile_only_when_needed() {
        let best = card("a2dp-sink", "bluetooth");
        assert_eq!(best.headset_profile_for_mic().map(|p| p.name.as_str()), Some("headset-head-unit"));
        assert_eq!(card("headset-head-unit", "bluetooth").headset_profile_for_mic(), None);
        assert_eq!(card("a2dp-sink", "usb").headset_profile_for_mic(), None);

        let mut unavailable = card("a2dp-sink", "bluetooth");
        unavailable.profiles[2].available = false;
        let best = unavailable.headset_profile_for_mic().map(|p| p.name.as_str());
        assert_eq!(best, Some("headset-head-unit-cvsd"), "available wins over priority");
        unavailable.profiles[1].available = false;
        assert!(unavailable.headset_profile_for_mic().is_some(), "mic is still off");
    }

    #[test]
    fn only_a_real_mic_needs_the_headset_profile() {
        let mut source = DeviceInfo {
            index: 1,
            name: String::new(),
            description: String::new(),
            volume: ChannelVolumes::default(),
            map: Map::default(),
            mute: false,
            monitor: None,
            running: false,
            ports: Vec::new(),
            active_port: None,
            card: Some(1),
            form_factor: None,
            bus: None,
            driver: String::new(),
            codec: None,
            spec: Spec { format: libpulse_binding::sample::Format::S16NE, channels: 2, rate: 48_000 },
            latency_usec: 0,
        };
        let a2dp = card("a2dp-sink", "bluetooth");
        assert!(source.mic_is_off(&a2dp));
        assert!(!source.mic_is_off(&card("headset-head-unit", "bluetooth")));
        source.monitor = Some(5);
        assert!(!source.mic_is_off(&a2dp));
    }

    #[test]
    fn percent_round_trip_and_bounds() {
        assert_eq!(from_percent(100.0), Volume::NORMAL);
        assert_eq!(to_percent(Volume::NORMAL), 100.0);
        assert_eq!(from_percent(-5.0), Volume::MUTED);
        assert_eq!(from_percent(1e12), Volume::MAX);
    }
}
