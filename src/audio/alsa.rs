//! ALSA via dlopen(libasound.so.2).

use std::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void, CStr, CString};
use std::sync::OnceLock;
use std::time::Duration;

use librespot_playback::audio_backend::{Sink, SinkError, SinkResult};
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;

use super::{AudioOut, FormatPref, Notify, OutBuf, OutSpec, SampleFormat};

const SND_PCM_STREAM_PLAYBACK: c_int = 0;
const SND_PCM_FORMAT_S16_LE: c_int = 2;
const SND_PCM_FORMAT_S32_LE: c_int = 10;
const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;
const CHANNELS: usize = 2;

type RawPcm = c_void;

struct Lib {
    open: unsafe extern "C" fn(*mut *mut RawPcm, *const c_char, c_int, c_int) -> c_int,
    set_params:
        unsafe extern "C" fn(*mut RawPcm, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int,
    writei: unsafe extern "C" fn(*mut RawPcm, *const c_void, c_ulong) -> c_long,
    recover: unsafe extern "C" fn(*mut RawPcm, c_int, c_int) -> c_int,
    delay: unsafe extern "C" fn(*mut RawPcm, *mut c_long) -> c_int,
    drop: unsafe extern "C" fn(*mut RawPcm) -> c_int,
    drain: unsafe extern "C" fn(*mut RawPcm) -> c_int,
    close: unsafe extern "C" fn(*mut RawPcm) -> c_int,
    strerror: unsafe extern "C" fn(c_int) -> *const c_char,
}

// Function pointers into a library that is never unloaded.
unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

fn lib() -> Result<&'static Lib, String> {
    static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(load).as_ref().map_err(Clone::clone)
}

fn load() -> Result<Lib, String> {
    unsafe {
        let mut handle = std::ptr::null_mut();
        for name in [c"libasound.so.2", c"libasound.so"] {
            handle = libc::dlopen(name.as_ptr(), libc::RTLD_NOW);
            if !handle.is_null() {
                break;
            }
        }
        if handle.is_null() {
            return Err("không tìm thấy libasound.so.2".into());
        }
        macro_rules! sym {
            ($name:literal) => {{
                let p = libc::dlsym(handle, concat!($name, "\0").as_ptr() as *const c_char);
                if p.is_null() {
                    return Err(format!("libasound thiếu hàm {}", $name));
                }
                std::mem::transmute(p)
            }};
        }
        Ok(Lib {
            open: sym!("snd_pcm_open"),
            set_params: sym!("snd_pcm_set_params"),
            writei: sym!("snd_pcm_writei"),
            recover: sym!("snd_pcm_recover"),
            delay: sym!("snd_pcm_delay"),
            drop: sym!("snd_pcm_drop"),
            drain: sym!("snd_pcm_drain"),
            close: sym!("snd_pcm_close"),
            strerror: sym!("snd_strerror"),
        })
    }
}

fn err_str(lib: &Lib, code: c_int) -> String {
    unsafe { CStr::from_ptr((lib.strerror)(code)) }
        .to_string_lossy()
        .into_owned()
}

/// An open, configured stereo PCM.
struct Pcm {
    raw: *mut RawPcm,
    lib: &'static Lib,
    frame_bytes: usize,
}

// Only ever used from one thread at a time.
unsafe impl Send for Pcm {}

impl Pcm {
    fn open(
        device: &str,
        format: SampleFormat,
        rate: u32,
        latency_us: u32,
        soft_resample: bool,
    ) -> Result<Pcm, String> {
        let lib = lib()?;
        let name = CString::new(device).unwrap_or_else(|_| c"default".into());
        let mut raw = std::ptr::null_mut();
        let r = unsafe { (lib.open)(&mut raw, name.as_ptr(), SND_PCM_STREAM_PLAYBACK, 0) };
        if r < 0 {
            return Err(format!("mở {device}: {}", err_str(lib, r)));
        }
        let (fmt, sample_bytes) = match format {
            SampleFormat::S16 => (SND_PCM_FORMAT_S16_LE, 2),
            SampleFormat::S32 => (SND_PCM_FORMAT_S32_LE, 4),
        };
        let r = unsafe {
            (lib.set_params)(
                raw,
                fmt,
                SND_PCM_ACCESS_RW_INTERLEAVED,
                CHANNELS as c_uint,
                rate,
                soft_resample as c_int,
                latency_us,
            )
        };
        if r < 0 {
            unsafe { (lib.close)(raw) };
            return Err(format!("{format:?} {rate} Hz: {}", err_str(lib, r)));
        }
        Ok(Pcm {
            raw,
            lib,
            frame_bytes: sample_bytes * CHANNELS,
        })
    }

    fn write_bytes(&mut self, data: &[u8]) -> Result<(), String> {
        let mut offset = 0usize;
        let mut retries = 0;
        while offset < data.len() {
            let frames = ((data.len() - offset) / self.frame_bytes) as c_ulong;
            if frames == 0 {
                break;
            }
            let r = unsafe {
                (self.lib.writei)(self.raw, data[offset..].as_ptr() as *const c_void, frames)
            };
            if r < 0 {
                retries += 1;
                let rr = unsafe { (self.lib.recover)(self.raw, r as c_int, 1) };
                if rr < 0 || retries > 5 {
                    return Err(err_str(self.lib, r as c_int));
                }
                continue;
            }
            offset += r as usize * self.frame_bytes;
        }
        Ok(())
    }

    fn delay(&mut self) -> u32 {
        let mut d: c_long = 0;
        let r = unsafe { (self.lib.delay)(self.raw, &mut d) };
        if r < 0 {
            0
        } else {
            d.max(0) as u32
        }
    }

    fn drain(self) {
        unsafe { (self.lib.drain)(self.raw) };
        // Drop closes.
    }
}

impl Drop for Pcm {
    fn drop(&mut self) {
        unsafe {
            (self.lib.drop)(self.raw);
            (self.lib.close)(self.raw);
        }
    }
}

fn as_bytes<T>(s: &[T]) -> &[u8] {
    // i16/i32 slices reinterpreted as little-endian bytes (the device is little endian).
    unsafe { std::slice::from_raw_parts(s.as_ptr() as *const u8, std::mem::size_of_val(s)) }
}

/// Opens the device, preferring the native rate and 32-bit samples so nothing
/// is resampled or truncated. Falls back to 16-bit, then to common rates
/// (the caller resamples), and finally lets ALSA resample.
fn open_best(device: &str, rate: u32, latency_us: u32, pref: FormatPref) -> Result<(Pcm, OutSpec), String> {
    let formats: &[SampleFormat] = match pref {
        FormatPref::Auto | FormatPref::S32 => &[SampleFormat::S32, SampleFormat::S16],
        FormatPref::S16 => &[SampleFormat::S16],
    };
    let mut rates = vec![rate];
    for r in [48_000, 44_100] {
        if !rates.contains(&r) {
            rates.push(r);
        }
    }
    let mut errors: Vec<String> = Vec::new();
    for attempt in 0..12 {
        errors.clear();
        for &r in &rates {
            for &f in formats {
                match Pcm::open(device, f, r, latency_us, false) {
                    Ok(p) => return Ok((p, OutSpec { rate: r, format: f })),
                    Err(e) => errors.push(e),
                }
            }
        }
        match Pcm::open(device, formats[0], rate, latency_us, true) {
            Ok(p) => {
                return Ok((
                    p,
                    OutSpec {
                        rate,
                        format: formats[0],
                    },
                ))
            }
            Err(e) => errors.push(e),
        }
        // The Spotify player may still be releasing the device.
        let busy = errors.iter().any(|e| e.to_ascii_lowercase().contains("busy"));
        if !busy || attempt == 11 {
            break;
        }
        std::thread::sleep(Duration::from_millis(120));
    }
    for e in &errors {
        log::warn!("audio open: {e}");
    }
    Err(errors.into_iter().next().unwrap_or_else(|| "không mở được thiết bị âm thanh".into()))
}

// ----------------------------------------------------------------- local player

pub struct AlsaOut {
    device: String,
    latency_us: u32,
    pref: FormatPref,
    pcm: Option<(Pcm, OutSpec)>,
}

impl AlsaOut {
    pub fn new(device: String, latency_ms: u32, pref: FormatPref) -> Self {
        Self {
            device,
            latency_us: latency_ms.clamp(50, 1000) * 1000,
            pref,
            pcm: None,
        }
    }
}

impl AudioOut for AlsaOut {
    fn open(&mut self, rate: u32) -> Result<OutSpec, String> {
        self.close();
        let (pcm, spec) = open_best(&self.device, rate, self.latency_us, self.pref)?;
        log::info!("local audio: {rate} Hz source -> device {:?}", spec);
        self.pcm = Some((pcm, spec));
        Ok(spec)
    }

    fn write(&mut self, buf: OutBuf) -> Result<(), String> {
        let Some((pcm, _)) = self.pcm.as_mut() else {
            return Err("thiết bị âm thanh chưa mở".into());
        };
        let r = match buf {
            OutBuf::S16(s) => pcm.write_bytes(as_bytes(s)),
            OutBuf::S32(s) => pcm.write_bytes(as_bytes(s)),
        };
        if r.is_err() {
            self.pcm = None;
        }
        r
    }

    fn delay_frames(&mut self) -> u32 {
        self.pcm.as_mut().map(|p| p.0.delay()).unwrap_or(0)
    }

    fn drain(&mut self) {
        if let Some((pcm, _)) = self.pcm.take() {
            pcm.drain();
        }
    }

    fn close(&mut self) {
        self.pcm = None;
    }
}

// ----------------------------------------------------------------- Spotify sink

pub struct AlsaSink {
    device: String,
    latency_us: u32,
    pref: FormatPref,
    pcm: Option<(Pcm, OutSpec)>,
    notify: Notify,
}

impl AlsaSink {
    pub fn new(device: String, latency_ms: u32, pref: FormatPref, notify: Notify) -> Self {
        Self {
            device,
            latency_us: latency_ms.clamp(20, 1000) * 1000,
            pref,
            pcm: None,
            notify,
        }
    }

    fn open(&mut self) -> SinkResult<()> {
        if self.pcm.is_some() {
            return Ok(());
        }
        // Spotify streams are always 44.1 kHz; let ALSA convert if the device can't.
        let formats: &[SampleFormat] = match self.pref {
            FormatPref::S16 => &[SampleFormat::S16],
            _ => &[SampleFormat::S32, SampleFormat::S16],
        };
        let mut last = String::new();
        for soft in [false, true] {
            for &f in formats {
                match Pcm::open(&self.device, f, 44_100, self.latency_us, soft) {
                    Ok(p) => {
                        self.pcm = Some((
                            p,
                            OutSpec {
                                rate: 44_100,
                                format: f,
                            },
                        ));
                        return Ok(());
                    }
                    Err(e) => last = e,
                }
            }
        }
        log::error!("audio: {last}");
        (self.notify)(format!("Lỗi âm thanh: {last}"));
        Err(SinkError::ConnectionRefused(last))
    }
}

impl Sink for AlsaSink {
    fn start(&mut self) -> SinkResult<()> {
        self.open()
    }

    fn stop(&mut self) -> SinkResult<()> {
        // Drop instead of drain so pausing is instant, and free the device for
        // the local player.
        self.pcm = None;
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        let AudioPacket::Samples(samples) = packet else {
            return Err(SinkError::OnWrite("raw audio packets are not supported".into()));
        };
        self.open()?;
        let Some((pcm, spec)) = self.pcm.as_mut() else {
            return Err(SinkError::NotConnected("no pcm".into()));
        };
        let r = match spec.format {
            // 32-bit keeps the soft volume's precision instead of truncating to 16 bits.
            SampleFormat::S32 => pcm.write_bytes(as_bytes(&converter.f64_to_s32(&samples))),
            SampleFormat::S16 => pcm.write_bytes(as_bytes(&converter.f64_to_s16(&samples))),
        };
        r.map_err(|e| {
            self.pcm = None;
            SinkError::OnWrite(e)
        })
    }
}
