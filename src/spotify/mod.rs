//! Spotify backend: login via Spotify Connect (zeroconf), playback through Spirc,
//! browsing through Spotify's internal endpoints. Runs on the tokio runtime and
//! talks to the UI thread through channels.

pub mod home;
pub mod images;
pub mod library;
pub mod types;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use librespot_connect::{
    ConnectConfig, LoadContextOptions, LoadRequest, LoadRequestOptions, Options, PlayingTrack,
    Spirc,
};
use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::config::DeviceType;
use librespot_core::error::ErrorKind;
use librespot_core::{Session, SessionConfig};
use librespot_discovery::Discovery;
use librespot_metadata::audio::{AudioItem, UniqueFields};
use librespot_playback::config::{Bitrate, PlayerConfig};
use librespot_playback::mixer::{self, Mixer, MixerConfig};
use librespot_playback::player::{Player, PlayerEvent};
use sha1::{Digest, Sha1};
use tokio::sync::{mpsc, Semaphore};
use tokio::time::Instant;

use crate::config::{Config, Paths};
use crate::gfx::text::clean;
use crate::ui::UiMsg;
pub use types::*;

type UiTx = std::sync::mpsc::Sender<UiMsg>;
type SpircTask = Pin<Box<dyn Future<Output = ()> + Send>>;

/// State shared with the tasks spawned for data loading.
struct Shared {
    ui: UiTx,
    meta: Mutex<HashMap<String, TrackInfo>>,
    meta_pending: Mutex<HashSet<String>>,
    images_inflight: Mutex<HashSet<(String, u32)>>,
    image_slots: Semaphore,
    image_dir: PathBuf,
}

impl Shared {
    fn emit(&self, e: Event) {
        let _ = self.ui.send(UiMsg::Backend(e));
    }
}

pub fn spawn(
    rt: &tokio::runtime::Runtime,
    cfg: Config,
    paths: Paths,
    ui: UiTx,
) -> mpsc::UnboundedSender<Cmd> {
    let (tx, rx) = mpsc::unbounded_channel();
    rt.spawn(run(cfg, paths, rx, ui));
    tx
}

pub fn local_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then(|| ip.to_string())
}

fn device_id(name: &str) -> String {
    Sha1::digest(name.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn audio_item_info(item: &AudioItem) -> TrackInfo {
    let (artists, album) = match &item.unique_fields {
        UniqueFields::Track { artists, album, .. } => (
            artists
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            album.clone(),
        ),
        UniqueFields::Local { artists, album, .. } => (
            artists.clone().unwrap_or_default(),
            album.clone().unwrap_or_default(),
        ),
        UniqueFields::Episode { show_name, .. } => (show_name.clone(), String::new()),
    };
    let mut covers: Vec<_> = item.covers.iter().collect();
    covers.sort_by_key(|c| c.width);
    let small = covers.iter().find(|c| c.width >= 64).or(covers.first());
    let large = covers
        .iter()
        .rev()
        .find(|c| c.width <= 700 && c.width >= 300)
        .or(covers.last());
    TrackInfo {
        uri: item.uri.clone(),
        name: clean(&item.name),
        artists: clean(&artists),
        album: clean(&album),
        duration_ms: item.duration_ms,
        cover_small: small.map(|c| c.url.clone()),
        cover_large: large.map(|c| c.url.clone()),
        explicit: item.is_explicit,
        ..Default::default()
    }
}

fn is_auth_error(e: &librespot_core::Error) -> bool {
    matches!(
        e.kind,
        ErrorKind::PermissionDenied | ErrorKind::Unauthenticated | ErrorKind::InvalidArgument
    )
}

async fn run(
    cfg: Config,
    paths: Paths,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    ui: UiTx,
) {
    let _ = std::fs::create_dir_all(paths.image_cache_dir());
    let image_dir = paths.image_cache_dir();
    tokio::task::spawn_blocking({
        let d = image_dir.clone();
        move || images::trim_cache(&d)
    });
    let shared = Arc::new(Shared {
        ui,
        meta: Mutex::new(HashMap::new()),
        meta_pending: Mutex::new(HashSet::new()),
        images_inflight: Mutex::new(HashSet::new()),
        image_slots: Semaphore::new(3),
        image_dir,
    });
    shared.emit(Event::Conn(ConnState::Starting));

    let device_id = device_id(&cfg.device_name);
    let session_config = SessionConfig {
        device_id: device_id.clone(),
        // Like the Spotify apps: when a playlist or album ends, continue with similar songs.
        autoplay: Some(cfg.autoplay),
        ..Default::default()
    };
    let audio_dir = (cfg.audio_cache_mb > 0).then(|| paths.audio_cache_dir());
    let cache = match Cache::new(
        Some(paths.credentials_dir()),
        Some(paths.credentials_dir()),
        audio_dir,
        Some(cfg.audio_cache_mb * 1024 * 1024),
    ) {
        Ok(c) => Some(c),
        Err(e) => {
            log::warn!("cache disabled: {e}");
            None
        }
    };
    let initial_volume = cache
        .as_ref()
        .and_then(|c| c.volume())
        .unwrap_or((cfg.initial_volume.min(100) as u32 * 65535 / 100) as u16);
    let connect_config = ConnectConfig {
        name: cfg.device_name.clone(),
        device_type: DeviceType::Speaker,
        is_group: false,
        initial_volume,
        disable_volume: false,
        volume_steps: 64,
    };

    let mixer: Arc<dyn Mixer> = match mixer::find(Some("softvol")).map(|f| f(MixerConfig::default()))
    {
        Some(Ok(m)) => m,
        _ => {
            log::error!("could not create soft mixer");
            shared.emit(Event::Conn(ConnState::Offline {
                message: "Không khởi tạo được bộ trộn âm thanh".into(),
            }));
            return;
        }
    };
    mixer.set_volume(initial_volume);
    shared.emit(Event::Volume(initial_volume));

    let player_config = PlayerConfig {
        bitrate: match cfg.bitrate {
            0..=96 => Bitrate::Bitrate96,
            97..=160 => Bitrate::Bitrate160,
            _ => Bitrate::Bitrate320,
        },
        normalisation: cfg.normalize,
        position_update_interval: Some(Duration::from_secs(5)),
        ..Default::default()
    };

    let mut session = Session::new(session_config.clone(), cache.clone());
    let sink_device = cfg.audio_device.clone();
    let latency = cfg.audio_latency_ms;
    let format = crate::audio::FormatPref::parse(&cfg.audio_output_format);
    let notify_shared = shared.clone();
    let player = Player::new(player_config, session.clone(), mixer.get_soft_volume(), move || {
        crate::audio::make_sink(
            sink_device,
            latency,
            format,
            Box::new(move |msg| notify_shared.emit(Event::Toast(msg))),
        )
    });
    let mut player_events = player.get_player_event_channel();

    let mut discovery: Option<Discovery> = None;
    let far_future = || Instant::now() + Duration::from_secs(86400 * 365);
    let mut discovery_retry_at = Instant::now();

    let mut creds: Option<Credentials> = cache.as_ref().and_then(|c| c.credentials());
    let mut connecting = creds.is_some();
    if creds.is_none() {
        shared.emit(Event::Conn(ConnState::NeedLogin {
            device_name: cfg.device_name.clone(),
            ip: local_ip(),
        }));
    }
    let mut spirc: Option<Spirc> = None;
    let mut spirc_task: Option<SpircTask> = None;
    let mut retry_at: Option<Instant> = None;
    let mut backoff = Duration::from_secs(3);
    let mut attempts = 0u32;
    let mut active = false;
    let mut volume = initial_volume;
    let mut repeat = Repeat::Off;

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(discovery_retry_at), if discovery.is_none() => {
                match Discovery::builder(device_id.clone(), session_config.client_id.clone())
                    .name(cfg.device_name.clone())
                    .device_type(DeviceType::Speaker)
                    .launch()
                {
                    Ok(d) => {
                        log::info!("zeroconf discovery running as \"{}\"", cfg.device_name);
                        discovery = Some(d);
                    }
                    Err(e) => {
                        log::warn!("discovery failed: {e}; retrying");
                        discovery_retry_at = Instant::now() + Duration::from_secs(5);
                    }
                }
            }
            new_creds = async {
                match discovery.as_mut() {
                    Some(d) => d.next().await,
                    None => std::future::pending().await,
                }
            } => {
                match new_creds {
                    Some(c) => {
                        log::info!("got credentials from the Spotify app for {:?}", c.username);
                        if let Some(s) = spirc.take() {
                            let _ = s.shutdown();
                        }
                        if let Some(t) = spirc_task.take() {
                            tokio::spawn(t);
                        }
                        if !session.is_invalid() {
                            session.shutdown();
                        }
                        creds = Some(c);
                        retry_at = None;
                        backoff = Duration::from_secs(3);
                        connecting = true;
                    }
                    None => {
                        log::warn!("discovery stopped; restarting it");
                        discovery = None;
                        discovery_retry_at = Instant::now() + Duration::from_secs(2);
                    }
                }
            }
            _ = tokio::time::sleep_until(retry_at.unwrap_or_else(far_future)), if retry_at.is_some() => {
                retry_at = None;
                connecting = creds.is_some();
            }
            _ = async {}, if connecting && creds.is_some() => {
                connecting = false;
                shared.emit(Event::Conn(ConnState::Connecting));
                if attempts > 0 || session.is_invalid() {
                    if !session.is_invalid() {
                        session.shutdown();
                    }
                    session = Session::new(session_config.clone(), cache.clone());
                    player.set_session(session.clone());
                }
                attempts += 1;
                let c = creds.clone().unwrap_or_default();
                match Spirc::new(connect_config.clone(), session.clone(), c, player.clone(), mixer.clone()).await {
                    Ok((s, task)) => {
                        let username = session.username();
                        log::info!("connected as {username}");
                        spirc = Some(s);
                        spirc_task = Some(Box::pin(task));
                        backoff = Duration::from_secs(3);
                        // Prefer the reusable credentials Spotify just handed us.
                        if let Some(stored) = cache.as_ref().and_then(|c| c.credentials()) {
                            creds = Some(stored);
                        }
                        shared.emit(Event::Conn(ConnState::Connected { username }));
                    }
                    Err(e) => {
                        log::warn!("connect failed: {e}");
                        if is_auth_error(&e) {
                            let _ = std::fs::remove_file(paths.credentials_dir().join("credentials.json"));
                            creds = None;
                            shared.emit(Event::Conn(ConnState::NeedLogin {
                                device_name: cfg.device_name.clone(),
                                ip: local_ip(),
                            }));
                        } else {
                            shared.emit(Event::Conn(ConnState::Offline {
                                message: format!("Không kết nối được Spotify ({e}). Thử lại sau {}s…", backoff.as_secs()),
                            }));
                            retry_at = Some(Instant::now() + backoff);
                            backoff = (backoff * 2).min(Duration::from_secs(60));
                        }
                    }
                }
            }
            _ = async {
                if let Some(t) = spirc_task.as_mut() {
                    t.await;
                }
            }, if spirc_task.is_some() => {
                spirc_task = None;
                spirc = None;
                active = false;
                log::warn!("Spotify connection lost");
                if creds.is_some() {
                    shared.emit(Event::Conn(ConnState::Offline {
                        message: "Mất kết nối, đang kết nối lại…".into(),
                    }));
                    retry_at = Some(Instant::now() + backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
            Some(ev) = player_events.recv() => {
                match ev {
                    PlayerEvent::Playing { position_ms, .. } => {
                        active = true;
                        shared.emit(Event::Playing { playing: true, position_ms });
                    }
                    PlayerEvent::Paused { position_ms, .. } => {
                        active = true;
                        shared.emit(Event::Playing { playing: false, position_ms });
                    }
                    PlayerEvent::Loading { position_ms, .. } => {
                        active = true;
                        shared.emit(Event::Loading { position_ms });
                    }
                    PlayerEvent::PositionChanged { position_ms, .. }
                    | PlayerEvent::PositionCorrection { position_ms, .. }
                    | PlayerEvent::Seeked { position_ms, .. } => {
                        shared.emit(Event::Position(position_ms));
                    }
                    PlayerEvent::Stopped { .. } => {
                        active = false;
                        shared.emit(Event::Stopped);
                    }
                    PlayerEvent::TrackChanged { audio_item } => {
                        let info = audio_item_info(&audio_item);
                        let uri = info.uri.clone();
                        shared.emit(Event::Track(info));
                        // Album/artist links come from the full metadata.
                        request_meta(&shared, &session, vec![uri]);
                    }
                    PlayerEvent::VolumeChanged { volume: v } => {
                        volume = v;
                        if let Some(c) = cache.as_ref() {
                            c.save_volume(v);
                        }
                        shared.emit(Event::Volume(v));
                    }
                    PlayerEvent::ShuffleChanged { shuffle } => shared.emit(Event::Shuffle(shuffle)),
                    PlayerEvent::RepeatChanged { context, track } => {
                        repeat = if track {
                            Repeat::Track
                        } else if context {
                            Repeat::Context
                        } else {
                            Repeat::Off
                        };
                        shared.emit(Event::Repeat(repeat));
                    }
                    PlayerEvent::Unavailable { .. } => {
                        shared.emit(Event::Toast("Bài hát này không phát được, chuyển bài…".into()));
                    }
                    PlayerEvent::SessionClientChanged { client_name, .. } => {
                        log::info!("controlled by {client_name}");
                    }
                    _ => {}
                }
            }
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { break };
                let connected = spirc.is_some();
                match cmd {
                    Cmd::Shutdown => break,
                    Cmd::LoadPlaylists => {
                        if !connected {
                            shared.emit(Event::Playlists(Err("Chưa kết nối Spotify".into())));
                            continue;
                        }
                        let (s, sh) = (session.clone(), shared.clone());
                        tokio::spawn(async move {
                            let r = library::playlists(&s).await.map_err(|e| e.to_string());
                            sh.emit(Event::Playlists(r));
                        });
                    }
                    Cmd::LoadHome => {
                        if !connected {
                            shared.emit(Event::Home(Err("Chưa kết nối Spotify".into())));
                            continue;
                        }
                        let (s, sh) = (session.clone(), shared.clone());
                        let (file, tz) = (paths.home_feed_file(), cfg.time_zone.clone());
                        tokio::spawn(async move {
                            let r = home::load(&s, &file, &tz).await;
                            if let Err(e) = &r {
                                log::warn!("home feed: {e}");
                            }
                            sh.emit(Event::Home(r));
                        });
                    }
                    Cmd::LoadSource { req, source } => {
                        if !connected {
                            shared.emit(Event::Tracks { req, result: Err("Chưa kết nối Spotify".into()) });
                            continue;
                        }
                        let (s, sh) = (session.clone(), shared.clone());
                        tokio::spawn(async move {
                            let r = library::load_source(&s, source).await.map_err(|e| e.to_string());
                            sh.emit(Event::Tracks { req, result: r });
                        });
                    }
                    Cmd::NeedMeta(uris) => {
                        if connected {
                            request_meta(&shared, &session, uris);
                        }
                    }
                    Cmd::Image { url, size } => {
                        let key = (url.clone(), size);
                        if !shared.images_inflight.lock().unwrap().insert(key.clone()) {
                            continue;
                        }
                        let (s, sh) = (session.clone(), shared.clone());
                        tokio::spawn(async move {
                            let image = {
                                let _slot = sh.image_slots.acquire().await;
                                if let Some(path) = url.strip_prefix("local:art:") {
                                    let path = std::path::PathBuf::from(path);
                                    tokio::task::spawn_blocking(move || {
                                        crate::local::art::load(&path, size).map(Arc::new)
                                    })
                                    .await
                                    .ok()
                                    .flatten()
                                } else {
                                    images::load(s, sh.image_dir.clone(), url.clone(), size).await
                                }
                            };
                            sh.images_inflight.lock().unwrap().remove(&key);
                            sh.emit(Event::Image { url, size, image });
                        });
                    }
                    Cmd::PlayContext { context_uri, index, shuffle } => {
                        let Some(s) = spirc.as_ref() else {
                            shared.emit(Event::Toast("Chưa kết nối Spotify".into()));
                            continue;
                        };
                        let context_options = shuffle.map(|sh| {
                            LoadContextOptions::Options(Options {
                                shuffle: sh,
                                repeat: repeat == Repeat::Context,
                                repeat_track: false,
                            })
                        });
                        let _ = s.activate();
                        let _ = s.load(LoadRequest::from_context_uri(
                            context_uri,
                            LoadRequestOptions {
                                start_playing: true,
                                seek_to: 0,
                                context_options,
                                playing_track: index.map(PlayingTrack::Index),
                            },
                        ));
                        active = true;
                    }
                    Cmd::TogglePlay => {
                        if let Some(s) = spirc.as_ref() {
                            if active {
                                let _ = s.play_pause();
                            } else {
                                let _ = s.transfer(None);
                            }
                        }
                    }
                    Cmd::Next => {
                        if let Some(s) = spirc.as_ref() {
                            let _ = s.next();
                        }
                    }
                    Cmd::Prev => {
                        if let Some(s) = spirc.as_ref() {
                            let _ = s.prev();
                        }
                    }
                    Cmd::SeekTo(ms) => {
                        if let Some(s) = spirc.as_ref() {
                            let _ = s.set_position_ms(ms);
                        }
                    }
                    Cmd::VolumeDelta(delta) => {
                        let v = (volume as i32 + delta * 65535 / 100).clamp(0, 65535) as u16;
                        volume = v;
                        match spirc.as_ref() {
                            Some(s) if active => {
                                let _ = s.set_volume(v);
                            }
                            _ => {
                                mixer.set_volume(v);
                                if let Some(c) = cache.as_ref() {
                                    c.save_volume(v);
                                }
                            }
                        }
                        shared.emit(Event::Volume(v));
                    }
                    Cmd::SetShuffle(on) => {
                        if let Some(s) = spirc.as_ref() {
                            let _ = s.shuffle(on);
                        }
                    }
                    Cmd::SetRepeat(r) => {
                        if let Some(s) = spirc.as_ref() {
                            let _ = s.repeat(r != Repeat::Off);
                            let _ = s.repeat_track(r == Repeat::Track);
                        }
                    }
                    Cmd::Pause => {
                        if let Some(s) = spirc.as_ref() {
                            if active {
                                let _ = s.pause();
                            }
                        }
                    }
                    Cmd::CheckUpdate { manual } => {
                        let url = cfg.update_url.trim().to_string();
                        if url.is_empty() {
                            if manual {
                                shared.emit(Event::Update(crate::update::UpdateState::Failed(
                                    "Chưa cấu hình update_url trong settings.json".into(),
                                )));
                            }
                            continue;
                        }
                        let sh = shared.clone();
                        tokio::spawn(async move {
                            use crate::update::UpdateState;
                            if manual {
                                sh.emit(Event::Update(UpdateState::Checking));
                            }
                            match crate::update::check(&url).await {
                                Ok(Some(info)) => sh.emit(Event::Update(UpdateState::Available(info))),
                                Ok(None) if manual => sh.emit(Event::Update(UpdateState::UpToDate)),
                                Ok(None) => {}
                                Err(e) => {
                                    log::warn!("update check: {e}");
                                    if manual {
                                        sh.emit(Event::Update(UpdateState::Failed(e)));
                                    }
                                }
                            }
                        });
                    }
                    Cmd::InstallUpdate(info) => {
                        let sh = shared.clone();
                        let (app_dir, data_dir) = (paths.app_dir.clone(), paths.data_dir.clone());
                        tokio::spawn(async move {
                            use crate::update::UpdateState;
                            let pkg = crate::update::package_path(&data_dir);
                            let mut last_pct = u64::MAX;
                            let res = crate::update::download(&info, &pkg, |done, total| {
                                let pct = if total > 0 { done * 100 / total } else { 0 };
                                if pct != last_pct {
                                    last_pct = pct;
                                    sh.emit(Event::Update(UpdateState::Downloading { done, total }));
                                }
                            })
                            .await;
                            if let Err(e) = res {
                                sh.emit(Event::Update(UpdateState::Failed(e)));
                                return;
                            }
                            sh.emit(Event::Update(UpdateState::Installing));
                            let version = info.version.clone();
                            let r = tokio::task::spawn_blocking(move || {
                                crate::update::install(&pkg, &app_dir, &data_dir, &version)
                            })
                            .await
                            .unwrap_or_else(|e| Err(e.to_string()));
                            sh.emit(Event::Update(match r {
                                Ok(()) => UpdateState::Ready { version: info.version },
                                Err(e) => UpdateState::Failed(e),
                            }));
                        });
                    }
                    Cmd::TransferHere => {
                        if let Some(s) = spirc.as_ref() {
                            let _ = s.transfer(None);
                            active = true;
                        }
                    }
                    Cmd::Logout => {
                        if let Some(s) = spirc.take() {
                            let _ = s.shutdown();
                        }
                        if let Some(t) = spirc_task.take() {
                            tokio::spawn(t);
                        }
                        if !session.is_invalid() {
                            session.shutdown();
                        }
                        let _ = std::fs::remove_file(paths.credentials_dir().join("credentials.json"));
                        creds = None;
                        retry_at = None;
                        active = false;
                        shared.emit(Event::Conn(ConnState::NeedLogin {
                            device_name: cfg.device_name.clone(),
                            ip: local_ip(),
                        }));
                    }
                }
            }
        }
    }

    log::info!("shutting down backend");
    if let Some(s) = spirc.take() {
        let _ = s.shutdown();
    }
    if let Some(t) = spirc_task.take() {
        let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
    }
    if let Some(d) = discovery.take() {
        let _ = tokio::time::timeout(Duration::from_secs(1), d.shutdown()).await;
    }
    if !session.is_invalid() {
        session.shutdown();
    }
    shared.emit(Event::ShutdownDone);
}

/// Answers from the metadata cache, fetching whatever is missing.
fn request_meta(shared: &Arc<Shared>, session: &Session, uris: Vec<String>) {
    let mut known = Vec::new();
    let mut missing = Vec::new();
    {
        let cache = shared.meta.lock().unwrap();
        let mut pending = shared.meta_pending.lock().unwrap();
        for u in uris {
            if let Some(t) = cache.get(&u) {
                known.push(t.clone());
            } else if pending.insert(u.clone()) {
                missing.push(u);
            }
        }
    }
    if !known.is_empty() {
        shared.emit(Event::Meta(known));
    }
    if missing.is_empty() {
        return;
    }
    let (s, sh) = (session.clone(), shared.clone());
    tokio::spawn(async move {
        let result = library::track_meta(&s, &missing).await;
        {
            let mut pending = sh.meta_pending.lock().unwrap();
            for u in &missing {
                pending.remove(u);
            }
        }
        match result {
            Ok(list) => {
                let mut cache = sh.meta.lock().unwrap();
                for t in &list {
                    cache.insert(t.uri.clone(), t.clone());
                }
                drop(cache);
                sh.emit(Event::Meta(list));
            }
            Err(e) => log::warn!("metadata: {e}"),
        }
    });
}

/// `spoty --home-test`: checks the home feed end to end with the saved account.
pub async fn home_selftest(cfg: Config, paths: Paths) {
    match home::discover_hash().await {
        Ok(h) => println!("web player home hash: {h}"),
        Err(e) => println!("hash lookup failed: {e}"),
    }
    let cache = Cache::new(Some(paths.credentials_dir()), None::<PathBuf>, None, None).ok();
    let Some(creds) = cache.as_ref().and_then(|c| c.credentials()) else {
        println!("no saved Spotify account in {}", paths.credentials_dir().display());
        return;
    };
    let session = Session::new(
        SessionConfig {
            device_id: device_id(&cfg.device_name),
            ..Default::default()
        },
        cache,
    );
    if let Err(e) = session.connect(creds, false).await {
        println!("login failed: {e}");
        return;
    }
    println!("logged in as {}", session.username());
    match home::load(&session, &paths.home_feed_file(), &cfg.time_zone).await {
        Ok(sections) => {
            for s in &sections {
                let names: Vec<&str> = s.items.iter().take(4).map(|i| i.title.as_str()).collect();
                println!("[{}] {} items: {}", s.title, s.items.len(), names.join(" | "));
            }
        }
        Err(e) => println!("home failed: {e}"),
    }
}
