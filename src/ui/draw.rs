//! Rendering of every screen. Only called when something changed.

use std::time::Instant;

use crate::gfx::{lerp_color, rgb, Canvas, Color, Fonts, Icon, IconCache, Rect, Weight};
use crate::spotify::{ConnState, Repeat, Source, TrackInfo};

use super::theme::*;
use super::widgets::{Key, Keyboard, ListState, BOTTOM_ROW, KEY_COLS, KEY_ROWS};
use super::{App, View};

struct Ctx<'a> {
    c: &'a mut Canvas,
    f: &'a mut Fonts,
    ic: &'a mut IconCache,
}

pub fn frame(c: &mut Canvas, f: &mut Fonts, ic: &mut IconCache, app: &mut App) {
    let mut x = Ctx { c, f, ic };
    x.c.reset_clip();
    x.c.clear(BG);
    if app.on_login_screen() {
        draw_login(&mut x, app);
    } else if matches!(app.stack.last(), Some(View::NowPlaying)) {
        draw_now_playing(&mut x, app);
    } else {
        // Take the view out so it can be drawn while `app` is borrowed mutably.
        // The placeholder is a Home view, which keeps `show_mini` correct.
        let top = app.stack.len() - 1;
        let mut view = std::mem::replace(&mut app.stack[top], View::Home(ListState::new()));
        match &mut view {
            View::Home(state) => draw_home(&mut x, app, state),
            View::Tracks(tv) => draw_tracks(&mut x, app, tv),
            View::Search(kb) => draw_search(&mut x, app, kb),
            View::Local(state) => draw_local_home(&mut x, app, state),
            View::Feed(fs) => draw_feed(&mut x, app, fs),
            View::Entries(ev) => draw_entries(&mut x, app, ev),
            View::Picker(pv) => draw_picker(&mut x, app, pv),
            View::NowPlaying => {}
        }
        app.stack[top] = view;
    }
    if app.menu.is_some() {
        draw_menu(&mut x, app);
    }
    if app.update.dialog {
        draw_update(&mut x, app);
    }
    if let Some((msg, _)) = app.toast.clone() {
        draw_toast(&mut x, &msg);
    }
}

// ---------------------------------------------------------------- helpers

fn fmt_time(ms: u32) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

fn clock() -> String {
    #[cfg(unix)]
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
    #[cfg(not(unix))]
    {
        let s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60)
    }
}

fn anim_time() -> f32 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f32()
}

fn spinner(x: &mut Ctx, app: &mut App, cx: f32, cy: f32, r: f32, color: Color) {
    let t = anim_time();
    let n = 10;
    let head = ((t * 12.0) as usize) % n;
    for i in 0..n {
        let ang = i as f32 / n as f32 * std::f32::consts::TAU;
        let age = (head + n - i) % n;
        let a = 1.0 - age as f32 / n as f32;
        let col = lerp_color(BG, color, 0.15 + 0.85 * a);
        x.c.fill_circle(cx + ang.sin() * r, cy - ang.cos() * r, r * 0.16, col);
    }
    app.anim_request = true;
}

fn cover(x: &mut Ctx, app: &mut App, url: Option<&str>, px: i32, py: i32, size: u32, radius: i32) {
    if let Some(u) = url {
        if let Some((img, _)) = app.images.get(u, size) {
            x.c.blit_rounded(px, py, &img, radius);
            return;
        }
    }
    x.c.fill_rounded(px, py, size as i32, size as i32, radius, PLACEHOLDER);
    let is = (size as f32 * 0.45) as u32;
    let o = (size - is) as i32 / 2;
    x.ic.draw(x.c, Icon::Note, px + o, py + o, is, DIM);
}

fn liked_tile(x: &mut Ctx, px: i32, py: i32, size: i32) {
    x.c.fill_vgradient(px, py, size, size, LIKED_TOP, LIKED_BOTTOM);
    let is = (size as f32 * 0.5) as u32;
    let o = (size - is as i32) / 2;
    x.ic.draw(x.c, Icon::Heart, px + o, py + o, is, TEXT);
}

fn icon_tile(x: &mut Ctx, icon: Icon, px: i32, py: i32, size: i32, bg: Color, fg: Color) {
    x.c.fill_rounded(px, py, size, size, 6, bg);
    let is = (size as f32 * 0.5) as u32;
    let o = (size - is as i32) / 2;
    x.ic.draw(x.c, icon, px + o, py + o, is, fg);
}

fn show_mini(app: &App) -> bool {
    app.pb.track.is_some() && !matches!(app.stack.last(), Some(View::NowPlaying))
}

/// Content area between the top bar and the bottom widgets.
fn content_rect(x: &Ctx, app: &App) -> Rect {
    let bottom = x.c.h as i32 - HINTS_H - if show_mini(app) { MINI_H } else { 0 };
    Rect::new(0, TOP_BAR_H, x.c.w as i32, bottom - TOP_BAR_H)
}

fn top_bar(x: &mut Ctx, app: &mut App, title: &str, opaque: bool) {
    let w = x.c.w as i32;
    if opaque {
        x.c.fill_rect(0, 0, w, TOP_BAR_H, BG);
    }
    // Right side: connection, battery, clock.
    let mut right = w - SIDE_PAD;
    let time = clock();
    let tw = x.f.measure(&time, 22.0, Weight::Bold) as i32;
    x.f.draw(x.c, right - tw, 16, &time, 22.0, Weight::Bold, TEXT);
    right -= tw + 18;
    if let Some(b) = app.battery {
        let bw = 34;
        let by = 21;
        let col = if b.charging {
            ACCENT
        } else if b.percent < 15 {
            ERROR
        } else {
            TEXT
        };
        x.c.fill_rounded(right - bw, by, bw, 18, 4, SUBTEXT);
        x.c.fill_rounded(right - bw + 2, by + 2, bw - 4, 14, 3, BG);
        let fill = ((bw - 6) as f32 * b.percent as f32 / 100.0).round() as i32;
        x.c.fill_rect(right - bw + 3, by + 3, fill.max(1), 12, col);
        x.c.fill_rect(right, by + 5, 3, 8, SUBTEXT);
        right -= bw + 8;
        let pct = format!("{}%", b.percent);
        let pw = x.f.measure(&pct, 18.0, Weight::Regular) as i32;
        x.f.draw(x.c, right - pw, 18, &pct, 18.0, Weight::Regular, SUBTEXT);
        right -= pw + 18;
    }
    let (dot, label) = match &app.conn {
        ConnState::Connected { .. } => (ACCENT, String::new()),
        ConnState::Starting | ConnState::Connecting => (WARN, "Đang kết nối…".into()),
        ConnState::Offline { .. } => (ERROR, "Mất kết nối".into()),
        ConnState::NeedLogin { .. } => (DIM, String::new()),
    };
    if !label.is_empty() {
        let lw = x.f.measure(&label, 18.0, Weight::Regular) as i32;
        x.f.draw(x.c, right - lw, 18, &label, 18.0, Weight::Regular, SUBTEXT);
        right -= lw + 10;
    }
    x.c.fill_circle(right as f32 - 6.0, 30.0, 6.0, dot);
    right -= 24;
    // Title on the left.
    let max = right - SIDE_PAD - 12;
    x.f.draw_fit(x.c, SIDE_PAD, 12, max, title, 28.0, Weight::Bold, TEXT);
}

fn hints(x: &mut Ctx, items: &[(&str, &str)]) {
    let w = x.c.w as i32;
    let y = x.c.h as i32 - HINTS_H;
    x.c.fill_rect(0, y, w, HINTS_H, SURFACE);
    let mut px = SIDE_PAD;
    let cy = y + HINTS_H / 2;
    for (btn, label) in items {
        let pad_icon = match *btn {
            "LR" => Some(Icon::PadLR),
            "UD" => Some(Icon::PadUD),
            _ => None,
        };
        let bw = if pad_icon.is_some() {
            40
        } else if btn.chars().count() == 1 {
            28
        } else {
            x.f.measure(btn, 14.0, Weight::Bold) as i32 + 18
        };
        x.c.fill_rounded(px, cy - 14, bw, 28, 14, HIGHLIGHT);
        if let Some(icon) = pad_icon {
            x.ic.draw(x.c, icon, px + (bw - 20) / 2, cy - 10, 20, TEXT);
        } else {
            let size = if btn.chars().count() == 1 { 17.0 } else { 14.0 };
            let tw = x.f.measure(btn, size, Weight::Bold) as i32;
            let th = x.f.line_height(size) as i32;
            x.f.draw(x.c, px + (bw - tw) / 2, cy - th / 2, btn, size, Weight::Bold, TEXT);
        }
        px += bw + 8;
        let lw = x.f.draw(x.c, px, cy - 12, label, 18.0, Weight::Regular, SUBTEXT) as i32;
        px += lw + 22;
        if px > w - 60 {
            break;
        }
    }
}

fn mini_player(x: &mut Ctx, app: &mut App) {
    let Some(t) = app.pb.track.clone() else {
        return;
    };
    let w = x.c.w as i32;
    let y = x.c.h as i32 - HINTS_H - MINI_H;
    x.c.fill_rect(0, y, w, MINI_H, ELEVATED);
    // Progress line along the top edge.
    let pos = app.pb.position();
    if t.duration_ms > 0 {
        let pw = (w as f32 * pos as f32 / t.duration_ms as f32) as i32;
        x.c.fill_rect(0, y, w, 3, HIGHLIGHT);
        x.c.fill_rect(0, y, pw, 3, if app.pb.active { ACCENT } else { SUBTEXT });
    }
    cover(x, app, t.cover_small.as_deref(), SIDE_PAD, y + 12, MINI_THUMB, 6);
    let tx = SIDE_PAD + MINI_THUMB as i32 + 16;
    let max = w - tx - 150;
    x.f.draw_fit(x.c, tx, y + 14, max, &t.name, 21.0, Weight::Bold, TEXT);
    let sub = if app.pb.active {
        t.artists.clone()
    } else {
        format!("{} • đang dừng", t.artists)
    };
    x.f.draw_fit(x.c, tx, y + 44, max, &sub, 18.0, Weight::Regular, SUBTEXT);
    let icon = if app.pb.playing { Icon::Pause } else { Icon::Play };
    if app.pb.loading {
        spinner(x, app, (w - SIDE_PAD - 22) as f32, (y + MINI_H / 2) as f32, 16.0, TEXT);
    } else {
        x.ic.draw(x.c, icon, w - SIDE_PAD - 40, y + MINI_H / 2 - 20, 40, TEXT);
    }
}

/// Draws the highlight bar and the scrollbar of a list; returns the y of row 0.
fn list_frame(x: &mut Ctx, state: &ListState, area: Rect, len: usize) -> f32 {
    let y0 = area.y as f32 - state.scroll;
    if len > 0 {
        let hy = (y0 + state.hl).round() as i32;
        x.c.set_clip(area);
        x.c.fill_rounded(12, hy + 2, x.c.w as i32 - 24, state.row_h as i32 - 4, 10, HIGHLIGHT);
        x.c.reset_clip();
    }
    let total = len as f32 * state.row_h;
    if total > area.h as f32 + 1.0 {
        let bar_h = (area.h as f32 * area.h as f32 / total).max(30.0);
        let max_scroll = total - area.h as f32;
        let by = area.y as f32 + (area.h as f32 - bar_h) * (state.scroll / max_scroll).clamp(0.0, 1.0);
        x.c.fill_rounded(x.c.w as i32 - 8, by as i32, 4, bar_h as i32, 2, DIM);
    }
    y0
}

// ---------------------------------------------------------------- screens

fn draw_login(x: &mut Ctx, app: &mut App) {
    let w = x.c.w as i32;
    let cx = w / 2;
    top_bar(x, app, "", true);
    if let Some(logo) = app.logos.big.as_ref() {
        x.c.blit_rgba(cx - logo.w as i32 / 2, 44, logo);
    }
    x.f.draw_centered(x.c, cx, 222, "Spoty", 52.0, Weight::Bold, TEXT);
    x.f.draw_centered(
        x.c,
        cx,
        298,
        "Đăng nhập bằng Spotify Connect",
        26.0,
        Weight::Regular,
        SUBTEXT,
    );
    let (name, ip) = match &app.conn {
        ConnState::NeedLogin { device_name, ip } => (device_name.clone(), ip.clone()),
        _ => (app.cfg.device_name.clone(), None),
    };
    let bx = 110;
    let by = 356;
    let bw = w - 2 * bx;
    x.c.fill_rounded(bx, by, bw, 236, 18, ELEVATED);
    let steps = [
        "Kết nối máy này và điện thoại vào cùng một mạng Wi-Fi.".to_string(),
        "Mở ứng dụng Spotify trên điện thoại (cần tài khoản Premium).".to_string(),
        format!("Bấm biểu tượng thiết bị và chọn \"{name}\"."),
        "Spoty sẽ tự đăng nhập và ghi nhớ tài khoản cho lần sau.".to_string(),
    ];
    for (i, s) in steps.iter().enumerate() {
        let y = by + 26 + i as i32 * 52;
        x.c.fill_circle((bx + 42) as f32, (y + 16) as f32, 17.0, ACCENT);
        let n = format!("{}", i + 1);
        x.f.draw_centered(x.c, bx + 42, y + 1, &n, 22.0, Weight::Bold, BG);
        x.f.draw_fit(x.c, bx + 76, y, bw - 100, s, 23.0, Weight::Regular, TEXT);
    }
    let status = match ip {
        Some(ip) => format!("Đang chờ đăng nhập…  •  IP: {ip}"),
        None => "Chưa có Wi-Fi — hãy bật Wi-Fi trong cài đặt của máy".into(),
    };
    let col = if status.starts_with("Chưa") { WARN } else { SUBTEXT };
    x.f.draw_centered(x.c, cx, 624, &status, 21.0, Weight::Regular, col);
    spinner(x, app, (cx) as f32, 690.0, 14.0, ACCENT);
    hints(x, &[("X", "Nghe nhạc trên máy"), ("Y", "Đang phát"), ("MENU", "Tùy chọn")]);
}

fn draw_home(x: &mut Ctx, app: &mut App, state: &mut ListState) {
    let title = match app.username() {
        Some(_) => "Thư viện",
        None => "Spoty",
    };
    let area = content_rect(x, app);
    let len = app.home_len();
    state.set_geometry(HOME_ROW_H as f32, area.h as f32, len);
    let y0 = list_frame(x, state, area, len);
    x.c.set_clip(area);
    let w = x.c.w as i32;
    let playlists = app.playlists.take();
    for i in state.visible(len) {
        let ry = (y0 + i as f32 * HOME_ROW_H as f32).round() as i32;
        let ty = ry + (HOME_ROW_H - HOME_THUMB as i32) / 2;
        let tx = SIDE_PAD + 4;
        let text_x = tx + HOME_THUMB as i32 + 18;
        let max = w - text_x - SIDE_PAD - 10;
        let (title_s, sub, title_col): (String, String, Color) = match i {
            0 => {
                icon_tile(x, Icon::Home, tx, ty, HOME_THUMB as i32, rgb(0x1a, 0x4d, 0x33), ACCENT);
                let sub = match &app.feed {
                    Some(Ok(s)) => {
                        let names: Vec<&str> = s.iter().take(3).map(|x| x.title.as_str()).collect();
                        names.join(" • ")
                    }
                    _ => "Đề xuất, mix hằng ngày, nghe gần đây".into(),
                };
                ("Dành cho bạn".into(), sub, TEXT)
            }
            1 => {
                liked_tile(x, tx, ty, HOME_THUMB as i32);
                ("Bài hát đã thích".into(), "Playlist • Tự động".into(), TEXT)
            }
            2 => {
                icon_tile(x, Icon::Search, tx, ty, HOME_THUMB as i32, ELEVATED, TEXT);
                ("Tìm kiếm".into(), "Tìm bài hát, nghệ sĩ trên Spotify".into(), TEXT)
            }
            3 => {
                icon_tile(x, Icon::Folder, tx, ty, HOME_THUMB as i32, rgb(0x1f, 0x3b, 0x5c), TEXT);
                let sub = match (&app.local.index, app.local.scanning) {
                    (_, Some((d, t))) if t > 0 => format!("Đang quét… {d}/{t}"),
                    (Some(ix), _) => format!(
                        "{} bài • {} album • Lossless & Lossy",
                        ix.lib.tracks.len(),
                        ix.albums.len()
                    ),
                    (None, _) if app.cfg.music_dir.is_empty() => "Chọn thư mục để quét file nhạc".into(),
                    (None, _) => "Đang đọc thư viện…".into(),
                };
                ("Nhạc trên máy".into(), sub, TEXT)
            }
            n => match &playlists {
                Some(Ok(list)) => {
                    let p = &list[n - 4];
                    cover(x, app, p.cover.as_deref(), tx, ty, HOME_THUMB, 6);
                    let owner = match app.username() {
                        Some(u) if u == p.owner => "của bạn".to_string(),
                        _ if p.owner.is_empty() => String::new(),
                        _ => format!("của {}", p.owner),
                    };
                    let mut sub = String::from("Playlist");
                    if !owner.is_empty() {
                        sub += &format!(" • {owner}");
                    }
                    if p.length > 0 {
                        sub += &format!(" • {} bài", p.length);
                    }
                    (p.name.clone(), sub, TEXT)
                }
                Some(Err(e)) => {
                    icon_tile(x, Icon::Library, tx, ty, HOME_THUMB as i32, ELEVATED, ERROR);
                    ("Không tải được playlist — nhấn A để thử lại".into(), e.clone(), ERROR)
                }
                None => {
                    let (msg, sub) = match &app.conn {
                        ConnState::Offline { message } => {
                            ("Đang chờ kết nối lại…".to_string(), message.clone())
                        }
                        ConnState::Connected { .. } => {
                            ("Đang tải playlist…".into(), String::new())
                        }
                        _ => ("Đang kết nối Spotify…".into(), String::new()),
                    };
                    let c = (tx + HOME_THUMB as i32 / 2) as f32;
                    spinner(x, app, c, (ty + HOME_THUMB as i32 / 2) as f32, 18.0, SUBTEXT);
                    (msg, sub, SUBTEXT)
                }
            },
        };
        let has_sub = !sub.is_empty();
        let title_y = if has_sub { ry + 14 } else { ry + 24 };
        x.f.draw_fit(x.c, text_x, title_y, max, &title_s, 23.0, Weight::Bold, title_col);
        if has_sub {
            x.f.draw_fit(x.c, text_x, ry + 45, max, &sub, 18.0, Weight::Regular, SUBTEXT);
        }
    }
    app.playlists = playlists;
    x.c.reset_clip();
    top_bar(x, app, title, true);
    if show_mini(app) {
        mini_player(x, app);
    }
    hints(
        x,
        &[("A", "Mở"), ("X", "Phát ngẫu nhiên"), ("Y", "Đang phát"), ("MENU", "Tùy chọn")],
    );
}

fn draw_tracks(x: &mut Ctx, app: &mut App, tv: &mut super::TracksView) {
    let area_full = content_rect(x, app);
    let w = x.c.w as i32;
    let header_h = 104;
    // Header.
    let hy = area_full.y + 12;
    let hsize = 80;
    match &tv.source {
        Source::Liked => liked_tile(x, SIDE_PAD, hy, hsize),
        Source::Search(_) => icon_tile(x, Icon::Search, SIDE_PAD, hy, hsize, ELEVATED, TEXT),
        Source::Local { .. } if tv.cover.is_none() => {
            icon_tile(x, Icon::Note, SIDE_PAD, hy, hsize, rgb(0x1f, 0x3b, 0x5c), TEXT)
        }
        _ => {
            let url = tv.cover.clone().or_else(|| {
                tv.list
                    .as_ref()
                    .and_then(|l| l.as_ref().ok())
                    .and_then(|l| l.uris.first())
                    .and_then(|u| app.meta.get(u))
                    .and_then(|t| t.cover_small.clone())
            });
            if matches!(tv.source, Source::Artist { .. }) && url.is_none() {
                icon_tile(x, Icon::Person, SIDE_PAD, hy, hsize, ELEVATED, TEXT);
            } else {
                cover(x, app, url.as_deref(), SIDE_PAD, hy, hsize as u32, 8);
            }
        }
    }
    let kind = match &tv.source {
        Source::Liked | Source::Playlist { .. } => "PLAYLIST",
        Source::Album { .. } => "ALBUM",
        Source::Artist { .. } => "NGHỆ SĨ",
        Source::Search(_) => "TÌM KIẾM",
        Source::Local { kind, .. } => kind,
    };
    let tx = SIDE_PAD + hsize + 20;
    x.f.draw(x.c, tx, hy + 2, kind, 15.0, Weight::Bold, SUBTEXT);
    x.f.draw_fit(x.c, tx, hy + 20, w - tx - SIDE_PAD, &tv.source.title(), 32.0, Weight::Bold, TEXT);
    let count = match &tv.list {
        Some(Ok(l)) => format!("{} bài hát", l.uris.len()),
        Some(Err(_)) => "Lỗi tải danh sách".into(),
        None => "Đang tải…".into(),
    };
    x.f.draw(x.c, tx, hy + 60, &count, 18.0, Weight::Regular, SUBTEXT);

    let area = Rect::new(area_full.x, area_full.y + header_h, area_full.w, area_full.h - header_h);
    x.c.fill_rect(SIDE_PAD, area.y - 1, w - 2 * SIDE_PAD, 1, HIGHLIGHT);
    match &tv.list {
        None => {
            let cy = (area.y + area.h / 2) as f32;
            spinner(x, app, (w / 2) as f32, cy, 26.0, SUBTEXT);
        }
        Some(Err(e)) => {
            x.f.draw_centered(x.c, w / 2, area.y + 60, "Không tải được danh sách bài hát", 24.0, Weight::Bold, ERROR);
            let e = e.clone();
            x.f.draw_centered(x.c, w / 2, area.y + 100, &e, 18.0, Weight::Regular, SUBTEXT);
            x.f.draw_centered(x.c, w / 2, area.y + 140, "Nhấn A để thử lại", 20.0, Weight::Regular, TEXT);
        }
        Some(Ok(list)) => {
            let len = list.uris.len();
            if len == 0 {
                x.f.draw_centered(x.c, w / 2, area.y + 60, "Không có bài hát nào", 24.0, Weight::Regular, SUBTEXT);
            }
            tv.state.set_geometry(TRACK_ROW_H as f32, area.h as f32, len);
            let y0 = list_frame(x, &tv.state, area, len);
            let playing_uri = app.pb.track.as_ref().map(|t| t.uri.clone());
            // Prefetch metadata a little beyond the visible rows.
            let vis = tv.state.visible(len);
            let pre = vis.start.saturating_sub(10)..(vis.end + 25).min(len);
            for u in &list.uris[pre] {
                app.want_meta(u);
            }
            x.c.set_clip(area);
            for i in vis {
                let uri = &list.uris[i];
                let ry = (y0 + i as f32 * TRACK_ROW_H as f32).round() as i32;
                let thumb_y = ry + (TRACK_ROW_H - TRACK_THUMB as i32) / 2;
                let tx = SIDE_PAD + 4;
                let text_x = tx + TRACK_THUMB as i32 + 16;
                let is_current = playing_uri.as_deref() == Some(uri.as_str());
                let meta: Option<TrackInfo> = app.meta.get(uri).cloned();
                match &meta {
                    Some(t) => {
                        cover(x, app, t.cover_small.as_deref(), tx, thumb_y, TRACK_THUMB, 4);
                        let right_w = 90;
                        let max = w - text_x - SIDE_PAD - right_w;
                        let col = if is_current { ACCENT } else { TEXT };
                        x.f.draw_fit(x.c, text_x, ry + 9, max, &t.name, 21.0, Weight::Regular, col);
                        let mut ax = text_x;
                        if let Some(q) = t.quality.as_deref() {
                            ax += quality_pill(x, ax, ry + 41, q, true);
                        }
                        if t.explicit {
                            x.c.fill_rounded(ax, ry + 42, 18, 18, 3, SUBTEXT);
                            x.f.draw_centered(x.c, ax + 9, ry + 41, "E", 13.0, Weight::Bold, BG);
                            ax += 24;
                        }
                        x.f.draw_fit(x.c, ax, ry + 38, max - (ax - text_x), &t.artists, 17.0, Weight::Regular, SUBTEXT);
                        let d = fmt_time(t.duration_ms);
                        x.f.draw_right(x.c, w - SIDE_PAD - 12, ry + 22, &d, 18.0, Weight::Regular, SUBTEXT);
                        if is_current {
                            let ic = if app.pb.playing { Icon::Speaker } else { Icon::Pause };
                            x.ic.draw(x.c, ic, w - SIDE_PAD - 90, ry + 22, 22, ACCENT);
                        }
                    }
                    None => {
                        x.c.fill_rounded(tx, thumb_y, TRACK_THUMB as i32, TRACK_THUMB as i32, 4, PLACEHOLDER);
                        x.c.fill_rounded(text_x, ry + 16, 260, 14, 7, PLACEHOLDER);
                        x.c.fill_rounded(text_x, ry + 42, 160, 12, 6, SURFACE);
                    }
                }
            }
            x.c.reset_clip();
        }
    }
    top_bar(x, app, "", true);
    if show_mini(app) {
        mini_player(x, app);
    }
    hints(
        x,
        &[("A", "Phát"), ("X", "Ngẫu nhiên"), ("Y", "Đang phát"), ("B", "Quay lại"), ("START", "Tùy chọn")],
    );
}

fn draw_now_playing(x: &mut Ctx, app: &mut App) {
    let w = x.c.w as i32;
    let h = x.c.h as i32;
    let Some(t) = app.pb.track.clone() else {
        top_bar(x, app, "Đang phát", true);
        x.f.draw_centered(x.c, w / 2, h / 2 - 20, "Chưa có bài nào đang phát", 26.0, Weight::Regular, SUBTEXT);
        hints(x, &[("B", "Quay lại")]);
        return;
    };
    let meta = app.meta.get(&t.uri).cloned();
    let large = t
        .cover_large
        .clone()
        .or_else(|| meta.as_ref().and_then(|m| m.cover_large.clone()));

    // Background: gradient from the cover's colour, cross-faded when it changes.
    let accent = large
        .as_deref()
        .and_then(|u| app.images.get(u, NOW_COVER))
        .map(|(_, c)| c);
    if let Some(a) = accent {
        if a != app.bg_to {
            app.bg_from = current_bg(app);
            app.bg_to = a;
            app.bg_t0 = Instant::now();
        }
    }
    let bg = current_bg(app);
    if bg != app.bg_to {
        app.anim_request = true;
    }
    let content_h = h - HINTS_H;
    x.c.fill_vgradient(0, 0, w, content_h * 3 / 4, bg, lerp_color(bg, BG, 0.85));
    x.c.fill_rect(0, content_h * 3 / 4, w, content_h - content_h * 3 / 4, lerp_color(bg, BG, 0.85));
    top_bar(x, app, "Đang phát", false);

    // Cover.
    let cs = NOW_COVER as i32;
    let cx0 = 52;
    let cy0 = 108;
    cover(x, app, large.as_deref(), cx0, cy0, NOW_COVER, 10);

    // Info column.
    let ix = cx0 + cs + 44;
    let iw = w - ix - SIDE_PAD - 8;
    let mut y = cy0 + 6;
    let lines = x.f.wrap(&t.name, 36.0, Weight::Bold, iw as f32, 2);
    for l in &lines {
        x.f.draw(x.c, ix, y, l, 36.0, Weight::Bold, TEXT);
        y += 46;
    }
    y += 4;
    let artist_lines = x.f.wrap(&t.artists, 24.0, Weight::Regular, iw as f32, 2);
    for l in &artist_lines {
        x.f.draw(x.c, ix, y, l, 24.0, Weight::Regular, SUBTEXT);
        y += 32;
    }
    if !t.album.is_empty() {
        x.f.draw_fit(x.c, ix, y + 4, iw, &t.album, 20.0, Weight::Regular, DIM);
        y += 34;
    }
    let quality = match (&t.quality, app.owner) {
        (Some(q), _) => q.clone(),
        (None, super::Owner::Spotify) => format!("Spotify • Ogg Vorbis {} kbps", app.cfg.bitrate.min(320)),
        (None, _) => String::new(),
    };
    if !quality.is_empty() {
        quality_pill(x, ix, y + 10, &quality, false);
    }

    // Progress.
    let pos = app.pb.position();
    let py = cy0 + 272;
    let frac = if t.duration_ms > 0 {
        pos as f32 / t.duration_ms as f32
    } else {
        0.0
    };
    x.c.fill_rounded(ix, py, iw, 6, 3, rgb(0x55, 0x55, 0x55));
    let filled = (iw as f32 * frac).round() as i32;
    x.c.fill_rounded(ix, py, filled.max(6), 6, 3, TEXT);
    x.c.fill_circle((ix + filled) as f32, py as f32 + 3.0, 8.0, TEXT);
    x.f.draw(x.c, ix, py + 16, &fmt_time(pos), 17.0, Weight::Regular, SUBTEXT);
    x.f.draw_right(x.c, ix + iw, py + 16, &fmt_time(t.duration_ms), 17.0, Weight::Regular, SUBTEXT);

    // Controls.
    let ccy = py + 104;
    let mid = ix + iw / 2;
    let play_r = 42;
    x.c.fill_circle(mid as f32, ccy as f32, play_r as f32, TEXT);
    if app.pb.loading {
        spinner(x, app, mid as f32, ccy as f32, 18.0, BG);
    } else {
        let icon = if app.pb.playing { Icon::Pause } else { Icon::Play };
        x.ic.draw(x.c, icon, mid - 20, ccy - 20, 40, BG);
    }
    let gap = (iw / 2 - play_r) / 2 + 6;
    x.ic.draw(x.c, Icon::Prev, mid - play_r - gap + 2 - 20, ccy - 20, 40, TEXT);
    x.ic.draw(x.c, Icon::Next, mid + play_r + gap - 2 - 20, ccy - 20, 40, TEXT);
    let sh_col = if app.pb.shuffle { ACCENT } else { SUBTEXT };
    x.ic.draw(x.c, Icon::Shuffle, ix - 2, ccy - 16, 32, sh_col);
    if app.pb.shuffle {
        x.c.fill_circle(ix as f32 + 14.0, ccy as f32 + 24.0, 3.0, ACCENT);
    }
    let rp_col = if app.pb.repeat == Repeat::Off { SUBTEXT } else { ACCENT };
    let rx = ix + iw - 30;
    x.ic.draw(x.c, Icon::Repeat, rx, ccy - 16, 32, rp_col);
    if app.pb.repeat == Repeat::Track {
        x.c.fill_circle(rx as f32 + 28.0, ccy as f32 - 14.0, 9.0, ACCENT);
        x.f.draw_centered(x.c, rx + 28, ccy - 25, "1", 14.0, Weight::Bold, BG);
    }
    if app.pb.repeat != Repeat::Off {
        x.c.fill_circle(rx as f32 + 16.0, ccy as f32 + 24.0, 3.0, ACCENT);
    }

    if !app.pb.active {
        x.f.draw_centered(
            x.c,
            mid,
            ccy + 60,
            "Đang dừng ở máy này — nhấn A để phát tại đây",
            17.0,
            Weight::Regular,
            SUBTEXT,
        );
    }

    // Volume across the bottom.
    let vy = cy0 + cs + 58;
    let vol = app.pb.volume as f32 / 65535.0;
    x.ic.draw(x.c, Icon::Speaker, cx0, vy - 14, 28, SUBTEXT);
    let vx = cx0 + 44;
    let vw = w - vx - SIDE_PAD - 70;
    x.c.fill_rounded(vx, vy - 3, vw, 6, 3, rgb(0x55, 0x55, 0x55));
    x.c.fill_rounded(vx, vy - 3, ((vw as f32 * vol) as i32).max(6), 6, 3, ACCENT);
    let pct = format!("{}%", (vol * 100.0).round() as i32);
    x.f.draw_right(x.c, w - SIDE_PAD, vy - 13, &pct, 19.0, Weight::Regular, SUBTEXT);

    hints(
        x,
        &[
            ("A", "Phát/Dừng"),
            ("LR", "Bài"),
            ("L/R", "Tua"),
            ("UD", "Âm lượng"),
            ("X", "Trộn"),
            ("Y", "Lặp"),
            ("B", "Về"),
        ],
    );
}

fn current_bg(app: &App) -> Color {
    let t = app.bg_t0.elapsed().as_secs_f32() / 0.45;
    lerp_color(app.bg_from, app.bg_to, t)
}

fn draw_search(x: &mut Ctx, app: &mut App, kb: &mut Keyboard) {
    let w = x.c.w as i32;
    top_bar(x, app, "Tìm kiếm", true);
    // Text field.
    let fx = SIDE_PAD;
    let fy = TOP_BAR_H + 16;
    let fw = w - 2 * SIDE_PAD;
    let fh = 64;
    x.c.fill_rounded(fx, fy, fw, fh, 12, TEXT);
    x.ic.draw(x.c, Icon::Search, fx + 18, fy + 16, 32, BG);
    let tx = fx + 64;
    if kb.text.is_empty() {
        let hint = if kb.vi {
            "Bài hát, nghệ sĩ hoặc album… (gõ Telex)"
        } else {
            "Bài hát, nghệ sĩ hoặc album…"
        };
        x.f.draw(x.c, tx, fy + 14, hint, 26.0, Weight::Regular, DIM);
    } else {
        let max_w = (fw - 64 - 84) as f32;
        let shown = {
            // Keep the end of long queries visible.
            let mut s = kb.text.clone();
            while x.f.measure(&s, 26.0, Weight::Regular) > max_w && !s.is_empty() {
                s.remove(0);
            }
            s
        };
        let tw = x.f.draw(x.c, tx, fy + 14, &shown, 26.0, Weight::Regular, BG) as i32;
        if !kb.on_clear {
            x.c.fill_rect(tx + tw + 3, fy + 16, 3, 32, ACCENT);
        }
        // Clear button: reached with ▲ from the top row of keys, or SELECT.
        let (cx, cy) = ((fx + fw - 38) as f32, (fy + fh / 2) as f32);
        if kb.on_clear {
            x.c.fill_rounded(fx + fw - 64, fy + 6, 52, 52, 26, ACCENT);
            x.ic.draw(x.c, Icon::Close, cx as i32 - 14, cy as i32 - 14, 28, BG);
        } else {
            x.c.fill_circle(cx, cy, 17.0, DIM);
            x.ic.draw(x.c, Icon::Close, cx as i32 - 11, cy as i32 - 11, 22, TEXT);
        }
    }

    // Keyboard.
    let key_w = 86;
    let key_h = 70;
    let gap = 8;
    let total_w = KEY_COLS as i32 * key_w + (KEY_COLS as i32 - 1) * gap;
    let x0 = (w - total_w) / 2;
    let y0 = fy + fh + 28;
    let (sel_start, sel_span) = Keyboard::span_of(kb.row, kb.col);
    let on_keys = !kb.on_clear;
    for (r, row) in KEY_ROWS.iter().enumerate() {
        for (cidx, ch) in row.chars().enumerate() {
            let kx = x0 + cidx as i32 * (key_w + gap);
            let ky = y0 + r as i32 * (key_h + gap);
            let selected = on_keys && kb.row == r && sel_start == cidx;
            let (bg, fg) = if selected { (ACCENT, BG) } else { (ELEVATED, TEXT) };
            x.c.fill_rounded(kx, ky, key_w, key_h, 10, bg);
            let s = ch.to_uppercase().to_string();
            x.f.draw_centered(x.c, kx + key_w / 2, ky + 16, &s, 30.0, Weight::Bold, fg);
        }
    }
    let r = KEY_ROWS.len();
    let ky = y0 + r as i32 * (key_h + gap);
    let mut col = 0;
    for (key, span) in BOTTOM_ROW {
        let kx = x0 + col as i32 * (key_w + gap);
        let kw = span as i32 * key_w + (span as i32 - 1) * gap;
        let selected = on_keys && kb.row == r && sel_start == col && sel_span == span;
        let (bg, fg) = match (selected, key) {
            (true, _) => (ACCENT, BG),
            (false, Key::Search) => (HIGHLIGHT, ACCENT),
            _ => (ELEVATED, TEXT),
        };
        x.c.fill_rounded(kx, ky, kw, key_h, 10, bg);
        match key {
            Key::Search => {
                let label = "Tìm";
                let lw = x.f.measure(label, 24.0, Weight::Bold) as i32;
                let start = kx + (kw - lw - 36) / 2;
                x.ic.draw(x.c, Icon::Search, start, ky + 21, 28, fg);
                x.f.draw(x.c, start + 36, ky + 19, label, 24.0, Weight::Bold, fg);
            }
            Key::Lang => {
                // Both layouts, the active one lit: "VI  EN".
                let off = lerp_color(bg, fg, 0.4);
                let half = kw / 2;
                let (vi, en) = if kb.vi { (fg, off) } else { (off, fg) };
                x.f.draw_centered(x.c, kx + half / 2 + 8, ky + 19, "VI", 24.0, Weight::Bold, vi);
                x.f.draw_centered(x.c, kx + half + half / 2 - 8, ky + 19, "EN", 24.0, Weight::Bold, en);
                x.c.fill_rect(kx + half - 1, ky + 22, 2, key_h - 44, off);
            }
            _ => {
                let label = match key {
                    Key::Space => "Dấu cách",
                    Key::Delete => "Xóa",
                    _ => "",
                };
                x.f.draw_centered(x.c, kx + kw / 2, ky + 19, label, 24.0, Weight::Bold, fg);
            }
        }
        col += span;
    }
    if kb.vi {
        let ty = ky + key_h + 18;
        x.f.draw_centered(
            x.c,
            w / 2,
            ty,
            "Telex:  s f r x j = dấu   ·   aa ee oo = â ê ô   ·   aw ow uw = ă ơ ư   ·   dd = đ",
            20.0,
            Weight::Regular,
            DIM,
        );
    }
    let a_label = if kb.on_clear { "Xóa hết" } else { "Nhập" };
    hints(
        x,
        &[
            ("A", a_label),
            ("B", "Xóa"),
            ("X", "Cách"),
            ("L1", "VI/EN"),
            ("SELECT", "Xóa hết"),
            ("START", "Tìm"),
        ],
    );
}

fn draw_menu(x: &mut Ctx, app: &mut App) {
    let full = x.c.full();
    x.c.dim(full, 160);
    let Some(menu) = app.menu.as_mut() else {
        return;
    };
    let w = x.c.w as i32;
    let h = x.c.h as i32;
    let row_h = 62;
    let pw = 680;
    let ph = 92 + menu.items.len() as i32 * row_h + 20;
    let px = (w - pw) / 2;
    let py = ((h - ph) / 2).max(20);
    x.c.fill_rounded(px, py, pw, ph, 18, ELEVATED);
    x.f.draw(x.c, px + 32, py + 24, "Tùy chọn", 28.0, Weight::Bold, TEXT);
    let area = Rect::new(px, py + 80, pw, menu.items.len() as i32 * row_h);
    menu.state.set_geometry(row_h as f32, area.h as f32, menu.items.len());
    let hy = area.y + menu.state.hl.round() as i32;
    x.c.fill_rounded(px + 14, hy + 3, pw - 28, row_h - 6, 10, HIGHLIGHT);
    for (i, (label, action)) in menu.items.iter().enumerate() {
        let ry = area.y + i as i32 * row_h;
        let (text, col) = match action {
            super::MenuAction::Logout if menu.confirm_logout => {
                ("Nhấn A lần nữa để đăng xuất".to_string(), ERROR)
            }
            super::MenuAction::Logout | super::MenuAction::Exit => (label.clone(), ERROR),
            _ => (label.clone(), TEXT),
        };
        x.f.draw_fit(x.c, px + 34, ry + 16, pw - 68, &text, 23.0, Weight::Regular, col);
    }
}

fn draw_toast(x: &mut Ctx, msg: &str) {
    let w = x.c.w as i32;
    let h = x.c.h as i32;
    let tw = (x.f.measure(msg, 20.0, Weight::Regular) as i32 + 48).min(w - 40);
    let th = 52;
    let tx = (w - tw) / 2;
    let ty = h - HINTS_H - MINI_H - th - 16;
    x.c.fill_rounded(tx, ty, tw, th, 26, TEXT);
    x.f.draw_fit(x.c, tx + 24, ty + 12, tw - 48, msg, 20.0, Weight::Regular, BG);
}

// ---------------------------------------------------------------- local music

/// Draws a small format label; `short` keeps only the codec ("FLAC").
/// Returns the width used (plus spacing).
fn quality_pill(x: &mut Ctx, px: i32, py: i32, q: &str, short: bool) -> i32 {
    let codec = q.split(" • ").next().unwrap_or(q);
    let lossless = matches!(codec, "FLAC" | "ALAC" | "WAV" | "AIFF");
    let high_rate = q
        .split(" / ")
        .nth(1)
        .and_then(|r| r.trim_end_matches(" kHz").parse::<f32>().ok())
        .map(|k| k > 48.0)
        .unwrap_or(false);
    let hires = lossless && (q.contains("24-bit") || q.contains("32-bit") || high_rate);
    let text = if short { codec.to_string() } else { q.to_string() };
    let (size, h) = if short { (12.0, 18) } else { (16.0, 28) };
    let (bg, fg) = if hires {
        (rgb(0x4a, 0x3b, 0x12), rgb(0xF5, 0xC5, 0x42))
    } else if lossless {
        (rgb(0x16, 0x3d, 0x28), ACCENT)
    } else {
        (HIGHLIGHT, SUBTEXT)
    };
    let label = if hires && !short {
        format!("Hi-Res  {text}")
    } else {
        text
    };
    let tw = x.f.measure(&label, size, Weight::Bold) as i32;
    let pad = if short { 6 } else { 12 };
    x.c.fill_rounded(px, py, tw + pad * 2, h, h / 2 - 2, bg);
    let th = x.f.line_height(size) as i32;
    x.f.draw(x.c, px + pad, py + (h - th) / 2, &label, size, Weight::Bold, fg);
    tw + pad * 2 + 8
}

#[allow(clippy::too_many_arguments)]
fn local_row(
    x: &mut Ctx,
    app: &mut App,
    ry: i32,
    icon: Icon,
    cover_url: Option<&str>,
    title: &str,
    sub: &str,
    busy: bool,
) {
    let w = x.c.w as i32;
    let ty = ry + (HOME_ROW_H - HOME_THUMB as i32) / 2;
    let tx = SIDE_PAD + 4;
    match cover_url {
        Some(u) => cover(x, app, Some(u), tx, ty, HOME_THUMB, 6),
        None if busy => {
            x.c.fill_rounded(tx, ty, HOME_THUMB as i32, HOME_THUMB as i32, 6, ELEVATED);
            let c = (tx + HOME_THUMB as i32 / 2) as f32;
            spinner(x, app, c, (ty + HOME_THUMB as i32 / 2) as f32, 16.0, TEXT);
        }
        None => icon_tile(x, icon, tx, ty, HOME_THUMB as i32, ELEVATED, TEXT),
    }
    let text_x = tx + HOME_THUMB as i32 + 18;
    let max = w - text_x - SIDE_PAD - 10;
    let title_y = if sub.is_empty() { ry + 24 } else { ry + 14 };
    x.f.draw_fit(x.c, text_x, title_y, max, title, 23.0, Weight::Bold, TEXT);
    if !sub.is_empty() {
        x.f.draw_fit(x.c, text_x, ry + 45, max, sub, 18.0, Weight::Regular, SUBTEXT);
    }
}

fn draw_local_home(x: &mut Ctx, app: &mut App, state: &mut ListState) {
    let area = content_rect(x, app);
    let len = 6;
    state.set_geometry(HOME_ROW_H as f32, area.h as f32, len);
    let y0 = list_frame(x, state, area, len);
    let none = app.cfg.music_dir.is_empty();
    let (n_all, n_alb, n_art, lossless) = app
        .local
        .index
        .as_ref()
        .map(|ix| {
            (
                ix.lib.tracks.len(),
                ix.albums.len(),
                ix.artists.len(),
                ix.lib.tracks.iter().filter(|t| t.lossless).count(),
            )
        })
        .unwrap_or((0, 0, 0, 0));
    let pick_hint = String::from("Chưa chọn thư mục nhạc");
    let folder_name = std::path::Path::new(&app.cfg.music_dir)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let scanning = app.local.scanning;
    let rescan_sub = match (scanning, &app.local.error) {
        (Some((d, t)), _) if t > 0 => format!("Đang quét… {d}/{t} file"),
        (Some(_), _) => String::from("Đang tìm file nhạc…"),
        (None, Some(e)) => e.clone(),
        (None, None) => String::from("Tìm bài mới, bỏ bài đã xóa"),
    };
    let pick = |s: String| if none { pick_hint.clone() } else { s };
    let rows: [(Icon, &str, String); 6] = [
        (Icon::Note, "Tất cả bài hát", pick(format!("{n_all} bài • {lossless} lossless"))),
        (Icon::Disc, "Album", pick(format!("{n_alb} album"))),
        (Icon::Person, "Nghệ sĩ", pick(format!("{n_art} nghệ sĩ"))),
        (Icon::Folder, "Thư mục", pick(format!("Duyệt theo thư mục trong \u{201c}{folder_name}\u{201d}"))),
        (Icon::Refresh, "Quét lại thư viện", rescan_sub),
        (
            Icon::Check,
            "Chọn thư mục nhạc",
            if none {
                String::from("Nhấn A để chọn thư mục chứa nhạc của bạn")
            } else {
                app.cfg.music_dir.clone()
            },
        ),
    ];
    x.c.set_clip(area);
    for i in state.visible(len) {
        let ry = (y0 + i as f32 * HOME_ROW_H as f32).round() as i32;
        let (icon, title, sub) = &rows[i];
        let busy = i == 4 && scanning.is_some();
        local_row(x, app, ry, *icon, None, title, sub, busy);
    }
    x.c.reset_clip();
    top_bar(x, app, "Nhạc trên máy", true);
    if show_mini(app) {
        mini_player(x, app);
    }
    hints(
        x,
        &[("A", "Mở"), ("X", "Trộn tất cả"), ("Y", "Đang phát"), ("B", "Quay lại"), ("START", "Tùy chọn")],
    );
}

fn draw_entries(x: &mut Ctx, app: &mut App, ev: &mut super::EntriesView) {
    let area = content_rect(x, app);
    let w = x.c.w as i32;
    let len = ev.entries.len();
    if len == 0 {
        x.f.draw_centered(x.c, w / 2, area.y + 80, "Không có gì ở đây", 24.0, Weight::Regular, SUBTEXT);
    }
    ev.state.set_geometry(HOME_ROW_H as f32, area.h as f32, len);
    let y0 = list_frame(x, &ev.state, area, len);
    x.c.set_clip(area);
    for i in ev.state.visible(len) {
        let ry = (y0 + i as f32 * HOME_ROW_H as f32).round() as i32;
        let e = &ev.entries[i];
        let (icon, cov, title, sub) = (e.icon, e.cover.clone(), e.title.clone(), e.subtitle.clone());
        local_row(x, app, ry, icon, cov.as_deref(), &title, &sub, false);
    }
    x.c.reset_clip();
    let title = format!("{} ({len})", ev.title);
    top_bar(x, app, &title, true);
    if show_mini(app) {
        mini_player(x, app);
    }
    hints(
        x,
        &[("A", "Mở / Phát"), ("X", "Phát ngẫu nhiên"), ("Y", "Đang phát"), ("B", "Quay lại")],
    );
}

fn draw_picker(x: &mut Ctx, app: &mut App, pv: &mut super::PickerView) {
    let full = content_rect(x, app);
    let w = x.c.w as i32;
    // Current path (keep the end visible).
    let path = pv.path.to_string_lossy().replace('\\', "/");
    let mut shown = path.clone();
    while x.f.measure(&shown, 19.0, Weight::Regular) > (w - 2 * SIDE_PAD - 40) as f32
        && shown.chars().count() > 4
    {
        let mut chars = shown.chars();
        chars.next();
        shown = chars.as_str().to_string();
    }
    if shown != path {
        shown = format!("…{shown}");
    }
    x.ic.draw(x.c, Icon::Folder, SIDE_PAD, full.y + 10, 24, SUBTEXT);
    x.f.draw(x.c, SIDE_PAD + 34, full.y + 9, &shown, 19.0, Weight::Regular, SUBTEXT);
    let area = Rect::new(full.x, full.y + 46, full.w, full.h - 46);
    x.c.fill_rect(SIDE_PAD, area.y - 1, w - 2 * SIDE_PAD, 1, HIGHLIGHT);
    let len = pv.len();
    pv.state.set_geometry(TRACK_ROW_H as f32, area.h as f32, len);
    let y0 = list_frame(x, &pv.state, area, len);
    x.c.set_clip(area);
    for i in pv.state.visible(len) {
        let ry = (y0 + i as f32 * TRACK_ROW_H as f32).round() as i32;
        let ty = ry + (TRACK_ROW_H - 44) / 2;
        if i == 0 {
            icon_tile(x, Icon::Check, SIDE_PAD + 4, ty, 44, rgb(0x16, 0x3d, 0x28), ACCENT);
            x.f.draw(x.c, SIDE_PAD + 64, ry + 20, "Dùng thư mục này", 22.0, Weight::Bold, ACCENT);
        } else {
            icon_tile(x, Icon::Folder, SIDE_PAD + 4, ty, 44, ELEVATED, TEXT);
            let name = pv.dirs[i - 1].clone();
            x.f.draw_fit(x.c, SIDE_PAD + 64, ry + 20, w - SIDE_PAD * 2 - 80, &name, 22.0, Weight::Regular, TEXT);
        }
    }
    if pv.dirs.is_empty() {
        x.f.draw(x.c, SIDE_PAD + 64, area.y + TRACK_ROW_H + 20, "(không có thư mục con)", 19.0, Weight::Regular, DIM);
    }
    x.c.reset_clip();
    top_bar(x, app, "Chọn thư mục nhạc", true);
    if show_mini(app) {
        mini_player(x, app);
    }
    hints(
        x,
        &[("A", "Vào / Chọn"), ("START", "Chọn thư mục này"), ("B", "Lên trên"), ("SELECT", "Hủy")],
    );
}

// ---------------------------------------------------------------- updates

fn update_button(x: &mut Ctx, bx: i32, by: i32, key: &str, label: &str, strong: bool) -> i32 {
    let lw = x.f.measure(label, 20.0, Weight::Bold) as i32;
    let bw = lw + 70;
    let (bg, fg) = if strong { (ACCENT, BG) } else { (HIGHLIGHT, TEXT) };
    x.c.fill_rounded(bx, by, bw, 44, 22, bg);
    x.c.fill_circle((bx + 24) as f32, (by + 22) as f32, 13.0, if strong { BG } else { ELEVATED });
    x.f.draw_centered(x.c, bx + 24, by + 11, key, 16.0, Weight::Bold, if strong { ACCENT } else { TEXT });
    x.f.draw(x.c, bx + 48, by + 9, label, 20.0, Weight::Bold, fg);
    bw + 14
}

fn draw_update(x: &mut Ctx, app: &mut App) {
    use crate::update::UpdateState;
    let full = x.c.full();
    x.c.dim(full, 170);
    let w = x.c.w as i32;
    let h = x.c.h as i32;
    let pw = 720;
    let ph = 460;
    let px = (w - pw) / 2;
    let py = (h - ph) / 2;
    x.c.fill_rounded(px, py, pw, ph, 20, ELEVATED);
    if let Some(logo) = app.logos.small.as_ref() {
        x.c.blit_rgba(px + 32, py + 28, logo);
    }
    x.f.draw(x.c, px + 112, py + 30, "Cập nhật Spoty", 30.0, Weight::Bold, TEXT);
    let cur = format!("Phiên bản hiện tại: {}", crate::update::current_version());
    x.f.draw(x.c, px + 112, py + 70, &cur, 19.0, Weight::Regular, SUBTEXT);
    let inner_x = px + 36;
    let inner_w = pw - 72;
    let mut y = py + 124;
    let state = app.update.state.clone();
    let (primary, secondary): (Option<&str>, &str) = match &state {
        None | Some(UpdateState::Checking) => {
            spinner(x, app, (inner_x + 16) as f32, (y + 16) as f32, 13.0, TEXT);
            x.f.draw(x.c, inner_x + 44, y + 2, "Đang kiểm tra bản cập nhật…", 22.0, Weight::Regular, TEXT);
            (None, "Đóng")
        }
        Some(UpdateState::UpToDate) => {
            x.ic.draw(x.c, Icon::Check, inner_x, y, 32, ACCENT);
            x.f.draw(x.c, inner_x + 44, y + 2, "Bạn đang dùng bản mới nhất.", 22.0, Weight::Regular, TEXT);
            (Some("OK"), "Đóng")
        }
        Some(UpdateState::Available(info)) => {
            let head = format!("Có phiên bản mới: {}", info.version);
            x.f.draw(x.c, inner_x, y, &head, 26.0, Weight::Bold, ACCENT);
            if info.size > 0 {
                let sz = format!("Dung lượng tải: {:.1} MB", info.size as f64 / 1_048_576.0);
                x.f.draw(x.c, inner_x, y + 40, &sz, 18.0, Weight::Regular, SUBTEXT);
            }
            y += 76;
            let notes = if info.notes.trim().is_empty() {
                String::from("Cải thiện và sửa lỗi.")
            } else {
                info.notes.clone()
            };
            let mut lines = 0;
            'outer: for para in notes.lines() {
                for l in x.f.wrap(para, 19.0, Weight::Regular, inner_w as f32, 3) {
                    if lines >= 6 {
                        break 'outer;
                    }
                    x.f.draw(x.c, inner_x, y, &l, 19.0, Weight::Regular, TEXT);
                    y += 28;
                    lines += 1;
                }
            }
            (Some("Cập nhật ngay"), "Để sau")
        }
        Some(UpdateState::Downloading { done, total }) => {
            let frac = if *total > 0 {
                (*done as f32 / *total as f32).min(1.0)
            } else {
                0.0
            };
            x.f.draw(x.c, inner_x, y, "Đang tải bản cập nhật…", 22.0, Weight::Regular, TEXT);
            y += 48;
            x.c.fill_rounded(inner_x, y, inner_w, 10, 5, HIGHLIGHT);
            x.c.fill_rounded(inner_x, y, ((inner_w as f32 * frac) as i32).max(10), 10, 5, ACCENT);
            let t = format!(
                "{}%  •  {:.1} / {:.1} MB",
                (frac * 100.0) as u32,
                *done as f64 / 1_048_576.0,
                *total as f64 / 1_048_576.0
            );
            x.f.draw(x.c, inner_x, y + 22, &t, 18.0, Weight::Regular, SUBTEXT);
            x.f.draw(x.c, inner_x, y + 60, "Nhạc vẫn phát bình thường trong lúc tải.", 18.0, Weight::Regular, DIM);
            (None, "Chạy nền")
        }
        Some(UpdateState::Installing) => {
            spinner(x, app, (inner_x + 16) as f32, (y + 16) as f32, 13.0, TEXT);
            x.f.draw(x.c, inner_x + 44, y + 2, "Đang cài đặt…", 22.0, Weight::Regular, TEXT);
            (None, "Chạy nền")
        }
        Some(UpdateState::Ready { version }) => {
            x.ic.draw(x.c, Icon::Check, inner_x, y, 32, ACCENT);
            let t = format!("Đã cài xong phiên bản {version}.");
            x.f.draw(x.c, inner_x + 44, y + 2, &t, 22.0, Weight::Bold, TEXT);
            for (i, l) in x
                .f
                .wrap(
                    "Khởi động lại để dùng bản mới. Bản cũ được giữ lại và tự khôi phục nếu bản mới lỗi.",
                    18.0,
                    Weight::Regular,
                    inner_w as f32,
                    3,
                )
                .iter()
                .enumerate()
            {
                x.f.draw(x.c, inner_x, y + 52 + i as i32 * 26, l, 18.0, Weight::Regular, SUBTEXT);
            }
            (Some("Khởi động lại"), "Để sau")
        }
        Some(UpdateState::Failed(e)) => {
            x.f.draw(x.c, inner_x, y, "Không cập nhật được", 24.0, Weight::Bold, ERROR);
            y += 44;
            for l in x.f.wrap(e, 19.0, Weight::Regular, inner_w as f32, 4) {
                x.f.draw(x.c, inner_x, y, &l, 19.0, Weight::Regular, TEXT);
                y += 28;
            }
            (Some("Thử lại"), "Đóng")
        }
    };
    let by = py + ph - 68;
    let mut bx = inner_x;
    if let Some(p) = primary {
        bx += update_button(x, bx, by, "A", p, true);
    }
    update_button(x, bx, by, "B", secondary, false);
}

// ---------------------------------------------------------------- home feed

const SHELF_H: i32 = 300;
const CARD: i32 = 180;
const CARD_STRIDE: i32 = 202;

fn draw_feed(x: &mut Ctx, app: &mut App, fs: &mut super::widgets::FeedState) {
    use crate::spotify::home::FeedKind;
    let area = content_rect(x, app);
    let w = x.c.w as i32;
    let feed = app.feed.take();
    match &feed {
        Some(Ok(sections)) if !sections.is_empty() => {
            let lens: Vec<usize> = sections.iter().map(|s| s.items.len()).collect();
            fs.set_geometry(
                SHELF_H as f32,
                CARD_STRIDE as f32,
                (w - 2 * SIDE_PAD) as f32,
                area.h as f32,
                &lens,
            );
            x.c.set_clip(area);
            for (i, sec) in sections.iter().enumerate() {
                let sy = area.y + i as i32 * SHELF_H - fs.y.round() as i32;
                if sy > area.y + area.h || sy + SHELF_H < area.y {
                    continue;
                }
                x.f.draw_fit(x.c, SIDE_PAD, sy + 8, w - 2 * SIDE_PAD, &sec.title, 25.0, Weight::Bold, TEXT);
                let ox = fs.xs.get(i).copied().unwrap_or(0.0).round() as i32;
                let cy = sy + 52;
                for (j, item) in sec.items.iter().enumerate() {
                    let cx = SIDE_PAD + j as i32 * CARD_STRIDE - ox;
                    if cx > w || cx + CARD < 0 {
                        continue;
                    }
                    let round = item.kind == FeedKind::Artist;
                    let selected = i == fs.row && fs.cols.get(i).copied() == Some(j);
                    if selected {
                        if round {
                            let c = (cx + CARD / 2) as f32;
                            x.c.fill_circle(c, (cy + CARD / 2) as f32, (CARD / 2 + 5) as f32, TEXT);
                        } else {
                            x.c.fill_rounded(cx - 5, cy - 5, CARD + 10, CARD + 10, 14, TEXT);
                        }
                    }
                    let radius = if round { CARD / 2 } else { 8 };
                    cover(x, app, item.image.as_deref(), cx, cy, CARD as u32, radius);
                    let col = if selected { TEXT } else { rgb(0xE0, 0xE0, 0xE0) };
                    x.f.draw_fit(x.c, cx, cy + CARD + 10, CARD, &item.title, 19.0, Weight::Bold, col);
                    x.f.draw_fit(x.c, cx, cy + CARD + 37, CARD, &item.subtitle, 16.0, Weight::Regular, SUBTEXT);
                }
            }
            x.c.reset_clip();
        }
        Some(Err(e)) => {
            let cy = area.y + area.h / 2 - 60;
            x.f.draw_centered(x.c, w / 2, cy, "Không tải được đề xuất từ Spotify", 25.0, Weight::Bold, ERROR);
            let lines = x.f.wrap(e, 18.0, Weight::Regular, (w - 160) as f32, 2);
            for (k, l) in lines.iter().enumerate() {
                x.f.draw_centered(x.c, w / 2, cy + 44 + k as i32 * 26, l, 18.0, Weight::Regular, SUBTEXT);
            }
            x.f.draw_centered(x.c, w / 2, cy + 110, "Nhấn A để thử lại", 20.0, Weight::Regular, TEXT);
        }
        _ => {
            let cy = area.y + area.h / 2;
            spinner(x, app, (w / 2) as f32, (cy - 30) as f32, 24.0, SUBTEXT);
            let msg = if app.username().is_some() {
                "Đang tải đề xuất từ Spotify…"
            } else {
                "Đang kết nối Spotify…"
            };
            x.f.draw_centered(x.c, w / 2, cy + 14, msg, 20.0, Weight::Regular, SUBTEXT);
        }
    }
    app.feed = feed;
    top_bar(x, app, "Dành cho bạn", true);
    if app.feed_loading && matches!(app.feed, Some(Ok(_))) {
        // Small spinner next to the title while refreshing.
        let tw = x.f.measure("Dành cho bạn", 28.0, Weight::Bold) as i32;
        spinner(x, app, (SIDE_PAD + tw + 24) as f32, 30.0, 9.0, SUBTEXT);
    }
    if show_mini(app) {
        mini_player(x, app);
    }
    hints(
        x,
        &[("A", "Mở"), ("X", "Phát ngẫu nhiên"), ("Y", "Đang phát"), ("SELECT", "Làm mới"), ("B", "Thư viện")],
    );
}
