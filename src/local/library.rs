//! Scanning the chosen music folder into a cached track list.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use super::decode;
use super::LocalEvent;
use crate::ui::UiMsg;

/// Extensions we can play. Everything else in the folder is ignored.
pub const EXTENSIONS: &[&str] = &[
    "flac", "wav", "wave", "aif", "aiff", "aifc", "m4a", "m4b", "mp4", "aac", "alac", "mp3",
    "mp2", "ogg", "oga", "opus", "webm", "mka",
];

/// Files smaller than this are notification sounds, not music.
const MIN_FILE_BYTES: u64 = 48 * 1024;
const MAX_DEPTH: usize = 16;
const SKIP_DIRS: &[&str] = &[
    "android",
    "$recycle.bin",
    "system volume information",
    "lost+found",
    "notifications",
    "ringtones",
    "alarms",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalTrack {
    pub path: String,
    /// Folder relative to the music root ("" for the root), '/'-separated.
    pub dir: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track: u32,
    pub disc: u32,
    pub duration_ms: u32,
    pub codec: String,
    pub lossless: bool,
    pub rate: u32,
    pub bits: u32,
    pub channels: u32,
    pub kbps: u32,
    pub size: u64,
    pub mtime: u64,
}

impl LocalTrack {
    pub fn uri(&self) -> String {
        format!("file:{}", self.path)
    }

    /// Album identity: title + main artist, so featured artists on some
    /// tracks ("RPT MCK, tlinh") don't split an album into pieces.
    pub fn album_key(&self) -> String {
        format!(
            "{}\u{1}{}",
            super::primary_artist(self).to_lowercase(),
            self.album.trim().to_lowercase()
        )
    }

    /// "FLAC • 24-bit / 96 kHz", "MP3 • 320 kbps".
    pub fn quality(&self) -> String {
        // 44100 -> "44.1 kHz", 22050 -> "22.05 kHz", 96000 -> "96 kHz".
        let khz = format!("{:.2}", self.rate as f64 / 1000.0);
        let khz = format!("{} kHz", khz.trim_end_matches('0').trim_end_matches('.'));
        if self.lossless {
            format!("{} • {}-bit / {}", self.codec, self.bits.max(16), khz)
        } else if self.kbps > 0 {
            format!("{} • {} kbps", self.codec, self.kbps)
        } else {
            format!("{} • {}", self.codec, khz)
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct Library {
    pub root: String,
    pub tracks: Vec<LocalTrack>,
}

impl Library {
    pub fn load_cache(cache: &Path, root: &str) -> Option<Library> {
        let bytes = std::fs::read(cache).ok()?;
        let lib: Library = serde_json::from_slice(&bytes).ok()?;
        (lib.root == root).then_some(lib)
    }

    fn save_cache(&self, cache: &Path) {
        if let Ok(bytes) = serde_json::to_vec(self) {
            let tmp = cache.with_extension("tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, cache);
            }
        }
    }
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn walk(root: &Path) -> Vec<(PathBuf, u64, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            let path = e.path();
            if meta.is_dir() {
                if depth < MAX_DEPTH && !SKIP_DIRS.contains(&name.to_lowercase().as_str()) {
                    stack.push((path, depth + 1));
                }
            } else if meta.len() >= MIN_FILE_BYTES && is_audio(&path) {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                out.push((path, meta.len(), mtime));
            }
        }
    }
    out
}

pub(crate) fn read_track(root: &Path, path: &Path, size: u64, mtime: u64) -> Option<LocalTrack> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let opened = decode::open(path, false).ok()?;
    let info = decode::stream_info(opened.format.as_ref(), &ext)?;
    let t = opened.tags;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let parent = path.parent().unwrap_or(root);
    let dir = parent
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let folder_name = parent
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let kbps = if !info.lossless && info.duration_ms > 0 {
        (size * 8 / info.duration_ms as u64) as u32
    } else {
        0
    };
    Some(LocalTrack {
        path: path.to_string_lossy().to_string(),
        dir,
        title: if t.title.is_empty() {
            crate::gfx::text::clean(&stem)
        } else {
            t.title
        },
        artist: t.artist,
        album: if t.album.is_empty() {
            crate::gfx::text::clean(&folder_name)
        } else {
            t.album
        },
        album_artist: t.album_artist,
        track: t.track,
        disc: t.disc,
        duration_ms: info.duration_ms,
        codec: info.codec,
        lossless: info.lossless,
        rate: info.rate,
        bits: info.bits,
        channels: info.channels,
        kbps,
        size,
        mtime,
    })
}

/// Scans `root` in the background, reusing cached entries for unchanged files.
pub fn spawn_scan(root: PathBuf, cache: PathBuf, tx: Sender<UiMsg>) {
    std::thread::Builder::new()
        .name("scan".into())
        .spawn(move || {
            let send = |e: LocalEvent| {
                let _ = tx.send(UiMsg::Local(e));
            };
            if !root.is_dir() {
                send(LocalEvent::Library(Err(format!(
                    "Không mở được thư mục {}",
                    root.display()
                ))));
                return;
            }
            let root_s = root.to_string_lossy().to_string();
            let cached: HashMap<String, LocalTrack> = Library::load_cache(&cache, &root_s)
                .map(|l| l.tracks.into_iter().map(|t| (t.path.clone(), t)).collect())
                .unwrap_or_default();
            let files = walk(&root);
            let total = files.len();
            send(LocalEvent::ScanProgress { done: 0, total });
            let mut tracks = Vec::with_capacity(total);
            for (i, (path, size, mtime)) in files.into_iter().enumerate() {
                let key = path.to_string_lossy().to_string();
                let reuse = cached
                    .get(&key)
                    .filter(|t| t.size == size && t.mtime == mtime)
                    .cloned();
                if let Some(t) = reuse.or_else(|| read_track(&root, &path, size, mtime)) {
                    tracks.push(t);
                }
                if i % 25 == 24 {
                    send(LocalEvent::ScanProgress { done: i + 1, total });
                }
            }
            let lib = Library {
                root: root_s,
                tracks,
            };
            lib.save_cache(&cache);
            log::info!("local library: {} tracks in {}", lib.tracks.len(), root.display());
            send(LocalEvent::Library(Ok(Arc::new(lib))));
        })
        .expect("spawn scan thread");
}
