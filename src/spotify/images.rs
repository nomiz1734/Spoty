//! Cover art: disk cache on the SD card, HTTP fetch, decode + resize off the UI thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use librespot_core::Session;
use sha1::{Digest, Sha1};

use crate::gfx::image::decode_cover;
use crate::gfx::Image;

const CACHE_LIMIT_BYTES: u64 = 150 * 1024 * 1024;

fn cache_path(dir: &Path, url: &str) -> PathBuf {
    let digest = Sha1::digest(url.as_bytes());
    let name: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{name}.jpg"))
}

pub async fn load(session: Session, cache_dir: PathBuf, url: String, size: u32) -> Option<Arc<Image>> {
    let path = cache_path(&cache_dir, &url);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => {
            let data = match session.spclient().request_url(&url).await {
                Ok(d) => d.to_vec(),
                Err(e) => {
                    log::debug!("image {url}: {e}");
                    return None;
                }
            };
            let _ = tokio::fs::write(&path, &data).await;
            data
        }
    };
    let decoded =
        tokio::task::spawn_blocking(move || decode_cover(&bytes, size as usize)).await;
    match decoded {
        Ok(Ok(img)) => Some(Arc::new(img)),
        Ok(Err(e)) => {
            log::debug!("decode {url}: {e}");
            let _ = std::fs::remove_file(&path);
            None
        }
        Err(_) => None,
    }
}

/// Keeps the image cache under its size limit by deleting the oldest files.
pub fn trim_cache(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();
    let total: u64 = files.iter().map(|f| f.1).sum();
    if total <= CACHE_LIMIT_BYTES {
        return;
    }
    files.sort_by_key(|f| f.0);
    let mut excess = total - CACHE_LIMIT_BYTES * 3 / 4;
    for (_, len, path) in files {
        if excess == 0 {
            break;
        }
        let _ = std::fs::remove_file(path);
        excess = excess.saturating_sub(len);
    }
}
