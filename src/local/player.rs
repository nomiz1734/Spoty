//! Local-file playback engine (its own thread).
//!
//! Audio path, best case: decoder -> device at the file's own rate and bit depth,
//! untouched (bit-perfect). Only when needed: FFT resampling (device can't do the
//! rate), float gain (volume below 100% or ReplayGain), TPDF dither (16-bit device).
//! Tracks with the same rate are played back-to-back without reopening the
//! device, so albums are gapless.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

use rubato::{FftFixedInOut, Resampler};

use super::decode::{Chunk, TrackDecoder};
use super::{track_info, LocalCmd, LocalEvent, LocalTrack, RgMode};
use crate::audio::{AudioOut, FormatPref, OutBuf, OutSpec, SampleFormat};
use crate::spotify::Repeat;
use crate::ui::UiMsg;

pub struct Settings {
    pub device: String,
    pub latency_ms: u32,
    pub format: FormatPref,
    pub replaygain: RgMode,
}

pub fn spawn(settings: Settings, tx: Sender<UiMsg>) -> Sender<LocalCmd> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("local-audio".into())
        .spawn(move || {
            let out = crate::audio::make_output(&settings.device, settings.latency_ms, settings.format);
            Player::new(out, settings.replaygain, tx).run(cmd_rx);
        })
        .expect("spawn local player");
    cmd_tx
}

/// High-quality fixed-ratio resampler for stereo f32.
struct Rs {
    inner: FftFixedInOut<f32>,
    pending: [Vec<f32>; 2],
    skip: usize,
}

impl Rs {
    fn new(from: u32, to: u32) -> Option<Self> {
        let inner = FftFixedInOut::<f32>::new(from as usize, to as usize, 1024, 2).ok()?;
        let skip = inner.output_delay();
        Some(Self {
            inner,
            pending: [Vec::new(), Vec::new()],
            skip,
        })
    }

    fn emit(&mut self, res: &[Vec<f32>], out: &mut Vec<f32>) {
        let n = res[0].len();
        let start = self.skip.min(n);
        self.skip -= start;
        for i in start..n {
            out.push(res[0][i]);
            out.push(res[1][i]);
        }
    }

    fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        for f in input.chunks_exact(2) {
            self.pending[0].push(f[0]);
            self.pending[1].push(f[1]);
        }
        loop {
            let need = self.inner.input_frames_next();
            if self.pending[0].len() < need {
                break;
            }
            let chunk = [&self.pending[0][..need], &self.pending[1][..need]];
            if let Ok(res) = self.inner.process(&chunk, None) {
                self.emit(&res, out);
            }
            for p in &mut self.pending {
                p.drain(..need);
            }
        }
    }

    /// Pushes the tail (and the filter delay) out with silence.
    fn flush(&mut self, out: &mut Vec<f32>) {
        let need = self.inner.input_frames_next();
        let real = self.pending[0].len();
        let silence = vec![0.0f32; (need * 2 - real % need) * 2];
        let before = out.len();
        self.push(&silence, out);
        // Keep the real input plus the filter delay still owed; drop the padding.
        let ratio = self.inner.output_frames_next() as f64 / need as f64;
        let keep = before + ((real as f64 * ratio) as usize + self.inner.output_delay()) * 2;
        out.truncate(keep.min(out.len()));
    }
}

struct Pending {
    /// Stream frame where the queued track starts.
    at: u64,
    queue_index: usize,
}

struct Player {
    tx: Sender<UiMsg>,
    out: Box<dyn AudioOut>,
    rg_mode: RgMode,
    queue: Vec<LocalTrack>,
    /// Queue indices in play order.
    order: Vec<usize>,
    /// Track being decoded / heard (differ during a gapless hand-over).
    dec_q: usize,
    shown_q: usize,
    shuffle: bool,
    repeat: Repeat,
    playing: bool,
    dec: Option<TrackDecoder>,
    spec: Option<OutSpec>,
    src_rate: u32,
    rs: Option<Rs>,
    written: u64,
    track_start: u64,
    track_offset_ms: u32,
    pending: Option<Pending>,
    volume_gain: f32,
    track_gain: f32,
    cur_gain: f32,
    rng: u32,
    last_pos: Instant,
    failures: u32,
}

fn volume_to_gain(v: u16) -> f32 {
    if v == 0 {
        return 0.0;
    }
    if v == u16::MAX {
        return 1.0;
    }
    // 60 dB range, like Spotify's logarithmic volume.
    10f32.powf(3.0 * (v as f32 / 65535.0 - 1.0))
}

fn to_stereo(c: Chunk) -> Vec<i32> {
    match c.channels {
        2 => c.samples,
        1 => c.samples.iter().flat_map(|&s| [s, s]).collect(),
        n => c
            .samples
            .chunks_exact(n)
            .flat_map(|f| {
                // Simple downmix: front L/R plus half of everything else.
                let (mut l, mut r) = (f[0] as i64, f[1] as i64);
                for (i, &s) in f.iter().enumerate().skip(2) {
                    if i % 2 == 0 {
                        l += s as i64 / 2;
                    } else {
                        r += s as i64 / 2;
                    }
                }
                let scale = 2 + (n as i64 - 2) / 2;
                [(l * 2 / scale) as i32, (r * 2 / scale) as i32]
            })
            .collect(),
    }
}

impl Player {
    fn new(out: Box<dyn AudioOut>, rg_mode: RgMode, tx: Sender<UiMsg>) -> Self {
        Self {
            tx,
            out,
            rg_mode,
            queue: Vec::new(),
            order: Vec::new(),
            dec_q: 0,
            shown_q: 0,
            shuffle: false,
            repeat: Repeat::Off,
            playing: false,
            dec: None,
            spec: None,
            src_rate: 0,
            rs: None,
            written: 0,
            track_start: 0,
            track_offset_ms: 0,
            pending: None,
            volume_gain: 1.0,
            track_gain: 1.0,
            cur_gain: 1.0,
            rng: 0x1234_5678,
            last_pos: Instant::now(),
            failures: 0,
        }
    }

    fn emit(&self, e: LocalEvent) {
        let _ = self.tx.send(UiMsg::Local(e));
    }

    fn rand(&mut self) -> f32 {
        // xorshift32 -> [0, 1)
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }

    fn run(mut self, rx: Receiver<LocalCmd>) {
        loop {
            loop {
                let active = self.playing && self.dec.is_some();
                let cmd = if active {
                    match rx.try_recv() {
                        Ok(c) => c,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return self.out.close(),
                    }
                } else {
                    match rx.recv_timeout(Duration::from_millis(250)) {
                        Ok(c) => c,
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return self.out.close(),
                    }
                };
                if !self.handle(cmd) {
                    self.out.close();
                    return;
                }
            }
            if self.playing && self.dec.is_some() {
                self.step();
            }
            self.check_pending();
            if self.playing && self.last_pos.elapsed() > Duration::from_secs(1) {
                self.last_pos = Instant::now();
                let pos = self.position_ms();
                self.emit(LocalEvent::Position(pos));
            }
        }
    }

    // ------------------------------------------------------------ commands

    fn handle(&mut self, cmd: LocalCmd) -> bool {
        match cmd {
            LocalCmd::Play {
                queue,
                index,
                shuffle,
            } => {
                self.reset_output();
                self.queue = queue;
                self.shuffle = shuffle;
                let index = index.min(self.queue.len().saturating_sub(1));
                self.build_order(index);
                self.failures = 0;
                self.playing = true;
                self.emit(LocalEvent::Shuffle(shuffle));
                self.start(index, 0);
            }
            LocalCmd::Toggle => {
                if self.playing {
                    self.pause();
                } else {
                    self.resume();
                }
            }
            LocalCmd::Pause => {
                if self.playing {
                    self.pause();
                }
            }
            LocalCmd::Next => {
                self.reset_output();
                match self.after(self.shown_q, false) {
                    Some(q) => self.start(q, 0),
                    None => self.stop_at_end(),
                }
            }
            LocalCmd::Prev => {
                if self.queue.is_empty() {
                    return true;
                }
                let pos = self.position_ms();
                self.reset_output();
                let pos_in_order = self.order.iter().position(|&i| i == self.shown_q).unwrap_or(0);
                let target = if pos > 3000 {
                    self.shown_q
                } else if pos_in_order > 0 {
                    self.order[pos_in_order - 1]
                } else if self.repeat == Repeat::Context && !self.order.is_empty() {
                    *self.order.last().unwrap()
                } else {
                    self.shown_q
                };
                self.start(target, 0);
            }
            LocalCmd::Seek(ms) => {
                if self.queue.is_empty() {
                    return true;
                }
                self.reset_output();
                self.start(self.shown_q, ms);
            }
            LocalCmd::Volume(v) => self.volume_gain = volume_to_gain(v),
            LocalCmd::Shuffle(on) => {
                self.shuffle = on;
                if !self.queue.is_empty() {
                    self.build_order(self.shown_q);
                }
                self.emit(LocalEvent::Shuffle(on));
            }
            LocalCmd::Repeat(r) => self.repeat = r,
            LocalCmd::Shutdown => return false,
        }
        true
    }

    fn build_order(&mut self, current: usize) {
        let n = self.queue.len();
        if self.shuffle {
            let mut rest: Vec<usize> = (0..n).filter(|&i| i != current).collect();
            for i in (1..rest.len()).rev() {
                let j = (self.rand() * (i + 1) as f32) as usize % (i + 1);
                rest.swap(i, j);
            }
            self.order = std::iter::once(current).chain(rest).collect();
        } else {
            self.order = (0..n).collect();
        }
    }

    /// The queue index after `q`, following repeat/shuffle rules.
    fn after(&mut self, q: usize, auto: bool) -> Option<usize> {
        if self.order.is_empty() {
            return None;
        }
        if auto && self.repeat == Repeat::Track {
            return Some(q);
        }
        let pos = self.order.iter().position(|&i| i == q)?;
        if pos + 1 < self.order.len() {
            return Some(self.order[pos + 1]);
        }
        if self.repeat != Repeat::Off {
            if self.shuffle {
                let first = self.order[0];
                self.build_order(first);
            }
            return Some(self.order[0]);
        }
        None
    }

    fn open_decoder(&mut self, q: usize) -> bool {
        let Some(track) = self.queue.get(q) else {
            return false;
        };
        match TrackDecoder::open(std::path::Path::new(&track.path)) {
            Ok(d) => {
                self.track_gain = self.replay_gain(&d);
                self.dec = Some(d);
                self.dec_q = q;
                self.failures = 0;
                true
            }
            Err(e) => {
                log::warn!("local: {}: {e}", track.path);
                self.emit(LocalEvent::Error(format!("Không phát được \"{}\": {e}", track.title)));
                self.failures += 1;
                false
            }
        }
    }

    fn replay_gain(&self, d: &TrackDecoder) -> f32 {
        let (gain, peak) = match self.rg_mode {
            RgMode::Off => return 1.0,
            RgMode::Track => (d.tags.rg_track_gain, d.tags.rg_track_peak),
            RgMode::Album => (
                d.tags.rg_album_gain.or(d.tags.rg_track_gain),
                d.tags.rg_album_peak.or(d.tags.rg_track_peak),
            ),
        };
        let Some(db) = gain else { return 1.0 };
        let g = 10f32.powf(db / 20.0);
        // Never boost past the point of clipping.
        let limit = peak.filter(|p| *p > 0.0).map(|p| 1.0 / p).unwrap_or(1.0);
        g.min(limit)
    }

    /// Starts queue entry `q` at `ms`, skipping unplayable files.
    fn start(&mut self, mut q: usize, ms: u32) {
        loop {
            if self.open_decoder(q) {
                break;
            }
            if self.failures >= 5 {
                return self.stop_at_end();
            }
            match self.after(q, false) {
                Some(n) => q = n,
                None => return self.stop_at_end(),
            }
        }
        if ms > 0 {
            if let Some(d) = self.dec.as_mut() {
                if let Err(e) = d.seek(ms) {
                    log::warn!("seek: {e}");
                }
            }
        }
        self.shown_q = q;
        self.track_offset_ms = ms;
        self.track_start = 0;
        self.emit(LocalEvent::Track(track_info(&self.queue[q])));
        self.emit(LocalEvent::State {
            playing: self.playing,
            position_ms: ms,
        });
    }

    fn pause(&mut self) {
        let pos = self.position_ms();
        self.reset_output();
        self.playing = false;
        // Reopen at the exact heard position so nothing buffered is lost.
        if !self.queue.is_empty() {
            self.start(self.shown_q, pos);
        }
        self.emit(LocalEvent::State {
            playing: false,
            position_ms: pos,
        });
    }

    fn resume(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        self.playing = true;
        if self.dec.is_none() {
            // Finished earlier: start again from the first track in order.
            let first = self.order.first().copied().unwrap_or(0);
            self.start(first, 0);
        }
        self.emit(LocalEvent::State {
            playing: true,
            position_ms: self.track_offset_ms,
        });
    }

    fn stop_at_end(&mut self) {
        self.reset_output();
        self.dec = None;
        self.playing = false;
        self.track_offset_ms = 0;
        self.emit(LocalEvent::State {
            playing: false,
            position_ms: 0,
        });
    }

    // ------------------------------------------------------------ audio

    fn reset_output(&mut self) {
        self.out.close();
        self.spec = None;
        self.rs = None;
        self.pending = None;
        self.written = 0;
        self.track_start = 0;
    }

    fn position_ms(&mut self) -> u32 {
        let Some(spec) = self.spec else {
            return self.track_offset_ms;
        };
        let played = self.written.saturating_sub(self.out.delay_frames() as u64);
        let since = played.saturating_sub(self.track_start);
        self.track_offset_ms + (since * 1000 / spec.rate.max(1) as u64) as u32
    }

    fn check_pending(&mut self) {
        let Some(p) = &self.pending else { return };
        let played = self.written.saturating_sub(self.out.delay_frames() as u64);
        if played >= p.at {
            let (at, q) = (p.at, p.queue_index);
            self.pending = None;
            self.shown_q = q;
            self.track_start = at;
            self.track_offset_ms = 0;
            self.emit(LocalEvent::Track(track_info(&self.queue[q])));
            self.emit(LocalEvent::State {
                playing: self.playing,
                position_ms: 0,
            });
        }
    }

    fn step(&mut self) {
        let Some(dec) = self.dec.as_mut() else { return };
        let rate = dec.rate;
        let bits = dec.info.bits;
        match dec.next_chunk() {
            Ok(Some(chunk)) => {
                let stereo = to_stereo(chunk);
                if let Err(e) = self.output(&stereo, rate, bits) {
                    log::warn!("local audio: {e}");
                    self.emit(LocalEvent::Error(format!("Lỗi âm thanh: {e}")));
                    self.pause();
                }
            }
            Ok(None) => self.track_finished(),
            Err(e) => {
                log::warn!("decode: {e}");
                self.track_finished();
            }
        }
    }

    fn track_finished(&mut self) {
        let Some(next) = self.after(self.dec_q, true) else {
            // End of the queue: let the buffer play out.
            self.flush_resampler();
            self.out.drain();
            self.spec = None;
            self.rs = None;
            self.pending = None;
            self.dec = None;
            self.playing = false;
            self.track_offset_ms = 0;
            self.emit(LocalEvent::State {
                playing: false,
                position_ms: 0,
            });
            self.emit(LocalEvent::QueueEnded);
            return;
        };
        if !self.open_decoder(next) {
            if self.failures >= 5 {
                return self.stop_at_end();
            }
            // Skip the broken file and try the one after it.
            self.dec_q = next;
            return self.track_finished();
        }
        let new_rate = self.dec.as_ref().map(|d| d.rate).unwrap_or(0);
        if self.spec.is_some() && new_rate == self.src_rate {
            // Gapless: keep streaming; the UI switches when the old track has played out.
            self.pending = Some(Pending {
                at: self.written,
                queue_index: next,
            });
        } else {
            self.flush_resampler();
            self.out.drain();
            self.spec = None;
            self.rs = None;
            self.shown_q = next;
            self.track_offset_ms = 0;
            self.track_start = 0;
            self.emit(LocalEvent::Track(track_info(&self.queue[next])));
            self.emit(LocalEvent::State {
                playing: self.playing,
                position_ms: 0,
            });
        }
    }

    fn flush_resampler(&mut self) {
        if let Some(mut rs) = self.rs.take() {
            let mut tail = Vec::new();
            rs.flush(&mut tail);
            if !tail.is_empty() {
                let _ = self.write_float(&mut tail);
            }
        }
    }

    fn output(&mut self, stereo: &[i32], src_rate: u32, bits: u32) -> Result<(), String> {
        if self.spec.is_none() || src_rate != self.src_rate {
            let spec = self.out.open(src_rate)?;
            self.spec = Some(spec);
            self.src_rate = src_rate;
            self.rs = if spec.rate != src_rate {
                log::info!("resampling {src_rate} -> {} Hz", spec.rate);
                Rs::new(src_rate, spec.rate)
            } else {
                None
            };
            self.written = 0;
            self.track_start = 0;
        }
        let spec = self.spec.unwrap();
        let target = self.volume_gain * self.track_gain;
        if self.rs.is_none() && target == 1.0 && self.cur_gain == 1.0 {
            // Bit-perfect path.
            let frames = stereo.len() / 2;
            match spec.format {
                SampleFormat::S32 => self.out.write(OutBuf::S32(stereo))?,
                SampleFormat::S16 if bits <= 16 && bits > 0 => {
                    let s: Vec<i16> = stereo.iter().map(|&v| (v >> 16) as i16).collect();
                    self.out.write(OutBuf::S16(&s))?;
                }
                SampleFormat::S16 => {
                    let mut f: Vec<f32> = stereo.iter().map(|&v| v as f32 / 2_147_483_648.0).collect();
                    return self.write_float(&mut f);
                }
            }
            self.written += frames as u64;
            return Ok(());
        }
        let mut f: Vec<f32> = stereo.iter().map(|&v| v as f32 / 2_147_483_648.0).collect();
        if let Some(rs) = self.rs.as_mut() {
            let mut o = Vec::with_capacity(f.len() + 64);
            rs.push(&f, &mut o);
            f = o;
        }
        // Gain, ramped across the block so volume changes don't click.
        let frames = f.len() / 2;
        if frames > 0 {
            let (g0, g1) = (self.cur_gain, target);
            for (i, fr) in f.chunks_exact_mut(2).enumerate() {
                let g = g0 + (g1 - g0) * (i + 1) as f32 / frames as f32;
                fr[0] *= g;
                fr[1] *= g;
            }
            self.cur_gain = target;
        }
        self.write_float(&mut f)
    }

    fn write_float(&mut self, f: &mut [f32]) -> Result<(), String> {
        if f.is_empty() {
            return Ok(());
        }
        let Some(spec) = self.spec else {
            return Ok(());
        };
        let frames = f.len() / 2;
        match spec.format {
            SampleFormat::S32 => {
                let s: Vec<i32> = f
                    .iter()
                    .map(|&x| (x as f64 * 2_147_483_647.0).round().clamp(i32::MIN as f64, i32::MAX as f64) as i32)
                    .collect();
                self.out.write(OutBuf::S32(&s))?;
            }
            SampleFormat::S16 => {
                // TPDF dither: two uniform randoms -> triangular noise of ±1 LSB.
                let mut s = Vec::with_capacity(f.len());
                for &x in f.iter() {
                    let d = self.rand() - self.rand();
                    s.push((x * 32767.0 + d).round().clamp(-32768.0, 32767.0) as i16);
                }
                self.out.write(OutBuf::S16(&s))?;
            }
        }
        self.written += frames as u64;
        Ok(())
    }
}

pub fn to_stereo_pub(c: Chunk) -> Vec<i32> {
    to_stereo(c)
}

/// Runs interleaved stereo through the resampler; returns output frames.
pub fn resample_selftest(stereo: &[i32], from: u32, to: u32) -> usize {
    let Some(mut rs) = Rs::new(from, to) else { return 0 };
    let f: Vec<f32> = stereo.iter().map(|&v| v as f32 / 2_147_483_648.0).collect();
    let mut out = Vec::new();
    rs.push(&f, &mut out);
    rs.flush(&mut out);
    out.len() / 2
}
