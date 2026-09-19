//! Music files on the SD card: library scan, grouping and playback.

pub mod art;
pub mod decode;
pub mod library;
pub mod player;

use std::collections::HashMap;
use std::sync::Arc;

pub use library::{Library, LocalTrack};

use crate::spotify::{Repeat, TrackInfo};

pub enum LocalCmd {
    Play {
        queue: Vec<LocalTrack>,
        index: usize,
        shuffle: bool,
    },
    Toggle,
    Pause,
    Next,
    Prev,
    Seek(u32),
    Volume(u16),
    Shuffle(bool),
    Repeat(Repeat),
    Shutdown,
}

pub enum LocalEvent {
    Track(TrackInfo),
    State { playing: bool, position_ms: u32 },
    Position(u32),
    Shuffle(bool),
    /// The queue played to the end (not stopped by the user).
    QueueEnded,
    Error(String),
    ScanProgress { done: usize, total: usize },
    Library(Result<Arc<Library>, String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RgMode {
    Off,
    Track,
    Album,
}

impl RgMode {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "track" => RgMode::Track,
            "album" => RgMode::Album,
            _ => RgMode::Off,
        }
    }
}

pub const UNKNOWN_ARTIST: &str = "Không rõ nghệ sĩ";

pub fn track_info(t: &LocalTrack) -> TrackInfo {
    let artist = if t.artist.is_empty() {
        UNKNOWN_ARTIST.to_string()
    } else {
        t.artist.clone()
    };
    let art = format!("local:art:{}", t.path);
    TrackInfo {
        uri: t.uri(),
        name: t.title.clone(),
        artist_uri: Some(format!("local:artist:{}", artist_key(t))),
        artists: artist,
        album: t.album.clone(),
        album_uri: Some(format!("local:album:{}", t.album_key())),
        duration_ms: t.duration_ms,
        cover_small: Some(art.clone()),
        cover_large: Some(art),
        explicit: false,
        quality: Some(t.quality()),
    }
}

/// Splits a credit like "RPT MCK, Trung Trần" or "Lil Wuyn/ 16 BrT" into artists.
/// "&" is left alone because many band names contain it.
pub fn split_artists(s: &str) -> Vec<String> {
    let mut parts: Vec<String> = vec![s.to_string()];
    for sep in [",", "/", ";", " feat. ", " feat ", " ft. ", " ft ", " Feat. ", " Ft. ", " x ", " X "] {
        parts = parts
            .iter()
            .flat_map(|p| p.split(sep).map(str::to_string).collect::<Vec<_>>())
            .collect();
    }
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        let p = p.trim().to_string();
        if !p.is_empty() && !out.iter().any(|o| o.to_lowercase() == p.to_lowercase()) {
            out.push(p);
        }
    }
    out
}

/// The album artist if tagged, else the first credited artist.
pub fn primary_artist(t: &LocalTrack) -> String {
    let credit = if t.album_artist.trim().is_empty() {
        &t.artist
    } else {
        &t.album_artist
    };
    split_artists(credit)
        .into_iter()
        .next()
        .unwrap_or_else(|| UNKNOWN_ARTIST.to_string())
}

fn artist_key(t: &LocalTrack) -> String {
    primary_artist(t).to_lowercase()
}

/// An album, artist or folder: a titled list of tracks.
#[derive(Clone, Debug)]
pub struct Group {
    pub key: String,
    pub title: String,
    pub subtitle: String,
    /// A track whose cover represents the group.
    pub cover_path: Option<String>,
    /// Indices into `Library::tracks`, in play order.
    pub tracks: Vec<usize>,
}

/// Library views computed once per scan.
pub struct Index {
    pub lib: Arc<Library>,
    pub all: Vec<usize>,
    pub albums: Vec<Group>,
    pub artists: Vec<Group>,
}

fn sort_key(s: &str) -> String {
    s.to_lowercase()
}

impl Index {
    pub fn new(lib: Arc<Library>) -> Self {
        let t = &lib.tracks;
        let mut all: Vec<usize> = (0..t.len()).collect();
        all.sort_by_cached_key(|&i| (sort_key(&t[i].title), sort_key(&t[i].artist)));

        let mut by_album: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_artist: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, tr) in t.iter().enumerate() {
            by_album.entry(tr.album_key()).or_default().push(i);
            // A track shows up under every artist it credits, like on Spotify.
            let mut credits = split_artists(&tr.artist);
            credits.extend(split_artists(&tr.album_artist));
            if credits.is_empty() {
                credits.push(UNKNOWN_ARTIST.to_string());
            }
            let mut seen: Vec<String> = Vec::new();
            for a in credits {
                let k = a.to_lowercase();
                if !seen.contains(&k) {
                    by_artist.entry(k.clone()).or_default().push(i);
                    seen.push(k);
                }
            }
        }
        let album_order = |v: &mut Vec<usize>| {
            v.sort_by_cached_key(|&i| {
                (
                    sort_key(&t[i].album),
                    t[i].disc,
                    t[i].track,
                    sort_key(&t[i].title),
                )
            })
        };
        let mut albums: Vec<Group> = by_album
            .into_iter()
            .map(|(key, mut v)| {
                album_order(&mut v);
                let first = &t[v[0]];
                let who = primary_artist(first);
                Group {
                    key,
                    title: first.album.clone(),
                    subtitle: format!("{who} • {} bài", v.len()),
                    cover_path: Some(first.path.clone()),
                    tracks: v,
                }
            })
            .collect();
        albums.sort_by_cached_key(|g| sort_key(&g.title));

        let mut artists: Vec<Group> = by_artist
            .into_iter()
            .map(|(key, mut v)| {
                album_order(&mut v);
                let first = &t[v[0]];
                // Display name in its original spelling, as credited on the first track.
                let name = split_artists(&first.artist)
                    .into_iter()
                    .chain(split_artists(&first.album_artist))
                    .find(|a| a.to_lowercase() == key)
                    .unwrap_or_else(|| UNKNOWN_ARTIST.to_string());
                let n_albums = {
                    let mut a: Vec<&str> = v.iter().map(|&i| t[i].album.as_str()).collect();
                    a.sort();
                    a.dedup();
                    a.len()
                };
                Group {
                    key,
                    title: name,
                    subtitle: format!("{n_albums} album • {} bài", v.len()),
                    cover_path: Some(first.path.clone()),
                    tracks: v,
                }
            })
            .collect();
        artists.sort_by_cached_key(|g| sort_key(&g.title));

        Self {
            lib,
            all,
            albums,
            artists,
        }
    }

    /// Sub-folders (name, relative path, track count) and the tracks directly in `dir`.
    pub fn folder(&self, dir: &str) -> (Vec<(String, String, usize)>, Vec<usize>) {
        let t = &self.lib.tracks;
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let mut subs: HashMap<String, usize> = HashMap::new();
        let mut here = Vec::new();
        for (i, tr) in t.iter().enumerate() {
            if tr.dir == dir {
                here.push(i);
                continue;
            }
            let rest = if dir.is_empty() {
                Some(tr.dir.as_str())
            } else {
                tr.dir.strip_prefix(&prefix)
            };
            if let Some(name) = rest.and_then(|r| r.split('/').next()) {
                if !name.is_empty() {
                    *subs.entry(name.to_string()).or_default() += 1;
                }
            }
        }
        let mut folders: Vec<(String, String, usize)> = subs
            .into_iter()
            .map(|(name, n)| {
                let full = format!("{prefix}{name}");
                (name, full, n)
            })
            .collect();
        folders.sort_by_cached_key(|f| sort_key(&f.0));
        here.sort_by_cached_key(|&i| (t[i].disc, t[i].track, sort_key(&t[i].title)));
        (folders, here)
    }

    /// All tracks at or below `dir`.
    pub fn folder_recursive(&self, dir: &str) -> Vec<usize> {
        let t = &self.lib.tracks;
        let prefix = format!("{dir}/");
        let mut v: Vec<usize> = (0..t.len())
            .filter(|&i| dir.is_empty() || t[i].dir == dir || t[i].dir.starts_with(&prefix))
            .collect();
        v.sort_by_cached_key(|&i| (t[i].dir.clone(), t[i].disc, t[i].track, sort_key(&t[i].title)));
        v
    }

    pub fn album(&self, key: &str) -> Option<&Group> {
        self.albums.iter().find(|g| g.key == key)
    }

    pub fn artist(&self, key: &str) -> Option<&Group> {
        self.artists.iter().find(|g| g.key == key)
    }
}

/// `spoty --decode-test FILE...`: decodes a file completely and reports on it.
pub fn selftest(path: &std::path::Path) {
    let t0 = std::time::Instant::now();
    let mut dec = match decode::TrackDecoder::open(path) {
        Ok(d) => d,
        Err(e) => {
            println!("{}: OPEN FAILED: {e}", path.display());
            return;
        }
    };
    let info = dec.info.clone();
    let mut frames = 0u64;
    let mut peak = 0i64;
    let mut stereo_all: Vec<i32> = Vec::new();
    loop {
        match dec.next_chunk() {
            Ok(Some(c)) => {
                frames += (c.samples.len() / c.channels) as u64;
                for &s in &c.samples {
                    peak = peak.max((s as i64).abs());
                }
                if stereo_all.len() < 44_100 * 2 * 5 {
                    stereo_all.extend(player::to_stereo_pub(c));
                }
            }
            Ok(None) => break,
            Err(e) => {
                println!("  decode error: {e}");
                break;
            }
        }
    }
    let secs = frames as f64 / dec.rate.max(1) as f64;
    let peak_db = 20.0 * (peak.max(1) as f64 / 2_147_483_648.0).log10();
    println!(
        "{}\n  {} {}-bit {} Hz {}ch lossless={} | header {:.2}s decoded {:.2}s | peak {:.1} dBFS | {:.0}x realtime",
        path.display(),
        info.codec,
        info.bits,
        dec.rate,
        info.channels,
        info.lossless,
        info.duration_ms as f64 / 1000.0,
        secs,
        peak_db,
        secs / t0.elapsed().as_secs_f64().max(1e-6)
    );
    println!(
        "  tags: title={:?} artist={:?} album={:?} track={} rg={:?}",
        dec.tags.title, dec.tags.artist, dec.tags.album, dec.tags.track, dec.tags.rg_track_gain
    );
    // Seek to the middle and make sure decoding resumes.
    let mid = (secs * 500.0) as u32;
    match dec.seek(mid).and_then(|_| dec.next_chunk()) {
        Ok(Some(c)) => println!("  seek to {:.1}s ok ({} frames)", mid as f64 / 1000.0, c.samples.len() / c.channels),
        Ok(None) => println!("  seek to {:.1}s: end of stream", mid as f64 / 1000.0),
        Err(e) => println!("  seek failed: {e}"),
    }
    let target = if dec.rate == 48_000 { 44_100 } else { 48_000 };
    let out = player::resample_selftest(&stereo_all, dec.rate, target);
    println!(
        "  resample {} -> {} Hz: {} -> {} frames",
        dec.rate,
        target,
        stereo_all.len() / 2,
        out
    );
    let cover = art::load(path, 64).is_some();
    println!("  cover art: {}", if cover { "yes" } else { "no" });
}

/// `spoty --scan-test DIR`: scans a folder like the library does.
pub fn scan_selftest(dir: &std::path::Path) {
    let (tx, rx) = std::sync::mpsc::channel();
    let cache = std::env::temp_dir().join("spoty-scan-test.json");
    let t0 = std::time::Instant::now();
    library::spawn_scan(dir.to_path_buf(), cache, tx);
    while let Ok(m) = rx.recv() {
        if let crate::ui::UiMsg::Local(LocalEvent::Library(r)) = m {
            match r {
                Ok(lib) => {
                    let ix = Index::new(lib);
                    println!(
                        "{} tracks, {} albums, {} artists in {:.2}s",
                        ix.lib.tracks.len(),
                        ix.albums.len(),
                        ix.artists.len(),
                        t0.elapsed().as_secs_f64()
                    );
                    for t in ix.lib.tracks.iter().take(20) {
                        println!("  [{}] {} - {} ({})", t.dir, t.artist, t.title, t.quality());
                    }
                }
                Err(e) => println!("scan failed: {e}"),
            }
            break;
        }
    }
}

/// `spoty --play-test FILE...`: plays files as a queue and prints player events.
pub fn play_selftest(files: &[String], cfg: &crate::config::Config) {
    use std::time::{Duration, Instant};
    let queue: Vec<LocalTrack> = files
        .iter()
        .filter_map(|f| {
            let p = std::path::Path::new(f);
            let meta = std::fs::metadata(p).ok()?;
            library::read_track(p.parent()?, p, meta.len(), 0)
        })
        .collect();
    if queue.is_empty() {
        println!("no playable files");
        return;
    }
    let total: u32 = queue.iter().map(|t| t.duration_ms).sum();
    let (tx, rx) = std::sync::mpsc::channel();
    let player = player::spawn(
        player::Settings {
            device: cfg.audio_device.clone(),
            latency_ms: cfg.audio_latency_ms.max(200),
            format: crate::audio::FormatPref::parse(&cfg.audio_output_format),
            replaygain: RgMode::parse(&cfg.replaygain),
        },
        tx,
    );
    let volume = std::env::var("SPOTY_TEST_VOLUME")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .map(|p| (p.min(100) * 65535 / 100) as u16)
        .unwrap_or(u16::MAX);
    let _ = player.send(LocalCmd::Volume(volume));
    let _ = player.send(LocalCmd::Play {
        queue,
        index: 0,
        shuffle: false,
    });
    let t0 = Instant::now();
    let mut seeked = false;
    let deadline = t0 + Duration::from_millis(total as u64 + 5_000);
    while Instant::now() < deadline {
        let Ok(crate::ui::UiMsg::Local(ev)) = rx.recv_timeout(Duration::from_millis(500)) else {
            continue;
        };
        let at = t0.elapsed().as_secs_f32();
        match ev {
            LocalEvent::Track(t) => println!("{at:6.2}s track: {} [{}]", t.name, t.quality.unwrap_or_default()),
            LocalEvent::State { playing, position_ms } => {
                println!("{at:6.2}s state: playing={playing} pos={position_ms}ms");
                if !playing && at > 1.0 {
                    break;
                }
            }
            LocalEvent::Position(ms) => {
                println!("{at:6.2}s position {ms}ms");
                // Exercise seeking once, early in the first track.
                if !seeked && ms >= 2000 {
                    seeked = true;
                    println!("{at:6.2}s -> seek to 5000ms");
                    let _ = player.send(LocalCmd::Seek(5000));
                }
            }
            LocalEvent::Error(e) => println!("{at:6.2}s error: {e}"),
            _ => {}
        }
    }
    let _ = player.send(LocalCmd::Shutdown);
    std::thread::sleep(Duration::from_millis(200));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(album: &str, artist: &str) -> LocalTrack {
        LocalTrack {
            path: format!("/m/{album}/{artist}.flac"),
            dir: String::new(),
            title: artist.into(),
            artist: artist.into(),
            album: album.into(),
            album_artist: String::new(),
            track: 0,
            disc: 0,
            duration_ms: 0,
            codec: "FLAC".into(),
            lossless: true,
            rate: 44_100,
            bits: 16,
            channels: 2,
            kbps: 0,
            size: 0,
            mtime: 0,
        }
    }

    #[test]
    fn splits_credits() {
        assert_eq!(split_artists("RPT MCK, Trung Trần"), vec!["RPT MCK", "Trung Trần"]);
        assert_eq!(split_artists("Lil Wuyn/ 16 BrT"), vec!["Lil Wuyn", "16 BrT"]);
        assert_eq!(split_artists("Simon & Garfunkel"), vec!["Simon & Garfunkel"]);
        assert_eq!(split_artists("A feat. B"), vec!["A", "B"]);
    }

    #[test]
    fn featured_artists_do_not_split_albums() {
        let lib = Library {
            root: "/m".into(),
            tracks: vec![
                track("99%", "RPT MCK"),
                track("99%", "RPT MCK, Trung Trần"),
                track("99%", "RPT MCK, tlinh"),
                track("An", "Lil Wuyn/ 16 BrT"),
                track("An", "Lil Wuyn/ VSoul"),
                track("Lặng", "Shiki"),
                track("Lặng", "Shiki/ Obito"),
                track("Lặng", "SHiKi, tyronee"),
            ],
        };
        let ix = Index::new(Arc::new(lib));
        let albums: Vec<(String, usize)> = ix.albums.iter().map(|g| (g.title.clone(), g.tracks.len())).collect();
        assert_eq!(albums, vec![("99%".into(), 3), ("An".into(), 2), ("Lặng".into(), 3)]);
        let artist = |name: &str| ix.artists.iter().find(|g| g.title == name).map(|g| g.tracks.len());
        assert_eq!(artist("RPT MCK"), Some(3));
        assert_eq!(artist("Trung Trần"), Some(1));
        assert_eq!(artist("tlinh"), Some(1));
    }
}
