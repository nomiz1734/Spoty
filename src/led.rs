//! Lights that follow the music on the TrimUI Brick's RGB LEDs.
//!
//! Both audio outputs hand every block they write to `feed_*`. A small
//! analyzer splits it into bass / mid / treble loudness and stamps each 10 ms
//! slice with the time it will actually be heard, after the device buffer. A
//! thread picks those up about 30 times a second and turns them into colour
//! and brightness on the LED zones the stock firmware exposes under
//! /sys/class/led_anim:
//!
//! - back strip (`lr`) follows the bass,
//! - top light (`m`) follows the mids,
//! - front lights (`f1`, `f2`) follow the treble,
//!
//! and every drum hit moves all of them to a new colour. Whatever the LEDs were
//! showing before is saved and put back when syncing stops.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::config::Config;

const SYSFS: &str = "/sys/class/led_anim";
/// Firmware effect number for a steady colour (see /sys/class/led_anim/help).
const STATIC: &str = "4";
const TICK: Duration = Duration::from_millis(33);
/// Audio slices are cut this often.
const SLICE_PER_SEC: u32 = 100;
/// Slices not yet shown; about 6 s, far more than any output buffer.
const MAX_QUEUED: usize = 600;

// ------------------------------------------------------------------ analysis

/// Splits mono audio into three bands with one-pole filters and reports the
/// RMS of each every 10 ms. Cheap enough to run inside the audio thread.
struct Analyzer {
    rate: u32,
    a_lo: f32,
    a_hi: f32,
    lo: f32,
    hi: f32,
    acc: [f32; 3],
    n: u32,
    per: u32,
}

impl Analyzer {
    fn new(rate: u32) -> Self {
        let rate = rate.max(8_000);
        let coef = |hz: f32| 1.0 - (-2.0 * std::f32::consts::PI * hz / rate as f32).exp();
        Self {
            rate,
            a_lo: coef(150.0),
            a_hi: coef(2_500.0),
            lo: 0.0,
            hi: 0.0,
            acc: [0.0; 3],
            n: 0,
            per: rate / SLICE_PER_SEC,
        }
    }

    fn set_rate(&mut self, rate: u32) {
        if rate.max(8_000) != self.rate {
            *self = Self::new(rate);
        }
    }

    /// Takes one mono sample in -1..1; returns [bass, mid, treble] RMS once a slice is full.
    fn push(&mut self, x: f32) -> Option<[f32; 3]> {
        self.lo += self.a_lo * (x - self.lo);
        self.hi += self.a_hi * (x - self.hi);
        let bands = [self.lo, self.hi - self.lo, x - self.hi];
        for (acc, b) in self.acc.iter_mut().zip(bands) {
            *acc += b * b;
        }
        self.n += 1;
        if self.n < self.per {
            return None;
        }
        let n = self.n as f32;
        let out = self.acc.map(|a| (a / n).sqrt());
        self.acc = [0.0; 3];
        self.n = 0;
        Some(out)
    }
}

#[derive(Clone, Copy, Debug)]
struct Slice {
    /// When this audio reaches the speaker.
    at: Instant,
    bands: [f32; 3],
}

struct Shared {
    enabled: AtomicBool,
    running: AtomicBool,
    available: bool,
    analyzer: Mutex<Analyzer>,
    slices: Mutex<VecDeque<Slice>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

#[cfg_attr(not(all(target_os = "linux", not(feature = "desktop"))), allow(dead_code))]
fn feed(frames: impl Iterator<Item = f32>, rate: u32, delay_ms: u32) {
    let Some(sh) = SHARED.get() else { return };
    if !sh.enabled.load(Ordering::Relaxed) {
        return;
    }
    let start = Instant::now() + Duration::from_millis(delay_ms as u64);
    let mut out = Vec::new();
    {
        let mut an = sh.analyzer.lock().unwrap();
        an.set_rate(rate);
        for (i, x) in frames.enumerate() {
            if let Some(bands) = an.push(x) {
                let at = start + Duration::from_secs_f64(i as f64 / rate.max(1) as f64);
                out.push(Slice { at, bands });
            }
        }
    }
    if !out.is_empty() {
        let mut q = sh.slices.lock().unwrap();
        q.extend(out);
        while q.len() > MAX_QUEUED {
            q.pop_front();
        }
    }
}

/// Interleaved stereo i16 on its way to the device.
#[cfg_attr(not(all(target_os = "linux", not(feature = "desktop"))), allow(dead_code))]
pub fn feed_i16(samples: &[i16], rate: u32, delay_ms: u32) {
    feed(
        samples.chunks_exact(2).map(|c| (c[0] as f32 + c[1] as f32) / 65_536.0),
        rate,
        delay_ms,
    )
}

/// Interleaved stereo i32 on its way to the device.
#[cfg_attr(not(all(target_os = "linux", not(feature = "desktop"))), allow(dead_code))]
pub fn feed_i32(samples: &[i32], rate: u32, delay_ms: u32) {
    feed(
        samples.chunks_exact(2).map(|c| (c[0] as f32 + c[1] as f32) / 4_294_967_296.0),
        rate,
        delay_ms,
    )
}

/// Interleaved stereo f64 (Spotify's samples before conversion).
#[cfg_attr(not(all(target_os = "linux", not(feature = "desktop"))), allow(dead_code))]
pub fn feed_f64(samples: &[f64], rate: u32, delay_ms: u32) {
    feed(samples.chunks_exact(2).map(|c| ((c[0] + c[1]) * 0.5) as f32), rate, delay_ms)
}

// ------------------------------------------------------------------ lights

/// One zone's colour and how bright it should be (0..1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Light {
    pub rgb: [u8; 3],
    pub level: f32,
}

/// Turns band loudness into lights: brightness follows each band relative to
/// its own recent peak (so quiet songs still move), and a bass hit well above
/// its running average counts as a beat and jumps the colours on.
pub struct Engine {
    t: f32,
    hue: f32,
    env: [f32; 3],
    peak: [f32; 3],
    bass_avg: f32,
    last_beat: f32,
    pub beats: u32,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self {
            t: 0.0,
            hue: 0.0,
            env: [0.0; 3],
            peak: [1e-3; 3],
            bass_avg: 0.0,
            last_beat: -1.0,
            beats: 0,
        }
    }

    /// Advances by `dt` seconds. `bands` is the loudest audio heard during this
    /// tick, or None when nothing is playing. Returns [bass, mid, treble] lights.
    pub fn step(&mut self, dt: f32, bands: Option<[f32; 3]>) -> [Light; 3] {
        self.t += dt;
        let x = bands.unwrap_or([0.0; 3]);
        for b in 0..3 {
            self.peak[b] = (self.peak[b] * (-dt / 4.0).exp()).max(x[b]).max(1e-3);
            // Below -60 dBFS is silence, however the gain has drifted.
            let norm = if x[b] < 1e-3 { 0.0 } else { (x[b] / self.peak[b]).clamp(0.0, 1.0) };
            self.env[b] = if norm > self.env[b] {
                norm
            } else {
                self.env[b] * (-dt / 0.15).exp()
            };
        }
        let beat = bands.is_some()
            && x[0] > self.bass_avg * 1.35
            && self.env[0] > 0.5
            && self.t - self.last_beat > 0.25;
        self.bass_avg += (1.0 - (-dt / 0.8).exp()) * (x[0] - self.bass_avg);
        if beat {
            self.last_beat = self.t;
            self.beats += 1;
            self.hue = (self.hue + 47.0) % 360.0;
        }
        self.hue = (self.hue + 12.0 * dt) % 360.0;
        // Colour in 15° steps: each change costs the firmware a re-arm, so the
        // slow drift should not rewrite it every frame.
        let offsets = [0.0, 120.0, 240.0];
        std::array::from_fn(|b| {
            let h = ((self.hue + offsets[b]) / 15.0).round() * 15.0;
            Light {
                rgb: hsv(h),
                level: self.env[b].powf(0.8),
            }
        })
    }
}

/// Fully saturated, full-value colour at hue `h` degrees.
fn hsv(h: f32) -> [u8; 3] {
    let h = h.rem_euclid(360.0) / 60.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    let (r, g, b) = match h as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    [r, g, b].map(|c: f32| (c * 255.0).round() as u8)
}

// ------------------------------------------------------------------ sysfs

/// One LED zone of the firmware: which band drives it and its three files.
struct Zone {
    band: usize,
    effect: &'static str,
    color: &'static str,
    scale: &'static str,
}

/// The Brick's zones. Front lights share one brightness file; the top light's
/// brightness is the plain `max_scale`.
const ZONES: [Zone; 4] = [
    Zone { band: 0, effect: "effect_lr", color: "effect_rgb_hex_lr", scale: "max_scale_lr" },
    Zone { band: 1, effect: "effect_m", color: "effect_rgb_hex_m", scale: "max_scale" },
    Zone { band: 2, effect: "effect_f1", color: "effect_rgb_hex_f1", scale: "max_scale_f1f2" },
    Zone { band: 2, effect: "effect_f2", color: "effect_rgb_hex_f2", scale: "max_scale_f1f2" },
];

/// Writes lights to the firmware's files, skipping values that did not change.
pub struct Leds {
    base: PathBuf,
    zones: Vec<&'static Zone>,
    max: f32,
    saved: Vec<(&'static str, String)>,
    last: HashMap<&'static str, String>,
    warned: bool,
}

impl Leds {
    /// `None` when the firmware has no LED files (other devices, the PC).
    pub fn open(base: &Path, max_brightness: u8) -> Option<Self> {
        let zones: Vec<&'static Zone> = ZONES
            .iter()
            .filter(|z| base.join(z.effect).exists() && base.join(z.color).exists())
            .collect();
        if zones.is_empty() {
            return None;
        }
        Some(Self {
            base: base.to_path_buf(),
            zones,
            max: max_brightness.min(100) as f32,
            saved: Vec::new(),
            last: HashMap::new(),
            warned: false,
        })
    }

    pub fn zone_names(&self) -> Vec<&'static str> {
        self.zones.iter().map(|z| z.effect.trim_start_matches("effect_")).collect()
    }

    fn files(&self) -> Vec<&'static str> {
        let mut files = Vec::new();
        for z in &self.zones {
            for f in [z.scale, z.color, z.effect] {
                if !files.contains(&f) && self.base.join(f).exists() {
                    files.push(f);
                }
            }
        }
        files
    }

    fn put(&mut self, file: &'static str, value: String, force: bool) {
        if !force && self.last.get(file) == Some(&value) {
            return;
        }
        match std::fs::write(self.base.join(file), &value) {
            Ok(()) => {
                self.last.insert(file, value);
            }
            Err(e) if !self.warned => {
                self.warned = true;
                log::warn!("led: không ghi được {file}: {e}");
            }
            Err(_) => {}
        }
    }

    /// Remembers what the LEDs show now, so `restore` can put it back.
    pub fn begin(&mut self) {
        self.saved = self
            .files()
            .into_iter()
            .filter_map(|f| {
                let v = std::fs::read_to_string(self.base.join(f)).ok()?;
                Some((f, v.trim().to_string()))
            })
            .collect();
        self.last.clear();
    }

    pub fn show(&mut self, lights: &[Light; 3]) {
        let zones = self.zones.clone();
        for z in &zones {
            let [r, g, b] = lights[z.band].rgb;
            let color = format!("{r:02X}{g:02X}{b:02X} ");
            if self.last.get(z.color) != Some(&color) {
                self.put(z.color, color, false);
                // A new colour only takes effect when the effect is set again.
                self.put(z.effect, STATIC.to_string(), true);
            }
        }
        // Zones sharing a brightness file take the brighter of the two.
        let mut scales: Vec<(&'static str, f32)> = Vec::new();
        for z in &zones {
            let level = lights[z.band].level.clamp(0.0, 1.0);
            match scales.iter_mut().find(|(f, _)| *f == z.scale) {
                Some((_, l)) => *l = l.max(level),
                None => scales.push((z.scale, level)),
            }
        }
        for (file, level) in scales {
            if self.base.join(file).exists() {
                let value = ((level * self.max).round() as u32).to_string();
                self.put(file, value, false);
            }
        }
    }

    /// Puts back what `begin` saved: brightness and colours first, then the
    /// effects, which make the firmware pick the rest up.
    pub fn restore(&mut self) {
        let saved = std::mem::take(&mut self.saved);
        let (effects, rest): (Vec<_>, Vec<_>) =
            saved.into_iter().partition(|(f, _)| f.starts_with("effect_") && !f.starts_with("effect_rgb"));
        for (file, value) in rest {
            let value = if file.starts_with("effect_rgb_hex") { format!("{value} ") } else { value };
            self.put(file, value, true);
        }
        for (file, value) in effects {
            self.put(file, value, true);
        }
        self.last.clear();
    }
}

// ------------------------------------------------------------------ thread

/// Starts the light thread if this device has LEDs; `cfg.led_sync` turns it on.
pub fn start(cfg: &Config) {
    let leds = Leds::open(Path::new(SYSFS), cfg.led_brightness);
    let sh = SHARED.get_or_init(|| Shared {
        enabled: AtomicBool::new(false),
        running: AtomicBool::new(true),
        available: leds.is_some(),
        analyzer: Mutex::new(Analyzer::new(44_100)),
        slices: Mutex::new(VecDeque::new()),
        thread: Mutex::new(None),
    });
    let Some(leds) = leds else {
        log::info!("led: không có {SYSFS}, tắt tính năng đèn theo nhạc");
        return;
    };
    log::info!("led: vùng đèn {:?}", leds.zone_names());
    let handle = std::thread::Builder::new()
        .name("led".into())
        .spawn(move || run(leds))
        .ok();
    *sh.thread.lock().unwrap() = handle;
    set_enabled(cfg.led_sync);
}

fn run(mut leds: Leds) {
    let Some(sh) = SHARED.get() else { return };
    let mut engine = Engine::new();
    let mut on = false;
    let mut prev = Instant::now();
    while sh.running.load(Ordering::Relaxed) {
        std::thread::sleep(TICK);
        let now = Instant::now();
        let dt = (now - prev).as_secs_f32().min(0.2);
        prev = now;
        let want = sh.enabled.load(Ordering::Relaxed);
        if want != on {
            on = want;
            if on {
                leds.begin();
                engine = Engine::new();
            } else {
                leds.restore();
            }
            sh.slices.lock().unwrap().clear();
        }
        if !on {
            continue;
        }
        // The loudest audio that became audible since the last tick.
        let mut due: Option<[f32; 3]> = None;
        {
            let mut q = sh.slices.lock().unwrap();
            while q.front().is_some_and(|s| s.at <= now) {
                let s = q.pop_front().unwrap();
                due = Some(match due {
                    None => s.bands,
                    Some(d) => std::array::from_fn(|i| d[i].max(s.bands[i])),
                });
            }
        }
        let lights = engine.step(dt, due);
        leds.show(&lights);
    }
    if on {
        leds.restore();
    }
}

/// Turns syncing on or off (right stick press, the menu). Ignored on devices without LEDs.
pub fn set_enabled(on: bool) {
    if let Some(sh) = SHARED.get() {
        sh.enabled.store(on && sh.available, Ordering::Relaxed);
    }
}

pub fn available() -> bool {
    SHARED.get().is_some_and(|s| s.available)
}

/// Puts the LEDs back as they were and stops the thread (on exit).
pub fn shutdown() {
    if let Some(sh) = SHARED.get() {
        sh.running.store(false, Ordering::Relaxed);
        if let Some(h) = sh.thread.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

/// `spoty --led-test`: shows what the firmware offers and runs a visible test,
/// so the user can report what the lights actually did.
pub fn selftest() {
    let base = Path::new(SYSFS);
    if !base.exists() {
        println!("Không có {SYSFS}: firmware của máy này không cho điều khiển LED.");
        return;
    }
    let mut names: Vec<String> = std::fs::read_dir(base)
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    names.sort();
    println!("== File trong {SYSFS} ==\n{}", names.join(" "));
    if let Ok(help) = std::fs::read_to_string(base.join("help")) {
        println!("== help ==");
        for line in help.lines().take(80) {
            println!("{line}");
        }
    }
    let Some(mut leds) = Leds::open(base, 100) else {
        println!("Không thấy vùng LED nào quen thuộc (lr, m, f1, f2).");
        return;
    };
    println!("== Vùng LED dùng được: {:?} ==", leds.zone_names());
    leds.begin();
    let pause = |s: f32| std::thread::sleep(Duration::from_secs_f32(s));
    for (i, (name, rgb)) in [("ĐỎ", [255, 0, 0]), ("XANH LÁ", [0, 255, 0]), ("XANH DƯƠNG", [0, 0, 255])]
        .into_iter()
        .enumerate()
    {
        println!("{}. Mọi đèn phải đang màu {name}, sáng hết cỡ", i + 1);
        leds.show(&[Light { rgb, level: 1.0 }; 3]);
        pause(2.5);
    }
    println!("4. Đèn TRẮNG sáng dần từ tối tới sáng trong 3 giây (xem có mượt không)");
    for i in 0..=30 {
        leds.show(&[Light { rgb: [255, 255, 255], level: i as f32 / 30.0 }; 3]);
        pause(0.1);
    }
    println!("5. Mô phỏng nhạc 8 giây: dải sau nháy theo nhịp trống (2 nhịp/giây), mỗi nhịp đổi màu");
    let mut engine = Engine::new();
    for i in 0..240 {
        let t = i as f32 * 0.033;
        let hit = (t % 0.5) < 0.08;
        let bands = [
            if hit { 0.5 } else { 0.04 },
            0.15 + 0.1 * (t * 3.0).sin().abs(),
            0.02 + 0.08 * (t * 7.0).sin().abs(),
        ];
        let lights = engine.step(0.033, Some(bands));
        leds.show(&lights);
        pause(0.033);
    }
    leds.restore();
    println!("Xong, đèn đã được trả về như cũ ({} nhịp mô phỏng).", engine.beats);
    println!("Gửi lại cho Claude: bước nào đúng như mô tả, bước nào không, và phần 'help' ở trên.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, rate: u32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    fn bands_of(samples: &[f32]) -> [f32; 3] {
        let mut an = Analyzer::new(44_100);
        let mut last = [0.0; 3];
        for &x in samples {
            if let Some(b) = an.push(x) {
                last = b;
            }
        }
        last
    }

    #[test]
    fn splits_bass_and_treble() {
        let bass = bands_of(&tone(50.0, 44_100, 0.5));
        assert!(bass[0] > 5.0 * bass[2], "50 Hz is bass: {bass:?}");
        let treble = bands_of(&tone(8_000.0, 44_100, 0.5));
        assert!(treble[2] > 5.0 * treble[0], "8 kHz is treble: {treble:?}");
        let mid = bands_of(&tone(700.0, 44_100, 0.5));
        assert!(mid[1] > mid[0] && mid[1] > mid[2], "700 Hz is mid: {mid:?}");
    }

    #[test]
    fn beats_change_the_colour_and_silence_goes_dark() {
        let mut e = Engine::new();
        let mut colours = std::collections::HashSet::new();
        // Four seconds of a kick drum twice a second over a steady bed.
        for i in 0..120 {
            let t = i as f32 * 0.033;
            let hit = (t % 0.5) < 0.07;
            let lights = e.step(0.033, Some([if hit { 0.5 } else { 0.05 }, 0.1, 0.05]));
            colours.insert(lights[0].rgb);
        }
        assert!((6..=9).contains(&e.beats), "about 8 kicks, got {}", e.beats);
        assert!(colours.len() >= 4, "each kick moves the colour");
        // Nothing playing: everything fades to black within a second.
        let mut lights = e.step(0.033, None);
        for _ in 0..30 {
            lights = e.step(0.033, None);
        }
        assert!(lights.iter().all(|l| l.level < 0.01), "{lights:?}");
    }

    #[test]
    fn hues_are_pure() {
        assert_eq!(hsv(0.0), [255, 0, 0]);
        assert_eq!(hsv(120.0), [0, 255, 0]);
        assert_eq!(hsv(240.0), [0, 0, 255]);
        assert_eq!(hsv(360.0 + 60.0), [255, 255, 0]);
    }

    #[test]
    fn writes_the_firmware_files_and_puts_them_back() {
        let dir = std::env::temp_dir().join(format!("spoty-led-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let before = [
            ("effect_lr", "2"),
            ("effect_rgb_hex_lr", "FF8000"),
            ("max_scale_lr", "60"),
            ("effect_m", "0"),
            ("effect_rgb_hex_m", "FFFFFF"),
            ("max_scale", "30"),
        ];
        for (f, v) in before {
            std::fs::write(dir.join(f), v).unwrap();
        }
        // Only the zones whose files exist are used: here lr and m.
        let mut leds = Leds::open(&dir, 80).unwrap();
        assert_eq!(leds.zone_names(), vec!["lr", "m"]);
        leds.begin();
        let red = Light { rgb: [255, 0, 0], level: 0.5 };
        let blue = Light { rgb: [0, 0, 255], level: 1.0 };
        leds.show(&[red, blue, blue]);
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
        assert_eq!(read("effect_rgb_hex_lr"), "FF0000 ", "firmware wants a trailing space");
        assert_eq!(read("effect_lr"), "4", "static effect re-armed for the new colour");
        assert_eq!(read("max_scale_lr"), "40", "half of max brightness 80");
        assert_eq!(read("effect_rgb_hex_m"), "0000FF ");
        assert_eq!(read("max_scale"), "80");
        leds.restore();
        for (f, v) in before {
            assert_eq!(read(f).trim(), v, "{f} restored");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
