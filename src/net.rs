//! The app's HTTPS client, shared by the home feed and music downloads.
//!
//! One connection pool, one place that follows redirects, and one file
//! downloader that can resume with a `Range` header. Every request is bounded
//! by a timeout: hyper has none of its own, and a server that accepts the
//! connection then goes quiet would otherwise hang the screen for ever.

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{HeaderName, HeaderValue, CONTENT_LENGTH, CONTENT_RANGE, LOCATION};
use hyper::{Method, Request};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

type Https = hyper_rustls::HttpsConnector<HttpConnector>;

/// Whole-request budget for small JSON calls.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Allowed gap between two chunks while a file is downloading.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(45);

fn client() -> &'static Client<Https, Full<Bytes>> {
    static CLIENT: OnceLock<Client<Https, Full<Bytes>>> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .build();
        Client::builder(TokioExecutor::new()).build(https)
    })
}

/// Resolves `reference` (absolute, root-relative or relative) against `base`.
pub fn resolve(base: &str, reference: &str) -> String {
    if reference.starts_with("https://") || reference.starts_with("http://") {
        return reference.to_string();
    }
    if let Some(rest) = reference.strip_prefix('/') {
        // Absolute path on the same host.
        let host_end = base
            .find("://")
            .and_then(|i| base[i + 3..].find('/').map(|j| i + 3 + j))
            .unwrap_or(base.len());
        return format!("{}/{}", &base[..host_end], rest);
    }
    let dir = match base.rfind('/') {
        Some(i) => &base[..=i],
        None => base,
    };
    format!("{dir}{reference}")
}

/// Percent-encodes a path so names with spaces or Vietnamese letters survive.
pub fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 8);
    for b in path.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn build(
    method: &Method,
    url: &str,
    headers: &[(&str, String)],
    body: Option<&[u8]>,
) -> Result<Request<Full<Bytes>>, String> {
    let mut req = Request::builder().method(method.clone()).uri(url);
    let h = req.headers_mut().ok_or("địa chỉ không hợp lệ")?;
    for (k, v) in headers {
        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|e| e.to_string())?;
        let value = HeaderValue::from_str(v).map_err(|e| e.to_string())?;
        h.insert(name, value);
    }
    req.body(Full::new(Bytes::copy_from_slice(body.unwrap_or_default())))
        .map_err(|e| e.to_string())
}

/// One request, following redirects for GETs. Returns the status and the body.
pub async fn fetch(
    method: Method,
    url: &str,
    headers: &[(&str, String)],
    body: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<(u16, Bytes), String> {
    let mut url = url.to_string();
    for _ in 0..6 {
        let req = build(&method, &url, headers, body.as_deref())?;
        let resp = tokio::time::timeout(timeout, client().request(req))
            .await
            .map_err(|_| "máy chủ không trả lời".to_string())?
            .map_err(|e| format!("mạng: {e}"))?;
        let status = resp.status();
        if status.is_redirection() && method == Method::GET {
            if let Some(loc) = resp.headers().get(LOCATION).and_then(|v| v.to_str().ok()) {
                url = resolve(&url, loc);
                continue;
            }
        }
        let bytes = tokio::time::timeout(timeout, resp.into_body().collect())
            .await
            .map_err(|_| "máy chủ ngắt giữa chừng".to_string())?
            .map_err(|e| e.to_string())?
            .to_bytes();
        return Ok((status.as_u16(), bytes));
    }
    Err("quá nhiều lần chuyển hướng".into())
}

/// Streams a GET into `dest`.
///
/// With `resume_from > 0` it asks for the rest of the file and appends, so a
/// download that dropped out picks up where it stopped. `progress` is called
/// with (bytes on disk, total) and returns false to abort. Returns the total
/// size of the file on disk.
pub async fn get_to_file(
    url: &str,
    headers: &[(&str, String)],
    dest: &Path,
    resume_from: u64,
    mut progress: impl FnMut(u64, u64) -> bool,
) -> Result<u64, String> {
    let mut url = url.to_string();
    let mut headers = headers.to_vec();
    if resume_from > 0 {
        headers.push(("range", format!("bytes={resume_from}-")));
    }
    let resp = loop {
        let req = build(&Method::GET, &url, &headers, None)?;
        let resp = tokio::time::timeout(CALL_TIMEOUT, client().request(req))
            .await
            .map_err(|_| "máy chủ không trả lời".to_string())?
            .map_err(|e| format!("mạng: {e}"))?;
        if resp.status().is_redirection() {
            match resp.headers().get(LOCATION).and_then(|v| v.to_str().ok()) {
                Some(loc) => {
                    url = resolve(&url, loc);
                    continue;
                }
                None => return Err("redirect không có Location".into()),
            }
        }
        break resp;
    };
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("máy chủ trả lỗi HTTP {}", status.as_u16()));
    }
    // 206 means the server honoured the Range; 200 means it sent the whole file
    // again, so start over from the beginning.
    let resuming = resume_from > 0 && status.as_u16() == 206;
    let body_len = resp
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let total = resp
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit('/').next().and_then(|t| t.parse::<u64>().ok()))
        .or_else(|| body_len.map(|n| if resuming { n + resume_from } else { n }))
        .unwrap_or(0);

    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(!resuming)
        .open(dest)
        .map_err(|e| e.to_string())?;
    let mut done = 0u64;
    if resuming {
        done = resume_from;
        file.seek(SeekFrom::Start(resume_from)).map_err(|e| e.to_string())?;
    }
    let mut body = resp.into_body();
    loop {
        let frame = tokio::time::timeout(CHUNK_TIMEOUT, body.frame())
            .await
            .map_err(|_| "tải bị treo".to_string())?;
        let Some(frame) = frame else { break };
        let frame = frame.map_err(|e| format!("tải bị gián đoạn: {e}"))?;
        if let Ok(data) = frame.into_data() {
            file.write_all(&data).map_err(|e| e.to_string())?;
            done += data.len() as u64;
            if !progress(done, total) {
                return Err("đã hủy".into());
            }
        }
    }
    file.sync_all().map_err(|e| e.to_string())?;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::{encode_path, resolve};

    #[test]
    fn relative_urls() {
        let base = "https://host/a/b/update.json";
        assert_eq!(resolve(base, "https://x/y.tar.gz"), "https://x/y.tar.gz");
        assert_eq!(resolve(base, "/c/d.tar.gz"), "https://host/c/d.tar.gz");
        assert_eq!(resolve(base, "d.tar.gz"), "https://host/a/b/d.tar.gz");
    }

    /// Real HTTPS, redirects and a resumed Range request, against our own
    /// release asset: `cargo test -- --ignored resumes_a_download`.
    #[test]
    #[ignore = "cần mạng"]
    fn resumes_a_download() {
        use std::path::PathBuf;
        let url = "https://github.com/nomiz1734/Spoty/releases/latest/download/update.json";
        let dest: PathBuf = std::env::temp_dir().join("spoty-net-test.json");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        let n = rt
            .block_on(super::get_to_file(url, &[], &dest, 0, |_, _| true))
            .expect("tải được");
        let whole = std::fs::read(&dest).unwrap();
        assert_eq!(n as usize, whole.len());
        assert!(whole.starts_with(b"{"), "đọc được update.json");

        // Cut it in half, then ask for the rest and check the file is whole again.
        let half = whole.len() as u64 / 2;
        let f = std::fs::OpenOptions::new().write(true).open(&dest).unwrap();
        f.set_len(half).unwrap();
        drop(f);
        let mut seen_start = u64::MAX;
        let n = rt
            .block_on(super::get_to_file(url, &[], &dest, half, |done, _| {
                seen_start = seen_start.min(done);
                true
            }))
            .expect("tải tiếp được");
        assert_eq!(n as usize, whole.len(), "nối tiếp, không tải lại từ đầu");
        assert!(seen_start > half, "tiến trình bắt đầu từ chỗ đã có");
        assert_eq!(std::fs::read(&dest).unwrap(), whole);
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn encodes_paths() {
        assert_eq!(encode_path("a/b c.flac"), "a/b%20c.flac");
        assert_eq!(encode_path("Đừng.flac"), "%C4%90%E1%BB%ABng.flac");
        assert_eq!(encode_path("ok-_.~/x.flac"), "ok-_.~/x.flac");
    }
}
