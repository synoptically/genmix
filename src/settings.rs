//! User settings, stored in `$XDG_CONFIG_HOME/genmix/settings.toml`.
//!
//! `glib::KeyFile` writes `key=value` under one section header. For the bools and
//! ints used here that is valid TOML, so no TOML crate is needed.

use std::path::{Path, PathBuf};

use gtk::glib;

const GROUP: &str = "settings";
pub const MIN_PERCENT: u32 = 100;
pub const MAX_PERCENT: u32 = 200;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub allow_overamp: bool,
    pub max_percent: u32,
    pub show_monitors: bool,
    pub show_event_streams: bool,
    pub show_meters: bool,
    pub show_details: bool,
    pub skip_speaker_warning: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            allow_overamp: false,
            max_percent: 150,
            show_monitors: false,
            show_event_streams: false,
            show_meters: true,
            show_details: true,
            skip_speaker_warning: false,
        }
    }
}

impl Settings {
    /// Highest value a slider offers, unless a device is already above it.
    pub fn limit(&self) -> f64 {
        f64::from(if self.allow_overamp { self.max_percent } else { MIN_PERCENT })
    }

    pub fn load() -> Self {
        Self::load_from(&path())
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&path())
    }

    /// A missing or broken file gives defaults: settings are never worth failing over.
    fn load_from(path: &Path) -> Self {
        let file = glib::KeyFile::new();
        let d = Self::default();
        if file.load_from_file(path, glib::KeyFileFlags::NONE).is_err() {
            return d;
        }
        let flag = |key, default| file.boolean(GROUP, key).unwrap_or(default);
        let percent = file.integer(GROUP, "max_percent").map_or(d.max_percent, |v| v.max(0) as u32);
        Self {
            allow_overamp: flag("allow_overamp", d.allow_overamp),
            max_percent: percent.clamp(MIN_PERCENT, MAX_PERCENT),
            show_monitors: flag("show_monitors", d.show_monitors),
            show_event_streams: flag("show_event_streams", d.show_event_streams),
            show_meters: flag("show_meters", d.show_meters),
            show_details: flag("show_details", d.show_details),
            skip_speaker_warning: flag("skip_speaker_warning", d.skip_speaker_warning),
        }
    }

    fn save_to(&self, path: &Path) -> Result<(), String> {
        let file = glib::KeyFile::new();
        file.set_boolean(GROUP, "allow_overamp", self.allow_overamp);
        file.set_integer(GROUP, "max_percent", self.max_percent as i32);
        file.set_boolean(GROUP, "show_monitors", self.show_monitors);
        file.set_boolean(GROUP, "show_event_streams", self.show_event_streams);
        file.set_boolean(GROUP, "show_meters", self.show_meters);
        file.set_boolean(GROUP, "show_details", self.show_details);
        file.set_boolean(GROUP, "skip_speaker_warning", self.skip_speaker_warning);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        file.save_to_file(path).map_err(|e| e.to_string())
    }
}

fn path() -> PathBuf {
    glib::user_config_dir().join("genmix").join("settings.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `create_dir` refuses a path that already exists, so no one can plant a link.
    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("genmix-test-{}-{name}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        dir.join("settings.toml")
    }

    fn remove(path: PathBuf) {
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn round_trip() {
        let path = temp_file("round");
        let s = Settings { allow_overamp: true, max_percent: 175, ..Settings::default() };
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), s);
        remove(path);
    }

    #[test]
    fn bad_input_gives_safe_values() {
        assert_eq!(Settings::load_from(Path::new("/nonexistent/genmix")), Settings::default());
        let path = temp_file("bad");
        std::fs::write(&path, "[settings]\nmax_percent=9000\nallow_overamp=maybe\n").unwrap();
        let s = Settings::load_from(&path);
        assert_eq!((s.max_percent, s.allow_overamp), (MAX_PERCENT, false));
        std::fs::write(&path, "not a key file").unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
        remove(path);
    }

    #[test]
    fn slider_limit_ignores_max_while_overamp_is_off() {
        assert_eq!(Settings::default().limit(), 100.0);
        assert_eq!(Settings { allow_overamp: true, ..Settings::default() }.limit(), 150.0);
    }
}
