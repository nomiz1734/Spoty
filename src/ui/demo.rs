//! Offline rendering of every screen with sample data (`--screenshots DIR`),
//! plus the launcher icon (`--icon FILE`). Handy for checking the UI on a PC.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::config::{Config, Paths};
use crate::gfx::{self, lerp_color, rgb, Canvas, Fonts, IconCache, Image};
use crate::local::{Library, LocalTrack};
use crate::platform::Battery;
use crate::spotify::{Cmd, ConnState, PlaylistInfo, Repeat, Source, TrackInfo, TrackList};
use crate::spotify::home::{FeedItem, FeedKind, FeedSection};
use crate::update::{UpdateInfo, UpdateState};

use super::widgets::{FeedState, Keyboard, ListState};
use super::{draw, App, Menu, Owner, PickerView, TracksView, View};

fn fake_cover(url: &str, size: u32) -> Image {
    let mut hash: u32 = 2166136261;
    for b in url.bytes() {
        hash = (hash ^ b as u32).wrapping_mul(16777619);
    }
    let a = rgb((hash >> 16) as u8, (hash >> 8) as u8, hash as u8);
    let b = rgb((hash >> 3) as u8 / 2, (hash >> 11) as u8 / 2, (hash >> 19) as u8);
    let n = size as usize;
    let mut c = Canvas::new(n, n);
    c.fill_vgradient(0, 0, n as i32, n as i32, a, b);
    let s = size as f32;
    c.fill_circle(s * 0.5, s * 0.5, s * 0.3, lerp_color(a, rgb(255, 255, 255), 0.3));
    c.fill_circle(s * 0.5, s * 0.5, s * 0.08, b);
    Image { w: n, h: n, px: c.px }
}

fn settle(app: &mut App) {
    for v in app.stack.iter_mut() {
        if let View::Feed(fs) = v {
            fs.settle();
            continue;
        }
        let s = match v {
            View::Home(s) | View::Local(s) => s,
            View::Tracks(tv) => &mut tv.state,
            View::Entries(ev) => &mut ev.state,
            View::Picker(pv) => &mut pv.state,
            _ => continue,
        };
        s.scroll = s.target;
        s.hl = s.sel as f32 * s.row_h;
    }
    if let Some(m) = app.menu.as_mut() {
        m.state.hl = m.state.sel as f32 * m.state.row_h;
    }
    app.bg_from = app.bg_to;
    app.bg_t0 = Instant::now() - std::time::Duration::from_secs(1);
}

fn render(c: &mut Canvas, f: &mut Fonts, ic: &mut IconCache, app: &mut App, dir: &Path, name: &str) {
    // Several passes: the first one requests covers, which we then satisfy.
    for _ in 0..3 {
        draw::frame(c, f, ic, app);
        let reqs = std::mem::take(&mut app.images.requests);
        for r in reqs {
            if let Cmd::Image { url, size } = r {
                let img = fake_cover(&url, size);
                app.images.insert(url, size, Some(Arc::new(img)));
            }
        }
        settle(app);
    }
    let path = dir.join(format!("{name}.png"));
    match gfx::png::save_xrgb(&path, c.w, c.h, &c.px) {
        Ok(()) => println!("wrote {}", path.display()),
        Err(e) => eprintln!("{}: {e}", path.display()),
    }
}

fn demo_library() -> Library {
    let albums = [
        ("Sky Decade", "Sơn Tùng M-TP", "FLAC", 24, 96_000, true),
        ("Truyện Ngắn", "Hà Anh Tuấn", "FLAC", 16, 44_100, true),
        ("22", "MONO", "MP3", 0, 44_100, false),
        ("Hoàng", "Hoàng Thùy Linh", "ALAC", 16, 44_100, true),
        ("Show của Đen", "Đen", "AAC", 0, 44_100, false),
        ("Dreamy Night", "Various Artists", "Opus", 0, 48_000, false),
    ];
    let titles = ["Mở đầu", "Bài hát thứ hai", "Ngày mai", "Chiều nay", "Lối về", "Giấc mơ"];
    let mut tracks = Vec::new();
    for (ai, (album, artist, codec, bits, rate, lossless)) in albums.iter().enumerate() {
        for (ti, t) in titles.iter().enumerate().take(4 + ai % 3) {
            tracks.push(LocalTrack {
                path: format!("/mnt/SDCARD/Music/{album}/{:02} {t}.flac", ti + 1),
                dir: album.to_string(),
                title: format!("{t} ({album})"),
                artist: artist.to_string(),
                album: album.to_string(),
                album_artist: artist.to_string(),
                track: ti as u32 + 1,
                disc: 1,
                duration_ms: 180_000 + (ti as u32 * 17_000),
                codec: codec.to_string(),
                lossless: *lossless,
                rate: *rate,
                bits: *bits,
                channels: 2,
                kbps: if *lossless { 0 } else { 256 },
                size: 8_000_000,
                mtime: 0,
            });
        }
    }
    Library {
        root: "/mnt/SDCARD/Music".into(),
        tracks,
    }
}

pub fn screenshots(mut fonts: Fonts, dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (local_tx, _local_rx) = std::sync::mpsc::channel();
    let paths = Paths {
        app_dir: dir.to_path_buf(),
        data_dir: dir.join("demo-data"),
    };
    let mut app = App::new(Config::default(), paths, cmd_tx, local_tx);
    let mut c = Canvas::new(1024, 768);
    let mut ic = IconCache::default();
    app.battery = Some(Battery {
        percent: 76,
        charging: false,
    });

    app.conn = ConnState::NeedLogin {
        device_name: "TrimUI Brick Pro".into(),
        ip: Some("192.168.1.42".into()),
    };
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "01_login");

    app.conn = ConnState::Connected {
        username: "hoang".into(),
    };
    app.cfg.music_dir = "/mnt/SDCARD/Music".into();
    app.set_library(Arc::new(demo_library()));
    let names = [
        ("Nhạc Việt Chill", "hoang", 128),
        ("Lofi Học Bài", "spotify", 250),
        ("V-Pop Thịnh Hành", "spotify", 50),
        ("Rap Việt Hay Nhất", "hoang", 77),
        ("Acoustic Buổi Sáng", "spotify", 64),
        ("Nhạc Trịnh Công Sơn", "hoang", 91),
    ];
    app.playlists = Some(Ok(names
        .iter()
        .enumerate()
        .map(|(i, (n, o, l))| PlaylistInfo {
            uri: format!("spotify:playlist:demo{i}"),
            name: n.to_string(),
            owner: o.to_string(),
            length: *l,
            cover: Some(format!("demo:pl{i}")),
        })
        .collect()));
    let songs = [
        ("Nơi Này Có Anh", "Sơn Tùng M-TP", "m-tp M-TP", 260_000, false),
        ("Có Chắc Yêu Là Đây", "Sơn Tùng M-TP", "Có Chắc Yêu Là Đây", 202_000, false),
        ("Ngày Mai Người Ta Lấy Chồng", "Thành Đạt", "Single", 285_000, false),
        ("Tháng Tư Là Lời Nói Dối Của Em", "Hà Anh Tuấn", "Truyện Ngắn", 301_000, false),
        ("Chúng Ta Của Hiện Tại", "Sơn Tùng M-TP", "Single", 301_000, false),
        ("Bước Qua Nhau", "Vũ.", "Bước Qua Nhau", 257_000, false),
        ("Thằng Điên", "JustaTee, Phương Ly", "Single", 285_000, true),
        ("See Tình", "Hoàng Thùy Linh", "LINK", 185_000, false),
    ];
    let mut uris = Vec::new();
    for (i, (n, a, al, d, e)) in songs.iter().enumerate() {
        let uri = format!("spotify:track:demo{i}");
        uris.push(uri.clone());
        app.meta.insert(
            uri.clone(),
            TrackInfo {
                uri,
                name: n.to_string(),
                artists: a.to_string(),
                artist_uri: Some("spotify:artist:demo".into()),
                album: al.to_string(),
                album_uri: Some("spotify:album:demo".into()),
                duration_ms: *d,
                cover_small: Some(format!("demo:al{}", i % 4)),
                cover_large: Some(format!("demo:al{}", i % 4)),
                explicit: *e,
                quality: None,
            },
        );
    }
    app.pb.track = app.meta.get(&uris[1]).cloned();
    app.pb.playing = true;
    app.pb.active = true;
    app.pb.pos_ms = 83_000;
    app.pb.pos_at = Instant::now();
    app.pb.volume = (0.7 * 65535.0) as u16;
    app.pb.shuffle = true;
    app.pb.repeat = Repeat::Context;

    let mut home = ListState::new();
    home.sel = 2;
    app.stack = vec![View::Home(home)];
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "02_home");

    let card = |uri: &str, kind: FeedKind, title: &str, sub: &str| FeedItem {
        uri: uri.into(),
        kind,
        title: title.into(),
        subtitle: sub.into(),
        image: Some(format!("demo:{uri}")),
    };
    app.feed = Some(Ok(vec![
        FeedSection {
            title: "Được đề xuất cho hôm nay".into(),
            items: vec![
                card("spotify:album:a1", FeedKind::Album, "Buông", "Album • Hngle"),
                card("spotify:album:a2", FeedKind::Album, "Và Thế Giới Đã Mất Đi Một Người Cô Đơn", "Album • marzuz, Changg"),
                card("spotify:album:a3", FeedKind::Album, "NỔ", "Album • Wren Evans"),
                card("spotify:playlist:p1", FeedKind::Playlist, "Hip-hop Việt", "HIEUTHUHAI, Low G, B Ray"),
                card("spotify:playlist:p2", FeedKind::Playlist, "V-Pop Không Thể Thiếu", "Dangrangto, Hngle, Low G"),
            ],
        },
        FeedSection {
            title: "Gần đây".into(),
            items: vec![
                card("spotify:playlist:d3", FeedKind::Playlist, "Daily Mix 3", "Danh sách phát • Spotify"),
                card("spotify:artist:n1", FeedKind::Artist, "Nhuộm Collective", "Nghệ sĩ"),
                card("spotify:album:t1", FeedKind::Album, "trái tim băng bó", "Album • Dangrangto"),
                card("spotify:playlist:rc", FeedKind::Playlist, "RapCaviar", "Drake, Travis Scott"),
            ],
        },
        FeedSection {
            title: "Dựa trên nhạc bạn nghe gần đây".into(),
            items: vec![card("spotify:playlist:x1", FeedKind::Playlist, "Today's Top Hits", "Ariana Grande, Olivia Rodrigo")],
        },
    ]));
    app.stack.push(View::Feed(FeedState::new()));
    if let Some(View::Feed(fs)) = app.stack.last_mut() {
        fs.row = 0;
        fs.cols = vec![1, 0, 0];
    }
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "02b_feed");
    app.stack.pop();

    let mut state = ListState::new();
    state.sel = 3;
    app.stack.push(View::Tracks(TracksView {
        req: 1,
        source: Source::Playlist {
            uri: "spotify:playlist:demo0".into(),
            name: "Nhạc Việt Chill".into(),
        },
        cover: Some("demo:pl0".into()),
        list: Some(Ok(TrackList {
            context_uri: "spotify:playlist:demo0".into(),
            uris,
        })),
        state,
    }));
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "03_tracks");

    app.stack.push(View::NowPlaying);
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "04_now_playing");

    app.stack.pop();
    app.open_menu();
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "05_menu");
    app.menu = None::<Menu>;

    app.stack.truncate(1);
    let mut kb = Keyboard::new(true);
    for k in "nhacj lanhf chuwa tinhf".chars() {
        if k == ' ' {
            kb.space();
        } else {
            kb.type_char(k);
        }
    }
    kb.row = 2;
    kb.col = 3;
    app.stack.push(View::Search(kb.clone()));
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "06_search");
    kb.row = 0;
    kb.move_by(0, -1);
    app.stack.push(View::Search(kb));
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "06b_search_clear");
    app.stack.pop();

    // Local music.
    app.stack.truncate(1);
    app.open_local_home();
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "07_local_home");

    app.open_entries(1);
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "08_local_albums");

    let ix = app.local_index().unwrap();
    let album = ix
        .albums
        .iter()
        .find(|g| g.title == "Sky Decade")
        .cloned()
        .unwrap();
    let cover = album.cover_path.as_ref().map(|p| format!("local:art:{p}"));
    app.open_local_tracks(album.title.clone(), "ALBUM", &album.tracks, cover);
    // Pretend a hi-res FLAC from that album is playing.
    app.switch_owner(Owner::Local);
    app.pb.track = app.meta.get(&ix.lib.tracks[album.tracks[1]].uri()).cloned();
    app.pb.playing = true;
    app.pb.active = true;
    app.pb.pos_ms = 47_000;
    app.pb.pos_at = Instant::now();
    app.pb.volume = 65535;
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "09_local_tracks");

    app.stack.push(View::NowPlaying);
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "10_local_now_playing");
    app.stack.pop();

    let root = std::env::current_dir().unwrap_or_default();
    app.stack.push(View::Picker(PickerView::open(root)));
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "11_folder_picker");
    app.stack.pop();

    let info = UpdateInfo {
        version: "0.3.0".into(),
        notes: "• Thêm nhạc trên máy (FLAC, ALAC, WAV, MP3, AAC, Opus…)\n• Cập nhật OTA\n• Logo mới"
            .into(),
        url: "https://example.com/spoty-update.tar.gz".into(),
        sha256: String::new(),
        size: 5_400_000,
    };
    app.update.info = Some(info.clone());
    app.update.state = Some(UpdateState::Available(info));
    app.update.dialog = true;
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "12_update_available");
    app.update.state = Some(UpdateState::Downloading {
        done: 2_300_000,
        total: 5_400_000,
    });
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "13_update_downloading");
    app.update.dialog = false;

    app.stack.truncate(1);
    app.playlists = None;
    app.conn = ConnState::Connecting;
    app.toast("Phát ngẫu nhiên");
    render(&mut c, &mut fonts, &mut ic, &mut app, dir, "14_loading");

    // Rough frame-time benchmark while scrolling a long list.
    app.toast = None;
    let all = app.local_index().unwrap().all.clone();
    app.open_local_tracks("Tất cả".into(), "TRÊN MÁY", &all, None);
    let t0 = Instant::now();
    let frames = 300;
    for i in 0..frames {
        if let Some(View::Tracks(tv)) = app.stack.last_mut() {
            tv.state.scroll = (i % 40) as f32 * 3.0;
            tv.state.hl = tv.state.scroll + 50.0;
        }
        draw::frame(&mut c, &mut fonts, &mut ic, &mut app);
    }
    println!(
        "track list: {:.2} ms/frame",
        t0.elapsed().as_secs_f64() * 1000.0 / frames as f64
    );
}

/// The launcher icon (generated from the brand artwork by tools/make_brand.py).
pub fn write_icon(path: &Path, _size: u32) {
    match std::fs::write(path, include_bytes!("../../package/stock/icon.png")) {
        Ok(()) => println!("wrote {}", path.display()),
        Err(e) => eprintln!("{}: {e}", path.display()),
    }
}
