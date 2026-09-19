use std::sync::Arc;

use crate::gfx::Image;

#[derive(Clone, Debug, Default)]
pub struct TrackInfo {
    pub uri: String,
    pub name: String,
    pub artists: String,
    pub artist_uri: Option<String>,
    pub album: String,
    pub album_uri: Option<String>,
    pub duration_ms: u32,
    /// ~64-300 px cover, for lists.
    pub cover_small: Option<String>,
    /// ~640 px cover, for the now-playing screen.
    pub cover_large: Option<String>,
    pub explicit: bool,
    /// Audio format label, e.g. "FLAC • 24-bit / 96 kHz" (local files).
    pub quality: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PlaylistInfo {
    pub uri: String,
    pub name: String,
    pub owner: String,
    pub length: i32,
    pub cover: Option<String>,
}

/// Something that can be listed as tracks and played as a context.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Liked,
    Playlist { uri: String, name: String },
    Album { uri: String, name: String },
    Artist { uri: String, name: String },
    Search(String),
    /// Files on the SD card (never sent to the Spotify backend).
    Local { title: String, kind: &'static str },
}

impl Source {
    pub fn context_uri(&self, username: &str) -> String {
        match self {
            Source::Liked => format!("spotify:user:{username}:collection"),
            Source::Playlist { uri, .. } | Source::Album { uri, .. } | Source::Artist { uri, .. } => {
                uri.clone()
            }
            Source::Search(q) => {
                let q: String = q
                    .trim()
                    .split_whitespace()
                    .map(encode_component)
                    .collect::<Vec<_>>()
                    .join("+");
                format!("spotify:search:{q}")
            }
            Source::Local { .. } => "local:".into(),
        }
    }

    pub fn title(&self) -> String {
        match self {
            Source::Liked => "Bài hát đã thích".into(),
            Source::Playlist { name, .. } | Source::Album { name, .. } | Source::Artist { name, .. } => {
                name.clone()
            }
            Source::Search(q) => format!("Kết quả: \"{q}\""),
            Source::Local { title, .. } => title.clone(),
        }
    }
}

fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Clone, Debug)]
pub struct TrackList {
    pub context_uri: String,
    pub uris: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Repeat {
    #[default]
    Off,
    Context,
    Track,
}

#[derive(Clone, Debug)]
pub enum ConnState {
    Starting,
    /// No saved account: log in by picking this device in the Spotify app.
    NeedLogin { device_name: String, ip: Option<String> },
    Connecting,
    Connected { username: String },
    /// Connection failed; will retry automatically.
    Offline { message: String },
}

/// UI -> backend.
#[derive(Debug)]
pub enum Cmd {
    LoadPlaylists,
    LoadSource { req: u64, source: Source },
    NeedMeta(Vec<String>),
    Image { url: String, size: u32 },
    /// Play `context_uri` starting at `index` (None = beginning or random if shuffling).
    PlayContext { context_uri: String, index: Option<u32>, shuffle: Option<bool> },
    TogglePlay,
    Next,
    Prev,
    SeekTo(u32),
    VolumeDelta(i32),
    SetShuffle(bool),
    SetRepeat(Repeat),
    TransferHere,
    /// Pause Spotify playback (a local file is about to play).
    Pause,
    Logout,
    CheckUpdate { manual: bool },
    InstallUpdate(crate::update::UpdateInfo),
    Shutdown,
}

/// Backend -> UI.
#[derive(Debug)]
pub enum Event {
    Conn(ConnState),
    Track(TrackInfo),
    Playing { playing: bool, position_ms: u32 },
    Loading { position_ms: u32 },
    Position(u32),
    /// Playback stopped here (for example another device took over).
    Stopped,
    Volume(u16),
    Shuffle(bool),
    Repeat(Repeat),
    Playlists(Result<Vec<PlaylistInfo>, String>),
    Tracks { req: u64, result: Result<TrackList, String> },
    Meta(Vec<TrackInfo>),
    Image { url: String, size: u32, image: Option<Arc<Image>> },
    Toast(String),
    Update(crate::update::UpdateState),
    ShutdownDone,
}
