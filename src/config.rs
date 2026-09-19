use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where the app lives on the SD card and where it keeps its state.
#[derive(Clone, Debug)]
pub struct Paths {
    pub app_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn detect() -> Self {
        let app_dir = std::env::var_os("SPOTY_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(Path::to_path_buf))
            })
            .unwrap_or_else(|| PathBuf::from("."));
        let data_dir = std::env::var_os("SPOTY_DATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| app_dir.join("data"));
        let _ = std::fs::create_dir_all(&data_dir);
        Self { app_dir, data_dir }
    }

    pub fn config_file(&self) -> PathBuf {
        self.app_dir.join("settings.json")
    }
    pub fn log_file(&self) -> PathBuf {
        self.data_dir.join("spoty.log")
    }
    pub fn credentials_dir(&self) -> PathBuf {
        self.data_dir.join("account")
    }
    pub fn audio_cache_dir(&self) -> PathBuf {
        self.data_dir.join("audio_cache")
    }
    pub fn image_cache_dir(&self) -> PathBuf {
        self.data_dir.join("image_cache")
    }
    pub fn fonts_dir(&self) -> PathBuf {
        self.app_dir.join("assets").join("fonts")
    }
    pub fn home_feed_file(&self) -> PathBuf {
        self.data_dir.join("home_feed.json")
    }
    pub fn local_library_file(&self) -> PathBuf {
        self.data_dir.join("local_library.json")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Name shown in the Spotify Connect device list.
    pub device_name: String,
    /// 96, 160 or 320 kbps.
    pub bitrate: u32,
    /// Volume (percent) used the first time, before anything is remembered.
    pub initial_volume: u8,
    /// Volume change per button press, in percent.
    pub volume_step: u8,
    /// ALSA PCM device.
    pub audio_device: String,
    /// ALSA buffer latency. Higher = fewer dropouts, slower pause.
    pub audio_latency_ms: u32,
    /// Size of the on-SD-card audio cache in MB (0 disables it).
    pub audio_cache_mb: u64,
    /// Volume normalisation between tracks.
    pub normalize: bool,
    /// Screen rotation in degrees: 0, 90, 180 or 270.
    pub rotate: u32,
    /// Page-flip the framebuffer instead of copying (only if the driver supports it).
    pub fb_double_buffer: bool,
    /// Take the power button so a short press turns the screen off while music keeps playing.
    pub grab_power_button: bool,
    /// Turn the screen off after this many seconds without input while music plays (0 = never).
    pub screen_off_after_s: u32,
    /// Extra evdev key code -> button name overrides, e.g. {"305": "A"}.
    pub keymap: HashMap<String, String>,
    /// Folder scanned for local music ("" = not chosen yet).
    pub music_dir: String,
    /// Output sample format: "auto" (32-bit when possible), "s32" or "s16".
    pub audio_output_format: String,
    /// ReplayGain for local files: "off", "track" or "album".
    pub replaygain: String,
    /// Local player volume in percent (remembered).
    pub local_volume: u8,
    /// Manifest URL for OTA updates ("" disables them).
    pub update_url: String,
    /// Check for updates automatically at start.
    pub auto_update_check: bool,
    /// When a playlist/album ends, keep playing similar songs (Spotify and local music).
    pub autoplay: bool,
    /// Time zone sent to Spotify for the home feed (e.g. "Asia/Ho_Chi_Minh"; "" = system).
    pub time_zone: String,
    /// Search keyboard layout at start: "vi" (Telex) or "en" (plain letters).
    pub search_keyboard: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: "TrimUI Brick Pro".into(),
            bitrate: 320,
            initial_volume: 70,
            volume_step: 5,
            audio_device: "default".into(),
            audio_latency_ms: 120,
            audio_cache_mb: 1024,
            normalize: false,
            rotate: 0,
            fb_double_buffer: false,
            grab_power_button: true,
            screen_off_after_s: 120,
            keymap: HashMap::new(),
            music_dir: String::new(),
            audio_output_format: "auto".into(),
            replaygain: "off".into(),
            local_volume: 100,
            // Baked in at build time with SPOTY_UPDATE_URL (see build.ps1 -UpdateUrl).
            update_url: option_env!("SPOTY_UPDATE_URL").unwrap_or("").into(),
            auto_update_check: true,
            autoplay: true,
            time_zone: String::new(),
            search_keyboard: "vi".into(),
        }
    }
}

impl Config {
    pub fn save(&self, paths: &Paths) {
        let file = paths.config_file();
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let tmp = file.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::remove_file(&file);
                let _ = std::fs::rename(&tmp, &file);
            }
        }
    }

    /// Loads settings.json, writing a default one if it does not exist yet.
    pub fn load(paths: &Paths) -> Self {
        let file = paths.config_file();
        match std::fs::read_to_string(&file) {
            Ok(text) => match serde_json::from_str::<Config>(&text) {
                Ok(cfg) => {
                    // Rewrite so settings added by updates show up in the file.
                    cfg.save(paths);
                    cfg
                }
                Err(e) => {
                    log::warn!("settings.json is invalid ({e}), using defaults");
                    Config::default()
                }
            },
            Err(_) => {
                let cfg = Config::default();
                if let Ok(text) = serde_json::to_string_pretty(&cfg) {
                    let _ = std::fs::write(&file, text);
                }
                cfg
            }
        }
    }
}
