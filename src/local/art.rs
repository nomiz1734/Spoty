//! Cover art for local files: embedded picture, else cover.jpg/folder.jpg next to it.

use std::path::Path;

use super::decode;
use crate::gfx::image::decode_image;
use crate::gfx::Image;

const NAMES: &[&str] = &["cover", "folder", "front", "album", "albumart"];

fn folder_image(path: &Path) -> Option<Vec<u8>> {
    let dir = path.parent()?;
    let mut candidates: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let stem = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            matches!(ext.as_str(), "jpg" | "jpeg" | "png") && NAMES.contains(&stem.as_str())
        })
        .collect();
    candidates.sort_by_key(|p| {
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase();
        NAMES.iter().position(|n| *n == stem).unwrap_or(99)
    });
    std::fs::read(candidates.first()?).ok()
}

pub fn load(path: &Path, size: u32) -> Option<Image> {
    let embedded = decode::open(path, true)
        .ok()
        .and_then(|o| decode::best_visual(&o.visuals).map(|v| v.data.to_vec()));
    let bytes = embedded.or_else(|| folder_image(path))?;
    match decode_image(&bytes, size as usize) {
        Ok(img) => Some(img),
        Err(e) => {
            log::debug!("cover {}: {e}", path.display());
            None
        }
    }
}
