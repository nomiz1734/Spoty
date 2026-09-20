//! Over-the-air updates.
//!
//! The app reads a small JSON manifest (for example a GitHub release asset at
//! `https://github.com/<you>/<repo>/releases/latest/download/update.json`):
//!
//! ```json
//! { "version": "0.2.0", "notes": "…", "file": "spoty-update.tar.gz",
//!   "sha256": "…", "size": 5123456 }
//! ```
//!
//! `file` is relative to the manifest URL (or use an absolute `url`). The package
//! is downloaded over HTTPS, checked against the SHA-256, unpacked next to the app
//! (settings and data are kept), and the old binary is kept as `spoty.old`.
//! launch.sh restarts the app, and rolls back if the new version fails to start.

use std::io::Write;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::header::{CONTENT_LENGTH, LOCATION};
use hyper::{Method, Request, Response};
use librespot_core::http_client::HttpClient;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Exit code that makes launch.sh start the (new) app again.
pub const RESTART_EXIT_CODE: i32 = 42;

#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub enum UpdateState {
    Checking,
    UpToDate,
    Available(UpdateInfo),
    Downloading { done: u64, total: u64 },
    Installing,
    /// Installed; restart to use it.
    Ready { version: String },
    Failed(String),
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    url: Option<String>,
    sha256: String,
    #[serde(default)]
    size: u64,
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Compares dotted versions numerically ("0.10.0" > "0.9.3").
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['.', '-', '+'])
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parse(candidate), parse(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// GET with redirects (GitHub release downloads always redirect).
async fn get(client: &HttpClient, url: &str) -> Result<Response<Incoming>, String> {
    let mut url = url.to_string();
    for _ in 0..8 {
        if !url.starts_with("https://") {
            return Err("chỉ cho phép cập nhật qua https".into());
        }
        let req = Request::builder()
            .method(Method::GET)
            .uri(&url)
            .body(Bytes::new())
            .map_err(|e| e.to_string())?;
        let resp = client
            .request_fut(req)
            .map_err(|e| e.to_string())?
            .await
            .map_err(|e| format!("không kết nối được: {e}"))?;
        let status = resp.status();
        if status.is_redirection() {
            let loc = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or("redirect không có Location")?;
            url = crate::net::resolve(&url, loc);
            continue;
        }
        if !status.is_success() {
            return Err(format!("máy chủ trả lỗi HTTP {}", status.as_u16()));
        }
        return Ok(resp);
    }
    Err("quá nhiều lần chuyển hướng".into())
}

pub async fn check(manifest_url: &str) -> Result<Option<UpdateInfo>, String> {
    let client = HttpClient::new(None);
    let resp = get(&client, manifest_url).await?;
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    let m: Manifest =
        serde_json::from_slice(&body).map_err(|e| format!("update.json không hợp lệ: {e}"))?;
    if !is_newer(&m.version, current_version()) {
        return Ok(None);
    }
    let url = match (m.url, m.file) {
        (Some(u), _) => u,
        (None, Some(f)) => crate::net::resolve(manifest_url, &f),
        (None, None) => return Err("update.json thiếu \"file\" hoặc \"url\"".into()),
    };
    Ok(Some(UpdateInfo {
        version: m.version,
        notes: m.notes,
        url,
        sha256: m.sha256.to_ascii_lowercase(),
        size: m.size,
    }))
}

/// Downloads the package to `dest`, verifying its SHA-256.
pub async fn download(
    info: &UpdateInfo,
    dest: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<(), String> {
    let client = HttpClient::new(None);
    let resp = get(&client, &info.url).await?;
    let total = resp
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(info.size);
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut body = resp.into_body();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| format!("tải bị gián đoạn: {e}"))?;
        if let Ok(data) = frame.into_data() {
            hasher.update(&data);
            file.write_all(&data).map_err(|e| e.to_string())?;
            done += data.len() as u64;
            progress(done, total);
        }
    }
    file.sync_all().map_err(|e| e.to_string())?;
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if digest != info.sha256 {
        let _ = std::fs::remove_file(dest);
        return Err("file tải về bị hỏng (sai SHA-256)".into());
    }
    Ok(())
}

fn update_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("update")
}

pub fn package_path(data_dir: &Path) -> PathBuf {
    update_dir(data_dir).join("package.tar.gz")
}

fn files_under(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                files_under(&p, base, out);
            } else if let Ok(rel) = p.strip_prefix(base) {
                out.push(rel.to_path_buf());
            }
        }
    }
}

/// Unpacks a verified package over the app directory. Blocking.
pub fn install(pkg: &Path, app_dir: &Path, data_dir: &Path, version: &str) -> Result<(), String> {
    if cfg!(any(feature = "desktop", not(target_os = "linux"))) {
        return Err("chỉ cài được bản cập nhật trên máy TrimUI".into());
    }
    install_files(pkg, app_dir, data_dir, version)
}

/// The platform-independent part of `install` (also used by `--install-test`).
pub fn install_files(pkg: &Path, app_dir: &Path, data_dir: &Path, version: &str) -> Result<(), String> {
    let staging = update_dir(data_dir).join("staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let file = std::fs::File::open(pkg).map_err(|e| e.to_string())?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    archive.set_preserve_permissions(false);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        // unpack_in refuses paths that escape the staging directory.
        entry
            .unpack_in(&staging)
            .map_err(|e| format!("giải nén lỗi: {e}"))?;
    }

    // The new binary must be an ARM64 Linux executable.
    let bin = staging.join("spoty");
    let head = std::fs::read(&bin)
        .map(|b| b.into_iter().take(20).collect::<Vec<u8>>())
        .map_err(|_| "gói cập nhật thiếu file spoty".to_string())?;
    let is_arm64_elf = head.len() == 20
        && head.starts_with(b"\x7fELF")
        && u16::from_le_bytes([head[18], head[19]]) == 0xB7;
    if !is_arm64_elf {
        return Err("gói cập nhật không đúng kiến trúc ARM64".into());
    }

    let mut files = Vec::new();
    files_under(&staging, &staging, &mut files);
    for rel in files {
        let rel_s = rel.to_string_lossy().replace('\\', "/");
        if rel_s == "settings.json" || rel_s.starts_with("data/") {
            continue;
        }
        let src = staging.join(&rel);
        let dst = app_dir.join(&rel);
        if let Some(dir) = dst.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        if dst.exists() {
            if rel_s == "spoty" {
                let old = app_dir.join("spoty.old");
                let _ = std::fs::remove_file(&old);
                std::fs::rename(&dst, &old).map_err(|e| format!("sao lưu spoty: {e}"))?;
            } else {
                // FAT does not always allow renaming over an existing file.
                let _ = std::fs::remove_file(&dst);
            }
        }
        if std::fs::rename(&src, &dst).is_err() {
            std::fs::copy(&src, &dst).map_err(|e| format!("ghi {rel_s}: {e}"))?;
        }
    }
    std::fs::write(update_dir(data_dir).join("pending"), version).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(pkg);
    log::info!("update {version} installed");
    Ok(())
}

/// If this run is the first after an update, returns its version.
pub fn just_updated(data_dir: &Path) -> Option<String> {
    std::fs::read_to_string(update_dir(data_dir).join("pending"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Marks the new version as good, so launch.sh won't roll it back.
pub fn confirm(data_dir: &Path) {
    let _ = std::fs::remove_file(update_dir(data_dir).join("pending"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("v1.0", "0.9"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
    }

}
