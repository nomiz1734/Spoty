//! Opening and decoding local audio files.
//!
//! Symphonia handles FLAC, ALAC, WAV/AIFF (PCM), MP3, AAC and Vorbis; Opus
//! (Ogg or WebM) is demuxed by Symphonia and decoded with `opus-decoder`.
//! Output is interleaved i32 at full scale, so 16- and 24-bit sources stay exact.

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{
    CodecType, Decoder, DecoderOptions, CODEC_TYPE_AAC, CODEC_TYPE_ALAC, CODEC_TYPE_FLAC,
    CODEC_TYPE_MP1, CODEC_TYPE_MP2, CODEC_TYPE_MP3, CODEC_TYPE_NULL, CODEC_TYPE_OPUS,
    CODEC_TYPE_VORBIS,
};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::{
    Limit, MetadataOptions, MetadataRevision, StandardTagKey, StandardVisualKey, Visual,
};
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};

use crate::gfx::text::clean;

#[derive(Clone, Debug, Default)]
pub struct Tags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track: u32,
    pub disc: u32,
    pub rg_track_gain: Option<f32>,
    pub rg_track_peak: Option<f32>,
    pub rg_album_gain: Option<f32>,
    pub rg_album_peak: Option<f32>,
}

#[derive(Clone, Debug, Default)]
pub struct StreamInfo {
    /// Display name: FLAC, ALAC, WAV, MP3, AAC, Vorbis, Opus…
    pub codec: String,
    pub lossless: bool,
    pub rate: u32,
    /// Bits per sample of the source (0 for lossy codecs).
    pub bits: u32,
    pub channels: u32,
    pub duration_ms: u32,
}

fn parse_number(s: &str) -> u32 {
    s.trim()
        .split(['/', ' '])
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// "-6.52 dB" -> -6.52
fn parse_db(s: &str) -> Option<f32> {
    let t = s.trim().trim_end_matches(|c: char| c.is_alphabetic() || c.is_whitespace());
    t.replace('−', "-").parse().ok()
}

fn collect(rev: &MetadataRevision, tags: &mut Tags, visuals: &mut Vec<Visual>) {
    for tag in rev.tags() {
        let Some(key) = tag.std_key else { continue };
        // RIFF INFO strings are often NUL-padded.
        let v = tag
            .value
            .to_string()
            .trim_matches(|c: char| c == '\0' || c.is_whitespace())
            .to_string();
        let set = |dst: &mut String| {
            if dst.is_empty() {
                *dst = clean(v.trim());
            }
        };
        match key {
            StandardTagKey::TrackTitle => set(&mut tags.title),
            StandardTagKey::Artist => set(&mut tags.artist),
            StandardTagKey::Album => set(&mut tags.album),
            StandardTagKey::AlbumArtist => set(&mut tags.album_artist),
            StandardTagKey::TrackNumber if tags.track == 0 => tags.track = parse_number(&v),
            StandardTagKey::DiscNumber if tags.disc == 0 => tags.disc = parse_number(&v),
            StandardTagKey::ReplayGainTrackGain => tags.rg_track_gain = parse_db(&v),
            StandardTagKey::ReplayGainTrackPeak => tags.rg_track_peak = v.trim().parse().ok(),
            StandardTagKey::ReplayGainAlbumGain => tags.rg_album_gain = parse_db(&v),
            StandardTagKey::ReplayGainAlbumPeak => tags.rg_album_peak = v.trim().parse().ok(),
            _ => {}
        }
    }
    visuals.extend(rev.visuals().iter().cloned());
}

pub struct Opened {
    pub format: Box<dyn FormatReader>,
    pub tags: Tags,
    pub visuals: Vec<Visual>,
}

/// Probes a file and reads its tags (and embedded pictures if `visuals`).
pub fn open(path: &Path, visuals: bool) -> Result<Opened, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    // A large read buffer smooths over SD card latency spikes.
    let mss = MediaSourceStream::new(
        Box::new(file),
        MediaSourceStreamOptions {
            buffer_len: 256 * 1024,
        },
    );
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let meta_opts = MetadataOptions {
        limit_visual_bytes: if visuals {
            Limit::Maximum(16 * 1024 * 1024)
        } else {
            Limit::Maximum(0)
        },
        ..Default::default()
    };
    let fmt_opts = FormatOptions {
        enable_gapless: true,
        ..Default::default()
    };
    let mut probed = symphonia::default::get_probe()
        .format(&hint, mss, &fmt_opts, &meta_opts)
        .map_err(|e| format!("không đọc được file: {e}"))?;
    let mut tags = Tags::default();
    let mut vis = Vec::new();
    if let Some(mut m) = probed.metadata.get() {
        if let Some(rev) = m.skip_to_latest() {
            collect(rev, &mut tags, &mut vis);
        }
    }
    let mut format = probed.format;
    {
        let mut m = format.metadata();
        if let Some(rev) = m.skip_to_latest() {
            collect(rev, &mut tags, &mut vis);
        }
    }
    Ok(Opened {
        format,
        tags,
        visuals: vis,
    })
}

/// Picks the best embedded cover (front cover first).
pub fn best_visual(visuals: &[Visual]) -> Option<&Visual> {
    visuals
        .iter()
        .find(|v| v.usage == Some(StandardVisualKey::FrontCover))
        .or_else(|| visuals.first())
}

fn codec_label(codec: CodecType, ext: &str) -> (String, bool) {
    let name = match codec {
        CODEC_TYPE_FLAC => return ("FLAC".into(), true),
        CODEC_TYPE_ALAC => return ("ALAC".into(), true),
        CODEC_TYPE_MP3 => "MP3",
        CODEC_TYPE_MP2 | CODEC_TYPE_MP1 => "MP2",
        CODEC_TYPE_AAC => "AAC",
        CODEC_TYPE_VORBIS => "Vorbis",
        CODEC_TYPE_OPUS => "Opus",
        other => {
            let short = symphonia::default::get_codecs()
                .get_codec(other)
                .map(|d| d.short_name)
                .unwrap_or("");
            if short.starts_with("pcm") {
                let container = match ext {
                    "aif" | "aiff" | "aifc" => "AIFF",
                    _ => "WAV",
                };
                return (container.into(), true);
            }
            return (short.to_uppercase(), false);
        }
    };
    (name.into(), false)
}

fn duration_ms(n_frames: Option<u64>, tb: Option<TimeBase>, rate: u32) -> u32 {
    let Some(n) = n_frames else { return 0 };
    match tb {
        Some(tb) => {
            let t = tb.calc_time(n);
            (t.seconds * 1000 + (t.frac * 1000.0) as u64) as u32
        }
        None if rate > 0 => (n * 1000 / rate as u64) as u32,
        None => 0,
    }
}

/// Stream properties without decoding (used by the library scanner).
pub fn stream_info(format: &dyn FormatReader, ext: &str) -> Option<StreamInfo> {
    let track = format
        .default_track()
        .filter(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .or_else(|| {
            format
                .tracks()
                .iter()
                .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        })?;
    let p = &track.codec_params;
    let (codec, lossless) = codec_label(p.codec, ext);
    let rate = p.sample_rate.unwrap_or(if p.codec == CODEC_TYPE_OPUS { 48_000 } else { 0 });
    Some(StreamInfo {
        codec,
        lossless,
        rate,
        bits: if lossless {
            p.bits_per_sample.or(p.bits_per_coded_sample).unwrap_or(16)
        } else {
            0
        },
        channels: p.channels.map(|c| c.count() as u32).unwrap_or(2),
        duration_ms: duration_ms(p.n_frames, p.time_base, rate),
    })
}

enum Kind {
    Sym(Box<dyn Decoder>),
    Opus {
        dec: opus_decoder::OpusDecoder,
        /// Samples still to drop at the start (WebM pre-skip).
        pre_skip: usize,
    },
}

/// Interleaved samples at full i32 scale.
pub struct Chunk {
    pub samples: Vec<i32>,
    pub channels: usize,
}

pub struct TrackDecoder {
    format: Box<dyn FormatReader>,
    track_id: u32,
    time_base: Option<TimeBase>,
    kind: Kind,
    pub rate: u32,
    pub info: StreamInfo,
    pub tags: Tags,
    /// Frames to throw away after an accurate seek.
    discard: u64,
    sbuf: Option<SampleBuffer<i32>>,
    opus_buf: Vec<i16>,
    opus_channels: usize,
}

impl TrackDecoder {
    pub fn open(path: &Path) -> Result<Self, String> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let opened = open(path, false)?;
        let format = opened.format;
        let info = stream_info(format.as_ref(), &ext).ok_or("file không có luồng âm thanh")?;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or("file không có luồng âm thanh")?;
        let params = track.codec_params.clone();
        let track_id = track.id;
        let mut opus_channels = 0;
        let kind = if params.codec == CODEC_TYPE_OPUS {
            let ch = params.channels.map(|c| c.count()).unwrap_or(2);
            if ch > 2 {
                return Err("Opus nhiều kênh chưa được hỗ trợ".into());
            }
            opus_channels = ch;
            // Ogg reports pre-skip through packet trimming; WebM keeps it in OpusHead.
            let pre_skip = match &params.extra_data {
                Some(head) if head.len() >= 12 && head.starts_with(b"OpusHead") && ext != "ogg" && ext != "opus" && ext != "oga" => {
                    u16::from_le_bytes([head[10], head[11]]) as usize
                }
                _ => 0,
            };
            Kind::Opus {
                dec: opus_decoder::OpusDecoder::new(48_000, ch)
                    .map_err(|e| format!("Opus: {e}"))?,
                pre_skip,
            }
        } else {
            let dec = symphonia::default::get_codecs()
                .make(&params, &DecoderOptions::default())
                .map_err(|e| format!("không hỗ trợ định dạng {}: {e}", info.codec))?;
            Kind::Sym(dec)
        };
        Ok(Self {
            format,
            track_id,
            time_base: params.time_base,
            kind,
            rate: if params.codec == CODEC_TYPE_OPUS {
                48_000
            } else {
                params.sample_rate.unwrap_or(44_100)
            },
            info,
            tags: opened.tags,
            discard: 0,
            sbuf: None,
            opus_buf: vec![0; 5760 * 2],
            opus_channels,
        })
    }

    /// Next block of audio, or `None` at the end of the file.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>, String> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(None)
                }
                Err(SymError::ResetRequired) => return Ok(None),
                Err(e) => return Err(e.to_string()),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let (mut samples, channels) = match &mut self.kind {
                Kind::Sym(dec) => match dec.decode(&packet) {
                    Ok(buf) => {
                        if buf.frames() == 0 {
                            continue;
                        }
                        let spec = *buf.spec();
                        let need = buf.capacity();
                        let fits = self
                            .sbuf
                            .as_ref()
                            .map(|b| b.capacity() >= need * spec.channels.count())
                            .unwrap_or(false);
                        if !fits {
                            self.sbuf = Some(SampleBuffer::new(need as u64, spec));
                        }
                        let sb = self.sbuf.as_mut().unwrap();
                        sb.copy_interleaved_ref(buf);
                        (sb.samples().to_vec(), spec.channels.count())
                    }
                    Err(SymError::DecodeError(e)) => {
                        log::debug!("decode error (skipped): {e}");
                        continue;
                    }
                    Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        return Ok(None)
                    }
                    Err(e) => return Err(e.to_string()),
                },
                Kind::Opus { dec, pre_skip } => {
                    let ch = self.opus_channels;
                    let n = match dec.decode(packet.buf(), &mut self.opus_buf, false) {
                        Ok(n) => n,
                        Err(e) => {
                            log::debug!("opus packet skipped: {e}");
                            continue;
                        }
                    };
                    let mut start = packet.trim_start as usize;
                    if *pre_skip > 0 {
                        let s = (*pre_skip).min(n);
                        *pre_skip -= s;
                        start = start.max(s);
                    }
                    let end = n.saturating_sub(packet.trim_end as usize);
                    if start >= end {
                        continue;
                    }
                    let s: Vec<i32> = self.opus_buf[start * ch..end * ch]
                        .iter()
                        .map(|&v| (v as i32) << 16)
                        .collect();
                    (s, ch)
                }
            };
            if self.discard > 0 {
                let frames = (samples.len() / channels) as u64;
                let d = self.discard.min(frames);
                samples.drain(..d as usize * channels);
                self.discard -= d;
                if samples.is_empty() {
                    continue;
                }
            }
            return Ok(Some(Chunk { samples, channels }));
        }
    }

    /// Seeks to `ms`; the next chunk starts exactly there.
    pub fn seek(&mut self, ms: u32) -> Result<(), String> {
        let time = Time::new(ms as u64 / 1000, (ms % 1000) as f64 / 1000.0);
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|e| e.to_string())?;
        match &mut self.kind {
            Kind::Sym(d) => d.reset(),
            Kind::Opus { dec, pre_skip } => {
                dec.reset();
                *pre_skip = 0;
            }
        }
        self.discard = match self.time_base {
            Some(tb) => {
                let t = tb.calc_time(seeked.required_ts.saturating_sub(seeked.actual_ts));
                ((t.seconds as f64 + t.frac) * self.rate as f64).round() as u64
            }
            None => seeked.required_ts.saturating_sub(seeked.actual_ts),
        };
        Ok(())
    }
}
