//! REST client for a self-hosted [slskd](https://github.com/slskd/slskd), the
//! Soulseek daemon running on the user's own server.
//!
//! The flow is: create a search, wait for it, pick a file, ask slskd to fetch it
//! from the peer, wait for that transfer, then pull the finished file off the
//! server over plain HTTPS. slskd itself has no endpoint that serves file
//! contents, so nginx publishes its downloads folder at `/files/` behind the
//! same API key (see implementation_plan.md, 1.5).
//!
//! Responses are read as loose JSON rather than typed structs: field sets move
//! between slskd releases, and a missing field should cost one detail, not the
//! whole search.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hyper::Method;
use serde_json::{json, Value};

use crate::net;

/// Extensions we accept. Anything else a peer offers is ignored.
const AUDIO: [&str; 8] = ["flac", "wav", "aiff", "aif", "m4a", "mp3", "ogg", "opus"];
/// Formats that keep every sample. `m4a` can be ALAC or AAC, so it is not here.
const LOSSLESS: [&str; 4] = ["flac", "wav", "aiff", "aif"];

/// How long to wait for a peer to actually send the file.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(20 * 60);

pub fn is_audio(ext: &str) -> bool {
    AUDIO.contains(&ext)
}

#[derive(Clone, Debug)]
pub struct SearchResult {
    pub username: String,
    /// The path on the peer's machine, exactly as Soulseek reports it.
    pub filename: String,
    pub size: u64,
    pub bit_depth: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bitrate: Option<u32>,
    pub length_s: Option<u32>,
    pub is_lossless: bool,
    /// Peer upload speed in bytes per second.
    pub speed: Option<u64>,
    pub free_slot: bool,
    pub queue: u32,
}

impl SearchResult {
    /// The file name without the peer's folders.
    pub fn base_name(&self) -> &str {
        self.filename.rsplit(['\\', '/']).next().unwrap_or(&self.filename)
    }

    pub fn ext(&self) -> String {
        self.base_name().rsplit('.').next().unwrap_or("").to_ascii_lowercase()
    }

    /// "FLAC 16-bit/44.1 kHz · 32 MB", with whatever the peer reported.
    pub fn quality(&self) -> String {
        let mut s = self.ext().to_uppercase();
        match (self.bit_depth, self.sample_rate) {
            (Some(bits), Some(rate)) => s.push_str(&format!(" {bits}-bit/{}", khz(rate))),
            (None, Some(rate)) => s.push_str(&format!(" {}", khz(rate))),
            _ => {
                if let Some(kbps) = self.bitrate {
                    s.push_str(&format!(" {kbps}kbps"));
                }
            }
        }
        if let Some(secs) = self.length_s {
            s.push_str(&format!(" · {}:{:02}", secs / 60, secs % 60));
        }
        format!("{s} · {}", mb(self.size))
    }

    /// Best first: a free slot, then lossless, then a fast peer, then a short queue.
    fn rank(&self) -> (u8, u8, u64, i64) {
        (
            u8::from(!self.free_slot),
            u8::from(!self.is_lossless),
            u64::MAX - self.speed.unwrap_or(0),
            self.queue as i64,
        )
    }
}

fn khz(rate: u32) -> String {
    let v = rate as f32 / 1000.0;
    let s = format!("{v:.1}");
    format!("{} kHz", s.trim_end_matches(".0"))
}

fn mb(size: u64) -> String {
    format!("{:.0} MB", size as f64 / 1_048_576.0)
}

fn num(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(|x| x.as_u64()).filter(|n| *n > 0)
}

/// Encodes one path segment, including any slashes inside it.
fn segment(s: &str) -> String {
    net::encode_path(s).replace('/', "%2F")
}

fn b64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Reads the flat list of files out of a search-responses payload.
///
/// Accepts either the array from `/searches/{id}/responses` or the object from
/// `/searches/{id}?includeResponses=true`.
pub fn parse_responses(v: &Value, limit: usize) -> Vec<SearchResult> {
    let list = match v {
        Value::Array(a) => a.clone(),
        _ => v
            .get("responses")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default(),
    };
    let mut out = Vec::new();
    for resp in &list {
        let username = resp.get("username").and_then(|u| u.as_str()).unwrap_or("").to_string();
        if username.is_empty() {
            continue;
        }
        let speed = num(resp, "uploadSpeed");
        let free_slot = resp.get("hasFreeUploadSlot").and_then(|b| b.as_bool()).unwrap_or(false);
        let queue = num(resp, "queueLength").unwrap_or(0) as u32;
        let files = resp.get("files").and_then(|f| f.as_array()).cloned().unwrap_or_default();
        for f in &files {
            let filename = f.get("filename").and_then(|s| s.as_str()).unwrap_or("").to_string();
            let size = f.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
            if filename.is_empty() || size == 0 {
                continue;
            }
            let ext = filename.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            if !is_audio(&ext) {
                continue;
            }
            out.push(SearchResult {
                username: username.clone(),
                filename,
                size,
                bit_depth: num(f, "bitDepth").map(|n| n as u32),
                sample_rate: num(f, "sampleRate").map(|n| n as u32),
                bitrate: num(f, "bitRate").map(|n| n as u32),
                length_s: num(f, "length").map(|n| n as u32),
                is_lossless: LOSSLESS.contains(&ext.as_str()),
                speed,
                free_slot,
                queue,
            });
        }
    }
    out.sort_by_key(|r| r.rank());
    out.truncate(limit);
    out
}

/// One slskd server, addressed through nginx.
#[derive(Clone)]
pub struct Server {
    /// Reachable from anywhere, e.g. through a Cloudflare Tunnel. Always https.
    public: String,
    /// The same server on the home network, tried first when set.
    lan: Option<String>,
    key: String,
    /// Whether the LAN address answered, and when that was checked.
    lan_state: Arc<Mutex<Option<(bool, Instant)>>>,
}

/// How long a LAN check is trusted before asking again.
const LAN_OK_FOR: Duration = Duration::from_secs(60);
const LAN_DOWN_FOR: Duration = Duration::from_secs(20);
/// At home the NAS answers in milliseconds; away, give up quickly.
const LAN_PROBE: Duration = Duration::from_millis(1500);

impl Server {
    /// `None` until both the URL and the API key are set in settings.json.
    ///
    /// The public URL must be `https://`: the API key travels on every request.
    /// The LAN URL may be plain `http://`, but only to a private address.
    pub fn new(url: &str, key: &str, lan_url: &str) -> Option<Self> {
        let public = url.trim().trim_end_matches('/').to_string();
        let key = key.trim().to_string();
        if public.is_empty() || key.is_empty() {
            return None;
        }
        if !public.starts_with("https://") {
            log::warn!("slskd_url phải bắt đầu bằng https:// (đang là {public})");
            return None;
        }
        let lan = lan_url.trim().trim_end_matches('/').to_string();
        let lan = if lan.is_empty() {
            None
        } else if net::allowed(&lan) {
            Some(lan)
        } else {
            log::warn!("bỏ qua slskd_lan_url {lan}: chỉ dùng https, hoặc http tới địa chỉ trong mạng nhà");
            None
        };
        Some(Self { public, lan, key, lan_state: Arc::new(Mutex::new(None)) })
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("x-api-key", self.key.clone()),
            ("accept", "application/json".to_string()),
            // Cloudflare's bot checks challenge requests that carry no user agent.
            ("user-agent", format!("Spoty/{}", env!("CARGO_PKG_VERSION"))),
        ]
    }

    /// The address to use right now: the LAN one if it answers, else the public one.
    async fn base(&self) -> (String, bool) {
        let Some(lan) = self.lan.clone() else {
            return (self.public.clone(), false);
        };
        let cached = *self.lan_state.lock().unwrap();
        if let Some((ok, at)) = cached {
            let ttl = if ok { LAN_OK_FOR } else { LAN_DOWN_FOR };
            if at.elapsed() < ttl {
                return if ok { (lan, true) } else { (self.public.clone(), false) };
            }
        }
        // An authenticated request, not just a TCP connect: another network
        // can have a different machine at the same private address.
        let probe = format!("{lan}/api/v0/searches");
        let ok = matches!(
            net::fetch(Method::GET, &probe, &self.headers(), None, LAN_PROBE).await,
            Ok((200, _))
        );
        let was = cached.map(|(ok, _)| ok);
        if was != Some(ok) {
            log::info!("slskd: {}", if ok { "dùng đường LAN" } else { "dùng đường tunnel" });
        }
        *self.lan_state.lock().unwrap() = Some((ok, Instant::now()));
        if ok {
            (lan, true)
        } else {
            (self.public.clone(), false)
        }
    }

    /// Forgets that the LAN answered, after a request over it failed.
    fn lan_failed(&self) {
        *self.lan_state.lock().unwrap() = Some((false, Instant::now()));
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<(u16, Value), String> {
        let mut headers = self.headers();
        let payload = body.map(|b| {
            headers.push(("content-type", "application/json".to_string()));
            serde_json::to_vec(&b).unwrap_or_default()
        });
        let (base, on_lan) = self.base().await;
        let url = format!("{base}{path}");
        let result = net::fetch(method.clone(), &url, &headers, payload.clone(), net::CALL_TIMEOUT).await;
        let (status, bytes) = match result {
            Ok(r) => r,
            // Walked out of the house mid-way: try once more through the tunnel.
            Err(_) if on_lan => {
                self.lan_failed();
                let url = format!("{}{path}", self.public);
                net::fetch(method, &url, &headers, payload, net::CALL_TIMEOUT).await?
            }
            Err(e) => return Err(e),
        };
        if status == 401 || status == 403 || (502..=504).contains(&status) || (520..=530).contains(&status) {
            return Err(net::status_message(status));
        }
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        Ok((status, value))
    }

    /// Which way requests go right now, for the self-test.
    pub async fn route(&self) -> &'static str {
        if self.base().await.1 {
            "LAN"
        } else {
            "tunnel"
        }
    }

    /// Checks the server answers and the key works, for the settings screen.
    pub async fn ping(&self) -> Result<(), String> {
        let (status, _) = self.call(Method::GET, "/api/v0/transfers/downloads", None).await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(net::status_message(status))
        }
    }

    /// Runs one search and returns the best matches.
    pub async fn search(&self, text: &str, limit: usize) -> Result<Vec<SearchResult>, String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(Vec::new());
        }
        // Filter on the server: a popular query can match thousands of files and
        // the handheld would spend seconds parsing JSON it then throws away.
        let body = json!({
            "searchText": text,
            "responseLimit": 30,
            "fileLimit": 200,
            "filterResponses": true,
            "minimumPeerUploadSpeed": 100_000,
            "minimumResponseFileCount": 1,
            "searchTimeout": 15_000,
        });
        let (status, created) = self.call(Method::POST, "/api/v0/searches", Some(body)).await?;
        if !(200..300).contains(&status) {
            return Err(format!("không tạo được tìm kiếm (HTTP {status})"));
        }
        let id = created
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("slskd không trả về mã tìm kiếm")?
            .to_string();

        for _ in 0..40 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let (_, s) = self.call(Method::GET, &format!("/api/v0/searches/{id}"), None).await?;
            let done = s.get("isComplete").and_then(|v| v.as_bool()).unwrap_or(false)
                || s.get("state")
                    .and_then(|v| v.as_str())
                    .map(|st| st.contains("Completed"))
                    .unwrap_or(false);
            if done {
                break;
            }
        }

        let (status, v) = self
            .call(Method::GET, &format!("/api/v0/searches/{id}/responses"), None)
            .await?;
        let results = if (200..300).contains(&status) {
            parse_responses(&v, limit)
        } else {
            // Older builds only have the combined endpoint.
            let (_, v) = self
                .call(Method::GET, &format!("/api/v0/searches/{id}?includeResponses=true"), None)
                .await?;
            parse_responses(&v, limit)
        };
        let _ = self.call(Method::DELETE, &format!("/api/v0/searches/{id}"), None).await;
        log::info!("slskd: \"{text}\" -> {} kết quả", results.len());
        Ok(results)
    }

    /// Asks slskd to fetch one file from the peer.
    pub async fn enqueue(&self, r: &SearchResult) -> Result<(), String> {
        let path = format!("/api/v0/transfers/downloads/{}", segment(&r.username));
        let body = json!([{ "filename": r.filename, "size": r.size }]);
        let (status, v) = self.call(Method::POST, &path, Some(body)).await?;
        if (200..300).contains(&status) {
            return Ok(());
        }
        let msg = v
            .get("message")
            .and_then(|m| m.as_str())
            .map(|m| format!(": {m}"))
            .unwrap_or_default();
        Err(format!("không đặt được lệnh tải (HTTP {status}{msg})"))
    }

    /// Waits until the peer has sent the file to the server.
    pub async fn wait_transfer(
        &self,
        r: &SearchResult,
        mut progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<(), String> {
        let start = Instant::now();
        let mut last_state = String::new();
        loop {
            if start.elapsed() > TRANSFER_TIMEOUT {
                return Err("nguồn không gửi file (quá 20 phút)".into());
            }
            let (_, v) = self.call(Method::GET, "/api/v0/transfers/downloads", None).await?;
            if let Some(file) = find_file(&v, &r.filename) {
                let state = file.get("state").and_then(|s| s.as_str()).unwrap_or("");
                if state != last_state {
                    log::info!("slskd: {} -> {state}", r.base_name());
                    last_state = state.to_string();
                }
                if state.contains("Succeeded") {
                    return Ok(());
                }
                for bad in ["Errored", "Cancelled", "Rejected", "TimedOut"] {
                    if state.contains(bad) {
                        return Err(format!("nguồn không gửi được file ({state})"));
                    }
                }
                let done = file.get("bytesTransferred").and_then(|n| n.as_u64()).unwrap_or(0);
                if !progress(done, r.size) {
                    return Err("đã hủy".into());
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Finds where the finished file sits under the server's downloads folder.
    ///
    /// slskd keeps the peer's folder layout, and how much of it survives differs
    /// between versions, so probe the handful of shapes it uses. `HEAD` costs
    /// nothing and the answer is definitive.
    pub async fn locate(&self, r: &SearchResult) -> Result<String, String> {
        let candidates = candidates(&r.username, &r.filename);
        if candidates.is_empty() {
            return Err("tên file rỗng".into());
        }
        let (base, _) = self.base().await;
        let mut tried = Vec::new();
        for path in candidates {
            let url = format!("{base}/files/{}", net::encode_path(&path));
            let (status, _) =
                net::fetch(Method::HEAD, &url, &self.headers(), None, net::CALL_TIMEOUT).await?;
            if (200..300).contains(&status) {
                log::info!("slskd: file nằm ở /files/{path}");
                return Ok(path);
            }
            tried.push(path);
        }
        log::warn!("slskd: không thấy file trên máy chủ, đã thử: {tried:?}");
        Err("không tìm thấy file trên máy chủ (kiểm tra mục /files/ của nginx)".into())
    }

    /// Pulls the finished file off the server into `dest`.
    pub async fn download(
        &self,
        remote: &str,
        dest: &Path,
        resume_from: u64,
        progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<u64, String> {
        let (base, on_lan) = self.base().await;
        let url = format!("{base}/files/{}", net::encode_path(remote));
        let result = net::get_to_file(&url, &self.headers(), dest, resume_from, progress).await;
        if result.is_err() && on_lan {
            // The caller retries from where the file stopped; that attempt
            // goes through the tunnel.
            self.lan_failed();
        }
        result
    }

    /// Frees the copy on the server; the disk there is small. Best effort.
    pub async fn delete_remote(&self, remote: &str) {
        let path = format!("/api/v0/files/downloads/{}", segment(&b64(remote.as_bytes())));
        match self.call(Method::DELETE, &path, None).await {
            Ok((status, _)) if (200..300).contains(&status) => {}
            Ok((status, _)) => log::warn!("không xóa được {remote} trên máy chủ: HTTP {status}"),
            Err(e) => log::warn!("không xóa được {remote} trên máy chủ: {e}"),
        }
    }
}

/// Where a finished file might sit, most likely first: slskd keeps the peer's
/// folder layout, and how much of it survives differs between versions.
fn candidates(username: &str, filename: &str) -> Vec<String> {
    let parts: Vec<&str> = filename
        .split(['\\', '/'])
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .collect();
    let Some(base) = parts.last().copied() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(leaf) = parts.iter().rev().nth(1).copied() {
        out.push(format!("{leaf}/{base}"));
        out.push(format!("{username}/{leaf}/{base}"));
        if let Some(parent) = parts.iter().rev().nth(2).copied() {
            out.push(format!("{parent}/{leaf}/{base}"));
        }
    }
    out.push(base.to_string());
    out.push(format!("{username}/{base}"));
    out
}

/// Finds the transfer entry for `filename` anywhere in the downloads tree.
fn find_file<'a>(v: &'a Value, filename: &str) -> Option<&'a Value> {
    match v {
        Value::Object(map) => {
            if map.get("filename").and_then(|f| f.as_str()) == Some(filename) {
                return Some(v);
            }
            map.values().find_map(|child| find_file(child, filename))
        }
        Value::Array(items) => items.iter().find_map(|child| find_file(child, filename)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONSES: &str = r#"[
      {"username":"slow","uploadSpeed":200000,"hasFreeUploadSlot":true,"queueLength":0,
       "files":[{"filename":"@@a\\Music\\01 - Bài.mp3","size":9000000,"bitRate":320}]},
      {"username":"fast","uploadSpeed":3200000,"hasFreeUploadSlot":true,"queueLength":0,
       "files":[
         {"filename":"@@b\\Nhạc\\01 - Bài.flac","size":32000000,"bitDepth":16,"sampleRate":44100,"length":245},
         {"filename":"@@b\\Nhạc\\cover.jpg","size":120000}]},
      {"username":"busy","uploadSpeed":9000000,"hasFreeUploadSlot":false,"queueLength":12,
       "files":[{"filename":"@@c\\01 - Bài.flac","size":31000000}]}
    ]"#;

    #[test]
    fn parses_and_ranks_results() {
        let v: Value = serde_json::from_str(RESPONSES).unwrap();
        let r = parse_responses(&v, 10);
        // cover.jpg is dropped, the rest is ranked: free slot, lossless, speed.
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].username, "fast");
        assert!(r[0].is_lossless);
        assert_eq!(r[1].username, "slow");
        assert_eq!(r[2].username, "busy", "no free slot goes last");
        assert_eq!(r[0].base_name(), "01 - Bài.flac");
        assert_eq!(r[0].quality(), "FLAC 16-bit/44.1 kHz · 4:05 · 31 MB");
        assert_eq!(r[1].quality(), "MP3 320kbps · 9 MB");
    }

    #[test]
    fn finds_transfer_in_nested_downloads() {
        let v: Value = serde_json::from_str(
            r#"[{"username":"fast","directories":[{"directory":"Nhạc","files":[
                 {"filename":"@@b\\Nhạc\\01 - Bài.flac","state":"InProgress","bytesTransferred":123}]}]}]"#,
        )
        .unwrap();
        let f = find_file(&v, "@@b\\Nhạc\\01 - Bài.flac").expect("found");
        assert_eq!(f["bytesTransferred"], 123);
        assert!(find_file(&v, "khác.flac").is_none());
    }

    #[test]
    fn refuses_plain_http_and_blanks() {
        assert!(Server::new("https://music.example", "k", "").is_some());
        assert!(Server::new("https://music.example/", "k", "").is_some());
        assert!(Server::new("http://music.example", "k", "").is_none(), "API key phải đi qua TLS");
        assert!(Server::new("", "k", "").is_none());
        assert!(Server::new("https://music.example", "  ", "").is_none());
    }

    #[test]
    fn lan_url_only_inside_the_house() {
        let lan = |url: &str| Server::new("https://spoty.nomiz.homes", "k", url).unwrap().lan;
        assert_eq!(lan("http://192.168.1.230:5080/").as_deref(), Some("http://192.168.1.230:5080"));
        assert_eq!(lan("https://nas.nomiz.homes:8443").as_deref(), Some("https://nas.nomiz.homes:8443"));
        assert_eq!(lan(""), None);
        // Plain http to a public address would leak the key: ignored, tunnel only.
        assert_eq!(lan("http://113.23.61.13:5080"), None);
        assert_eq!(lan("http://nas.nomiz.homes:5080"), None);
    }

    #[test]
    fn server_paths_are_probed_in_order() {
        let c = candidates("fast", "@@b\\Nhạc\\Album\\01 - Bài.flac");
        assert_eq!(
            c,
            vec![
                "Album/01 - Bài.flac",
                "fast/Album/01 - Bài.flac",
                "Nhạc/Album/01 - Bài.flac",
                "01 - Bài.flac",
                "fast/01 - Bài.flac",
            ]
        );
        // A bare file name still gives something to try.
        assert_eq!(candidates("u", "x.flac"), vec!["x.flac", "u/x.flac"]);
        assert!(candidates("u", "").is_empty());
        // No traversal ever reaches the URL.
        assert!(candidates("u", "..\\..\\etc\\x.flac").iter().all(|p| !p.contains("..")));
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(b64(b"a"), "YQ==");
        assert_eq!(b64(b"ab"), "YWI=");
        assert_eq!(b64(b"abc"), "YWJj");
        assert_eq!(b64(b"Nh\xe1\xba\xa1c"), "TmjhuqFj");
    }
}
