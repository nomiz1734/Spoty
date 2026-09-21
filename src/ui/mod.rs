//! UI thread: state, input handling and the frame loop.

pub mod demo;
mod draw;
pub mod theme;
mod telex;
pub mod widgets;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use crate::config::{Config, Paths};
use crate::download::{DownloadCmd, DownloadEvent, SearchResult, Stage};
use crate::gfx::image::{accent_color, decode_png};
use crate::gfx::{Canvas, Color, Fonts, Icon, IconCache, Image, RgbaImage};
use crate::local::{self, LocalCmd, LocalEvent};
use crate::platform::{self, Battery, Button, Screen};
use crate::spotify::home::{FeedItem, FeedKind, FeedSection};
use crate::spotify::{Cmd, ConnState, Event, PlaylistInfo, Repeat, Source, TrackInfo, TrackList};
use crate::update::{self, UpdateInfo, UpdateState};
use crate::wifi_transfer::{self, Wifi, WifiEvent};
use widgets::{FeedState, Keyboard, ListState};

#[cfg_attr(not(any(target_os = "linux", feature = "desktop")), allow(dead_code))]
pub enum UiMsg {
    Input(Button, bool),
    Backend(Event),
    Local(LocalEvent),
    Download(DownloadEvent),
    Wifi(WifiEvent),
}

/// How the UI loop ended.
#[derive(PartialEq, Eq)]
pub enum Exit {
    Quit,
    /// An update was installed: launch.sh starts the new version.
    Restart,
}

pub enum ImgSlot {
    Loading,
    Ready(Arc<Image>, Color),
    Failed(Instant),
}

/// Decoded cover art kept on the UI side, evicted least-recently-used.
#[derive(Default)]
pub struct ImageStore {
    map: HashMap<(String, u32), (ImgSlot, u64)>,
    tick: u64,
    pub requests: Vec<Cmd>,
}

impl ImageStore {
    const MAX: usize = 160;

    pub fn get(&mut self, url: &str, size: u32) -> Option<(Arc<Image>, Color)> {
        self.tick += 1;
        let key = (url.to_string(), size);
        if let Some((slot, used)) = self.map.get_mut(&key) {
            *used = self.tick;
            match slot {
                ImgSlot::Ready(img, c) => return Some((img.clone(), *c)),
                ImgSlot::Failed(at) if at.elapsed() > Duration::from_secs(30) => {}
                _ => return None,
            }
        }
        self.map.insert(key, (ImgSlot::Loading, self.tick));
        self.requests.push(Cmd::Image {
            url: url.to_string(),
            size,
        });
        None
    }

    fn insert(&mut self, url: String, size: u32, image: Option<Arc<Image>>) {
        let slot = match image {
            Some(img) => {
                let c = accent_color(&img);
                ImgSlot::Ready(img, c)
            }
            None => ImgSlot::Failed(Instant::now()),
        };
        self.map.insert((url, size), (slot, self.tick));
        if self.map.len() > Self::MAX {
            let mut entries: Vec<_> = self
                .map
                .iter()
                .filter(|(_, (s, _))| !matches!(s, ImgSlot::Loading))
                .map(|(k, (_, used))| (k.clone(), *used))
                .collect();
            entries.sort_by_key(|e| e.1);
            for (k, _) in entries.into_iter().take(self.map.len() - Self::MAX * 3 / 4) {
                self.map.remove(&k);
            }
        }
    }
}

pub enum View {
    Home(ListState),
    /// Spotify's personalised home ("Dành cho bạn").
    Feed(FeedState),
    Tracks(TracksView),
    NowPlaying,
    Search(Keyboard, SearchTarget),
    /// Results from the slskd server, with the queue underneath.
    Downloads(DownloadsView),
    /// "Nhận nhạc qua WiFi": the address to open on the phone.
    Wifi,
    /// Local music home: all songs / albums / artists / folders / settings.
    Local(ListState),
    Entries(EntriesView),
    Picker(PickerView),
}

/// What a typed query searches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchTarget {
    Spotify,
    /// The slskd server: download the file into the local library.
    Download,
}

pub struct DownloadsView {
    pub query: String,
    pub state: ListState,
}

/// Everything the download screens show. It lives on `App`, not in the view,
/// because events keep arriving while the user is somewhere else.
#[derive(Default)]
pub struct Downloads {
    pub searching: bool,
    /// Id of the newest search, so late results from an older one are dropped.
    pub id: u64,
    pub results: Vec<SearchResult>,
    pub error: Option<String>,
    /// Files waiting; the first one is being fetched.
    pub queue: Vec<String>,
    pub progress: Option<(String, Stage, u64, u64)>,
    /// Files the user already picked, so rows can say so.
    pub picked: HashSet<String>,
}

pub struct TracksView {
    pub req: u64,
    pub source: Source,
    pub cover: Option<String>,
    pub list: Option<Result<TrackList, String>>,
    pub state: ListState,
}

impl TracksView {
    fn is_local(&self) -> bool {
        matches!(self.source, Source::Local { .. })
    }
}

/// A list of albums, artists, or a folder's contents.
pub struct EntriesView {
    pub title: String,
    pub entries: Vec<Entry>,
    pub state: ListState,
}

pub struct Entry {
    pub title: String,
    pub subtitle: String,
    pub cover: Option<String>,
    pub icon: Icon,
    pub action: EntryAction,
}

pub enum EntryAction {
    Tracks {
        title: String,
        kind: &'static str,
        tracks: Vec<usize>,
        cover: Option<String>,
    },
    Folder(String),
    Play {
        tracks: Vec<usize>,
        index: usize,
    },
}

/// Choosing the music folder.
pub struct PickerView {
    pub path: PathBuf,
    pub dirs: Vec<String>,
    pub state: ListState,
}

impl PickerView {
    fn open(path: PathBuf) -> Self {
        let mut dirs: Vec<String> = std::fs::read_dir(&path)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.metadata().map(|m| m.is_dir()).unwrap_or(false))
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| !n.starts_with('.'))
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort_by_key(|d| d.to_lowercase());
        Self {
            path,
            dirs,
            state: ListState::new(),
        }
    }

    /// Row 0 is "use this folder", then the sub-folders.
    pub fn len(&self) -> usize {
        1 + self.dirs.len()
    }
}

#[derive(Clone, Debug)]
pub enum MenuAction {
    GoAlbum(String, String),
    GoArtist(String, String),
    Transfer,
    ScreenOff,
    Refresh,
    LocalMusic,
    PickFolder,
    Updates,
    /// Search the slskd server and download into the local library.
    Downloads,
    /// Receive files from a phone over the local network.
    WifiTransfer,
    Logout,
    Exit,
}

pub struct Menu {
    pub items: Vec<(String, MenuAction)>,
    pub state: ListState,
    pub confirm_logout: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Spotify,
    Local,
}

pub struct Playback {
    pub track: Option<TrackInfo>,
    pub playing: bool,
    pub loading: bool,
    pub active: bool,
    pub pos_ms: u32,
    pub pos_at: Instant,
    pub volume: u16,
    pub shuffle: bool,
    pub repeat: Repeat,
}

impl Playback {
    fn new(volume: u16) -> Self {
        Self {
            track: None,
            playing: false,
            loading: false,
            active: false,
            pos_ms: 0,
            pos_at: Instant::now(),
            volume,
            shuffle: false,
            repeat: Repeat::Off,
        }
    }

    pub fn position(&self) -> u32 {
        let mut p = self.pos_ms;
        if self.playing && !self.loading {
            p += self.pos_at.elapsed().as_millis() as u32;
        }
        match &self.track {
            Some(t) if t.duration_ms > 0 => p.min(t.duration_ms),
            _ => p,
        }
    }
}

#[derive(Default)]
pub struct LocalState {
    pub index: Option<Arc<local::Index>>,
    /// file: URI -> index into the library.
    pub by_uri: HashMap<String, usize>,
    pub scanning: Option<(usize, usize)>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct UpdateUi {
    pub state: Option<UpdateState>,
    pub info: Option<UpdateInfo>,
    pub dialog: bool,
}

pub struct Logos {
    pub big: Option<RgbaImage>,
    pub small: Option<RgbaImage>,
}

pub struct App {
    pub cfg: Config,
    paths: Paths,
    pub conn: ConnState,
    /// Playback of the current owner (what the UI shows and controls).
    pub pb: Playback,
    /// Playback of the other owner, kept in sync in the background.
    other: Playback,
    pub owner: Owner,
    pub playlists: Option<Result<Vec<PlaylistInfo>, String>>,
    pub meta: HashMap<String, TrackInfo>,
    meta_requested: HashSet<String>,
    pub images: ImageStore,
    pub stack: Vec<View>,
    pub menu: Option<Menu>,
    pub toast: Option<(String, Instant)>,
    pub battery: Option<Battery>,
    battery_at: Instant,
    screen_on: bool,
    held: HashMap<Button, (Instant, Instant)>,
    next_req: u64,
    pub dirty: bool,
    /// Something on screen is animating and wants frames.
    pub animating: bool,
    /// Set by the renderer when it drew something animated (spinner, fade).
    pub anim_request: bool,
    last_input: Instant,
    /// The screen was turned off automatically, so any key wakes it.
    idle_off: bool,
    /// Now-playing background colour cross-fade.
    pub bg_from: Color,
    pub bg_to: Color,
    pub bg_t0: Instant,
    pub local: LocalState,
    /// Local tracks of the last queue started, for autoplay.
    local_queue: Vec<usize>,
    pub feed: Option<Result<Vec<FeedSection>, String>>,
    pub feed_loading: bool,
    feed_auto_opened: bool,
    pub update: UpdateUi,
    pub logos: Logos,
    quit: bool,
    restart: bool,
    cmd: UnboundedSender<Cmd>,
    /// Music downloads from the user's own slskd server.
    dl: UnboundedSender<DownloadCmd>,
    pub dls: Downloads,
    /// The WiFi upload server, while its screen is open.
    pub wifi: Option<Wifi>,
    /// The app's tokio runtime, for starting the WiFi server on demand.
    rt: tokio::runtime::Handle,
    local_tx: Sender<LocalCmd>,
    ui_tx: Option<Sender<UiMsg>>,
    started: Instant,
    update_checked: bool,
    update_confirmed: bool,
}

fn percent_to_u16(p: u8) -> u16 {
    (p.min(100) as u32 * 65535 / 100) as u16
}

fn default_browse_root() -> PathBuf {
    let sd = Path::new("/mnt/SDCARD");
    if sd.is_dir() {
        sd.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
    }
}

impl App {
    fn new(
        cfg: Config,
        paths: Paths,
        cmd: UnboundedSender<Cmd>,
        dl: UnboundedSender<DownloadCmd>,
        rt: tokio::runtime::Handle,
        local_tx: Sender<LocalCmd>,
    ) -> Self {
        let local_volume = percent_to_u16(cfg.local_volume);
        Self {
            conn: ConnState::Starting,
            pb: Playback::new(0),
            other: Playback::new(local_volume),
            owner: Owner::Spotify,
            playlists: None,
            meta: HashMap::new(),
            meta_requested: HashSet::new(),
            images: ImageStore::default(),
            stack: vec![View::Home(ListState::new())],
            menu: None,
            toast: None,
            battery: platform::read_battery(),
            battery_at: Instant::now(),
            screen_on: true,
            held: HashMap::new(),
            next_req: 1,
            dirty: true,
            animating: false,
            anim_request: false,
            last_input: Instant::now(),
            idle_off: false,
            bg_from: theme::ELEVATED,
            bg_to: theme::ELEVATED,
            bg_t0: Instant::now(),
            local: LocalState::default(),
            local_queue: Vec::new(),
            feed: {
                let cached = crate::spotify::home::cached(&paths.home_feed_file());
                (!cached.is_empty()).then_some(Ok(cached))
            },
            feed_loading: false,
            feed_auto_opened: false,
            update: UpdateUi::default(),
            logos: Logos {
                big: decode_png(include_bytes!("../../assets/brand/tile-168.png")).ok(),
                small: decode_png(include_bytes!("../../assets/brand/tile-64.png")).ok(),
            },
            quit: false,
            restart: false,
            cmd,
            dl,
            dls: Downloads::default(),
            wifi: None,
            rt,
            local_tx,
            ui_tx: None,
            started: Instant::now(),
            update_checked: false,
            update_confirmed: false,
            cfg,
            paths,
        }
    }

    fn send(&self, c: Cmd) {
        let _ = self.cmd.send(c);
    }

    fn local_send(&self, c: LocalCmd) {
        let _ = self.local_tx.send(c);
    }

    pub fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
        self.dirty = true;
    }

    pub fn username(&self) -> Option<&str> {
        match &self.conn {
            ConnState::Connected { username } => Some(username),
            _ => None,
        }
    }

    pub fn logged_out(&self) -> bool {
        matches!(self.conn, ConnState::NeedLogin { .. })
    }

    /// The login screen replaces the Spotify home while logged out.
    pub fn on_login_screen(&self) -> bool {
        self.logged_out() && matches!(self.stack.last(), Some(View::Home(_)))
    }

    /// Marks URIs whose metadata the renderer wants; sent after the frame.
    pub fn want_meta(&mut self, uri: &str) {
        if uri.starts_with("file:") {
            return;
        }
        if !self.meta.contains_key(uri) && self.meta_requested.insert(uri.to_string()) {
            self.images.requests.push(Cmd::NeedMeta(vec![uri.to_string()]));
        }
    }

    fn flush_requests(&mut self) {
        let mut meta = Vec::new();
        for r in std::mem::take(&mut self.images.requests) {
            match r {
                Cmd::NeedMeta(mut v) => meta.append(&mut v),
                other => self.send(other),
            }
        }
        if !meta.is_empty() {
            self.send(Cmd::NeedMeta(meta));
        }
    }

    // ------------------------------------------------------------ playback owner

    fn pb_for(&mut self, o: Owner) -> &mut Playback {
        if o == self.owner {
            &mut self.pb
        } else {
            &mut self.other
        }
    }

    /// Hands the speaker to `o`, pausing the other player.
    fn switch_owner(&mut self, o: Owner) {
        if o == self.owner {
            return;
        }
        std::mem::swap(&mut self.pb, &mut self.other);
        self.owner = o;
        match o {
            Owner::Local => self.send(Cmd::Pause),
            Owner::Spotify => self.local_send(LocalCmd::Pause),
        }
    }

    fn cmd_toggle(&mut self) {
        match self.owner {
            Owner::Spotify => self.send(Cmd::TogglePlay),
            Owner::Local => self.local_send(LocalCmd::Toggle),
        }
        if self.pb.active {
            self.pb.pos_ms = self.pb.position();
            self.pb.pos_at = Instant::now();
            self.pb.playing = !self.pb.playing; // optimistic
        }
    }

    fn cmd_skip(&mut self, next: bool) {
        match (self.owner, next) {
            (Owner::Spotify, true) => self.send(Cmd::Next),
            (Owner::Spotify, false) => self.send(Cmd::Prev),
            (Owner::Local, true) => self.local_send(LocalCmd::Next),
            (Owner::Local, false) => self.local_send(LocalCmd::Prev),
        }
    }

    fn cmd_seek(&mut self, ms: u32) {
        self.pb.pos_ms = ms;
        self.pb.pos_at = Instant::now();
        match self.owner {
            Owner::Spotify => self.send(Cmd::SeekTo(ms)),
            Owner::Local => self.local_send(LocalCmd::Seek(ms)),
        }
    }

    fn cmd_volume(&mut self, delta_percent: i32) {
        match self.owner {
            Owner::Spotify => self.send(Cmd::VolumeDelta(delta_percent)),
            Owner::Local => {
                let v = (self.pb.volume as i32 + delta_percent * 65535 / 100).clamp(0, 65535) as u16;
                self.pb.volume = v;
                self.cfg.local_volume = ((v as u32 * 100 + 32767) / 65535) as u8;
                self.local_send(LocalCmd::Volume(v));
            }
        }
    }

    fn cmd_shuffle(&mut self, on: bool) {
        self.pb.shuffle = on;
        match self.owner {
            Owner::Spotify => self.send(Cmd::SetShuffle(on)),
            Owner::Local => self.local_send(LocalCmd::Shuffle(on)),
        }
    }

    fn cmd_repeat(&mut self, r: Repeat) {
        self.pb.repeat = r;
        match self.owner {
            Owner::Spotify => self.send(Cmd::SetRepeat(r)),
            Owner::Local => self.local_send(LocalCmd::Repeat(r)),
        }
    }

    // ------------------------------------------------------------ local music

    fn local_index(&self) -> Option<Arc<local::Index>> {
        self.local.index.clone()
    }

    fn set_library(&mut self, lib: Arc<local::Library>) {
        let index = Arc::new(local::Index::new(lib));
        self.local.by_uri.clear();
        // One cover per album, so a list of an album's tracks decodes its art once.
        let mut album_cover: HashMap<usize, String> = HashMap::new();
        for g in &index.albums {
            if let Some(p) = &g.cover_path {
                for &i in &g.tracks {
                    album_cover.insert(i, format!("local:art:{p}"));
                }
            }
        }
        for (i, t) in index.lib.tracks.iter().enumerate() {
            let mut info = local::track_info(t);
            if let Some(c) = album_cover.get(&i) {
                info.cover_small = Some(c.clone());
                info.cover_large = Some(c.clone());
            }
            self.local.by_uri.insert(info.uri.clone(), i);
            self.meta.insert(info.uri.clone(), info);
        }
        self.local.index = Some(index);
    }

    fn start_scan(&mut self) {
        if self.cfg.music_dir.is_empty() || self.local.scanning.is_some() {
            return;
        }
        self.local.scanning = Some((0, 0));
        self.local.error = None;
        local::library::spawn_scan(
            PathBuf::from(&self.cfg.music_dir),
            self.paths.local_library_file(),
            self.local_event_tx(),
        );
    }

    fn local_event_tx(&self) -> Sender<UiMsg> {
        self.ui_tx.clone().expect("ui sender set in run()")
    }

    fn play_local(&mut self, tracks: &[usize], index: usize, shuffle: bool) {
        let Some(ix) = self.local_index() else { return };
        if tracks.is_empty() {
            return;
        }
        let queue: Vec<local::LocalTrack> = tracks.iter().map(|&i| ix.lib.tracks[i].clone()).collect();
        self.local_queue = tracks.to_vec();
        self.switch_owner(Owner::Local);
        self.pb.active = true;
        self.pb.volume = percent_to_u16(self.cfg.local_volume);
        self.local_send(LocalCmd::Volume(self.pb.volume));
        self.local_send(LocalCmd::Repeat(self.pb.repeat));
        self.local_send(LocalCmd::Play {
            queue,
            index,
            shuffle,
        });
        if shuffle {
            self.toast("Phát ngẫu nhiên");
        }
    }

    fn open_local_tracks(&mut self, title: String, kind: &'static str, tracks: &[usize], cover: Option<String>) {
        let Some(ix) = self.local_index() else { return };
        let uris: Vec<String> = tracks.iter().map(|&i| ix.lib.tracks[i].uri()).collect();
        let mut state = ListState::new();
        if let Some(cur) = &self.pb.track {
            if let Some(i) = uris.iter().position(|u| *u == cur.uri) {
                state.set(i, uris.len());
            }
        }
        self.stack.push(View::Tracks(TracksView {
            req: 0,
            source: Source::Local { title, kind },
            cover,
            list: Some(Ok(TrackList {
                context_uri: "local".into(),
                uris,
            })),
            state,
        }));
    }

    fn open_local_home(&mut self) {
        if !matches!(self.stack.last(), Some(View::Local(_))) {
            self.stack.push(View::Local(ListState::new()));
        }
    }

    fn open_picker(&mut self) {
        let start = if self.cfg.music_dir.is_empty() {
            default_browse_root()
        } else {
            PathBuf::from(&self.cfg.music_dir)
        };
        let start = if start.is_dir() { start } else { default_browse_root() };
        self.stack.push(View::Picker(PickerView::open(start)));
    }

    fn open_entries(&mut self, which: usize) {
        let Some(ix) = self.local_index() else {
            self.toast("Hãy chọn thư mục nhạc trước");
            return self.open_picker();
        };
        let cover = |p: &Option<String>| p.as_ref().map(|p| format!("local:art:{p}"));
        let view = match which {
            1 => EntriesView {
                title: "Album".into(),
                entries: ix
                    .albums
                    .iter()
                    .map(|g| Entry {
                        title: g.title.clone(),
                        subtitle: g.subtitle.clone(),
                        cover: cover(&g.cover_path),
                        icon: Icon::Disc,
                        action: EntryAction::Tracks {
                            title: g.title.clone(),
                            kind: "ALBUM",
                            tracks: g.tracks.clone(),
                            cover: cover(&g.cover_path),
                        },
                    })
                    .collect(),
                state: ListState::new(),
            },
            2 => EntriesView {
                title: "Nghệ sĩ".into(),
                entries: ix
                    .artists
                    .iter()
                    .map(|g| Entry {
                        title: g.title.clone(),
                        subtitle: g.subtitle.clone(),
                        cover: cover(&g.cover_path),
                        icon: Icon::Person,
                        action: EntryAction::Tracks {
                            title: g.title.clone(),
                            kind: "NGHỆ SĨ",
                            tracks: g.tracks.clone(),
                            cover: cover(&g.cover_path),
                        },
                    })
                    .collect(),
                state: ListState::new(),
            },
            _ => self.folder_view(""),
        };
        self.stack.push(View::Entries(view));
    }

    fn folder_view(&self, dir: &str) -> EntriesView {
        let Some(ix) = self.local_index() else {
            return EntriesView {
                title: "Thư mục".into(),
                entries: Vec::new(),
                state: ListState::new(),
            };
        };
        let (subs, here) = ix.folder(dir);
        let mut entries: Vec<Entry> = subs
            .into_iter()
            .map(|(name, full, n)| Entry {
                title: name,
                subtitle: format!("Thư mục • {n} bài"),
                cover: None,
                icon: Icon::Folder,
                action: EntryAction::Folder(full),
            })
            .collect();
        for (pos, &i) in here.iter().enumerate() {
            let t = &ix.lib.tracks[i];
            let info = self.meta.get(&t.uri());
            entries.push(Entry {
                title: t.title.clone(),
                subtitle: format!(
                    "{} • {}",
                    info.map(|m| m.artists.clone()).unwrap_or_default(),
                    t.quality()
                ),
                cover: info.and_then(|m| m.cover_small.clone()),
                icon: Icon::Note,
                action: EntryAction::Play {
                    tracks: here.clone(),
                    index: pos,
                },
            });
        }
        let title = if dir.is_empty() {
            Path::new(&self.cfg.music_dir)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Thư mục".into())
        } else {
            dir.rsplit('/').next().unwrap_or(dir).to_string()
        };
        EntriesView {
            title,
            entries,
            state: ListState::new(),
        }
    }

    /// Tracks behind an entry (for shuffle-play).
    fn entry_tracks(&self, e: &Entry) -> Vec<usize> {
        match &e.action {
            EntryAction::Tracks { tracks, .. } => tracks.clone(),
            EntryAction::Play { tracks, .. } => tracks.clone(),
            EntryAction::Folder(dir) => self
                .local_index()
                .map(|ix| ix.folder_recursive(dir))
                .unwrap_or_default(),
        }
    }

    fn choose_music_dir(&mut self, path: PathBuf) {
        self.cfg.music_dir = path.to_string_lossy().to_string();
        self.cfg.save(&self.paths);
        // Anything still downloading would land in the old folder.
        let _ = self.dl.send(DownloadCmd::Cancel);
        let _ = self.dl.send(DownloadCmd::MusicDir(path.clone()));
        self.local.index = None;
        self.local.scanning = None;
        self.stack.retain(|v| !matches!(v, View::Picker(_) | View::Entries(_)));
        self.open_local_home();
        self.start_scan();
        self.toast("Đang quét thư mục nhạc…");
    }

    // ------------------------------------------------------------ Spotify screens

    fn home_len(&self) -> usize {
        4 + match &self.playlists {
            Some(Ok(p)) => p.len(),
            _ => 1, // loading / error row
        }
    }

    fn open_source(&mut self, source: Source, cover: Option<String>) {
        let req = self.next_req;
        self.next_req += 1;
        self.send(Cmd::LoadSource {
            req,
            source: source.clone(),
        });
        self.stack.push(View::Tracks(TracksView {
            req,
            source,
            cover,
            list: None,
            state: ListState::new(),
        }));
    }

    fn open_now_playing(&mut self) {
        if self.pb.track.is_none() {
            self.toast("Chưa có bài nào đang phát");
            return;
        }
        if !matches!(self.stack.last(), Some(View::NowPlaying)) {
            self.stack.push(View::NowPlaying);
        }
    }

    fn open_menu(&mut self) {
        let mut items: Vec<(String, MenuAction)> = Vec::new();
        let focus: Option<TrackInfo> = match self.stack.last() {
            Some(View::Tracks(tv)) => tv
                .list
                .as_ref()
                .and_then(|l| l.as_ref().ok())
                .and_then(|l| l.uris.get(tv.state.sel))
                .and_then(|u| self.meta.get(u).cloned()),
            Some(View::NowPlaying) => self
                .pb
                .track
                .as_ref()
                .map(|t| self.meta.get(&t.uri).cloned().unwrap_or_else(|| t.clone())),
            _ => None,
        };
        if let Some(t) = focus {
            if let Some(u) = t.album_uri.clone() {
                items.push((format!("Mở album: {}", t.album), MenuAction::GoAlbum(u, t.album.clone())));
            }
            if let Some(u) = t.artist_uri.clone() {
                let first = t.artists.split(", ").next().unwrap_or("").to_string();
                items.push((format!("Mở nghệ sĩ: {first}"), MenuAction::GoArtist(u, first)));
            }
        }
        let in_local = matches!(
            self.stack.last(),
            Some(View::Local(_) | View::Entries(_) | View::Picker(_))
        );
        if in_local {
            items.push(("Chọn thư mục nhạc".into(), MenuAction::PickFolder));
        } else {
            items.push(("Nhạc trên máy".into(), MenuAction::LocalMusic));
        }
        if !self.logged_out() {
            items.push(("Chuyển nhạc Spotify về máy này".into(), MenuAction::Transfer));
        }
        items.push(("Tắt màn hình (nhạc vẫn phát)".into(), MenuAction::ScreenOff));
        if !self.logged_out() {
            items.push(("Làm mới thư viện Spotify".into(), MenuAction::Refresh));
        }
        let update_label = match &self.update.info {
            Some(info) => format!("Cập nhật lên phiên bản {}", info.version),
            None => "Kiểm tra cập nhật".into(),
        };
        if !self.cfg.slskd_url.trim().is_empty() {
            items.push(("Tải nhạc".into(), MenuAction::Downloads));
        }
        items.push(("Nhận nhạc qua WiFi".into(), MenuAction::WifiTransfer));
        items.push((update_label, MenuAction::Updates));
        if !self.logged_out() {
            items.push(("Đăng xuất Spotify".into(), MenuAction::Logout));
        }
        items.push(("Thoát Spoty".into(), MenuAction::Exit));
        self.menu = Some(Menu {
            items,
            state: ListState::new(),
            confirm_logout: false,
        });
    }

    fn set_screen(&mut self, screen: &mut dyn Screen, on: bool) {
        if self.screen_on == on {
            return;
        }
        self.screen_on = on;
        self.idle_off = false;
        screen.set_backlight(on);
        self.held.clear();
        self.dirty = true;
    }

    fn run_menu_action(&mut self, action: MenuAction, screen: &mut dyn Screen) {
        if !matches!(action, MenuAction::Logout) {
            self.menu = None;
        }
        match action {
            MenuAction::GoAlbum(uri, name) => {
                if let Some(key) = uri.strip_prefix("local:album:") {
                    if let Some(g) = self.local_index().and_then(|ix| ix.album(key).cloned()) {
                        let cover = g.cover_path.as_ref().map(|p| format!("local:art:{p}"));
                        self.open_local_tracks(g.title.clone(), "ALBUM", &g.tracks, cover);
                    }
                } else {
                    self.open_source(Source::Album { uri, name }, None);
                }
            }
            MenuAction::GoArtist(uri, name) => {
                if let Some(key) = uri.strip_prefix("local:artist:") {
                    if let Some(g) = self.local_index().and_then(|ix| ix.artist(key).cloned()) {
                        let cover = g.cover_path.as_ref().map(|p| format!("local:art:{p}"));
                        self.open_local_tracks(g.title.clone(), "NGHỆ SĨ", &g.tracks, cover);
                    }
                } else {
                    self.open_source(Source::Artist { uri, name }, None);
                }
            }
            MenuAction::Transfer => {
                self.switch_owner(Owner::Spotify);
                self.send(Cmd::TransferHere);
                self.toast("Đang chuyển nhạc về máy này…");
            }
            MenuAction::ScreenOff => self.set_screen(screen, false),
            MenuAction::Refresh => {
                self.playlists = None;
                self.send(Cmd::LoadPlaylists);
            }
            MenuAction::LocalMusic => self.open_local_home(),
            MenuAction::PickFolder => self.open_picker(),
            MenuAction::Updates => {
                self.update.dialog = true;
                if self.update.info.is_none() {
                    self.send(Cmd::CheckUpdate { manual: true });
                    self.update.state = Some(UpdateState::Checking);
                }
            }
            MenuAction::Downloads => self.open_download_search(),
            MenuAction::WifiTransfer => self.start_wifi(),
            MenuAction::Logout => {
                let confirm = self.menu.as_ref().map(|m| m.confirm_logout).unwrap_or(false);
                if confirm {
                    self.menu = None;
                    self.send(Cmd::Logout);
                    self.playlists = None;
                    self.stack.truncate(1);
                } else if let Some(m) = self.menu.as_mut() {
                    m.confirm_logout = true;
                }
            }
            MenuAction::Exit => self.quit = true,
        }
    }

    // ------------------------------------------------------------ events

    fn handle_backend(&mut self, ev: Event) {
        self.dirty = true;
        match ev {
            Event::Conn(state) => {
                let now_connected = matches!(state, ConnState::Connected { .. });
                self.conn = state;
                if now_connected && !matches!(self.playlists, Some(Ok(_))) {
                    self.playlists = None;
                    self.send(Cmd::LoadPlaylists);
                }
                if now_connected {
                    self.feed_loading = true;
                    self.send(Cmd::LoadHome);
                    // Like the Spotify app, start on the home feed (once per run).
                    if !self.feed_auto_opened && self.stack.len() == 1 && self.menu.is_none() {
                        self.stack.push(View::Feed(FeedState::new()));
                    }
                    self.feed_auto_opened = true;
                    // Retry anything that failed while offline.
                    self.meta_requested.clear();
                    let retry: Vec<(u64, Source)> = self
                        .stack
                        .iter()
                        .filter_map(|v| match v {
                            View::Tracks(tv) if !tv.is_local() && matches!(tv.list, Some(Err(_))) => {
                                Some((tv.req, tv.source.clone()))
                            }
                            _ => None,
                        })
                        .collect();
                    for (req, source) in retry {
                        self.send(Cmd::LoadSource { req, source });
                    }
                }
            }
            Event::Track(t) => {
                let merged = match self.meta.get(&t.uri) {
                    Some(m) => TrackInfo {
                        album_uri: m.album_uri.clone(),
                        artist_uri: m.artist_uri.clone(),
                        ..t
                    },
                    None => t,
                };
                let p = self.pb_for(Owner::Spotify);
                p.track = Some(merged);
                p.pos_ms = 0;
                p.pos_at = Instant::now();
            }
            Event::Playing {
                playing,
                position_ms,
            } => {
                if playing {
                    // Spotify started (maybe from the phone): it takes the speaker.
                    self.switch_owner(Owner::Spotify);
                }
                let p = self.pb_for(Owner::Spotify);
                p.playing = playing;
                p.loading = false;
                p.active = true;
                p.pos_ms = position_ms;
                p.pos_at = Instant::now();
            }
            Event::Loading { position_ms } => {
                self.switch_owner(Owner::Spotify);
                let p = self.pb_for(Owner::Spotify);
                p.loading = true;
                p.active = true;
                p.pos_ms = position_ms;
                p.pos_at = Instant::now();
            }
            Event::Position(ms) => {
                let p = self.pb_for(Owner::Spotify);
                p.pos_ms = ms;
                p.pos_at = Instant::now();
            }
            Event::Stopped => {
                let p = self.pb_for(Owner::Spotify);
                p.pos_ms = p.position();
                p.playing = false;
                p.loading = false;
                p.active = false;
                p.pos_at = Instant::now();
            }
            Event::Volume(v) => self.pb_for(Owner::Spotify).volume = v,
            Event::Shuffle(s) => self.pb_for(Owner::Spotify).shuffle = s,
            Event::Repeat(r) => self.pb_for(Owner::Spotify).repeat = r,
            Event::Playlists(r) => {
                if let Err(e) = &r {
                    log::warn!("playlists: {e}");
                }
                self.playlists = Some(r);
            }
            Event::Tracks { req, result } => {
                for v in self.stack.iter_mut() {
                    if let View::Tracks(tv) = v {
                        if tv.req == req && !tv.is_local() {
                            if let Ok(list) = &result {
                                // Jump to the playing track if it is in this list.
                                if let Some(cur) = self.pb.track.as_ref() {
                                    if let Some(i) = list.uris.iter().position(|u| *u == cur.uri) {
                                        tv.state.set(i, list.uris.len());
                                    }
                                }
                            }
                            tv.list = Some(result);
                            break;
                        }
                    }
                }
            }
            Event::Meta(list) => {
                for t in list {
                    for p in [&mut self.pb, &mut self.other] {
                        if let Some(cur) = p.track.as_mut() {
                            if cur.uri == t.uri {
                                cur.album_uri = t.album_uri.clone();
                                cur.artist_uri = t.artist_uri.clone();
                            }
                        }
                    }
                    self.meta_requested.remove(&t.uri);
                    self.meta.insert(t.uri.clone(), t);
                }
            }
            Event::Home(r) => {
                self.feed_loading = false;
                match r {
                    Ok(sections) => self.feed = Some(Ok(sections)),
                    // Keep showing the cached feed if a refresh fails.
                    Err(e) if !matches!(self.feed, Some(Ok(_))) => self.feed = Some(Err(e)),
                    Err(_) => {}
                }
            }
            Event::Image { url, size, image } => self.images.insert(url, size, image),
            Event::Toast(msg) => self.toast(msg),
            Event::Update(state) => {
                match &state {
                    UpdateState::Available(info) => {
                        self.update.info = Some(info.clone());
                        if !self.update.dialog {
                            self.toast(format!("Có bản cập nhật {} — xem trong MENU", info.version));
                        }
                    }
                    UpdateState::Ready { .. } => self.update.dialog = true,
                    _ => {}
                }
                self.update.state = Some(state);
            }
            Event::ShutdownDone => {}
        }
    }

    fn handle_local(&mut self, ev: LocalEvent) {
        self.dirty = true;
        match ev {
            LocalEvent::Track(t) => {
                let p = self.pb_for(Owner::Local);
                p.track = Some(t);
                p.active = true;
                p.loading = false;
                p.pos_ms = 0;
                p.pos_at = Instant::now();
            }
            LocalEvent::State {
                playing,
                position_ms,
            } => {
                if playing {
                    self.switch_owner(Owner::Local);
                }
                let p = self.pb_for(Owner::Local);
                p.playing = playing;
                p.loading = false;
                p.pos_ms = position_ms;
                p.pos_at = Instant::now();
            }
            LocalEvent::Position(ms) => {
                let p = self.pb_for(Owner::Local);
                p.pos_ms = ms;
                p.pos_at = Instant::now();
            }
            LocalEvent::Shuffle(s) => self.pb_for(Owner::Local).shuffle = s,
            LocalEvent::QueueEnded => {
                if self.cfg.autoplay && self.owner == Owner::Local {
                    self.local_autoplay();
                }
            }
            LocalEvent::Error(e) => self.toast(e),
            LocalEvent::ScanProgress { done, total } => self.local.scanning = Some((done, total)),
            LocalEvent::Library(r) => {
                self.local.scanning = None;
                match r {
                    Ok(lib) => {
                        let n = lib.tracks.len();
                        self.set_library(lib);
                        if n == 0 {
                            self.local.error = Some("Không tìm thấy file nhạc nào".into());
                        }
                    }
                    Err(e) => self.local.error = Some(e),
                }
            }
        }
    }

    fn handle_download(&mut self, ev: DownloadEvent) {
        use DownloadEvent as E;
        match ev {
            E::Searching => {
                self.dls.searching = true;
                self.dls.error = None;
                self.dls.results.clear();
            }
            E::Results { id, results } => {
                // A slow search that the user has already replaced is ignored.
                if id >= self.dls.id {
                    self.dls.id = id;
                    self.dls.searching = false;
                    self.dls.error = None;
                    self.dls.results = results;
                }
            }
            E::Queue(names) => self.dls.queue = names,
            E::Progress { name, stage, done, total } => {
                self.dls.progress = Some((name, stage, done, total))
            }
            E::Complete(path) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                self.dls.progress = None;
                self.toast(format!("Đã tải xong {name}"));
                self.start_scan();
            }
            E::Error(e) => {
                self.dls.searching = false;
                self.dls.progress = None;
                // The big message is for an empty list (search failed, server
                // down); with results on screen a toast is enough.
                self.dls.error = self.dls.results.is_empty().then(|| e.clone());
                self.toast(e);
            }
        }
    }

    fn handle_wifi(&mut self, ev: WifiEvent) {
        match ev {
            WifiEvent::Started(url) => {
                if let Some(w) = self.wifi.as_mut() {
                    w.url = url;
                }
            }
            WifiEvent::Received { name, size } => {
                if let Some(w) = self.wifi.as_mut() {
                    w.files.push((name.clone(), size));
                }
                self.toast(format!("Đã nhận {name}"));
            }
            WifiEvent::Error(e) => self.toast(e),
            WifiEvent::Stopped => {}
        }
    }

    fn open_search(&mut self, target: SearchTarget) {
        let vi = self.cfg.search_keyboard != "en";
        self.stack.push(View::Search(Keyboard::new(vi), target));
    }

    fn run_search(&mut self, target: SearchTarget, query: String) {
        match target {
            SearchTarget::Spotify => self.open_source(Source::Search(query), None),
            SearchTarget::Download => self.start_download_search(query),
        }
    }

    fn open_download_search(&mut self) {
        if self.cfg.music_dir.trim().is_empty() {
            self.toast("Hãy chọn thư mục nhạc trước");
            self.open_picker();
            return;
        }
        self.open_search(SearchTarget::Download);
    }

    /// Runs a query on the slskd server and opens the results screen.
    fn start_download_search(&mut self, query: String) {
        self.dls.searching = true;
        self.dls.error = None;
        self.dls.results.clear();
        let _ = self.dl.send(DownloadCmd::Search(query.clone()));
        self.stack.push(View::Downloads(DownloadsView {
            query,
            state: ListState::new(),
        }));
    }

    fn enqueue_download(&mut self, index: usize) {
        let Some(item) = self.dls.results.get(index).cloned() else {
            return;
        };
        let name = item.base_name().to_string();
        self.dls.picked.insert(item.filename.clone());
        let _ = self.dl.send(DownloadCmd::Enqueue(Box::new(item)));
        self.toast(format!("Đã thêm vào hàng đợi: {name}"));
    }

    fn start_wifi(&mut self) {
        if self.cfg.music_dir.trim().is_empty() {
            self.toast("Hãy chọn thư mục nhạc trước");
            self.open_picker();
            return;
        }
        if self.wifi.is_none() {
            let dir = PathBuf::from(&self.cfg.music_dir);
            let ui = self.local_event_tx();
            self.wifi = Some(wifi_transfer::start(&self.rt, dir, ui));
        }
        self.stack.push(View::Wifi);
    }

    /// Closes the WiFi server and picks up whatever arrived.
    fn stop_wifi(&mut self) {
        if let Some(mut w) = self.wifi.take() {
            w.stop();
            if !w.files.is_empty() {
                self.start_scan();
            }
        }
    }

    // ------------------------------------------------------------ input

    fn is_repeatable(&self, b: Button) -> bool {
        matches!(
            b,
            Button::Up | Button::Down | Button::L1 | Button::R1 | Button::VolUp | Button::VolDown
        ) || (matches!(b, Button::Left | Button::Right)
            && matches!(self.stack.last(), Some(View::Search(..) | View::Feed(_))))
            || (matches!(b, Button::RsLeft | Button::RsRight | Button::L2 | Button::R2)
                && matches!(self.stack.last(), Some(View::Search(..))))
    }

    fn handle_button(&mut self, b: Button, repeat: bool, screen: &mut dyn Screen) {
        self.dirty = true;
        self.last_input = Instant::now();
        if !self.screen_on && self.idle_off && !repeat {
            // Auto screen-off: any key only wakes the screen.
            self.set_screen(screen, true);
            return;
        }
        if b == Button::Power {
            if !repeat {
                let on = !self.screen_on;
                self.set_screen(screen, on);
            }
            return;
        }
        let step = self.cfg.volume_step.max(1) as i32;
        match b {
            Button::VolUp => return self.cmd_volume(step),
            Button::VolDown => return self.cmd_volume(-step),
            _ => {}
        }
        if !self.screen_on {
            return;
        }
        if self.update.dialog {
            return self.handle_update_button(b, repeat);
        }
        if b == Button::Menu && !repeat {
            if self.menu.is_some() {
                self.menu = None;
            } else {
                self.open_menu();
            }
            return;
        }
        if self.menu.is_some() {
            return self.handle_menu_button(b, repeat, screen);
        }
        if matches!(b, Button::L3 | Button::R3) {
            if !repeat {
                self.global_toggle();
            }
            return;
        }
        if self.on_login_screen() {
            match b {
                Button::X if !repeat => self.open_local_home(),
                Button::Y if !repeat => self.open_now_playing(),
                Button::B | Button::Start if !repeat => self.open_menu(),
                _ => {}
            }
            return;
        }
        let page = 7;
        let top = self.stack.len() - 1;
        let home_len = self.home_len();
        match &mut self.stack[top] {
            View::Home(state) => {
                let len = home_len;
                match b {
                    Button::Up => state.move_by(-1, len, !repeat),
                    Button::Down => state.move_by(1, len, !repeat),
                    Button::L1 => state.move_by(-page, len, false),
                    Button::R1 => state.move_by(page, len, false),
                    Button::L2 => state.set(0, len),
                    Button::R2 => state.set(len - 1, len),
                    Button::A if !repeat => {
                        let sel = state.sel;
                        self.activate_home_row(sel);
                    }
                    Button::X if !repeat => {
                        let sel = state.sel;
                        self.shuffle_home_row(sel);
                    }
                    Button::Y if !repeat => self.open_now_playing(),
                    Button::B | Button::Start if !repeat => self.open_menu(),
                    _ => {}
                }
            }
            View::Tracks(tv) => {
                let len = match &tv.list {
                    Some(Ok(l)) => l.uris.len(),
                    _ => 0,
                };
                let local = tv.is_local();
                match b {
                    Button::Up => tv.state.move_by(-1, len, !repeat),
                    Button::Down => tv.state.move_by(1, len, !repeat),
                    Button::L1 => tv.state.move_by(-page, len, false),
                    Button::R1 => tv.state.move_by(page, len, false),
                    Button::L2 if len > 0 => tv.state.set(0, len),
                    Button::R2 if len > 0 => tv.state.set(len - 1, len),
                    Button::A | Button::X if !repeat => match &tv.list {
                        Some(Ok(l)) if !l.uris.is_empty() => {
                            let shuffle = b == Button::X;
                            let sel = tv.state.sel;
                            if local {
                                let idx: Vec<usize> = l
                                    .uris
                                    .iter()
                                    .filter_map(|u| self.local.by_uri.get(u).copied())
                                    .collect();
                                self.play_local(&idx, sel, shuffle);
                            } else {
                                let c = Cmd::PlayContext {
                                    context_uri: l.context_uri.clone(),
                                    index: (!shuffle).then_some(sel as u32),
                                    shuffle: shuffle.then_some(true),
                                };
                                self.send(c);
                                if shuffle {
                                    self.toast("Phát ngẫu nhiên");
                                }
                                if self.username().is_some() {
                                    self.switch_owner(Owner::Spotify);
                                    self.pb.loading = true;
                                }
                            }
                        }
                        Some(Err(_)) if b == Button::A => {
                            let (req, source) = (tv.req, tv.source.clone());
                            tv.list = None;
                            self.send(Cmd::LoadSource { req, source });
                        }
                        _ => {}
                    },
                    Button::Select if !repeat => {
                        if let (Some(Ok(l)), Some(cur)) = (&tv.list, &self.pb.track) {
                            if let Some(i) = l.uris.iter().position(|u| *u == cur.uri) {
                                tv.state.set(i, len);
                            }
                        }
                    }
                    Button::Y if !repeat => self.open_now_playing(),
                    Button::B if !repeat => {
                        self.stack.pop();
                    }
                    Button::Start if !repeat => self.open_menu(),
                    _ => {}
                }
            }
            View::NowPlaying => match b {
                Button::A if !repeat => self.cmd_toggle(),
                Button::Left if !repeat => self.cmd_skip(false),
                Button::Right if !repeat => self.cmd_skip(true),
                Button::L1 | Button::R1 => {
                    let delta: i64 = if b == Button::L1 { -15_000 } else { 15_000 };
                    let dur = self.pb.track.as_ref().map(|t| t.duration_ms).unwrap_or(0) as i64;
                    let max = if dur > 0 { dur - 1000 } else { i64::MAX };
                    let target = (self.pb.position() as i64 + delta).clamp(0, max.max(0));
                    self.cmd_seek(target as u32);
                }
                Button::Up => self.cmd_volume(step),
                Button::Down => self.cmd_volume(-step),
                Button::X if !repeat => {
                    let s = !self.pb.shuffle;
                    self.cmd_shuffle(s);
                }
                Button::Y if !repeat => {
                    let r = match self.pb.repeat {
                        Repeat::Off => Repeat::Context,
                        Repeat::Context => Repeat::Track,
                        Repeat::Track => Repeat::Off,
                    };
                    self.cmd_repeat(r);
                }
                Button::B if !repeat => {
                    self.stack.pop();
                }
                Button::Start if !repeat => self.open_menu(),
                _ => {}
            },
            View::Search(kb, target) => {
                let vi = kb.vi;
                let target = *target;
                match b {
                    Button::Up => kb.move_by(0, -1),
                    Button::Down => kb.move_by(0, 1),
                    Button::Left => kb.move_by(-1, 0),
                    Button::Right => kb.move_by(1, 0),
                    Button::RsLeft | Button::L2 => kb.move_cursor(-1),
                    Button::RsRight | Button::R2 => kb.move_cursor(1),
                    Button::RsUp => kb.cursor_home(),
                    Button::RsDown => kb.cursor_end(),
                    Button::A => {
                        if kb.press() {
                            let q = kb.text.trim().to_string();
                            self.run_search(target, q);
                            return;
                        }
                    }
                    Button::X if !repeat => kb.space(),
                    Button::Select if !repeat => kb.clear(),
                    Button::L1 if !repeat => kb.vi = !kb.vi,
                    Button::Start if !repeat => {
                        let q = kb.text.trim().to_string();
                        if !q.is_empty() {
                            self.run_search(target, q);
                            return;
                        }
                    }
                    Button::B => {
                        if kb.text.is_empty() {
                            if !repeat {
                                self.stack.pop();
                            }
                            return;
                        }
                        kb.backspace();
                    }
                    Button::Y if !repeat => {
                        self.open_now_playing();
                        return;
                    }
                    _ => {}
                }
                if kb.vi != vi {
                    // Remember the layout for next time.
                    self.cfg.search_keyboard = if kb.vi { "vi" } else { "en" }.into();
                    self.cfg.save(&self.paths);
                }
            }
            View::Feed(fs) => {
                let lens: Vec<usize> = match &self.feed {
                    Some(Ok(s)) => s.iter().map(|x| x.items.len()).collect(),
                    _ => Vec::new(),
                };
                match b {
                    Button::Up => fs.move_row(-1, &lens),
                    Button::Down => fs.move_row(1, &lens),
                    Button::Left => fs.move_col(-1, &lens),
                    Button::Right => fs.move_col(1, &lens),
                    Button::L1 => fs.move_col(-4, &lens),
                    Button::R1 => fs.move_col(4, &lens),
                    Button::A | Button::X if !repeat => {
                        let (row, col) = (fs.row, fs.cols.get(fs.row).copied().unwrap_or(0));
                        let pick = self.feed_item(row, col);
                        match pick {
                            Some(item) if b == Button::A => self.open_feed_item(&item),
                            Some(item) => self.shuffle_uri(item.uri.clone()),
                            None if self.feed.as_ref().map(|f| f.is_err()).unwrap_or(true) => {
                                self.refresh_feed()
                            }
                            None => {}
                        }
                    }
                    Button::Select if !repeat => self.refresh_feed(),
                    Button::Y if !repeat => self.open_now_playing(),
                    Button::B if !repeat => {
                        self.stack.pop();
                    }
                    Button::Start if !repeat => self.open_menu(),
                    _ => {}
                }
            }
            View::Local(state) => {
                let len = 6;
                match b {
                    Button::Up => state.move_by(-1, len, !repeat),
                    Button::Down => state.move_by(1, len, !repeat),
                    Button::A if !repeat => {
                        let sel = state.sel;
                        self.activate_local_row(sel);
                    }
                    Button::X if !repeat => {
                        if let Some(ix) = self.local_index() {
                            self.play_local(&ix.all, 0, true);
                        }
                    }
                    Button::Y if !repeat => self.open_now_playing(),
                    Button::B if !repeat => {
                        self.stack.pop();
                    }
                    Button::Start if !repeat => self.open_menu(),
                    _ => {}
                }
            }
            View::Entries(ev) => {
                let len = ev.entries.len();
                match b {
                    Button::Up => ev.state.move_by(-1, len, !repeat),
                    Button::Down => ev.state.move_by(1, len, !repeat),
                    Button::L1 => ev.state.move_by(-page, len, false),
                    Button::R1 => ev.state.move_by(page, len, false),
                    Button::L2 if len > 0 => ev.state.set(0, len),
                    Button::R2 if len > 0 => ev.state.set(len - 1, len),
                    Button::A if !repeat && len > 0 => {
                        let sel = ev.state.sel;
                        self.activate_entry(top, sel);
                    }
                    Button::X if !repeat && len > 0 => {
                        let sel = ev.state.sel;
                        let tracks = self.entry_tracks_at(top, sel);
                        self.play_local(&tracks, 0, true);
                    }
                    Button::Y if !repeat => self.open_now_playing(),
                    Button::B if !repeat => {
                        self.stack.pop();
                    }
                    Button::Start if !repeat => self.open_menu(),
                    _ => {}
                }
            }
            View::Downloads(dv) => {
                let len = self.dls.results.len();
                let sel = dv.state.sel;
                match b {
                    Button::Up => dv.state.move_by(-1, len, true),
                    Button::Down => dv.state.move_by(1, len, true),
                    Button::L1 => dv.state.move_by(-page, len, false),
                    Button::R1 => dv.state.move_by(page, len, false),
                    Button::L2 if len > 0 => dv.state.set(0, len),
                    Button::R2 if len > 0 => dv.state.set(len - 1, len),
                    Button::A if !repeat && len > 0 => self.enqueue_download(sel),
                    Button::Select if !repeat => {
                        let _ = self.dl.send(DownloadCmd::Cancel);
                        self.dls.picked.clear();
                        self.toast("Đã hủy hàng đợi tải");
                    }
                    Button::B if !repeat => {
                        self.stack.pop();
                    }
                    Button::Y if !repeat => self.open_now_playing(),
                    Button::Start if !repeat => self.open_menu(),
                    _ => {}
                }
            }
            View::Wifi => match b {
                Button::B if !repeat => {
                    self.stop_wifi();
                    self.stack.pop();
                }
                Button::Y if !repeat => self.open_now_playing(),
                Button::Start if !repeat => self.open_menu(),
                _ => {}
            },
            View::Picker(pv) => {
                let len = pv.len();
                match b {
                    Button::Up => pv.state.move_by(-1, len, !repeat),
                    Button::Down => pv.state.move_by(1, len, !repeat),
                    Button::L1 => pv.state.move_by(-page, len, false),
                    Button::R1 => pv.state.move_by(page, len, false),
                    Button::A if !repeat => {
                        let sel = pv.state.sel;
                        if sel == 0 {
                            let path = pv.path.clone();
                            self.choose_music_dir(path);
                        } else if let Some(d) = pv.dirs.get(sel - 1) {
                            let next = pv.path.join(d);
                            *pv = PickerView::open(next);
                        }
                    }
                    Button::Start | Button::X if !repeat => {
                        let path = pv.path.clone();
                        self.choose_music_dir(path);
                    }
                    Button::B if !repeat => match pv.path.parent() {
                        Some(parent) => {
                            let child = pv.path.file_name().map(|s| s.to_string_lossy().to_string());
                            let mut up = PickerView::open(parent.to_path_buf());
                            if let Some(i) = child.and_then(|c| up.dirs.iter().position(|d| *d == c)) {
                                up.state.set(i + 1, up.len());
                            }
                            *pv = up;
                        }
                        None => {
                            self.stack.pop();
                        }
                    },
                    Button::Select if !repeat => {
                        self.stack.pop();
                    }
                    _ => {}
                }
            }
        }
    }

    fn entry_tracks_at(&self, view: usize, sel: usize) -> Vec<usize> {
        match &self.stack[view] {
            View::Entries(ev) => ev.entries.get(sel).map(|e| self.entry_tracks(e)).unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn activate_entry(&mut self, view: usize, sel: usize) {
        let View::Entries(ev) = &self.stack[view] else { return };
        let Some(e) = ev.entries.get(sel) else { return };
        match &e.action {
            EntryAction::Tracks {
                title,
                kind,
                tracks,
                cover,
            } => {
                let (title, kind, tracks, cover) = (title.clone(), *kind, tracks.clone(), cover.clone());
                self.open_local_tracks(title, kind, &tracks, cover);
            }
            EntryAction::Folder(dir) => {
                let v = self.folder_view(&dir.clone());
                self.stack.push(View::Entries(v));
            }
            EntryAction::Play { tracks, index } => {
                let (tracks, index) = (tracks.clone(), *index);
                self.play_local(&tracks, index, false);
            }
        }
    }

    fn activate_local_row(&mut self, sel: usize) {
        match sel {
            0 => match self.local_index() {
                Some(ix) => {
                    let all = ix.all.clone();
                    self.open_local_tracks("Tất cả bài hát".into(), "TRÊN MÁY", &all, None);
                }
                None => {
                    self.toast("Hãy chọn thư mục nhạc trước");
                    self.open_picker();
                }
            },
            1..=3 => self.open_entries(sel),
            4 => {
                if self.cfg.music_dir.is_empty() {
                    self.open_picker();
                } else {
                    self.start_scan();
                }
            }
            _ => self.open_picker(),
        }
    }

    // ------------------------------------------------------------ home feed

    fn open_feed(&mut self) {
        if !matches!(self.stack.last(), Some(View::Feed(_))) {
            self.stack.push(View::Feed(FeedState::new()));
        }
        if !self.feed_loading && !matches!(self.feed, Some(Ok(_))) {
            self.refresh_feed();
        }
    }

    fn refresh_feed(&mut self) {
        if self.username().is_none() {
            self.toast("Cần kết nối Spotify để tải đề xuất");
            return;
        }
        self.feed_loading = true;
        if matches!(self.feed, Some(Err(_))) {
            self.feed = None;
        }
        self.send(Cmd::LoadHome);
        self.toast("Đang làm mới đề xuất…");
    }

    fn feed_item(&self, row: usize, col: usize) -> Option<FeedItem> {
        match &self.feed {
            Some(Ok(s)) => s.get(row).and_then(|sec| sec.items.get(col)).cloned(),
            _ => None,
        }
    }

    fn open_feed_item(&mut self, item: &FeedItem) {
        let (uri, name) = (item.uri.clone(), item.title.clone());
        let source = match item.kind {
            FeedKind::Playlist => Source::Playlist { uri, name },
            FeedKind::Album => Source::Album { uri, name },
            FeedKind::Artist => Source::Artist { uri, name },
        };
        self.open_source(source, item.image.clone());
    }

    fn shuffle_uri(&mut self, context_uri: String) {
        if self.username().is_none() {
            return;
        }
        self.switch_owner(Owner::Spotify);
        self.send(Cmd::PlayContext {
            context_uri,
            index: None,
            shuffle: Some(true),
        });
        self.toast("Phát ngẫu nhiên");
    }

    /// Play/pause from any screen (joystick press).
    fn global_toggle(&mut self) {
        if self.pb.track.is_none() {
            self.toast("Chưa có bài nào đang phát");
            return;
        }
        let was_playing = self.pb.playing;
        self.cmd_toggle();
        if !matches!(self.stack.last(), Some(View::NowPlaying)) {
            self.toast(if was_playing { "Tạm dừng" } else { "Tiếp tục phát" });
        }
    }

    /// When a local queue ends: more by the same artist, then the rest of the
    /// library in random order.
    fn local_autoplay(&mut self) {
        let Some(ix) = self.local_index() else { return };
        let played: HashSet<usize> = self.local_queue.iter().copied().collect();
        let current = self.pb_for(Owner::Local).track.as_ref().map(|t| t.uri.clone());
        let seed = current
            .and_then(|u| self.local.by_uri.get(&u).copied())
            .or_else(|| self.local_queue.last().copied());
        let Some(seed) = seed else { return };
        let artist = local::primary_artist(&ix.lib.tracks[seed]).to_lowercase();
        let mut rng = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
            | 1;
        let mut shuffle = |v: &mut Vec<usize>| {
            for i in (1..v.len()).rev() {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                v.swap(i, (rng % (i as u64 + 1)) as usize);
            }
        };
        let fresh: Vec<usize> = (0..ix.lib.tracks.len()).filter(|i| !played.contains(i)).collect();
        let (mut same, mut others): (Vec<usize>, Vec<usize>) = fresh
            .into_iter()
            .partition(|&i| local::primary_artist(&ix.lib.tracks[i]).to_lowercase() == artist);
        shuffle(&mut same);
        shuffle(&mut others);
        same.truncate(10);
        let mut queue = same;
        queue.extend(others.into_iter().take(25usize.saturating_sub(queue.len())));
        if queue.is_empty() {
            return;
        }
        self.play_local(&queue, 0, false);
        self.toast("Tự động phát bài tương tự");
    }

    fn handle_menu_button(&mut self, b: Button, repeat: bool, screen: &mut dyn Screen) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let len = menu.items.len();
        match b {
            Button::Up => menu.state.move_by(-1, len, !repeat),
            Button::Down => menu.state.move_by(1, len, !repeat),
            Button::A if !repeat => {
                let action = menu.items[menu.state.sel].1.clone();
                if !matches!(action, MenuAction::Logout) {
                    menu.confirm_logout = false;
                }
                self.run_menu_action(action, screen);
            }
            Button::B | Button::Start if !repeat => self.menu = None,
            _ => {}
        }
    }

    fn handle_update_button(&mut self, b: Button, repeat: bool) {
        if repeat {
            return;
        }
        match b {
            Button::A => match self.update.state.clone() {
                Some(UpdateState::Available(info)) => {
                    self.update.state = Some(UpdateState::Downloading {
                        done: 0,
                        total: info.size,
                    });
                    self.send(Cmd::InstallUpdate(info));
                }
                Some(UpdateState::Ready { .. }) => {
                    self.restart = true;
                    self.quit = true;
                }
                Some(UpdateState::Failed(_)) => {
                    // Try again.
                    match self.update.info.clone() {
                        Some(info) => self.update.state = Some(UpdateState::Available(info)),
                        None => {
                            self.update.state = Some(UpdateState::Checking);
                            self.send(Cmd::CheckUpdate { manual: true });
                        }
                    }
                }
                Some(UpdateState::UpToDate) | None => self.update.dialog = false,
                _ => {}
            },
            Button::B | Button::Menu => self.update.dialog = false,
            _ => {}
        }
    }

    fn activate_home_row(&mut self, sel: usize) {
        match sel {
            0 => self.open_feed(),
            1 => self.open_source(Source::Liked, None),
            2 => self.open_search(SearchTarget::Spotify),
            3 => self.open_local_home(),
            n => match &self.playlists {
                Some(Ok(list)) => {
                    if let Some(p) = list.get(n - 4) {
                        let (uri, name, cover) = (p.uri.clone(), p.name.clone(), p.cover.clone());
                        self.open_source(Source::Playlist { uri, name }, cover);
                    }
                }
                Some(Err(_)) => {
                    self.playlists = None;
                    self.send(Cmd::LoadPlaylists);
                }
                None => {}
            },
        }
    }

    fn shuffle_home_row(&mut self, sel: usize) {
        if sel == 3 {
            if let Some(ix) = self.local_index() {
                self.play_local(&ix.all, 0, true);
            }
            return;
        }
        let Some(user) = self.username().map(str::to_string) else {
            return;
        };
        let uri = match sel {
            1 => Some(Source::Liked.context_uri(&user)),
            0 | 2 => None,
            n => match &self.playlists {
                Some(Ok(list)) => list.get(n - 4).map(|p| p.uri.clone()),
                _ => None,
            },
        };
        if let Some(context_uri) = uri {
            self.switch_owner(Owner::Spotify);
            self.send(Cmd::PlayContext {
                context_uri,
                index: None,
                shuffle: Some(true),
            });
            self.toast("Phát ngẫu nhiên");
        }
    }

    /// Generates repeat presses for held buttons.
    fn tick_repeats(&mut self, screen: &mut dyn Screen) {
        let now = Instant::now();
        let due: Vec<Button> = self
            .held
            .iter()
            .filter(|(_, (_, next))| *next <= now)
            .map(|(b, _)| *b)
            .collect();
        for b in due {
            if let Some((since, next)) = self.held.get_mut(&b) {
                let held_for = now.duration_since(*since);
                let interval = if held_for > Duration::from_millis(1500) {
                    Duration::from_millis(30)
                } else {
                    Duration::from_millis(70)
                };
                *next = now + interval;
            }
            self.handle_button(b, true, screen);
        }
    }

    fn next_repeat_deadline(&self) -> Option<Instant> {
        self.held.values().map(|(_, next)| *next).min()
    }

    fn update_animations(&mut self, dt: f32) {
        let mut moving = false;
        for v in self.stack.iter_mut() {
            match v {
                View::Home(s) | View::Local(s) => moving |= s.animate(dt),
                View::Tracks(tv) => moving |= tv.state.animate(dt),
                View::Entries(ev) => moving |= ev.state.animate(dt),
                View::Picker(pv) => moving |= pv.state.animate(dt),
                View::Downloads(dv) => moving |= dv.state.animate(dt),
                View::Feed(fs) => moving |= fs.animate(dt),
                _ => {}
            }
        }
        if let Some(m) = self.menu.as_mut() {
            moving |= m.state.animate(dt);
        }
        if let Some((_, t)) = &self.toast {
            if t.elapsed() > Duration::from_millis(2500) {
                self.toast = None;
                self.dirty = true;
            }
        }
        let want = moving || self.anim_request;
        self.anim_request = false;
        if want {
            self.dirty = true;
        }
        self.animating = want;
    }

    /// Turns the screen off after a while without input, while music plays.
    fn check_idle(&mut self, screen: &mut dyn Screen) {
        let secs = self.cfg.screen_off_after_s;
        if secs == 0 || !self.screen_on || self.menu.is_some() || self.update.dialog {
            return;
        }
        if self.pb.playing && self.last_input.elapsed() > Duration::from_secs(secs as u64) {
            self.set_screen(screen, false);
            self.idle_off = true;
        }
    }

    /// Once-per-run housekeeping: update checks and confirming a fresh update.
    fn tick_background(&mut self) {
        let up = self.started.elapsed();
        if !self.update_confirmed && up > Duration::from_secs(10) {
            self.update_confirmed = true;
            update::confirm(&self.paths.data_dir);
        }
        if !self.update_checked && up > Duration::from_secs(8) {
            self.update_checked = true;
            if self.cfg.auto_update_check && !self.cfg.update_url.trim().is_empty() {
                self.send(Cmd::CheckUpdate { manual: false });
            }
        }
    }
}

pub fn run(
    mut screen: Box<dyn Screen>,
    mut fonts: Fonts,
    rx: Receiver<UiMsg>,
    tx: Sender<UiMsg>,
    cmd: UnboundedSender<Cmd>,
    dl: UnboundedSender<DownloadCmd>,
    rt: tokio::runtime::Handle,
    cfg: Config,
    paths: Paths,
) -> Exit {
    let (w, h) = screen.size();
    let mut canvas = Canvas::new(w, h);
    let mut icons = IconCache::default();
    let local_tx = local::player::spawn(
        local::player::Settings {
            device: cfg.audio_device.clone(),
            latency_ms: cfg.audio_latency_ms.max(200),
            format: crate::audio::FormatPref::parse(&cfg.audio_output_format),
            replaygain: local::RgMode::parse(&cfg.replaygain),
        },
        tx.clone(),
    );
    let mut app = App::new(cfg, paths, cmd, dl, rt, local_tx);
    app.ui_tx = Some(tx.clone());
    if let Some(v) = update::just_updated(&app.paths.data_dir) {
        app.toast(format!("Đã cập nhật lên phiên bản {v}"));
    }
    // Local library: show the cached list at once, then rescan for changes.
    if !app.cfg.music_dir.is_empty() {
        if let Some(lib) =
            local::Library::load_cache(&app.paths.local_library_file(), &app.cfg.music_dir)
        {
            app.set_library(Arc::new(lib));
        }
        app.start_scan();
    }
    let mut last_frame = Instant::now();
    let mut last_second = 0u64;
    let frame = Duration::from_millis(16);

    loop {
        // Wait for input/backend messages, or until the next thing needs drawing.
        let mut timeout = if app.animating && app.screen_on {
            // Cap animations at ~60 fps even if the display has no vsync.
            (last_frame + frame).saturating_duration_since(Instant::now())
        } else if app.dirty && app.screen_on {
            Duration::ZERO
        } else if screen.needs_polling() {
            frame
        } else {
            Duration::from_millis(500)
        };
        if let Some(d) = app.next_repeat_deadline() {
            timeout = timeout.min(d.saturating_duration_since(Instant::now()));
        }
        let first = match rx.recv_timeout(timeout) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let mut pending: Vec<UiMsg> = first.into_iter().collect();
        while let Ok(m) = rx.try_recv() {
            pending.push(m);
        }
        for m in pending {
            match m {
                UiMsg::Input(b, true) => {
                    if app.is_repeatable(b) {
                        let now = Instant::now();
                        app.held.insert(b, (now, now + Duration::from_millis(280)));
                    }
                    app.handle_button(b, false, screen.as_mut());
                }
                UiMsg::Input(b, false) => {
                    app.held.remove(&b);
                }
                UiMsg::Backend(ev) => app.handle_backend(ev),
                UiMsg::Local(ev) => app.handle_local(ev),
                UiMsg::Download(ev) => app.handle_download(ev),
                UiMsg::Wifi(ev) => app.handle_wifi(ev),
            }
        }
        if !screen.pump(&tx) {
            break;
        }
        app.tick_repeats(screen.as_mut());

        let now = Instant::now();
        let dt = now.duration_since(last_frame).as_secs_f32().min(0.1);
        app.update_animations(dt);

        // Clock, progress bar and battery refresh.
        let second = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if second != last_second {
            last_second = second;
            if app.pb.playing || second % 60 == 0 || app.local.scanning.is_some() {
                app.dirty = true;
            }
            app.check_idle(screen.as_mut());
            app.tick_background();
            if app.battery_at.elapsed() > Duration::from_secs(30) {
                app.battery = platform::read_battery();
                app.battery_at = Instant::now();
            }
        }
        if app.quit {
            break;
        }
        if app.dirty && app.screen_on {
            app.dirty = false;
            draw::frame(&mut canvas, &mut fonts, &mut icons, &mut app);
            screen.present(&canvas);
            last_frame = now;
            app.flush_requests();
        } else {
            last_frame = now;
        }
    }

    // Remember the local volume, stop both players and wait briefly for Spotify.
    app.cfg.save(&app.paths);
    app.stop_wifi();
    app.local_send(LocalCmd::Shutdown);
    let _ = app.dl.send(DownloadCmd::Shutdown);
    let _ = app.cmd.send(Cmd::Shutdown);
    let deadline = Instant::now() + Duration::from_secs(3);
    while let Ok(m) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if matches!(m, UiMsg::Backend(Event::ShutdownDone)) {
            break;
        }
    }
    drop(screen);
    if app.restart {
        Exit::Restart
    } else {
        Exit::Quit
    }
}
