//! "Nhận nhạc qua WiFi": a small web server so a phone on the same network can
//! drop music straight into `music_dir`.
//!
//! It runs only while the user is on that screen, on the LAN, and it accepts
//! nothing but audio files, saved through the same name cleaning the downloader
//! uses. The page uploads each file as a raw `PUT` body — our own page is the
//! only client, so there is no multipart parsing to get wrong.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::download::{free_space, safe_name, unique_path};
use crate::ui::UiMsg;

pub const PORT: u16 = 8080;
/// Room to leave on the card after a file lands.
const KEEP_FREE: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub enum WifiEvent {
    /// The address to type on the phone.
    Started(String),
    Received { name: String, size: u64 },
    Error(String),
    Stopped,
}

/// A running server. Dropping it, or calling `stop`, shuts the socket down.
pub struct Wifi {
    pub url: String,
    stop: Option<oneshot::Sender<()>>,
    /// What has arrived this session, newest last.
    pub files: Vec<(String, u64)>,
}

impl Wifi {
    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// A stopped server with pre-filled state, for the screenshot tool.
pub fn preview(url: &str, files: Vec<(String, u64)>) -> Wifi {
    Wifi { url: url.to_string(), stop: None, files }
}

impl Drop for Wifi {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The address the phone should open. Asking the routing table which interface
/// would reach the internet gives the LAN address without listing interfaces;
/// no packet is actually sent.
fn lan_ip() -> Option<IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    match ip {
        IpAddr::V4(v4) if v4 != Ipv4Addr::UNSPECIFIED && !v4.is_loopback() => Some(ip),
        _ => None,
    }
}

/// Starts the server on the app's runtime.
pub fn start(rt: &tokio::runtime::Handle, dir: PathBuf, ui: Sender<UiMsg>) -> Wifi {
    start_on(rt, dir, ui, PORT)
}

fn start_on(rt: &tokio::runtime::Handle, dir: PathBuf, ui: Sender<UiMsg>, port: u16) -> Wifi {
    let (stop_tx, mut stop_rx) = oneshot::channel();
    let url = match lan_ip() {
        Some(ip) => format!("http://{ip}:{port}"),
        None => format!("http://<địa chỉ IP của máy>:{port}"),
    };
    let started = url.clone();
    rt.spawn(async move {
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                emit(&ui, WifiEvent::Error(format!("không mở được cổng {port}: {e}")));
                return;
            }
        };
        emit(&ui, WifiEvent::Started(started));
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let (dir, ui) = (dir.clone(), ui.clone());
                    tokio::spawn(async move {
                        let service = service_fn(move |req| route(req, dir.clone(), ui.clone()));
                        let _ = http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        }
        emit(&ui, WifiEvent::Stopped);
    });
    Wifi { url, stop: Some(stop_tx), files: Vec::new() }
}

fn emit(ui: &Sender<UiMsg>, ev: WifiEvent) {
    let _ = ui.send(UiMsg::Wifi(ev));
}

fn text(status: StatusCode, body: &str, kind: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", kind)
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap_or_default()
}

async fn route(
    req: Request<Incoming>,
    dir: PathBuf,
    ui: Sender<UiMsg>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    Ok(match (req.method(), path.as_str()) {
        (&Method::GET, "/") => text(StatusCode::OK, PAGE, "text/html; charset=utf-8"),
        (&Method::PUT, "/upload") | (&Method::POST, "/upload") => {
            match upload(req, &query, &dir, &ui).await {
                Ok(msg) => text(StatusCode::OK, &msg, "application/json"),
                Err(e) => {
                    let body = format!("{{\"error\":{}}}", json_string(&e));
                    emit(&ui, WifiEvent::Error(e));
                    text(StatusCode::BAD_REQUEST, &body, "application/json")
                }
            }
        }
        _ => text(StatusCode::NOT_FOUND, "không có gì ở đây", "text/plain; charset=utf-8"),
    })
}

/// Saves one uploaded file. The name comes from `?name=`, cleaned the same way
/// as a Soulseek download: basename only, no folders, audio extensions only.
async fn upload(
    req: Request<Incoming>,
    query: &str,
    dir: &std::path::Path,
    ui: &Sender<UiMsg>,
) -> Result<String, String> {
    let raw = param(query, "name").ok_or("thiếu tên file")?;
    let name = safe_name(&raw).ok_or_else(|| format!("không nhận file này: {raw}"))?;
    let declared: u64 = req
        .headers()
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if let Some(free) = free_space(dir) {
        if declared > 0 && free < declared + KEEP_FREE {
            return Err(format!("thẻ nhớ còn {} MB, không đủ chỗ", free / 1_048_576));
        }
    }
    let dest = unique_path(dir, &name);
    let mut part = dest.clone().into_os_string();
    part.push(".part");
    let part = PathBuf::from(part);

    let mut file = std::fs::File::create(&part).map_err(|e| format!("không tạo được file: {e}"))?;
    let mut body = req.into_body();
    let mut written = 0u64;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| {
            let _ = std::fs::remove_file(&part);
            format!("mất kết nối: {e}")
        })?;
        if let Ok(data) = frame.into_data() {
            use std::io::Write;
            if let Err(e) = file.write_all(&data) {
                let _ = std::fs::remove_file(&part);
                return Err(format!("không ghi được file: {e}"));
            }
            written += data.len() as u64;
        }
    }
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    // Rename once it is whole, so a library scan never sees a half file.
    std::fs::rename(&part, &dest).map_err(|e| format!("không lưu được file: {e}"))?;
    let saved = dest
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or(name);
    emit(ui, WifiEvent::Received { name: saved.clone(), size: written });
    Ok(format!("{{\"saved\":{},\"size\":{written}}}", json_string(&saved)))
}

fn param(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| percent_decode(v))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The upload page, served from memory. Same palette as the app.
const PAGE: &str = r##"<!doctype html>
<html lang="vi">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<title>Spoty — Nhận nhạc qua WiFi</title>
<style>
  :root { color-scheme: dark; --bg:#121212; --card:#1c1c1c; --line:#333; --text:#fff; --sub:#b3b3b3; --accent:#1ed760; --err:#f15e6c; }
  * { box-sizing: border-box; }
  body { margin:0; padding:24px 16px 48px; background:var(--bg); color:var(--text);
         font:16px/1.5 -apple-system, "Segoe UI", Roboto, sans-serif; }
  .wrap { max-width:560px; margin:0 auto; display:grid; gap:20px; }
  h1 { font-size:22px; margin:0; display:flex; align-items:center; gap:10px; }
  .dot { width:12px; height:12px; border-radius:50%; background:var(--accent); }
  #drop { border:2px dashed var(--line); border-radius:16px; padding:40px 20px; text-align:center;
          background:var(--card); transition:border-color .15s, background .15s; }
  #drop.over { border-color:var(--accent); background:#18251c; }
  #drop p { margin:0 0 6px; font-size:18px; }
  #drop small { color:var(--sub); }
  button { margin-top:18px; padding:12px 22px; font-size:16px; font-weight:600; border:0; border-radius:999px;
           background:var(--accent); color:#06180d; cursor:pointer; }
  ul { list-style:none; margin:0; padding:0; display:grid; gap:10px; }
  li { background:var(--card); border:1px solid var(--line); border-radius:12px; padding:12px 14px; }
  .row { display:flex; justify-content:space-between; gap:12px; font-size:15px; }
  .sub { color:var(--sub); font-size:13px; white-space:nowrap; }
  .bar { height:6px; border-radius:3px; background:#333; margin-top:10px; overflow:hidden; }
  .bar i { display:block; height:100%; width:0; background:var(--accent); transition:width .2s; }
  .done .sub { color:var(--accent); }
  .fail .sub { color:var(--err); }
  footer { color:var(--sub); font-size:13px; text-align:center; }
</style>
<div class="wrap">
  <h1><span class="dot"></span> Spoty — Nhận nhạc qua WiFi</h1>
  <div id="drop">
    <p>Kéo thả file nhạc vào đây</p>
    <small>FLAC · ALAC · WAV · AIFF · MP3 · AAC · OGG · Opus</small><br>
    <button type="button" id="pick">Chọn file</button>
    <input id="file" type="file" multiple accept=".flac,.wav,.aiff,.aif,.m4a,.mp3,.ogg,.opus,audio/*" hidden>
  </div>
  <ul id="list"></ul>
  <footer>File được lưu thẳng vào thư mục nhạc của Spoty. Giữ màn hình này mở trên máy cho tới khi tải xong.</footer>
</div>
<script>
  const drop = document.getElementById('drop');
  const input = document.getElementById('file');
  const list = document.getElementById('list');
  const queue = [];
  let busy = false;

  document.getElementById('pick').onclick = () => input.click();
  input.onchange = () => add(input.files);
  ['dragenter', 'dragover'].forEach(e => drop.addEventListener(e, ev => {
    ev.preventDefault(); drop.classList.add('over');
  }));
  ['dragleave', 'drop'].forEach(e => drop.addEventListener(e, ev => {
    ev.preventDefault(); drop.classList.remove('over');
  }));
  drop.addEventListener('drop', ev => add(ev.dataTransfer.files));

  function add(files) {
    for (const f of files) {
      const li = document.createElement('li');
      li.innerHTML = '<div class="row"><span></span><span class="sub">đang chờ</span></div>'
                   + '<div class="bar"><i></i></div>';
      li.querySelector('span').textContent = f.name;
      list.appendChild(li);
      queue.push({ file: f, li });
    }
    next();
  }

  function next() {
    if (busy || !queue.length) return;
    busy = true;
    const { file, li } = queue.shift();
    const sub = li.querySelector('.sub');
    const bar = li.querySelector('.bar i');
    const xhr = new XMLHttpRequest();
    xhr.open('PUT', '/upload?name=' + encodeURIComponent(file.name));
    xhr.upload.onprogress = e => {
      if (!e.lengthComputable) return;
      const pct = Math.round(e.loaded * 100 / e.total);
      bar.style.width = pct + '%';
      sub.textContent = pct + '%';
    };
    xhr.onload = () => {
      let msg = 'đã lưu';
      try { const r = JSON.parse(xhr.responseText); if (r.error) msg = r.error; } catch (e) {}
      const ok = xhr.status === 200;
      li.classList.add(ok ? 'done' : 'fail');
      sub.textContent = msg;
      bar.style.width = ok ? '100%' : '0';
      busy = false; next();
    };
    xhr.onerror = () => {
      li.classList.add('fail');
      sub.textContent = 'lỗi kết nối';
      busy = false; next();
    };
    xhr.send(file);
  }
</script>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_query_names() {
        let q = "name=01%20-%20%C4%90%E1%BB%ABng.flac";
        assert_eq!(param(q, "name").as_deref(), Some("01 - Đừng.flac"));
        assert_eq!(param("name=a+b.flac", "name").as_deref(), Some("a b.flac"));
        assert_eq!(param("other=x", "name"), None);
        // A truncated escape stays literal instead of panicking.
        assert_eq!(param("name=a%2", "name").as_deref(), Some("a%2"));
    }

    #[test]
    fn escapes_json_replies() {
        assert_eq!(json_string("Bài \"hay\"\\."), "\"Bài \\\"hay\\\"\\\\.\"");
    }

    /// The real thing: start the server, PUT a file at it over a socket, and
    /// check what lands in the folder.
    #[test]
    fn accepts_an_upload_and_refuses_the_rest() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::time::Duration;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("spoty-wifi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();

        // Any free port will do; the app itself always uses 8080.
        let mut server = None;
        for port in 18080..18090u16 {
            let w = start_on(rt.handle(), dir.clone(), tx.clone(), port);
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(UiMsg::Wifi(WifiEvent::Started(_))) => {
                    server = Some((w, port));
                    break;
                }
                _ => continue,
            }
        }
        let (mut server, port) = server.expect("mở được cổng");

        let put = |name: &str, body: &[u8]| -> String {
            let head = format!(
                "PUT /upload?name={name} HTTP/1.1\r\nHost: test\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
            sock.write_all(head.as_bytes()).unwrap();
            sock.write_all(body).unwrap();
            let mut resp = String::new();
            sock.read_to_string(&mut resp).unwrap();
            resp
        };

        let body = b"fLaC-not-really-but-bytes";
        let resp = put("01%20-%20B%C3%A0i%20h%C3%A1t.flac", body);
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert_eq!(std::fs::read(dir.join("01 - Bài hát.flac")).unwrap(), body);
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(UiMsg::Wifi(WifiEvent::Received { name, size })) => {
                assert_eq!(name, "01 - Bài hát.flac");
                assert_eq!(size, body.len() as u64);
            }
            other => panic!("thiếu sự kiện Received: {}", other.is_ok()),
        }

        // A second copy is kept beside the first, never on top of it.
        put("01%20-%20B%C3%A0i%20h%C3%A1t.flac", body);
        assert!(dir.join("01 - Bài hát (1).flac").exists());

        // Anything that is not music is refused, folders and all.
        for name in ["script.sh", "..%2F..%2Fetc%2Fpasswd", "x.flac%00.sh"] {
            let resp = put(name, b"x");
            assert!(resp.starts_with("HTTP/1.1 400"), "{name}: {resp}");
        }
        assert!(!dir.join("script.sh").exists());
        let saved: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(saved.len(), 2, "chỉ có 2 file nhạc được lưu");

        server.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_is_self_contained() {
        // Everything is inline: the device is the only server the phone can reach.
        assert!(!PAGE.contains("http://"), "trang không được gọi ra ngoài");
        assert!(!PAGE.contains("https://"), "trang không được gọi ra ngoài");
        assert!(PAGE.contains("/upload?name="));
    }
}
