//! Audio output for both players.
//!
//! On the device we talk to ALSA through `libasound.so.2`, loaded at runtime with
//! dlopen so the binary can be cross-compiled without an ARM sysroot.

#[cfg(all(target_os = "linux", not(feature = "desktop")))]
mod alsa;
#[cfg(feature = "desktop")]
mod desktop;

use librespot_playback::audio_backend::Sink;

pub type Notify = Box<dyn Fn(String) + Send + Sync>;

/// Sample format actually sent to the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(all(target_os = "linux", not(feature = "desktop"))), allow(dead_code))]
pub enum SampleFormat {
    S16,
    S32,
}

/// What the device was opened with. Always stereo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutSpec {
    pub rate: u32,
    pub format: SampleFormat,
}

/// Which formats to try, from settings.json `audio_output_format`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatPref {
    Auto,
    S16,
    S32,
}

impl FormatPref {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "s16" | "16" => FormatPref::S16,
            "s32" | "32" => FormatPref::S32,
            _ => FormatPref::Auto,
        }
    }
}

pub enum OutBuf<'a> {
    S16(&'a [i16]),
    S32(&'a [i32]),
}

/// Output used by the local-file player.
pub trait AudioOut {
    /// Opens the device as close to `rate` as it allows. If the returned rate
    /// differs, the caller resamples.
    fn open(&mut self, rate: u32) -> Result<OutSpec, String>;
    /// Writes interleaved stereo frames, blocking while the device buffer is full.
    fn write(&mut self, buf: OutBuf) -> Result<(), String>;
    /// Frames written but not yet heard.
    fn delay_frames(&mut self) -> u32;
    /// Plays out what is buffered, then closes.
    fn drain(&mut self);
    /// Stops immediately and closes (pause/seek/skip).
    fn close(&mut self);
}

pub fn make_output(device: &str, latency_ms: u32, pref: FormatPref) -> Box<dyn AudioOut> {
    let _ = (device, latency_ms, pref);
    #[cfg(all(target_os = "linux", not(feature = "desktop")))]
    return Box::new(alsa::AlsaOut::new(device.to_string(), latency_ms, pref));
    #[cfg(feature = "desktop")]
    return Box::new(desktop::RodioOut::new());
    #[allow(unreachable_code)]
    Box::new(NullOut::default())
}

#[cfg(all(target_os = "linux", not(feature = "desktop")))]
pub fn make_sink(device: String, latency_ms: u32, pref: FormatPref, notify: Notify) -> Box<dyn Sink> {
    Box::new(alsa::AlsaSink::new(device, latency_ms, pref, notify))
}

#[cfg(feature = "desktop")]
pub fn make_sink(_device: String, _latency_ms: u32, _pref: FormatPref, _notify: Notify) -> Box<dyn Sink> {
    let builder = librespot_playback::audio_backend::find(Some("rodio".into()))
        .expect("rodio backend compiled in");
    builder(None, librespot_playback::config::AudioFormat::S16)
}

#[cfg(not(any(target_os = "linux", feature = "desktop")))]
pub fn make_sink(_device: String, _latency_ms: u32, _pref: FormatPref, notify: Notify) -> Box<dyn Sink> {
    notify("Bản build này không có đầu ra âm thanh".into());
    Box::new(NullSink)
}

#[cfg(not(any(target_os = "linux", feature = "desktop")))]
struct NullSink;

#[cfg(not(any(target_os = "linux", feature = "desktop")))]
impl Sink for NullSink {
    fn write(
        &mut self,
        _: librespot_playback::decoder::AudioPacket,
        _: &mut librespot_playback::convert::Converter,
    ) -> librespot_playback::audio_backend::SinkResult<()> {
        std::thread::sleep(std::time::Duration::from_millis(20));
        Ok(())
    }
}

/// Silent output that keeps real-time pace (builds without an audio backend).
#[derive(Default)]
#[cfg_attr(any(feature = "desktop", target_os = "linux"), allow(dead_code))]
struct NullOut {
    spec: Option<OutSpec>,
}

impl AudioOut for NullOut {
    fn open(&mut self, rate: u32) -> Result<OutSpec, String> {
        let s = OutSpec {
            rate,
            format: SampleFormat::S32,
        };
        self.spec = Some(s);
        Ok(s)
    }
    fn write(&mut self, buf: OutBuf) -> Result<(), String> {
        let samples = match buf {
            OutBuf::S16(s) => s.len(),
            OutBuf::S32(s) => s.len(),
        };
        let rate = self.spec.map(|s| s.rate).unwrap_or(44_100).max(1);
        let ms = (samples / 2) as u64 * 1000 / rate as u64;
        std::thread::sleep(std::time::Duration::from_millis(ms));
        Ok(())
    }
    fn delay_frames(&mut self) -> u32 {
        0
    }
    fn drain(&mut self) {
        self.spec = None;
    }
    fn close(&mut self) {
        self.spec = None;
    }
}
