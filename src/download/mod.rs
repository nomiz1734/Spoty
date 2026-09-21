//! Downloading music into the local library from the user's own slskd server.
//!
//! One background task owns the queue: searches run on their own (they take
//! 15-40 s and must not block anything), while files are fetched strictly one
//! at a time — the handheld is also decoding audio, and its SD card does not
//! like two writers.
//!
//! Everything lands flat in `music_dir`, the folder the library already scans,
//! so a finished download only needs the usual incremental rescan.

pub mod slskd;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use tokio::sync::mpsc::{self, UnboundedSender};

use crate::config::Config;
use crate::ui::UiMsg;

pub use slskd::{SearchResult, Server};

/// Longest file name we write. exFAT allows 255; leave room for " (1)".
const MAX_NAME: usize = 120;
/// Refuse to start a download unless this much room is left afterwards.
const KEEP_FREE: u64 = 64 * 1024 * 1024;
/// Attempts at pulling a file off the server; later ones resume where the
/// previous one stopped (and fall back from the LAN to the tunnel).
const DOWNLOAD_TRIES: u32 = 3;

/// The configured server, if `slskd_url` and `slskd_api_key` are set.
pub fn server_from(cfg: &Config) -> Option<Server> {
    Server::new(&cfg.slskd_url, &cfg.slskd_api_key, &cfg.slskd_lan_url)
}

pub enum DownloadCmd {
    Search(String),
    Enqueue(Box<SearchResult>),
    /// Drops the queue and stops the file in flight.
    Cancel,
    /// The user picked another music folder.
    MusicDir(PathBuf),
    Shutdown,
}

/// Which half of the trip a file is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// The peer is sending it to our server.
    Peer,
    /// Our server is sending it to the device.
    Server,
}

#[derive(Clone, Debug)]
pub enum DownloadEvent {
    Searching,
    /// `id` counts up, so the UI can drop results from a search the user replaced.
    Results {
        id: u64,
        results: Vec<SearchResult>,
    },
    /// Names still waiting, first one is the file in flight.
    Queue(Vec<String>),
    Progress {
        name: String,
        stage: Stage,
        done: u64,
        total: u64,
    },
    /// Saved; the UI shows it and rescans the library.
    Complete(PathBuf),
    Error(String),
}

/// Starts the download worker on the runtime the app already has.
pub fn spawn(
    rt: &tokio::runtime::Handle,
    cfg: &Config,
    ui: Sender<UiMsg>,
) -> UnboundedSender<DownloadCmd> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let server = server_from(cfg);
    let delete_after = cfg.slskd_delete_after;
    let mut dir = PathBuf::from(&cfg.music_dir);
    rt.spawn(async move {
        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<()>();
        let mut queue: VecDeque<SearchResult> = VecDeque::new();
        let mut busy = false;
        let mut search_id = 0u64;
        let cancel = Arc::new(AtomicBool::new(false));
        loop {
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    match cmd {
                        DownloadCmd::Shutdown => break,
                        DownloadCmd::MusicDir(path) => dir = path,
                        DownloadCmd::Search(text) => {
                            let Some(server) = server.clone() else {
                                emit(&ui, DownloadEvent::Error(NOT_SET_UP.into()));
                                continue;
                            };
                            search_id += 1;
                            let (id, ui) = (search_id, ui.clone());
                            emit(&ui, DownloadEvent::Searching);
                            tokio::spawn(async move {
                                match server.search(&text, 60).await {
                                    Ok(results) => emit(&ui, DownloadEvent::Results { id, results }),
                                    Err(e) => emit(&ui, DownloadEvent::Error(e)),
                                }
                            });
                        }
                        DownloadCmd::Enqueue(item) => {
                            if server.is_none() {
                                emit(&ui, DownloadEvent::Error(NOT_SET_UP.into()));
                                continue;
                            }
                            if dir.as_os_str().is_empty() {
                                emit(&ui, DownloadEvent::Error("Hãy chọn thư mục nhạc trước".into()));
                                continue;
                            }
                            queue.push_back(*item);
                            emit(&ui, DownloadEvent::Queue(names(&queue)));
                        }
                        DownloadCmd::Cancel => {
                            queue.clear();
                            cancel.store(true, Ordering::Relaxed);
                            emit(&ui, DownloadEvent::Queue(Vec::new()));
                        }
                    }
                }
                _ = done_rx.recv() => busy = false,
            }
            if !busy && !queue.is_empty() {
                // Pop only once there is somewhere to send it, or the item is lost.
                if let (Some(server), Some(item)) = (server.clone(), queue.pop_front()) {
                    busy = true;
                    cancel.store(false, Ordering::Relaxed);
                    emit(&ui, DownloadEvent::Queue(names(&queue)));
                    let (ui, dir, cancel, done) =
                        (ui.clone(), dir.clone(), cancel.clone(), done_tx.clone());
                    tokio::spawn(async move {
                        match fetch_one(&server, &item, &dir, &ui, &cancel, delete_after).await {
                            Ok(path) => emit(&ui, DownloadEvent::Complete(path)),
                            Err(e) => emit(&ui, DownloadEvent::Error(e)),
                        }
                        let _ = done.send(());
                    });
                }
            }
        }
    });
    tx
}

const NOT_SET_UP: &str = "Chưa cấu hình máy chủ tải nhạc (slskd_url, slskd_api_key)";

fn emit(ui: &Sender<UiMsg>, ev: DownloadEvent) {
    let _ = ui.send(UiMsg::Download(ev));
}

fn names(queue: &VecDeque<SearchResult>) -> Vec<String> {
    queue.iter().map(|r| r.base_name().to_string()).collect()
}

/// Progress events, at most five a second.
///
/// The file body arrives in 16-64 KB chunks, so reporting every one of them
/// would push thousands of messages at the UI thread for a single song.
fn reporter(ui: &Sender<UiMsg>, name: &str, stage: Stage) -> impl FnMut(u64, u64) + use<> {
    let (ui, name) = (ui.clone(), name.to_string());
    let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
    move |done, total| {
        let finished = total > 0 && done >= total;
        if !finished && last.elapsed() < std::time::Duration::from_millis(200) {
            return;
        }
        last = std::time::Instant::now();
        emit(&ui, DownloadEvent::Progress { name: name.clone(), stage, done, total });
    }
}

/// The whole trip for one file: peer -> our server -> SD card.
async fn fetch_one(
    server: &Server,
    item: &SearchResult,
    dir: &Path,
    ui: &Sender<UiMsg>,
    cancel: &AtomicBool,
    delete_after: bool,
) -> Result<PathBuf, String> {
    let name = safe_name(&item.filename).ok_or("tên file không hợp lệ")?;
    if let Some(free) = free_space(dir) {
        if free < item.size + KEEP_FREE {
            return Err(format!("thẻ nhớ còn {} MB, không đủ chỗ", free / 1_048_576));
        }
    }
    server.enqueue(item).await?;
    {
        let mut report = reporter(ui, &name, Stage::Peer);
        server
            .wait_transfer(item, |done, total| {
                report(done, total);
                !cancel.load(Ordering::Relaxed)
            })
            .await?;
    }

    let remote = server.locate(item).await?;
    let dest = unique_path(dir, &name);
    let part = with_suffix(&dest, ".part");
    // A .part left by an earlier run may belong to another version of the
    // song; only resume what this job itself started.
    let _ = std::fs::remove_file(&part);
    let mut report = reporter(ui, &name, Stage::Server);
    for attempt in 1..=DOWNLOAD_TRIES {
        let resume = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        let result = server
            .download(&remote, &part, resume, |done, total| {
                report(done, total);
                !cancel.load(Ordering::Relaxed)
            })
            .await;
        match result {
            Ok(_) => break,
            Err(e) if cancel.load(Ordering::Relaxed) || attempt == DOWNLOAD_TRIES => {
                let _ = std::fs::remove_file(&part);
                return Err(e);
            }
            Err(e) => {
                log::warn!("tải {name} lỗi lần {attempt}: {e}; thử lại");
                tokio::time::sleep(std::time::Duration::from_secs(2 * attempt as u64)).await;
            }
        }
    }
    // Rename only once the file is whole, so a rescan never picks up a stub.
    std::fs::rename(&part, &dest).map_err(|e| format!("không lưu được file: {e}"))?;
    if delete_after {
        server.delete_remote(&remote).await;
    }
    Ok(dest)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Turns a peer's path into a file name that is safe to write here.
///
/// Soulseek hands over whatever the other machine had: Windows folders,
/// characters exFAT rejects, and in principle `..`. Keep the basename only.
pub fn safe_name(remote: &str) -> Option<String> {
    let base = remote.rsplit(['\\', '/']).next()?.trim();
    if base.is_empty() || base == "." || base == ".." {
        return None;
    }
    let (stem, ext) = base.rsplit_once('.')?;
    let ext = ext.trim().to_ascii_lowercase();
    if stem.trim().is_empty() || !slskd::is_audio(&ext) {
        return None;
    }
    let clean: String = stem
        .chars()
        .map(|c| match c {
            ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\\' | '/' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let clean = clean.trim().trim_end_matches('.').trim().to_string();
    if clean.is_empty() {
        return None;
    }
    let keep = MAX_NAME.saturating_sub(ext.len() + 1);
    let stem: String = clean.chars().take(keep).collect();
    Some(format!("{}.{ext}", stem.trim_end()))
}

/// `Bài.flac`, then `Bài (1).flac`, … so a second copy never overwrites the first.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    for n in 1..1000 {
        let candidate = dir.join(format!("{stem} ({n}).{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    path
}

#[cfg(unix)]
pub fn free_space(dir: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs only reads the path and fills the struct we own.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path.as_ptr(), &mut st) != 0 {
            return None;
        }
        Some(st.f_bavail as u64 * st.f_frsize as u64)
    }
}

#[cfg(not(unix))]
pub fn free_space(_dir: &Path) -> Option<u64> {
    None
}

/// `spoty --slskd-test <từ khóa>`: searches and prints what the UI would list.
pub async fn search_selftest(cfg: &Config, query: &str) {
    let Some(server) = server_from(cfg) else {
        println!("{NOT_SET_UP}");
        return;
    };
    match server.ping().await {
        Ok(()) => println!("máy chủ ok: {} (đi đường {})", cfg.slskd_url, server.route().await),
        Err(e) => {
            println!("không kết nối được: {e}");
            return;
        }
    }
    match server.search(query, 20).await {
        Ok(results) if results.is_empty() => println!("không có kết quả cho \"{query}\""),
        Ok(results) => {
            for (i, r) in results.iter().enumerate() {
                let speed = r.speed.unwrap_or(0) as f64 / 1_048_576.0;
                println!(
                    "{:>2}. {}\n    {} · {} · {:.1} MB/s{}{}",
                    i + 1,
                    r.base_name(),
                    r.quality(),
                    r.username,
                    speed,
                    if r.free_slot { "" } else { " · bận" },
                    if r.queue > 0 { format!(" · hàng đợi {}", r.queue) } else { String::new() },
                );
            }
        }
        Err(e) => println!("tìm kiếm lỗi: {e}"),
    }
}

/// `spoty --slskd-get <từ khóa>`: downloads the best match into the music
/// folder, through the same worker and queue the app uses.
pub async fn download_selftest(cfg: &Config, query: &str) {
    if server_from(cfg).is_none() {
        println!("{NOT_SET_UP}");
        return;
    }
    if cfg.music_dir.trim().is_empty() {
        println!("chưa chọn thư mục nhạc (music_dir)");
        return;
    }
    let (ui, rx) = std::sync::mpsc::channel();
    let tx = spawn(&tokio::runtime::Handle::current(), cfg, ui);
    let _ = tx.send(DownloadCmd::Search(query.to_string()));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30 * 60);
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        while let Ok(UiMsg::Download(ev)) = rx.try_recv() {
            match ev {
                DownloadEvent::Searching => println!("đang tìm \"{query}\"…"),
                DownloadEvent::Results { id, results } => match results.into_iter().next() {
                    Some(item) => {
                        println!(
                            "kết quả #{id} — tải: {} ({}) từ {}",
                            item.base_name(),
                            item.quality(),
                            item.username
                        );
                        let _ = tx.send(DownloadCmd::Enqueue(Box::new(item)));
                    }
                    None => {
                        println!("không có kết quả cho \"{query}\"");
                        return;
                    }
                },
                DownloadEvent::Progress { name, stage, done, total } => {
                    let pct = if total > 0 { done * 100 / total } else { 0 };
                    let line = match stage {
                        Stage::Peer => format!("  {name}: nguồn -> máy chủ {pct}%"),
                        Stage::Server => format!("  {name}: máy chủ -> máy {pct}%"),
                    };
                    if line != last {
                        println!("{line}");
                        last = line;
                    }
                }
                DownloadEvent::Complete(path) => {
                    println!("đã lưu: {}", path.display());
                    let _ = tx.send(DownloadCmd::Shutdown);
                    return;
                }
                DownloadEvent::Error(e) => {
                    println!("lỗi: {e}");
                    let _ = tx.send(DownloadCmd::Shutdown);
                    return;
                }
                DownloadEvent::Queue(waiting) => {
                    if !waiting.is_empty() {
                        println!("hàng đợi: {}", waiting.join(", "));
                    }
                }
            }
        }
    }
    println!("quá thời gian chờ");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_peer_file_names() {
        assert_eq!(
            safe_name("@@abcd\\Music\\Ballad\\01 - Đừng Như Thói Quen.flac").as_deref(),
            Some("01 - Đừng Như Thói Quen.flac")
        );
        assert_eq!(safe_name("a/b/c/song.MP3").as_deref(), Some("song.mp3"));
        assert_eq!(safe_name("x\\who? what: it.flac").as_deref(), Some("who_ what_ it.flac"));
        // No traversal, no folders, no non-audio, no extension-only names.
        assert_eq!(safe_name("..\\..\\etc\\passwd.flac").as_deref(), Some("passwd.flac"));
        assert_eq!(safe_name("a\\..").as_deref(), None);
        assert_eq!(safe_name("a\\cover.jpg"), None);
        assert_eq!(safe_name("a\\.flac"), None);
        assert_eq!(safe_name(""), None);
        let long = format!("{}.flac", "x".repeat(400));
        let name = safe_name(&long).unwrap();
        assert!(name.chars().count() <= MAX_NAME, "{}", name.chars().count());
        assert!(name.ends_with(".flac"));
    }

    #[test]
    fn never_overwrites() {
        let dir = std::env::temp_dir().join(format!("spoty-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = unique_path(&dir, "Bài.flac");
        assert_eq!(first.file_name().unwrap(), "Bài.flac");
        std::fs::write(&first, b"x").unwrap();
        let second = unique_path(&dir, "Bài.flac");
        assert_eq!(second.file_name().unwrap(), "Bài (1).flac");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A stand-in for slskd behind nginx, answering just what Spoty asks for.
    mod fake_slskd {
        use std::sync::{Arc, Mutex};

        use bytes::Bytes;
        use http_body_util::Full;
        use hyper::body::Incoming;
        use hyper::server::conn::http1;
        use hyper::service::service_fn;
        use hyper::{Method, Request, Response};
        use hyper_util::rt::TokioIo;

        pub const KEY: &str = "k";
        pub const BODY: &[u8] = b"fLaC-this-is-not-really-audio-but-the-bytes-must-match";
        const REMOTE: &str = r"@@b\Nhạc\Album\01 - Bài.flac";
        /// Where slskd put it: the peer's last folder, then the file.
        const SERVED: &str = "/files/Album/01%20-%20B%C3%A0i.flac";

        #[derive(Default)]
        pub struct Seen {
            pub deletes: Vec<String>,
            pub user_agents: Vec<String>,
        }

        fn reply(status: u16, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
            Response::builder()
                .status(status)
                .body(Full::new(body.into()))
                .unwrap()
        }

        async fn route(req: Request<Incoming>, seen: Arc<Mutex<Seen>>) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
            let ua = req.headers().get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            seen.lock().unwrap().user_agents.push(ua);
            if req.headers().get("x-api-key").and_then(|v| v.to_str().ok()) != Some(KEY) {
                return Ok(reply(403, ""));
            }
            let path = req.uri().path().to_string();
            let file = format!(
                r#"{{"filename":{},"size":{},"state":"Completed, Succeeded","bytesTransferred":{}}}"#,
                serde_json::to_string(REMOTE).unwrap(),
                BODY.len(),
                BODY.len()
            );
            Ok(match (req.method().clone(), path.as_str()) {
                (Method::GET, "/api/v0/searches") => reply(200, "[]"),
                (Method::POST, "/api/v0/searches") => reply(200, r#"{"id":"s1"}"#),
                (Method::GET, "/api/v0/searches/s1") => reply(200, r#"{"isComplete":true}"#),
                (Method::GET, "/api/v0/searches/s1/responses") => reply(
                    200,
                    format!(
                        r#"[{{"username":"fast","uploadSpeed":3000000,"hasFreeUploadSlot":true,"queueLength":0,"files":[{file}]}}]"#
                    ),
                ),
                (Method::DELETE, "/api/v0/searches/s1") => reply(204, ""),
                (Method::POST, "/api/v0/transfers/downloads/fast") => reply(201, ""),
                (Method::GET, "/api/v0/transfers/downloads") => reply(
                    200,
                    format!(r#"[{{"username":"fast","directories":[{{"directory":"Album","files":[{file}]}}]}}]"#),
                ),
                (Method::HEAD, p) if p == SERVED => reply(200, ""),
                (Method::GET, p) if p == SERVED => reply(200, BODY),
                (Method::DELETE, p) if p.starts_with("/api/v0/files/downloads/") => {
                    seen.lock().unwrap().deletes.push(p.to_string());
                    reply(204, "")
                }
                _ => reply(404, ""),
            })
        }

        /// Starts the fake on a free port of this machine; returns its URL.
        pub async fn start(seen: Arc<Mutex<Seen>>) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else { continue };
                    let seen = seen.clone();
                    tokio::spawn(async move {
                        let service = service_fn(move |req| route(req, seen.clone()));
                        let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
                    });
                }
            });
            format!("http://127.0.0.1:{port}")
        }
    }

    /// The whole trip over the LAN address: search, enqueue, wait, find the
    /// file on the server, pull it down, then delete it there — or keep it.
    #[test]
    fn downloads_over_the_lan_and_keeps_or_deletes() {
        use std::sync::{Arc, Mutex};

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("spoty-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let seen = Arc::new(Mutex::new(fake_slskd::Seen::default()));

        rt.block_on(async {
            let lan = fake_slskd::start(seen.clone()).await;
            // The public address is unreachable on purpose: everything must go over the LAN.
            let server = Server::new("https://spoty.invalid", fake_slskd::KEY, &lan).unwrap();
            assert_eq!(server.route().await, "LAN");

            let results = server.search("bài", 10).await.unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].base_name(), "01 - Bài.flac");

            let (ui, _events) = std::sync::mpsc::channel();
            let cancel = AtomicBool::new(false);

            // Default: delete the copy on the server once the device has it.
            let path = fetch_one(&server, &results[0], &dir, &ui, &cancel, true).await.unwrap();
            assert_eq!(path.file_name().unwrap(), "01 - Bài.flac");
            assert_eq!(std::fs::read(&path).unwrap(), fake_slskd::BODY);
            assert_eq!(seen.lock().unwrap().deletes.len(), 1);

            // slskd_delete_after = false: the NAS keeps its copy.
            let path = fetch_one(&server, &results[0], &dir, &ui, &cancel, false).await.unwrap();
            assert_eq!(path.file_name().unwrap(), "01 - Bài (1).flac");
            assert_eq!(std::fs::read(&path).unwrap(), fake_slskd::BODY);
            assert_eq!(seen.lock().unwrap().deletes.len(), 1, "không được xóa trên NAS");
        });

        // No .part left behind, and every request said who it was.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty());
        assert!(seen.lock().unwrap().user_agents.iter().all(|ua| ua.starts_with("Spoty/")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Wrong key: a clear message, not a raw status code.
    #[test]
    fn wrong_key_says_so() {
        use std::sync::{Arc, Mutex};

        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let lan = fake_slskd::start(Arc::new(Mutex::new(fake_slskd::Seen::default()))).await;
            // With a wrong key the LAN probe fails, so this also proves the
            // fallback: the request goes to the (unreachable) public address.
            let server = Server::new("https://spoty.invalid", "sai", &lan).unwrap();
            assert_eq!(server.route().await, "tunnel");
            let err = server.search("bài", 10).await.unwrap_err();
            assert!(err.contains("mạng") || err.contains("không trả lời"), "{err}");
        });
    }
}
