# GenMix

An audio mixer for Linux. It controls every device, stream and sound card on
PipeWire (through `pipewire-pulse`) or PulseAudio, including Bluetooth profiles
and codecs, and has a live mic preview.

Built with Rust, GTK4 and libpulse. All audio control goes through the libpulse API;
the app never runs `pactl`, `pw-cli` or `wpctl`.

## Install

You need Rust 1.92 or newer ([rustup](https://rustup.rs) if your distro is older),
GTK 4.12 or newer, and libpulse 14 or newer.

| Distro | Packages |
|---|---|
| Arch | `sudo pacman -S base-devel gtk4 libpulse` |
| Fedora | `sudo dnf install gcc pkgconf-pkg-config gtk4-devel pulseaudio-libs-devel` |
| Debian, Ubuntu | `sudo apt install build-essential pkg-config libgtk-4-dev libpulse-dev` |
| Gentoo | `emerge gui-libs/gtk media-libs/libpulse` |

Build and run:

```sh
cargo build --release
./target/release/genmix
```

To add it to your launcher:

```sh
install -Dm755 target/release/genmix ~/.local/bin/genmix
install -Dm644 data/org.genmix.GenMix.desktop ~/.local/share/applications/org.genmix.GenMix.desktop
```

The icon name is `multimedia-volume-control`, which common icon themes provide.

## Use

The tabs are Playback, Recording, Output Devices, Input Devices and Configuration.
The gear button opens the settings, stored in `$XDG_CONFIG_HOME/genmix/settings.toml`.

The app uses your GTK theme and color scheme. To force one, run for example
`GTK_THEME=Adwaita:dark genmix`.

### Volume above 100%

This is off by default. Sliders then stop at 100%, except that a device already above
100% keeps its real value and its slider extends to show it. Turn on "Allow volume above
100%" in the settings to raise the limit, up to 200%.

### Bluetooth codecs

PipeWire shows each codec as a profile. The best codec gets the plain name, so
`a2dp-sink` may mean AAC and `headset-head-unit` may mean mSBC. GenMix reads the codec
from the profile description, then from the profile name, then from the sink's
`api.bluez5.codec` property.

Which codecs appear depends on two things: what the headset supports, and how PipeWire
was built. AAC, LDAC and aptX each need an optional library at build time, and some
distros package them separately. If a codec is missing, check your PipeWire package
first. The app can only list what the server offers.

### Live preview

The headphone button on an input device plays that input back to you. It copies audio
between two streams inside the app and loads nothing on the server, so a crash leaves
nothing behind. One preview runs at a time.

The latency next to the toggle is measured. As a guide, these were measured on one
machine: 15 to 30 ms between virtual devices, 40 to 60 ms from a USB mic to a USB headset,
60 to 80 ms for a Bluetooth headset in HFP mode, and 200 ms or more when playing to A2DP.
A2DP's own buffering cannot be reduced from here.

A Bluetooth mic needs the headset (HSP/HFP) profile. In A2DP mode the app offers a
one-click switch and does not connect until you have switched.

WirePlumber has its own policy here: about 2 seconds after the last recording stream on
a Bluetooth mic closes, it may switch back to A2DP. This happens with any app, and can be
changed with the `bluetooth.autoswitch-to-headset-profile` setting.

## How each device type is handled

| Device | Behavior |
|---|---|
| ALSA card | Ports listed, unplugged ones greyed. Profiles shown under "Other" and "Off". |
| HDMI | Output icon from the port type. Cards with over 12 profiles start collapsed. Profiles for unconnected displays are marked unavailable and can still be selected. |
| USB headset | `device.form_factor` gives the headset icon and skips the feedback warning, even when the port type is only "Analog". |
| Bluetooth headset | Profiles grouped into A2DP, HSP/HFP and Off, labeled with their codec. The active codec shows on the card row. |

Preview cases:

| Case | Behavior |
|---|---|
| USB mic to headphones | Starts with no warning. |
| Output is a speaker, S/PDIF, or unknown | Warns first, with "Don't ask again". Unknown counts as a speaker on purpose. |
| Bluetooth mic in HFP | Works. PipeWire may rebuild the sink as the mic starts, so the app waits 1 second and resumes on that same sink. |
| Unplug, Bluetooth disconnect, or profile switch | Preview stops and the banner says why. It never moves to another output. |
| Target is "Default" and the default output changes | Preview stops and the banner says why, because the new output never passed the feedback warning. |

If the audio server restarts, the app shows "Reconnecting…", tries again every second,
and rebuilds the lists.

## Layout

```
src/backend/   libpulse: connection, events, commands, meters, preview. No GTK widgets.
src/ui/        GTK rows and the main window, which turns backend events into row updates.
src/settings.rs
```

Everything runs on one thread. libpulse uses the GLib main loop through
`libpulse-glib-binding`, so the backend reports to the UI with a plain callback.

## Tests

```sh
cargo test                                 # no audio server needed
cargo test -- --ignored --nocapture        # needs an audio server and a display
GENMIX_TEST_BLUETOOTH=1 cargo test -- --ignored --nocapture
```

The live test creates null sinks and removes them afterward. The Bluetooth variant
briefly changes the default sink, cycles a real headset through its profiles, and
restores both. It interrupts playing audio. Avoid running it many times in a row:
repeated cycling can drop the headset's connection.

## Known limits

- Tested on PipeWire 1.6 only. Plain PulseAudio uses the same protocol and API calls but
  has not been tested. That includes the "microphone is off" row on the Input tab, which
  exists for PulseAudio, where A2DP mode has no mic source at all.
- Recording rows show the level of their source, not of the stream itself. PulseAudio
  has no way to tap a single recording stream.
- Meters run only for the visible tab. Meters on the Input tab keep the mics awake,
  because a suspended mic reports no level, so a mic-in-use indicator may light up.
- `libpulse-binding` panics on a port type or availability value it does not know, and
  that aborts the app. Today's servers send only known values. The check is inside the
  crate, so the fix belongs upstream.
- The preview copies audio on the UI thread with small buffers, to keep latency low.
  Heavy CPU load causes dropouts in it, more than in other audio on the same machine.
- Preview volume is the volume of the preview's playback stream. With `flat-volumes`
  enabled on PulseAudio, that can also raise the output device's volume.
